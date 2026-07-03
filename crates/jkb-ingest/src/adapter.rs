//! Source adapters: turn raw bytes into a [`ParsedDocument`] (title + plain text).
//!
//! Each adapter is a [`SourceAdapter`]; [`parse`] dispatches by file extension. Ships
//! text, Markdown, PDF ([`pdf-extract`]), and HTML ([`scraper`]) adapters. URL
//! ingestion is not an adapter — it renders via a headless browser (so JavaScript
//! runs) and feeds the resulting DOM through [`HtmlAdapter`] (see
//! [`crate::Pipeline::ingest_url`]).

use std::path::Path;

use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};
use scraper::node::Node;
use scraper::{Html, Selector};

use crate::{Error, Result};

/// A parsed source: extracted plain text plus provenance metadata.
#[derive(Debug, Clone)]
pub struct ParsedDocument {
    /// A human title if one could be derived (e.g. the first Markdown heading).
    pub title: Option<String>,
    /// The extracted plain text (what gets chunked, embedded, and FTS-indexed).
    pub text: String,
    /// The source MIME type.
    pub mime: String,
}

/// Turns raw source bytes into a [`ParsedDocument`].
pub trait SourceAdapter {
    /// Whether this adapter handles `path` (by extension).
    fn accepts(&self, path: &Path) -> bool;

    /// Parse `bytes` (the raw source) into a document.
    ///
    /// # Errors
    /// Returns an error if the bytes cannot be parsed as this source type.
    fn parse(&self, bytes: &[u8]) -> Result<ParsedDocument>;
}

/// Plain-text adapter (`.txt` and the fallback for unknown extensions).
pub struct TextAdapter;

impl SourceAdapter for TextAdapter {
    fn accepts(&self, path: &Path) -> bool {
        matches!(extension(path).as_deref(), Some("txt" | "text") | None)
    }

    fn parse(&self, bytes: &[u8]) -> Result<ParsedDocument> {
        Ok(ParsedDocument {
            title: None,
            text: String::from_utf8_lossy(bytes).into_owned(),
            mime: "text/plain".to_owned(),
        })
    }
}

/// Markdown adapter (`.md`/`.markdown`): extracts plain text and the first heading.
pub struct MarkdownAdapter;

impl SourceAdapter for MarkdownAdapter {
    fn accepts(&self, path: &Path) -> bool {
        matches!(extension(path).as_deref(), Some("md" | "markdown"))
    }

    fn parse(&self, bytes: &[u8]) -> Result<ParsedDocument> {
        let source = String::from_utf8_lossy(bytes);
        let (title, text) = markdown_to_text(&source);
        Ok(ParsedDocument {
            title,
            text,
            mime: "text/markdown".to_owned(),
        })
    }
}

/// PDF adapter (`.pdf`): extracts the document's text via [`pdf-extract`]. Scanned
/// (image-only) PDFs yield little text — the pipeline warns on near-empty extraction
/// (OCR is v2).
pub struct PdfAdapter;

impl SourceAdapter for PdfAdapter {
    fn accepts(&self, path: &Path) -> bool {
        matches!(extension(path).as_deref(), Some("pdf"))
    }

    fn parse(&self, bytes: &[u8]) -> Result<ParsedDocument> {
        let text = pdf_extract::extract_text_from_mem(bytes)
            .map_err(|e| Error::Unsupported(format!("pdf: {e}")))?;
        Ok(ParsedDocument {
            title: None,
            text: normalize_ws(&text),
            mime: "application/pdf".to_owned(),
        })
    }
}

/// HTML adapter (`.html`/`.htm`, and the extractor for rendered URLs): pulls the
/// `<title>` and the visible text, dropping `script`/`style`/head content.
pub struct HtmlAdapter;

impl SourceAdapter for HtmlAdapter {
    fn accepts(&self, path: &Path) -> bool {
        matches!(extension(path).as_deref(), Some("html" | "htm"))
    }

    fn parse(&self, bytes: &[u8]) -> Result<ParsedDocument> {
        let html = String::from_utf8_lossy(bytes);
        let (title, text) = html_to_text(&html);
        Ok(ParsedDocument {
            title,
            text,
            mime: "text/html".to_owned(),
        })
    }
}

/// Parse `bytes` read from `path`, choosing an adapter by extension (text is the
/// fallback for unknown extensions).
///
/// # Errors
/// Returns an error if the chosen adapter fails (e.g. a malformed PDF), or
/// [`Error::Unsupported`] if no adapter accepts `path` (currently unreachable —
/// [`TextAdapter`] accepts anything).
pub fn parse(path: &Path, bytes: &[u8]) -> Result<ParsedDocument> {
    let pdf = PdfAdapter;
    let html = HtmlAdapter;
    let markdown = MarkdownAdapter;
    let text = TextAdapter;
    if pdf.accepts(path) {
        pdf.parse(bytes)
    } else if html.accepts(path) {
        html.parse(bytes)
    } else if markdown.accepts(path) {
        markdown.parse(bytes)
    } else if text.accepts(path) {
        text.parse(bytes)
    } else {
        Err(Error::Unsupported(path.display().to_string()))
    }
}

