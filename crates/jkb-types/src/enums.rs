//! Closed-world enumerations shared across jkb.
//!
//! All serialize as `snake_case` strings (e.g. `EdgeType::DependsOn` <-> `"depends_on"`),
//! which is how they are stored in the database and exposed over MCP.

use serde::{Deserialize, Serialize};

/// The kind of a namespace node (design D3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamespaceKind {
    /// A pure virtual folder (`tasks/`, `books/`).
    Logical,
    /// A subtree bound to an external backing root with a sync policy.
    Mount,
    /// System-internal namespace (`_sys/...`).
    System,
}

/// The type of a typed edge between two items (v1 set; more added in v2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeType {
    /// Source task depends on the destination task (must be acyclic).
    DependsOn,
    /// Source was derived from the destination (e.g. chunk from document).
    DerivedFrom,
    /// Source references the destination.
    References,
    /// Source is a structural parent of the destination.
    ParentOf,
}

/// How an item is placed within a namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementRole {
    /// The item's primary/home placement.
    Primary,
    /// A chunk of a source document, ordered by `position`.
    Chunk,
    /// A secondary association (e.g. a task mirrored under a `repos/...` namespace).
    Reference,
}

/// Direction(s) in which a bound file and the KB are kept in sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    /// Disk is authoritative; changes flow disk -> KB.
    Import,
    /// KB is authoritative; changes flow KB -> disk.
    Export,
    /// Both directions, with conflict detection.
    Bidirectional,
}

/// What to do when both sides of a bidirectional binding changed since last sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// Take the on-disk version.
    DiskWins,
    /// Take the KB version.
    KbWins,
    /// Overwrite neither; report the conflict.
    Manual,
}

/// The lifecycle state of a task.
///
/// Note: `blocked` is intentionally absent — it is *derived* from `depends_on`
/// edges (design D19), never stored, so there is a single source of truth.
/// `needs_review` is a real, settable status that unblocks dependents without being
/// `done` (see [`TaskStatus::unblocks_dependents`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Not started.
    Open,
    /// Actively being worked.
    InProgress,
    /// Implementation finished but awaiting operator approval. Unblocks dependents like
    /// a terminal status, but is **not** `done` until an operator approves it.
    NeedsReview,
    /// Completed.
    Done,
    /// Abandoned.
    Cancelled,
}

impl PlacementRole {
    /// The `snake_case` string stored in the database (matches the serde form).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Chunk => "chunk",
            Self::Reference => "reference",
        }
    }
}

impl TaskStatus {
    /// The `snake_case` string stored in the database (matches the serde form).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::InProgress => "in_progress",
            Self::NeedsReview => "needs_review",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether this is a terminal status (`done`/`cancelled`): no further work is
    /// expected. `needs_review` is deliberately **not** terminal — the work still needs
    /// operator approval before it is `done`.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled)
    }

    /// Whether a `depends_on` edge to a task in this status **unblocks** its dependents
    /// (and keeps the task itself out of the ready frontier). True for the terminal
    /// statuses *and* `needs_review`: the work is finished enough for dependents to
    /// proceed, even though `needs_review` is not yet `done`.
    #[must_use]
    pub fn unblocks_dependents(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled | Self::NeedsReview)
    }

    /// Parse a *manually settable* status string into a [`TaskStatus`].
    ///
    /// Returns `None` for unknown strings **and** for `blocked`, which is a derived
    /// state (a `depends_on` edge to a non-`done` task) and never set by hand — so
    /// there is a single source of truth (design D19). Callers turn `None` into an
    /// actionable rejection.
    #[must_use]
    pub fn from_manual_str(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Self::Open),
            "in_progress" => Some(Self::InProgress),
            "needs_review" => Some(Self::NeedsReview),
            "done" => Some(Self::Done),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

impl EdgeType {
    /// The `snake_case` string stored in the database (matches the serde form).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DependsOn => "depends_on",
            Self::DerivedFrom => "derived_from",
            Self::References => "references",
            Self::ParentOf => "parent_of",
        }
    }
}

