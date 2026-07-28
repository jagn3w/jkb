//! Shared quote-aware tokenization primitives for the line-oriented DSLs.
//!
//! The CLI query DSL ([`crate::query`]), the task quick-add parser ([`crate::task`]),
//! and the `tasks` file serializer (`jkb-sync`) all split a single line into
//! whitespace-separated tokens while keeping `"…"`-quoted spans together, then strip
//! a surrounding quote pair off free words. That logic was byte-for-byte triplicated;
//! it lives here once so a fix (quoting rules, UTF-8 edge cases) lands in one place.
//!
//! Two flavours (design D29.2):
//! - [`tokenize`] / [`unquote`] are **lenient and escape-free** — a stray `"` is just a
//!   literal character and a `\` is literal. The `tasks` serializer uses these so on-disk
//!   task text round-trips verbatim.
//! - [`tokenize_escaped`] / [`unquote_unescape`] / [`has_unterminated_quote`] are
//!   **escape-aware** — `\"` is a literal double-quote that does not toggle quote state
//!   and `\\` is a literal backslash. The strict DSLs (query, quick-add) use these so a
//!   title or term can contain a literal quote (e.g. `ship the 6\" pipe`) while a genuinely
//!   unterminated span is still rejected.

/// Split `input` into tokens on whitespace, keeping `"…"`-quoted spans together.
///
/// A double quote toggles "inside a quote"; whitespace inside a quote is preserved
/// in the current token, whitespace outside it ends the token. The quote characters
/// themselves are retained in the token (strip them with [`unquote`]).
#[must_use]
pub fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    for c in input.chars() {
        match c {
            '"' => {
                in_quote = !in_quote;
                current.push(c);
            }
            c if c.is_whitespace() && !in_quote => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Escape-aware [`tokenize`] for the strict line DSLs: `\"` is a literal quote that does
/// not toggle quote state, and `\\` is a literal backslash. Both escape sequences are kept
/// **raw** in the emitted token (resolve them with [`unquote_unescape`]); a lone `\` not
/// before `"`/`\` is an ordinary literal backslash. Behaves identically to [`tokenize`]
/// for input containing no backslashes.
#[must_use]
pub fn tokenize_escaped(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if matches!(chars.peek(), Some('"' | '\\')) => {
                // Keep the escape sequence raw so `unquote_unescape` can resolve it; the
                // escaped char neither toggles the quote nor ends the token.
                current.push('\\');
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '"' => {
                in_quote = !in_quote;
                current.push(c);
            }
            c if c.is_whitespace() && !in_quote => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Report whether `input` opens a `"` quote it never closes, treating `\"` as an escaped
/// literal quote that does not toggle quote state (and `\\` as a literal backslash).
///
/// [`tokenize`] is infallible — an unclosed quote simply runs to end of input (the `tasks`
/// serializer relies on this, treating a stray quote as an ordinary word). The stricter
/// line DSLs (query, quick-add) reject an unterminated span as user error; they call this
/// first, alongside [`tokenize_escaped`], and raise their own error.
#[must_use]
pub fn has_unterminated_quote(input: &str) -> bool {
    let mut in_quote = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if matches!(chars.peek(), Some('"' | '\\')) => {
                chars.next(); // consume the escaped char; it never toggles quote state
            }
            '"' => in_quote = !in_quote,
            _ => {}
        }
    }
    in_quote
}

/// Strip a single pair of surrounding double quotes, if present. Lenient/escape-free —
/// used by the `tasks` serializer so on-disk text round-trips verbatim.
#[must_use]
pub fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
}

/// Escape-aware [`unquote`] for the strict line DSLs: strip a surrounding `"…"` pair (a
/// token that came from a quoted span starts and ends with an unescaped `"`), then resolve
/// `\"`→`"` and `\\`→`\`. Returns an owned `String` because unescaping changes the content.
///
/// Assumes a balanced token — the strict callers reject unterminated spans via
/// [`has_unterminated_quote`] before reaching here.
#[must_use]
pub fn unquote_unescape(s: &str) -> String {
    let inner = if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    };
    unescape(inner)
}

