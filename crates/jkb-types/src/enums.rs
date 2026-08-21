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

/// Declare a database-facing enum **once**: the variant list, `ALL`, `as_str` and
/// `from_db_str` are all generated from the same lines, so they cannot be one variant apart.
///
/// They were three hand-written lists, and every safeguard over them was escapable. A
/// hand-written `ALL` could simply omit a variant; a test that walked it then proved nothing;
/// and the `match` meant to force the issue only forced its *pattern* to be extended, which
/// left the count and `ALL` untouched and the suite green. Meanwhile a missing `from_db_str`
/// arm is not cosmetic: `cmd_mount_create` reads a stored value with
/// `from_db_str(..).unwrap_or(default)`, so an unparseable one silently rewrites the mount —
/// the exact silent reset this pair of functions exists to prevent.
macro_rules! db_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $text:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $( $(#[$vmeta])* $variant, )+
        }

        impl $name {
            /// Every variant, in declaration order. Generated with the enum, so it is
            /// complete by construction rather than by review.
            pub const ALL: &'static [Self] = &[ $( Self::$variant ),+ ];

            /// The `snake_case` string stored in the database (matches the serde form).
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self { $( Self::$variant => $text, )+ }
            }

            /// The inverse of [`Self::as_str`], for reading a stored value back.
            ///
            /// It lives beside `as_str` (as [`TaskStatus::from_manual_str`] does) so the two
            /// spellings cannot drift: hand-written parse sites in other crates were
            /// catch-alls, so adding a variant compiled fine and silently reset a mount to
            /// the default on the very path added to stop silent resets.
            #[must_use]
            pub fn from_db_str(s: &str) -> Option<Self> {
                match s { $( $text => Some(Self::$variant), )+ _ => None }
            }
        }
    };
}

db_enum! {
    /// Direction(s) in which a bound file and the KB are kept in sync.
    pub enum SyncMode {
        /// Disk is authoritative; changes flow disk -> KB.
        Import => "import",
        /// KB is authoritative; changes flow KB -> disk.
        Export => "export",
        /// Both directions, with conflict detection.
        Bidirectional => "bidirectional",
    }
}

db_enum! {
    /// What to do when both sides of a bidirectional binding changed since last sync.
    pub enum ConflictPolicy {
        /// Take the on-disk version.
        DiskWins => "disk_wins",
        /// Take the KB version.
        KbWins => "kb_wins",
        /// Overwrite neither; report the conflict.
        Manual => "manual",
    }
}