impl SyncMode {
    /// The `snake_case` string stored in the database (matches the serde form).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Import => "import",
            Self::Export => "export",
            Self::Bidirectional => "bidirectional",
        }
    }
}

impl ConflictPolicy {
    /// The `snake_case` string stored in the database (matches the serde form).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DiskWins => "disk_wins",
            Self::KbWins => "kb_wins",
            Self::Manual => "manual",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ConflictPolicy, EdgeType, NamespaceKind, PlacementRole, SyncMode, TaskStatus};
    use serde::{de::DeserializeOwned, Serialize};
    use std::fmt::Debug;

    /// Assert a value serializes to exactly `"expected"` and deserializes back.
    fn roundtrip<T>(value: T, expected: &str)
    where
        T: Serialize + DeserializeOwned + PartialEq + Debug + Copy,
    {
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(json, format!("\"{expected}\""));
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(back, value);
    }

    #[test]
    fn enums_roundtrip_as_snake_case() {
        roundtrip(NamespaceKind::Mount, "mount");
        roundtrip(EdgeType::DependsOn, "depends_on");
        roundtrip(EdgeType::DerivedFrom, "derived_from");
        roundtrip(PlacementRole::Reference, "reference");
        roundtrip(SyncMode::Bidirectional, "bidirectional");
        roundtrip(ConflictPolicy::DiskWins, "disk_wins");
        roundtrip(ConflictPolicy::KbWins, "kb_wins");
        roundtrip(TaskStatus::InProgress, "in_progress");
    }

    #[test]
    fn as_str_matches_serde_form() {
        for role in [
            PlacementRole::Primary,
            PlacementRole::Chunk,
            PlacementRole::Reference,
        ] {
            assert_eq!(
                serde_json::to_string(&role).unwrap(),
                format!("\"{}\"", role.as_str())
            );
        }
        for edge in [
            EdgeType::DependsOn,
            EdgeType::DerivedFrom,
            EdgeType::References,
            EdgeType::ParentOf,
        ] {
            assert_eq!(
                serde_json::to_string(&edge).unwrap(),
                format!("\"{}\"", edge.as_str())
            );
        }
        for status in [
            TaskStatus::Open,
            TaskStatus::InProgress,
            TaskStatus::NeedsReview,
            TaskStatus::Done,
            TaskStatus::Cancelled,
        ] {
            assert_eq!(
                serde_json::to_string(&status).unwrap(),
                format!("\"{}\"", status.as_str())
            );
        }
    }

    #[test]
    fn task_status_terminal_and_manual_parse() {
        assert!(TaskStatus::Done.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
        assert!(!TaskStatus::Open.is_terminal());
        assert!(!TaskStatus::InProgress.is_terminal());
        // `needs_review` unblocks dependents but is NOT terminal (not yet done).
        assert!(!TaskStatus::NeedsReview.is_terminal());

        assert_eq!(TaskStatus::from_manual_str("open"), Some(TaskStatus::Open));
        assert_eq!(
            TaskStatus::from_manual_str("in_progress"),
            Some(TaskStatus::InProgress)
        );
        assert_eq!(
            TaskStatus::from_manual_str("needs_review"),
            Some(TaskStatus::NeedsReview)
        );
        // `blocked` is derived, never manually settable.
        assert_eq!(TaskStatus::from_manual_str("blocked"), None);
        assert_eq!(TaskStatus::from_manual_str("nope"), None);
    }

    #[test]
    fn needs_review_unblocks_without_being_terminal() {
        assert!(TaskStatus::Done.unblocks_dependents());
        assert!(TaskStatus::Cancelled.unblocks_dependents());
        assert!(TaskStatus::NeedsReview.unblocks_dependents());
        assert!(!TaskStatus::Open.unblocks_dependents());
        assert!(!TaskStatus::InProgress.unblocks_dependents());
    }
}