/// The lowercased file extension, if any.
fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
}

/// Flatten Markdown into plain text, returning `(first_heading, text)`. Block-level
/// tags become newlines so words don't run together.
fn markdown_to_text(source: &str) -> (Option<String>, String) {
    let mut text = String::new();
    let mut title: Option<String> = None;
    let mut in_first_heading = false;
    let mut heading_buf = String::new();

    for event in Parser::new(source) {
        match event {
            Event::Text(t) | Event::Code(t) => {
                text.push_str(&t);
                if in_first_heading {
                    heading_buf.push_str(&t);
                }
            }
            Event::Start(Tag::Heading { level, .. }) => {
                if title.is_none() && level == HeadingLevel::H1 {
                    in_first_heading = true;
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if in_first_heading {
                    title = Some(heading_buf.trim().to_owned());
                    in_first_heading = false;
                }
                text.push('\n');
            }
            Event::End(TagEnd::Paragraph | TagEnd::Item | TagEnd::CodeBlock) | Event::HardBreak => {
                text.push('\n');
            }
            Event::SoftBreak => text.push(' '),
            _ => {}
        }
    }
    (title, text.trim().to_owned())
}

/// Extract `(title, text)` from an HTML document: the `<title>`, and the visible
/// text with `script`/`style`/head content removed and whitespace collapsed.
fn html_to_text(html: &str) -> (Option<String>, String) {
    let document = Html::parse_document(html);

    let title = Selector::parse("title").ok().and_then(|sel| {
        document
            .select(&sel)
            .next()
            .map(|t| t.text().collect::<String>().trim().to_owned())
            .filter(|s| !s.is_empty())
    });

    // Skip text inside non-visible / non-content elements.
    let is_hidden = |node: &Node| {
        matches!(node, Node::Element(e)
            if matches!(e.name(), "script" | "style" | "noscript" | "template" | "head" | "title"))
    };

    let mut words: Vec<&str> = Vec::new();
    for node in document.tree.nodes() {
        if let Node::Text(t) = node.value() {
            if !node.ancestors().any(|a| is_hidden(a.value())) {
                words.extend(t.split_whitespace());
            }
        }
    }
    (title, words.join(" "))
}

/// Collapse all runs of whitespace to single spaces (for PDF/HTML extraction, whose
/// raw output is full of layout whitespace).
fn normalize_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::{parse, HtmlAdapter, MarkdownAdapter, PdfAdapter, SourceAdapter, TextAdapter};
    use std::path::Path;

    #[test]
    fn text_adapter_passes_bytes_through() {
        let doc = TextAdapter.parse(b"hello world").unwrap();
        assert_eq!(doc.text, "hello world");
        assert_eq!(doc.mime, "text/plain");
    }

    #[test]
    fn markdown_extracts_title_and_strips_formatting() {
        let md = b"# The Title\n\nSome **bold** and `code` text.";
        let doc = MarkdownAdapter.parse(md).unwrap();
        assert_eq!(doc.title.as_deref(), Some("The Title"));
        assert!(doc.text.contains("Some bold and code text"));
        assert!(!doc.text.contains('*'));
    }

    #[test]
    fn dispatch_picks_markdown_by_extension() {
        let doc = parse(Path::new("notes.md"), b"# Hi\n\nbody").unwrap();
        assert_eq!(doc.mime, "text/markdown");
        let doc = parse(Path::new("notes.txt"), b"# Hi").unwrap();
        assert_eq!(doc.mime, "text/plain");
    }

    #[test]
    fn html_adapter_extracts_title_and_visible_text_only() {
        let html = b"<html><head><title>My Page</title><style>.x{color:red}</style></head>\
                     <body><script>evil()</script><h1>Heading</h1><p>Hello  world.</p></body></html>";
        let doc = HtmlAdapter.parse(html).unwrap();
        assert_eq!(doc.mime, "text/html");
        assert_eq!(doc.title.as_deref(), Some("My Page"));
        assert!(doc.text.contains("Heading"));
        assert!(doc.text.contains("Hello world.")); // whitespace collapsed
        assert!(!doc.text.contains("evil")); // script dropped
        assert!(!doc.text.contains("color")); // style dropped
    }

    #[test]
    fn html_dispatches_by_extension() {
        let doc = parse(Path::new("page.html"), b"<title>T</title><p>body text</p>").unwrap();
        assert_eq!(doc.mime, "text/html");
        assert!(doc.text.contains("body text"));
    }

    #[test]
    fn pdf_adapter_accepts_pdfs_and_rejects_garbage() {
        assert!(PdfAdapter.accepts(Path::new("paper.pdf")));
        assert!(!PdfAdapter.accepts(Path::new("paper.txt")));
        // Non-PDF bytes are a clean error, not a panic.
        assert!(PdfAdapter.parse(b"this is definitely not a pdf").is_err());
    }
}