db_enum! {
/// The lifecycle state of a task.
///
/// Note: `blocked` is intentionally absent — it is *derived* from `depends_on`
/// edges (design D19), never stored, so there is a single source of truth.
/// `needs_review` is a real, settable status meaning "a reviewer is reviewing the
/// branch" — transient, and it does **not** unblock dependents (design D27.7): a task
/// under review is not yet landed and may bounce back (see
/// [`TaskStatus::unblocks_dependents`]).
pub enum TaskStatus {
    /// Not started.
    Open => "open",
    /// Actively being worked.
    InProgress => "in_progress",
    /// A reviewer is reviewing the branch (design D27.5) — transient and re-enterable.
    /// It does **not** unblock dependents and is **not** `done`: the work is not yet on
    /// the feature branch and may bounce back to the implementer.
    NeedsReview => "needs_review",
    /// Completed.
    Done => "done",
    /// Abandoned.
    Cancelled => "cancelled",
}
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
    /// Whether this is a terminal status (`done`/`cancelled`): no further work is
    /// expected. `needs_review` is deliberately **not** terminal — the work still needs
    /// operator approval before it is `done`.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled)
    }

    /// Whether a raw `items.status` string is terminal.
    ///
    /// The string-level spelling of [`TaskStatus::is_terminal`], for the callers that read a
    /// status straight out of the database. `matches!(s, "done" | "cancelled")` was written by
    /// hand in five places, including the two guards that stop a landing and an abandon from
    /// reopening finished work — which are exactly the places where getting the set wrong is
    /// most expensive, and the places least likely to be revisited when the set changes.
    #[must_use]
    pub fn is_terminal_str(status: Option<&str>) -> bool {
        status
            .and_then(Self::from_manual_str)
            .is_some_and(Self::is_terminal)
    }

    /// How far through the lifecycle this status is: `open` → `in_progress` → `needs_review` →
    /// `done` (design D27.7), with `cancelled` alongside `done` as the other way to be finished.
    ///
    /// The four-state lifecycle written down as an order, so *"did this task move backwards?"*
    /// is a question about data rather than about a list of event names. That question is the
    /// one [`jkb_core::transition::resumed`] asks to decide whether evidence of a landing still
    /// speaks for the work in flight: a task that has gone back to an earlier stage since a
    /// landing is being worked on again, and that landing describes something else.
    ///
    /// **Only comparisons are meaningful** — the numbers are ordinal and nothing should read
    /// them as a count or persist them. `cancelled` shares `done`'s rank because both mean
    /// *finished*: neither is further along than the other, and a task moving between them has
    /// not gone back to work.
    #[must_use]
    pub fn stage(self) -> u8 {
        match self {
            Self::Open => 0,
            Self::InProgress => 1,
            Self::NeedsReview => 2,
            Self::Done | Self::Cancelled => 3,
        }
    }

    /// Whether a move from `from` to `to` goes **backwards** through the lifecycle.
    ///
    /// `None` for either side — an unparseable status, or a history that starts before the
    /// transition log did — is *not* backwards. It cannot be shown to have gone back, and the
    /// caller that asks this retires evidence on a `true`, so an unobtainable answer must not
    /// be spelled as the stronger one.
    #[must_use]
    pub fn moved_backwards(from: Option<&str>, to: Option<&str>) -> bool {
        match (
            from.and_then(Self::from_manual_str),
            to.and_then(Self::from_manual_str),
        ) {
            (Some(from), Some(to)) => to.stage() < from.stage(),
            _ => false,
        }
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
    /// Every variant is settable by name, so this is [`Self::from_db_str`] under the name that
    /// carries the rule: what it returns `None` for is `blocked`, which is not a variant at all
    /// — it is *derived* from `depends_on` edges (design D19) and never stored, so there is one
    /// source of truth. Callers turn `None` into an actionable rejection.
    ///
    /// It was a third hand-written match over the same five strings, beside `as_str` and the
    /// enum. `ALL` and both spellings are generated together now, so the set a command accepts
    /// and the set the enum has cannot come apart — which is what the `--status` enumerations in
    /// `jkb guide` and `AGENTS.md` are checked against.
    #[must_use]
    pub fn from_manual_str(s: &str) -> Option<Self> {
        Self::from_db_str(s)
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

#[cfg(test)]
mod round_trip {
    use super::{ConflictPolicy, SyncMode};

    /// Every variant must survive `as_str` -> `from_db_str`.
    ///
    /// `ALL`, `as_str` and `from_db_str` are now generated together by `db_enum!`, so this
    /// can no longer fail by omission — it is the check that the *generated* pair really is
    /// a round trip, and the guard against a duplicated or mistyped stored string.
    #[test]
    fn db_strings_round_trip() {
        for m in SyncMode::ALL {
            assert_eq!(SyncMode::from_db_str(m.as_str()), Some(*m));
        }
        for p in ConflictPolicy::ALL {
            assert_eq!(ConflictPolicy::from_db_str(p.as_str()), Some(*p));
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

    /// Exactly which `(from, to)` pairs count as going backwards — asserted here, where the rule
    /// is declared, over **every** pair rather than through one caller's behaviour.
    ///
    /// This rule decides whether evidence that work landed still counts, it has been rewritten
    /// twice, and each rewrite was caught by an end-to-end test that exercised two of the
    /// twenty-five pairs. Reordering a rank — giving `cancelled` its own, say, which is the one
    /// place the order is arguable — would leave every one of those green while landings quietly
    /// stopped counting for a whole class of task.
    #[test]
    fn exactly_these_moves_go_backwards_through_the_lifecycle() {
        use TaskStatus::{Cancelled, Done, InProgress, NeedsReview, Open};
        // Ordered, so a rank change is visible as a reordering rather than as an arithmetic edit.
        assert!(Open.stage() < InProgress.stage());
        assert!(InProgress.stage() < NeedsReview.stage());
        assert!(NeedsReview.stage() < Done.stage());
        // `cancelled` and `done` are both *finished*; neither is further along than the other, so
        // moving between them is not going back to work.
        assert_eq!(Cancelled.stage(), Done.stage());

        let backwards = |from: TaskStatus, to: TaskStatus| {
            TaskStatus::moved_backwards(Some(from.as_str()), Some(to.as_str()))
        };
        for &from in TaskStatus::ALL {
            for &to in TaskStatus::ALL {
                let expected = to.stage() < from.stage();
                assert_eq!(
                    backwards(from, to),
                    expected,
                    "{} -> {}",
                    from.as_str(),
                    to.as_str()
                );
            }
        }

        // The cases the rule was got wrong on, named so a regression reads as itself.
        assert!(
            backwards(Done, InProgress),
            "a reopened task went back to work"
        );
        assert!(backwards(InProgress, Open), "`abandon` puts the work down");
        assert!(
            backwards(Cancelled, Open),
            "reviving a cancelled task is going back"
        );
        assert!(backwards(NeedsReview, InProgress), "changes were requested");
        assert!(
            !backwards(InProgress, InProgress),
            "a row that does not move must not supersede itself — the landing held for an open \
             subtask is `in_progress -> in_progress`"
        );
        assert!(!backwards(Done, Done), "the `done -land-> done` self-loop");
        assert!(!backwards(Done, Cancelled) && !backwards(Cancelled, Done));
        assert!(!backwards(Open, InProgress), "starting is forwards");

        // Unobtainable is not backwards: this answer retires evidence, so it must be *proven*.
        assert!(!TaskStatus::moved_backwards(None, Some("open")));
        assert!(!TaskStatus::moved_backwards(Some("done"), None));
        assert!(!TaskStatus::moved_backwards(Some("nonsense"), Some("open")));
        assert!(!TaskStatus::moved_backwards(Some("done"), Some("nonsense")));
        assert!(!TaskStatus::moved_backwards(None, None));
    }
}

/// [`Resolution`] is the state set of an investigation unit's machine (design S9).
///
/// The third machine on [`jkb_fsm`], and the one whose rules are **strategy-supplied**: two
/// tables over this one state set, differing where the strategies genuinely differ. See
/// `jkb_core::nstype::lifecycle`.
impl jkb_fsm::State for Resolution {
    const ALL: &'static [Self] = Self::ALL;

    fn name(self) -> &'static str {
        self.as_str()
    }

    /// A unit is at rest once it has ended, however it ended. The inherent
    /// [`Resolution::is_settled`] says exactly this and the two must agree, so it is the one
    /// definition — a second would be a second answer.
    fn is_settled(self) -> bool {
        Self::is_settled(self)
    }

    /// An unresolved unit is waiting on a person to go and investigate it.
    ///
    /// Nothing the *system* can do moves it: evidence has to arrive from outside, as an edge
    /// somebody links. That is unlike a task's `open`, which always has `cancel` and `start`
    /// available — which is why the task machine leaves this at its default and this one does
    /// not.
    fn awaits_input(self) -> bool {
        !Self::is_settled(self)
    }
}

/// A task starts [`TaskStatus::Open`] — the machine's initial state, and the value
/// `task::create` writes.
impl Default for TaskStatus {
    fn default() -> Self {
        Self::Open
    }
}

/// [`TaskStatus`] **is** the task lifecycle's state set (design S2.1).
///
/// Implemented here rather than in `jkb-core` because the orphan rule requires it beside the
/// enum — and that is the right home anyway: a parallel `TaskState` enum in the machine's crate
/// would be a fourth hand-written list over the same five strings, beside the enum, `as_str`,
/// and `staging::State`.
///
/// The richer state set that folds the claim in (`unstarted` / `claimed` / `implementing`) is
/// deliberately **not** what this is. D27.1 separated *"is anyone holding this?"* from *"how far
/// along is the work?"* precisely because they are different questions; re-fusing them in the
/// state enum would put that back. A claim is context, and a claim change is an effect.
impl jkb_fsm::State for TaskStatus {
    const ALL: &'static [Self] = Self::ALL;

    fn name(self) -> &'static str {
        self.as_str()
    }

    /// A task is at rest when it is terminal: `done` or `cancelled`. The trait calls this
    /// *settled* rather than *terminal* because not every machine's rest state is an ending —
    /// a synced file settles and is then edited again — but for a task the two coincide.
    fn is_settled(self) -> bool {
        Self::is_terminal(self)
    }
}
