//! `ingest.text`: store and index a document whose text a client has already extracted.
//!
//! The client half of `jkb ingest` — reading the file or rendering the URL, and parsing PDF, HTML or
//! Markdown into text (`jkb_ingest::read_source`) — runs where the command runs. What reaches this op
//! is only text: a path would have the host read a host path, a URL would have it fetch outside the
//! container's egress firewall, and bytes would put the host's PDF and HTML parsers in front of input
//! the container chose (design H4). The host chunks the text (plain character windows, not a format
//! parser) so the chunking strategy stays one setting of the host's, and captures it.
//!
//! A backend with no embedder — `jkb serve`, which never calls a model for a client — captures without
//! embedding: the document is keyword-searchable at once and gains vectors when the host runs
//! `jkb index --pending`.

use std::sync::Arc;

use jkb_ingest::adapter::ParsedDocument;
use jkb_ingest::Pipeline;
use jkb_types::Embedder;
use serde::{Deserialize, Serialize};

use crate::{ApiError, ErrorCode};

/// The longest `mime` accepted, in bytes. It is stored on every chunk.
pub const MAX_MIME_BYTES: usize = 255;

/// What `ingest.text` was asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngestAsk {
    /// The document's extracted plain text.
    pub text: String,
    /// The source's MIME type, as the client's parser named it (`text/markdown`).
    pub mime: String,
    /// The namespace to place it under.
    pub namespace: String,
    /// The bytes the text was parsed from, which address the document and are stored as its blob.
    /// Never on the wire: only a caller in the host's own process has them, so a client cannot name
    /// the hash its text is filed under — without them the document is addressed by its text.
    #[serde(skip)]
    pub raw: Option<Vec<u8>>,
}

/// What `ingest.text` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ingested {
    /// The document item's id.
    pub document: i64,
    /// Where the document is: its primary namespace, which for one ingested before is where that ingest
    /// put it, not where this request asked.
    pub namespace: String,
    /// How many chunks it was split into.
    pub chunk_count: usize,
    /// Whether it is vector-embedded.
    pub embedded: bool,
    /// Whether it had already been ingested, so nothing was written.
    pub already_ingested: bool,
    /// Non-fatal warnings: near-empty text, not embedded.
    pub warnings: Vec<String>,
}

/// `ingest.text`: capture `ask`'s text as a document and its chunks, embedding them with `embedder` when
/// there is one.
///
/// # Errors
/// [`ErrorCode::Invalid`] for an over-long `mime` or a namespace `ns` refuses, or a failed write.
pub fn ingest(
    db: &jkb_core::Db,
    actor: &'static str,
    embedder: Option<&Arc<dyn Embedder + Send + Sync>>,
    ask: &IngestAsk,
) -> Result<Ingested, ApiError> {
    if ask.mime.len() > MAX_MIME_BYTES {
        return Err(ApiError::with_code(
            ErrorCode::Invalid,
            format!(
                "a mime type of at most {MAX_MIME_BYTES} bytes ({} given)",
                ask.mime.len()
            ),
        ));
    }
    let embedder = embedder.map_or_else(
        || Arc::new(NoModel) as Arc<dyn Embedder + Send + Sync>,
        Arc::clone,
    );
    let parsed = ParsedDocument {
        title: None,
        text: ask.text.clone(),
        mime: ask.mime.clone(),
    };
    let outcome = Pipeline::new(embedder)
        .with_actor(actor)
        .ingest(db, ask.raw.as_deref(), &parsed, &ask.namespace)
        .map_err(refusal)?;
    let document = outcome.document;
    let namespace = db
        .read(move |conn| jkb_core::item::primary_namespace(conn, document))?
        .unwrap_or_default();
    Ok(Ingested {
        document: outcome.document.get(),
        namespace,
        chunk_count: outcome.chunk_count,
        embedded: outcome.embedded,
        already_ingested: outcome.already_ingested,
        warnings: outcome.warnings,
    })
}

/// An ingest failure as the wire's code: what a request can fix is `invalid`, the rest `internal`.
fn refusal(e: jkb_ingest::Error) -> ApiError {
    match e {
        jkb_ingest::Error::Core(e) => ApiError::from(e),
        jkb_ingest::Error::Types(e @ jkb_types::Error::Validation(_)) => {
            ApiError::with_code(ErrorCode::Invalid, e.to_string())
        }
        other => ApiError::with_code(ErrorCode::Internal, other.to_string()),
    }
}

/// The embedder of a backend that calls no model. It names the host's default model and dimension: the
/// model is part of an ingestion's idempotency key, so a repeat of the same text resumes this capture,
/// and the dimension names the vector table a repeat checks for vectors `jkb index --pending` wrote.
struct NoModel;

impl Embedder for NoModel {
    fn model(&self) -> &str {
        jkb_embed::ollama::DEFAULT_MODEL
    }

    fn dim(&self) -> usize {
        jkb_embed::ollama::DEFAULT_DIM
    }

    fn embed(&self, _text: &str) -> jkb_types::Result<Vec<f32>> {
        Err(Self::unavailable())
    }

    fn health_check(&self) -> jkb_types::Result<()> {
        Err(Self::unavailable())
    }
}

impl NoModel {
    fn unavailable() -> jkb_types::Error {
        jkb_types::Error::EmbedderUnavailable(
            "this backend calls no model for a client; the host embeds the document with \
             `jkb index --pending`"
                .to_owned(),
        )
    }
}
