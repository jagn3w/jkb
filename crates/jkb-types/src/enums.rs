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

/// The type of a typed edge between two items.
///
/// The first four are the v1 structural set; the rest are the **investigation
/// vocabulary** (design Dmem.4) — one global set from which a
/// `NamespaceType` strategy declares the subset it uses. `references` doubles as the
/// design's `relates_to` untyped associative escape hatch (an edge always exists for
/// "these are related but I can't say how yet"), so there is no synonym for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeType {
    /// Source task depends on the destination task (must be acyclic).
    DependsOn,
    /// Source was derived from the destination (e.g. chunk from document, mutation
    /// from parent candidate, cross-pollinated idea from its origin).
    DerivedFrom,
    /// Source references the destination — also the untyped associative escape hatch
    /// (design Dmem.4's `relates_to`): related, but not yet in a typed way.
    References,
    /// Source is a structural parent of the destination.
    ParentOf,
    /// Evidence **for** the destination (observation -> hypothesis). Signed: carries an
    /// optional `weight` (design Dmem.4).
    Supports,
    /// Evidence **against** the destination. Signed: carries an optional `weight`.
    Contradicts,
    /// The source kills the destination — the destination becomes a tombstone
    /// (`resolution = dead_end`), retained with this edge as the record of what killed it.
    Refutes,
    /// The source replaces/obsoletes the destination (`resolution = superseded`).
    Supersedes,
    /// The source (an obstruction/experiment) eliminates a whole region or regime — the
    /// pruning edge that makes anti-retread cheap.
    RulesOut,
    /// The source narrows the destination (a bisection step narrowing a window).
    Narrows,
    /// Confirming the source constrains the destination (CSP coupling: one confirmed
    /// mapping constrains its siblings).
    Constrains,
    /// The source (an anchor / ground truth / verified experiment) promotes the
    /// destination from probable to confirmed.
    Confirms,
    /// The source answers (fully or partly) the destination question/goal.
    Answers,
    /// The source spawned the destination (a contradiction or discovery creating work).
    Spawns,
    /// **Emergent** provenance: the source surfaced *while working on* the destination
    /// (from gastown/Beads' `discovered-from`), as distinct from being derived from it.
    DiscoveredFrom,
    /// Clustering: the source is a member of the destination family / regime / niche.
    MemberOf,
    /// The source tests the destination (an experiment testing a hypothesis).
    Tests,
    /// The source verifies the destination (an audit or repro-run verifying a claim/fix).
    Verifies,
    /// The source fixes the destination (a fix addressing a root cause).
    Fixes,
    /// The source reduces to the destination (a route reducing the goal to a lemma).
    ReducesTo,
    /// The source is **equivalent in strength** to the destination — the anti-progress
    /// edge (design Dmem.6): a route that reduces the goal to an equally-strong lemma has
    /// made no progress.
    EquivalentInStrengthTo,
    /// The source explains why the destination failed (a post-mortem of a dead end).
    ExplainsFailure,
    /// The source informs the destination's strategy (an obstruction informing a route).
    Informs,
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
/// `needs_review` is a real, settable status meaning "a reviewer is reviewing the
/// branch" — transient, and it does **not** unblock dependents (design D27.7): a task
/// under review is not yet landed and may bounce back (see
/// [`TaskStatus::unblocks_dependents`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Not started.
    Open,
    /// Actively being worked.
    InProgress,
    /// A reviewer is reviewing the branch (design D27.5) — transient and re-enterable.
    /// It does **not** unblock dependents and is **not** `done`: the work is not yet on
    /// the feature branch and may bounce back to the implementer.
    NeedsReview,
    /// Completed.
    Done,
    /// Abandoned.
    Cancelled,
}

/// How a unit of an investigation **ended** — the outcome axis, orthogonal to
/// [`TaskStatus`] (design Dmem.3).
///
/// `status` answers "how far along?"; `resolution` answers "how did it end?". A NULL
/// `items.resolution` reads as [`Resolution::Unresolved`], so nothing needs back-filling.
/// The load-bearing property: a `dead_end` or `superseded` unit is **never deleted** — it
/// is retained together with the edge to whatever killed it, and that graveyard is what
/// stops a fresh agent from re-treading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// Still live: on the frontier (or blocked by something that is). The default.
    Unresolved,
    /// Resolved affirmatively — a confirmed result, part of the confirmed core.
    Success,
    /// Tried and killed. A tombstone: retained, plus the edge to what killed it.
    DeadEnd,
    /// Replaced by a better/stronger unit (see [`EdgeType::Supersedes`]).
    Superseded,
    /// Dropped without being killed — deprioritized, not refuted.
    Abandoned,
}

