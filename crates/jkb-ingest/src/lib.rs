//! Staged, idempotent ingestion (design D7, D18, D21).
//!
//! [`Pipeline`] drives `parse → chunk → embed`, keyed by the `ingestions`
//! idempotency row: a completed run is a no-op, a captured-but-un-embedded run
//! resumes at the embed stage. Capture writes items (FTS-indexed immediately by
//! triggers) in one transaction; embedding is a separate, resumable stage that
//! never blocks capture ([`Pipeline::index_pending`] mops up un-embedded items).
//!
//! Raw sources are content-addressed in the `blobs` store ([`blob`]); source bytes
//! become a [`adapter::ParsedDocument`] via a [`adapter::SourceAdapter`]. This is
//! the crate where `jkb-core` (items) and `jkb-index` (vectors) meet, so its
//! [`Error`] absorbs both.
//!
//! Open the database with `sqlite-vec` registered so the embed stage can write
//! vectors: `Db::open_with(path, &[jkb_index::register])`.

pub mod adapter;
pub mod blob;
pub mod chunk;
mod error;
mod fetch;
mod pipeline;

pub use error::{Error, Result};
pub use pipeline::{Outcome, Pipeline};

/// Read and parse a source named on the command line: an `http(s)` URL is rendered in a headless
/// browser and its DOM parsed as HTML; anything else is a file, parsed by its extension. Returns the raw
/// bytes with the parsed document.
///
/// The client half of `jkb ingest`, so it runs where the command runs: in the dev container the file
/// is the container's and the URL is fetched through the container's egress firewall, and the host
/// daemon is sent only the extracted text — never a path to read, a page to fetch, or bytes for its
/// PDF and HTML parsers (design H4).
///
/// # Errors
/// [`Error::Fetch`] for a page that cannot be rendered, [`Error::Io`] for a file that cannot be read,
/// or the adapter's parse error.
pub fn read_source(source: &str) -> Result<(Vec<u8>, adapter::ParsedDocument)> {
    use adapter::SourceAdapter as _;
    if source.starts_with("http://") || source.starts_with("https://") {
        let html = fetch::render_url(source)?;
        let parsed = adapter::HtmlAdapter.parse(html.as_bytes())?;
        return Ok((html.into_bytes(), parsed));
    }
    let path = std::path::Path::new(source);
    let bytes = std::fs::read(path)?;
    let parsed = adapter::parse(path, &bytes)?;
    Ok((bytes, parsed))
}
