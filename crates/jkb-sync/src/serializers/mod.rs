//! Pluggable sync serializers: the mapping between a file's bytes and the item(s)
//! it serializes (design D24). A synced file is a *serialization* of items; the
//! serializer owns only the file boundary — parsing bytes into a [`SyncDoc`] and
//! rendering a [`SyncDoc`] back — never the stored knowledge, so one can be swapped
//! or added without risking data.
//!
//! Two serializers ship:
//! - [`document`] — the whole file is one item's text ([`DocumentSerializer`]).
//! - [`tasks`] — one `tasks.md` ⇄ many task items, with section headers mapped to
//!   namespaces, hierarchy/dependencies, checkbox status, and a stable `^id` per task
//!   ([`tasks::TasksSerializer`]).
//!
//! The engine (`engine.rs`) owns everything DB-side: mapping a [`SyncItem`]'s
//! `local_id` to a KB item, three-way merge, and quarantine. A serializer is a pure
//! function over bytes, so it is trivially testable and cannot corrupt the store.

mod document;
mod tasks;

pub use document::DocumentSerializer;
pub use tasks::TasksSerializer;

use jkb_types::{EdgeType, Error as TypeError};

use crate::{Error, Result};

/// A parsed file: the sections, items, and edges it serializes. For the `document`
/// serializer this is a single item with no sections or edges; for `tasks` it is many
/// task/text items placed under header-derived sections, plus `parent_of`/`depends_on`
/// edges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncDoc {
    /// Section headers, in file order. Each maps to a namespace under the file's mirror.
    pub sections: Vec<SyncSection>,
    /// The items the file serializes, in file order.
    pub items: Vec<SyncItem>,
    /// Edges between items, referenced by `local_id`.
    pub edges: Vec<SyncEdge>,
}

/// A section header (`## …`) that maps to a namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSection {
    /// Slug path relative to the file namespace (e.g. `backend` or `backend/api`).
    pub path: String,
    /// The literal header line, preserved verbatim for round-trip fidelity.
    pub header_line: String,
    /// Order among sections in the file.
    pub position: i64,
}

/// One item the file serializes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncItem {
    /// Stable in-file identity (the `^id`). Empty string for the `document` single item.
    pub local_id: String,
    /// Item kind: `document`, `task`, or `text` (preserved prose/blank blocks).
    pub kind: String,
    /// Text content: the whole file (`document`), the task title (`task`), or the raw
    /// block (`text`).
    pub content: String,
    /// The owning section's slug path; `None` = the file root (before any header).
    pub section: Option<String>,
    /// Order within the owning section (tasks and text items interleave).
    pub position: i64,
    /// Task status (`open`/`in_progress`/`needs_review`/`done`/`cancelled`); `None` for
    /// non-tasks.
    pub status: Option<String>,
    /// Task priority (`!pN`); `None` if unset.
    pub priority: Option<i64>,
    /// Task due date (`@date`); `None` if unset.
    pub due: Option<String>,
    /// `facet=value` tags (`#facet=value`).
    pub tags: Vec<(String, String)>,
    /// Extra reference-placement namespaces (`+ns`).
    pub mirrors: Vec<String>,
    /// The `local_id` of this item's parent (indentation → `parent_of`); `None` if top.
    pub parent: Option<String>,
}

impl SyncItem {
    /// A minimal item with the given identity, kind, and content and all else defaulted.
    #[must_use]
    pub fn new(local_id: impl Into<String>, kind: &str, content: impl Into<String>) -> Self {
        Self {
            local_id: local_id.into(),
            kind: kind.to_owned(),
            content: content.into(),
            section: None,
            position: 0,
            status: None,
            priority: None,
            due: None,
            tags: Vec::new(),
            mirrors: Vec::new(),
            parent: None,
        }
    }
}

/// An edge between two items, referenced by their `local_id`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncEdge {
    /// Source item `local_id`.
    pub src: String,
    /// Destination item `local_id`.
    pub dst: String,
    /// The edge type (`parent_of`, `depends_on`).
    pub edge_type: EdgeType,
}

/// A file-format serializer: parse bytes into a [`SyncDoc`] and render it back.
///
/// `Send + Sync` so a boxed serializer can cross into the writer thread.
pub trait SyncSerializer: Send + Sync {
    /// The stable name recorded on the mount/binding (`mounts.serializer`).
    fn name(&self) -> &'static str;

    /// Parse raw file bytes into the [`SyncDoc`] they represent.
    ///
    /// # Errors
    /// Returns a validation error if the bytes cannot be parsed for this format.
    fn parse(&self, bytes: &[u8]) -> Result<SyncDoc>;

    /// Render a [`SyncDoc`] back into file bytes. Must be idempotent:
    /// `render(parse(render(doc)))` equals `render(doc)`.
    ///
    /// # Errors
    /// Returns an error if the document cannot be rendered for this format.
    fn render(&self, doc: &SyncDoc) -> Result<Vec<u8>>;

    /// Whether a `parse` failure on a previously-synced file should be **quarantined**
    /// (keep last-good items, stash the bytes, flag the file) rather than surfaced as a
    /// hard error (design D25). Multi-item formats opt in; `document` does not, so its
    /// only failure (non-UTF-8) stays a hard, rolled-back error.
    fn quarantine_on_parse_error(&self) -> bool {
        false
    }
}

/// The serializer names available in this build.
pub const AVAILABLE: &[&str] = &["document", "tasks"];

/// Resolve a serializer by name, rejecting unknown names with an actionable error that
/// lists what *is* available (design D24: "unknown serializer rejected").
///
/// # Errors
/// Returns a validation error if `name` is not a serializer in this build.
pub fn resolve(name: &str) -> Result<Box<dyn SyncSerializer>> {
    match name {
        "document" => Ok(Box::new(DocumentSerializer)),
        "tasks" => Ok(Box::new(TasksSerializer)),
        other => Err(Error::Types(TypeError::Validation(format!(
            "unknown serializer `{other}`; available: {}",
            AVAILABLE.join(", ")
        )))),
    }
}

#[cfg(test)]
mod tests {
    use super::resolve;

    #[test]
    fn resolve_known_and_unknown() {
        assert_eq!(resolve("document").unwrap().name(), "document");
        assert_eq!(resolve("tasks").unwrap().name(), "tasks");
        let err = match resolve("spec") {
            Ok(_) => panic!("expected unknown-serializer error"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("unknown serializer `spec`"));
        assert!(err.contains("document"));
        assert!(err.contains("tasks"));
    }
}