/// A lowercase slug of `text`: runs of non-alphanumeric characters collapse to a single
/// `-`, and leading/trailing dashes are trimmed. Unicode letters and digits are kept and
/// lowercased ([`char::is_alphanumeric`], not the ASCII-only variant) so an accented title
/// slugs the same on every path. Returns an **empty** string when `text` has no
/// alphanumeric characters — callers supply their own fallback (e.g. `"task"`, `"section"`)
/// and any length cap.
///
/// This is the single source of truth for slugging so a task minted via the CLI, the MCP
/// server, or file sync derives the same slug from the same title.
#[must_use]
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_matches('-').to_owned()
}

/// Resolve `\"`→`"` and `\\`→`\`; any other `\x` is kept verbatim (a literal backslash
/// followed by `x`).
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some(escaped @ ('"' | '\\')) => out.push(escaped),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{tokenize, unquote};

    #[test]
    fn splits_on_whitespace() {
        assert_eq!(tokenize("a b  c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn keeps_quoted_spans_together() {
        assert_eq!(
            tokenize(r#"fix "a quoted title" done"#),
            vec!["fix", r#""a quoted title""#, "done"]
        );
    }

    #[test]
    fn detects_unterminated_quote() {
        use super::has_unterminated_quote;
        assert!(has_unterminated_quote(r#"hello "unclosed"#));
        assert!(!has_unterminated_quote(r#"hello "closed" world"#));
        assert!(!has_unterminated_quote("no quotes"));
    }

    #[test]
    fn slug_collapses_and_trims() {
        use super::slug;
        assert_eq!(slug("Fix the flaky test"), "fix-the-flaky-test");
        assert_eq!(slug("## 1. Backend & API"), "1-backend-api");
        assert_eq!(slug("  --leading & trailing--  "), "leading-trailing");
        // No alphanumerics → empty (callers supply their own fallback).
        assert_eq!(slug("--- !!! ---"), "");
    }

    #[test]
    fn slug_keeps_unicode_alphanumerics() {
        use super::slug;
        // Unicode letters/digits are lowercased and kept (not dropped as ASCII-only
        // would), so the same title slugs identically on every path.
        assert_eq!(slug("Café Résumé"), "café-résumé");
        assert_eq!(slug("naïve Ångström"), "naïve-ångström");
    }

    #[test]
    fn unquote_strips_one_pair() {
        assert_eq!(unquote(r#""hello""#), "hello");
        assert_eq!(unquote("plain"), "plain");
        assert_eq!(unquote(r#""only-open"#), r#""only-open"#);
    }

    #[test]
    fn tokenize_escaped_matches_plain_without_backslashes() {
        use super::tokenize_escaped;
        assert_eq!(tokenize_escaped("a b  c"), tokenize("a b  c"));
        assert_eq!(
            tokenize_escaped(r#"fix "a quoted title" done"#),
            tokenize(r#"fix "a quoted title" done"#)
        );
    }

    #[test]
    fn escaped_quote_is_a_literal_and_does_not_split() {
        use super::{tokenize_escaped, unquote_unescape};
        // `ship the 6\" pipe` — the escaped quote is literal and does not open a span.
        let tokens = tokenize_escaped(r#"ship the 6\" pipe"#);
        assert_eq!(tokens, vec!["ship", "the", r#"6\""#, "pipe"]);
        assert_eq!(unquote_unescape(&tokens[2]), r#"6""#);
    }

    #[test]
    fn escaped_backslash_is_a_literal_backslash() {
        use super::unquote_unescape;
        assert_eq!(unquote_unescape(r"a\\b"), r"a\b");
    }

    #[test]
    fn unescaped_quotes_still_flag_unterminated() {
        use super::has_unterminated_quote;
        // A lone unescaped quote is still an unterminated span…
        assert!(has_unterminated_quote(r#"ship the 6" pipe"#));
        // …but an escaped one is not.
        assert!(!has_unterminated_quote(r#"ship the 6\" pipe"#));
        // `\\` is a literal backslash, so the following quote is unescaped → unterminated.
        assert!(has_unterminated_quote(r#"a\\" b"#));
    }

    #[test]
    fn escaped_quote_inside_a_span_round_trips() {
        use super::{tokenize_escaped, unquote_unescape};
        // `"a \" b"` is one span whose interior escaped quote survives unquoting.
        let tokens = tokenize_escaped(r#"pre "a \" b" post"#);
        assert_eq!(tokens, vec!["pre", r#""a \" b""#, "post"]);
        assert_eq!(unquote_unescape(&tokens[1]), r#"a " b"#);
    }
}