impl Resolution {
    /// Every resolution, in declaration order (for `--help` text and validation).
    pub const ALL: &'static [Self] = &[
        Self::Unresolved,
        Self::Success,
        Self::DeadEnd,
        Self::Superseded,
        Self::Abandoned,
    ];

    /// The `snake_case` string stored in the database (matches the serde form).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unresolved => "unresolved",
            Self::Success => "success",
            Self::DeadEnd => "dead_end",
            Self::Superseded => "superseded",
            Self::Abandoned => "abandoned",
        }
    }

    /// Parse a resolution string, or `None` if unknown. The empty string and
    /// `unresolved` both mean [`Resolution::Unresolved`].
    #[must_use]
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "" | "unresolved" => Some(Self::Unresolved),
            "success" => Some(Self::Success),
            "dead_end" => Some(Self::DeadEnd),
            "superseded" => Some(Self::Superseded),
            "abandoned" => Some(Self::Abandoned),
            _ => None,
        }
    }

    /// Whether this unit is **settled** — anything other than `unresolved`. A settled
    /// blocker no longer blocks its dependents (mirroring
    /// [`TaskStatus::unblocks_dependents`]: a route that died will never complete, so
    /// waiting on it forever is worse than surfacing the dependent).
    #[must_use]
    pub fn is_settled(self) -> bool {
        !matches!(self, Self::Unresolved)
    }

    /// Whether this unit belongs to the **anti-retread set** (the tombstones bucket):
    /// somebody already tried this and it did not work out. `abandoned` is deliberately
    /// excluded — it was dropped, not disproved, so it is fair game to pick back up.
    #[must_use]
    pub fn is_tombstone(self) -> bool {
        matches!(self, Self::DeadEnd | Self::Superseded)
    }
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

    /// Whether a `depends_on` edge to a task in this status **unblocks** its dependents.
    /// True for exactly the **terminal** statuses (`done`/`cancelled`) — the terminal
    /// set (design D27.7). A `needs_review` dependency deliberately does **not** unblock:
    /// its work is not yet landed on the feature branch and may bounce back, so a
    /// dependent must not start against it. Dependents unblock only once the dependency
    /// reaches `done` (its work is landed) or `cancelled` (it will never complete).
    #[must_use]
    pub fn unblocks_dependents(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled)
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
    /// Every edge type, in declaration order (the global vocabulary). Used for
    /// `--help` text, validation, and a descriptor's declared subset.
    pub const ALL: &'static [Self] = &[
        Self::DependsOn,
        Self::DerivedFrom,
        Self::References,
        Self::ParentOf,
        Self::Supports,
        Self::Contradicts,
        Self::Refutes,
        Self::Supersedes,
        Self::RulesOut,
        Self::Narrows,
        Self::Constrains,
        Self::Confirms,
        Self::Answers,
        Self::Spawns,
        Self::DiscoveredFrom,
        Self::MemberOf,
        Self::Tests,
        Self::Verifies,
        Self::Fixes,
        Self::ReducesTo,
        Self::EquivalentInStrengthTo,
        Self::ExplainsFailure,
        Self::Informs,
    ];

    /// The `snake_case` string stored in the database (matches the serde form).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DependsOn => "depends_on",
            Self::DerivedFrom => "derived_from",
            Self::References => "references",
            Self::ParentOf => "parent_of",
            Self::Supports => "supports",
            Self::Contradicts => "contradicts",
            Self::Refutes => "refutes",
            Self::Supersedes => "supersedes",
            Self::RulesOut => "rules_out",
            Self::Narrows => "narrows",
            Self::Constrains => "constrains",
            Self::Confirms => "confirms",
            Self::Answers => "answers",
            Self::Spawns => "spawns",
            Self::DiscoveredFrom => "discovered_from",
            Self::MemberOf => "member_of",
            Self::Tests => "tests",
            Self::Verifies => "verifies",
            Self::Fixes => "fixes",
            Self::ReducesTo => "reduces_to",
            Self::EquivalentInStrengthTo => "equivalent_in_strength_to",
            Self::ExplainsFailure => "explains_failure",
            Self::Informs => "informs",
        }
    }

    /// Parse an edge-type string (the stored `snake_case` form), or `None` if unknown.
    /// `relates_to` is accepted as an alias for [`EdgeType::References`], the untyped
    /// associative escape hatch (design Dmem.4).
    #[must_use]
    pub fn from_str_opt(s: &str) -> Option<Self> {
        if s == "relates_to" {
            return Some(Self::References);
        }
        Self::ALL.iter().copied().find(|e| e.as_str() == s)
    }

    /// The sign this edge contributes to a node's **signed-evidence aggregate**
    /// (design Dmem.5 primitive 5): `+1` for [`EdgeType::Supports`], `-1` for
    /// [`EdgeType::Contradicts`], `0` (not evidence) for everything else. Multiplied by
    /// the edge's `weight` (NULL weight reads as 1.0).
    #[must_use]
    pub fn evidence_sign(self) -> f64 {
        match self {
            Self::Supports => 1.0,
            Self::Contradicts => -1.0,
            _ => 0.0,
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

    /// The inverse of [`Self::as_str`], for reading a stored value back.
    ///
    /// It lives beside `as_str` (as [`TaskStatus::from_manual_str`] does) so the two spellings
    /// cannot drift: hand-written parse sites in other crates were catch-alls, so adding a
    /// variant compiled fine and silently reset a mount to the default on the very path added
    /// to stop silent resets.
    #[must_use]
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "import" => Some(Self::Import),
            "export" => Some(Self::Export),
            "bidirectional" => Some(Self::Bidirectional),
            _ => None,
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

    /// The inverse of [`Self::as_str`], for reading a stored value back. See
    /// [`SyncMode::from_db_str`] for why this belongs here rather than at each call site.
    #[must_use]
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "disk_wins" => Some(Self::DiskWins),
            "kb_wins" => Some(Self::KbWins),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

#[cfg(test)]
mod round_trip {
    use super::{ConflictPolicy, SyncMode};

    /// Every variant must survive `as_str` -> `from_db_str`. This is the test that fails when
    /// someone adds a variant and forgets the parse arm — the catch-all parse sites this
    /// replaced could not fail, they just silently mapped the new value to the default.
    #[test]
    fn db_strings_round_trip() {
        for m in [SyncMode::Import, SyncMode::Export, SyncMode::Bidirectional] {
            assert_eq!(SyncMode::from_db_str(m.as_str()), Some(m));
        }
        for p in [
            ConflictPolicy::DiskWins,
            ConflictPolicy::KbWins,
            ConflictPolicy::Manual,
        ] {
            assert_eq!(ConflictPolicy::from_db_str(p.as_str()), Some(p));
        }
        assert_eq!(SyncMode::from_db_str("nope"), None);
        assert_eq!(ConflictPolicy::from_db_str("nope"), None);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConflictPolicy, EdgeType, NamespaceKind, PlacementRole, Resolution, SyncMode, TaskStatus,
    };
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
        for edge in EdgeType::ALL.iter().copied() {
            assert_eq!(
                serde_json::to_string(&edge).unwrap(),
                format!("\"{}\"", edge.as_str())
            );
        }
        for resolution in Resolution::ALL.iter().copied() {
            assert_eq!(
                serde_json::to_string(&resolution).unwrap(),
                format!("\"{}\"", resolution.as_str())
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
        // `needs_review` is transient (a reviewer is reviewing) and NOT terminal.
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
    fn edge_types_round_trip_through_strings_and_are_unique() {
        for edge in EdgeType::ALL.iter().copied() {
            assert_eq!(EdgeType::from_str_opt(edge.as_str()), Some(edge));
        }
        // No two variants share a stored string (a copy-paste in `as_str` would collide).
        let mut names: Vec<&str> = EdgeType::ALL.iter().map(|e| e.as_str()).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "edge-type strings must be unique");

        // `relates_to` is the documented alias for the associative escape hatch.
        assert_eq!(
            EdgeType::from_str_opt("relates_to"),
            Some(EdgeType::References)
        );
        assert_eq!(EdgeType::from_str_opt("nope"), None);
    }

    #[test]
    fn only_supports_and_contradicts_are_signed_evidence() {
        assert!((EdgeType::Supports.evidence_sign() - 1.0).abs() < f64::EPSILON);
        assert!((EdgeType::Contradicts.evidence_sign() + 1.0).abs() < f64::EPSILON);
        for edge in EdgeType::ALL
            .iter()
            .copied()
            .filter(|e| !matches!(e, EdgeType::Supports | EdgeType::Contradicts))
        {
            assert!(
                edge.evidence_sign().abs() < f64::EPSILON,
                "{} must not contribute evidence",
                edge.as_str()
            );
        }
    }

    #[test]
    fn resolution_parses_settles_and_tombstones() {
        for r in Resolution::ALL.iter().copied() {
            assert_eq!(Resolution::from_str_opt(r.as_str()), Some(r));
        }
        // An absent (NULL) resolution reads as `unresolved`.
        assert_eq!(
            Resolution::from_str_opt(""),
            Some(Resolution::Unresolved),
            "the empty string is NULL's in-memory reading"
        );
        assert_eq!(Resolution::from_str_opt("blocked"), None);

        // Only `unresolved` is unsettled — a settled blocker stops blocking.
        assert!(!Resolution::Unresolved.is_settled());
        for r in [
            Resolution::Success,
            Resolution::DeadEnd,
            Resolution::Superseded,
            Resolution::Abandoned,
        ] {
            assert!(r.is_settled(), "{} must settle", r.as_str());
        }

        // The anti-retread set is exactly dead_end + superseded: `abandoned` was dropped,
        // not disproved, so it must stay pickable.
        assert!(Resolution::DeadEnd.is_tombstone());
        assert!(Resolution::Superseded.is_tombstone());
        assert!(!Resolution::Abandoned.is_tombstone());
        assert!(!Resolution::Success.is_tombstone());
        assert!(!Resolution::Unresolved.is_tombstone());
    }

    #[test]
    fn only_terminal_statuses_unblock_dependents() {
        // The terminal set unblocks (design D27.7).
        assert!(TaskStatus::Done.unblocks_dependents());
        assert!(TaskStatus::Cancelled.unblocks_dependents());
        // `needs_review` no longer unblocks — a task under review may still bounce back.
        assert!(!TaskStatus::NeedsReview.unblocks_dependents());
        assert!(!TaskStatus::Open.unblocks_dependents());
        assert!(!TaskStatus::InProgress.unblocks_dependents());
    }
}
