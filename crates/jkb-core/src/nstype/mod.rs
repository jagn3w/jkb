//! Typed namespaces: the pluggable **investigation strategy** seam (design Dmem.1).
//!
//! A namespace can declare a *strategy* (`namespaces.metadata.type`, written by
//! [`crate::ns::set_type`]). The strategy is a [`NamespaceType`] descriptor: it declares
//! the item kinds and edge types the investigation uses, the verbs an agent drives it
//! with, how a unit's outcome rolls up from its edges, how the frontier is ranked, and
//! the acceptance test that says the investigation is finished.
//!
//! ## Why a descriptor rather than match arms
//! The substrate is universal — items, typed edges, tags, claims, changelog, search — but
//! the *coordination protocol* and the *"done" test* genuinely differ per problem type.
//! Forcing an AND/OR proof tree and a scored candidate population into one schema loses
//! the tree rollup and the diversity niching respectively. So the base engine stays thin
//! and every type-specific decision lives behind this trait.
//!
//! What earns a new descriptor is a genuinely different **protocol** (an evolutionary
//! population, a tournament — both deferred, design Dmem.7). What does *not* is a
//! different goal: proving a conjecture and disproving it share one structure and differ
//! only by an acceptance predicate, so [`conjecture::ConjectureAttack`] is one strategy
//! with two presets, not two strategies.
//!
//! ## Registration
//! [`resolve`] maps a name to a descriptor and rejects unknown names with the available
//! list — mirroring `jkb_sync::serializers::resolve`. Adding a strategy is one `static`
//! plus one match arm plus one entry in [`AVAILABLE`]; nothing else in the engine changes.
//!
//! Untyped namespaces (every namespace that predates this) resolve to no descriptor and
//! behave exactly as before.

pub mod conjecture;
pub mod debugging;

use rusqlite::{Connection, OptionalExtension};

use jkb_types::{EdgeType, Error as TypeError, ItemId, Resolution};

use crate::query::{Query, Scope};
use crate::{edge, Result};

/// The `goal` base kind: the investigation's root intent and its acceptance predicate.
pub const KIND_GOAL: &str = "goal";
/// The `node` base kind: the generic work/knowledge unit a strategy specializes.
pub const KIND_NODE: &str = "node";
/// The `artifact` base kind: a concrete produced result (repro, proof term, fix).
pub const KIND_ARTIFACT: &str = "artifact";
/// The `reflection` base kind: synthesized/derived memory (digest, post-mortem).
pub const KIND_REFLECTION: &str = "reflection";

/// The four base kinds every strategy inherits (design Dmem.2). A strategy refines and
/// extends this set; it never removes from it, so the generic reads (`inv digest`,
/// ancestry walks) work in any investigation.
pub const BASE_KINDS: &[&str] = &[KIND_GOAL, KIND_NODE, KIND_ARTIFACT, KIND_REFLECTION];

/// The tag facet carrying a unit's frontier rank (higher = more promising).
pub const FACET_PROMISE: &str = "promise";

/// Which of the four base roles a strategy's node kind plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseKind {
    /// The root intent + acceptance predicate.
    Goal,
    /// A work/knowledge unit.
    Node,
    /// A concrete produced result.
    Artifact,
    /// Synthesized/derived memory.
    Reflection,
}

/// One item `kind` a strategy uses, and what it means.
#[derive(Debug, Clone, Copy)]
pub struct NodeKindSpec {
    /// The stored `items.kind` string (e.g. `hypothesis`).
    pub kind: &'static str,
    /// Which base role it plays.
    pub base: BaseKind,
    /// One-line description, surfaced by `jkb inv kinds`.
    pub about: &'static str,
}

/// Whether a verb needs an existing unit to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetRule {
    /// The verb must be given a target uid (e.g. `rule-out <suspect-area>`).
    Required,
    /// The verb may be given a target (e.g. `hypothesize` under a symptom).
    Optional,
    /// The verb takes no target (e.g. `symptom`, which starts a thread).
    Forbidden,
}

/// A strategy verb: a named preset over "create a unit, link it, maybe resolve what it
/// acted on" (design Dmem.1's `cli_verbs`). Dispatched by `jkb inv do <verb> …`, which
/// resolves the namespace's strategy and looks the verb up here — so a strategy adds verbs
/// as *data*, with no CLI plumbing.
#[derive(Debug, Clone, Copy)]
pub struct VerbSpec {
    /// The verb name as typed (e.g. `rule-out`).
    pub verb: &'static str,
    /// One-line description for `jkb inv verbs`.
    pub about: &'static str,
    /// The item kind the verb creates.
    pub kind: &'static str,
    /// The edge linked between the new unit and the target, if any.
    pub edge: Option<EdgeType>,
    /// Whether the edge runs **target -> new unit** instead of new unit -> target. The
    /// blocking direction needs it: `lemma --on <approach>` must record *the approach*
    /// `depends_on` *the lemma*, so the approach leaves the frontier until the lemma lands.
    pub reverse: bool,
    /// Whether a target uid is required.
    pub target: TargetRule,
    /// The resolution stamped on the **target** by this verb — how a verb kills a route
    /// (`rule-out` → `dead_end`) or promotes one (`confirm` → `success`). The target is
    /// never deleted, only resolved, so the graveyard survives.
    pub resolves_target: Option<Resolution>,
    /// If non-empty, the target's `kind` must be one of these — the mechanism behind a
    /// gated verb (e.g. `reopen` demands a materially new mechanism/invariant/
    /// construction/obstruction, not just an assertion that the route deserves another go).
    pub requires_target_kind: &'static [&'static str],
    /// Tags applied to the new unit (e.g. `confidence=unverified`).
    pub tags: &'static [(&'static str, &'static str)],
}

impl VerbSpec {
    /// A verb that only creates a unit, with no target and no edge.
    #[must_use]
    pub const fn new(verb: &'static str, kind: &'static str, about: &'static str) -> Self {
        Self {
            verb,
            about,
            kind,
            edge: None,
            reverse: false,
            target: TargetRule::Forbidden,
            resolves_target: None,
            requires_target_kind: &[],
            tags: &[],
        }
    }

    /// A verb that creates a unit and links `edge` from it to a required target.
    #[must_use]
    pub const fn on(
        verb: &'static str,
        kind: &'static str,
        edge: EdgeType,
        about: &'static str,
    ) -> Self {
        Self {
            edge: Some(edge),
            target: TargetRule::Required,
            ..Self::new(verb, kind, about)
        }
    }

    /// A verb whose edge runs **from the target to the new unit** — used for the blocking
    /// direction (`target depends_on new`).
    #[must_use]
    pub const fn blocking(
        verb: &'static str,
        kind: &'static str,
        edge: EdgeType,
        about: &'static str,
    ) -> Self {
        Self {
            reverse: true,
            ..Self::on(verb, kind, edge, about)
        }
    }

    /// Make this verb's target optional.
    #[must_use]
    pub const fn optional_target(mut self) -> Self {
        self.target = TargetRule::Optional;
        self
    }

    /// Make this verb stamp `resolution` on its target.
    #[must_use]
    pub const fn resolving(mut self, resolution: Resolution) -> Self {
        self.resolves_target = Some(resolution);
        self
    }

    /// Restrict the kinds this verb's target may have (a gated verb).
    #[must_use]
    pub const fn target_kinds(mut self, kinds: &'static [&'static str]) -> Self {
        self.requires_target_kind = kinds;
        self
    }

    /// Attach fixed tags to the units this verb creates.
    #[must_use]
    pub const fn tagged(mut self, tags: &'static [(&'static str, &'static str)]) -> Self {
        self.tags = tags;
        self
    }
}

/// The verdict of a strategy's acceptance test (design Dmem.1's `goal_predicate`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoneState {
    /// Whether the investigation's acceptance predicate is satisfied.
    pub done: bool,
    /// Why — the human-readable verdict, e.g. what is still missing. This is the
    /// "keep going" signal: as long as it names an open residual, the frontier is live.
    pub summary: String,
}

impl DoneState {
    /// A "not finished" verdict with the reason.
    #[must_use]
    pub fn open(summary: impl Into<String>) -> Self {
        Self {
            done: false,
            summary: summary.into(),
        }
    }

    /// A "finished" verdict with the reason.
    #[must_use]
    pub fn done(summary: impl Into<String>) -> Self {
        Self {
            done: true,
            summary: summary.into(),
        }
    }
}

/// A registered investigation strategy.
///
/// `Send + Sync` and `'static` so a resolved descriptor can be used from the writer
/// thread and held across calls.
pub trait NamespaceType: Send + Sync {
    /// The stable name stored in `namespaces.metadata.type`.
    fn name(&self) -> &'static str;

    /// One-line description of what this strategy is for.
    fn about(&self) -> &'static str;

    /// The item kinds this strategy uses, beyond [`BASE_KINDS`].
    fn node_kinds(&self) -> &'static [NodeKindSpec];

    /// The subset of the global edge vocabulary this strategy uses. Advisory: it drives
    /// `--help` and `jkb inv kinds`, and is *not* enforced on writes — an investigation
    /// must always be able to record an association it has no vocabulary for yet
    /// (design Dmem.8, pitfall 1: no over-structuring).
    fn edge_types(&self) -> &'static [EdgeType];

    /// The verbs `jkb inv do` dispatches for this strategy.
    fn verbs(&self) -> &'static [VerbSpec];

    /// Whether `kind` is a valid unit kind here: one of [`BASE_KINDS`] or one of
    /// [`NamespaceType::node_kinds`].
    fn accepts_kind(&self, kind: &str) -> bool {
        BASE_KINDS.contains(&kind) || self.node_kinds().iter().any(|k| k.kind == kind)
    }

    /// The frontier query for `scope`: the ranked work queue. The default is
    /// [`base_frontier`], which is what most strategies want; a strategy overrides it to add
    /// its own predicates (the `debugging` strategy drops stale observations, for instance)
    /// — and **must start from [`base_frontier`]** so it inherits the exclusions every
    /// frontier needs.
    fn frontier(&self, scope: Scope) -> Query {
        base_frontier(scope)
    }

    /// The frontier rank of `node` — higher sorts first. The default blends the explicit
    /// `promise=` tag with the signed-evidence balance, so a unit that accumulated
    /// supporting observations rises without anyone re-tagging it.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    fn ranking(&self, conn: &Connection, node: ItemId) -> Result<f64> {
        Ok(promise(conn, node)? + edge::evidence_for(conn, node)?)
    }

    /// The resolution `node` *should* carry, derived from its incident edges — the
    /// rollup that keeps the outcome axis honest instead of relying on an agent to
    /// remember to set it. See [`default_rollup`] for the base rules.
    ///
    /// # Errors
    /// Returns an error if a query fails.
    fn resolution_rollup(&self, conn: &Connection, node: ItemId) -> Result<Resolution> {
        default_rollup(conn, node)
    }

    /// The acceptance test for the investigation rooted at `ns_path`: is it finished, and
    /// if not, what is missing? Terminates on this predicate, never on a timer
    /// (design Dmem.0).
    ///
    /// # Errors
    /// Returns an error if a query fails.
    fn goal_predicate(&self, conn: &Connection, ns_path: &str) -> Result<DoneState>;

    /// The named acceptance presets this strategy offers, if any — the enumerated bars a new
    /// goal can be seeded with. Empty (the default) means the strategy's "done" test is not
    /// parameterized, so `--accept` is meaningless for it and must be refused rather than
    /// stamping an unrelated predicate onto the goal body.
    fn acceptance_presets(&self) -> &'static [&'static str] {
        &[]
    }

    /// The acceptance-predicate text for one of this strategy's
    /// [`NamespaceType::acceptance_presets`], or `None` if it has no such preset. Lives on
    /// the descriptor so the CLI cannot apply one strategy's predicate to another's goal.
    fn acceptance_text(&self, preset: &str) -> Option<&'static str> {
        let _ = preset;
        None
    }
}

/// The strategies registered in this build.
pub const AVAILABLE: &[&str] = &[debugging::NAME, conjecture::NAME];

static DEBUGGING: debugging::Debugging = debugging::Debugging;
static CONJECTURE: conjecture::ConjectureAttack = conjecture::ConjectureAttack;

/// Resolve a strategy by name, rejecting unknown names with an actionable error listing
/// what *is* available (mirroring `jkb_sync::serializers::resolve`).
///
/// # Errors
/// Returns a validation error if `name` is not a strategy in this build.
pub fn resolve(name: &str) -> Result<&'static dyn NamespaceType> {
    match name {
        debugging::NAME => Ok(&DEBUGGING),
        conjecture::NAME => Ok(&CONJECTURE),
        other => Err(TypeError::Validation(format!(
            "unknown investigation type `{other}`; available: {}",
            AVAILABLE.join(", ")
        ))
        .into()),
    }
}

/// The strategy governing the namespace `path` — its own `type` or the nearest typed
/// ancestor's (see [`crate::ns::effective_type`]) — or `None` if it is an ordinary
/// untyped namespace. Returns the namespace the type came from alongside the descriptor.
///
/// # Errors
/// Returns an error if a query fails or the recorded type names no registered strategy
/// (which means the namespace was typed by a build that had one this build does not).
pub fn for_namespace(
    conn: &Connection,
    path: &str,
) -> Result<Option<(String, &'static dyn NamespaceType)>> {
    let Some((source, name)) = crate::ns::effective_type(conn, path)? else {
        return Ok(None);
    };
    Ok(Some((source, resolve(&name)?)))
}

/// The kinds that are never frontier *work*, however unresolved they look. Only
/// [`KIND_REFLECTION`] qualifies: a digest or post-mortem is **synthesized memory about**
/// the investigation, not a unit of it. Left in, the digest that
/// `jkb inv digest` writes is itself unresolved, unblocked and unclaimed — so it ranks as
/// work, and (ties breaking by uid) `reflection:digest:…` sorts ahead of `symptom:` and
/// `root-cause:`, making the summary an agent just read the first thing it is told to do.
///
/// `goal` deliberately stays: a freshly-opened investigation whose only unit is its goal
/// *should* show that goal as the work, which is exactly "go decompose this".
pub const NON_WORK_KINDS: &[&str] = &[KIND_REFLECTION];

/// The frontier every strategy starts from: the generalized frontier (unresolved,
/// unblocked) restricted to unclaimed units and stripped of [`NON_WORK_KINDS`].
///
/// A strategy overriding [`NamespaceType::frontier`] should build on this rather than
/// constructing a `Query` from scratch, so a new base exclusion cannot be silently missed
/// by one strategy.
#[must_use]
pub fn base_frontier(scope: Scope) -> Query {
    Query {
        frontier: true,
        claimed: Some(false),
        exclude_kinds: NON_WORK_KINDS.iter().map(|k| (*k).to_owned()).collect(),
        scope,
        ..Query::default()
    }
}

/// The numeric `promise=` tag on `node` (design Dmem.3's frontier rank), or `0.0` when
/// absent or unparseable. Higher is more promising.
///
/// # Errors
/// Returns an error if the query fails.
pub fn promise(conn: &Connection, node: ItemId) -> Result<f64> {
    let value: Option<String> = conn
        .prepare_cached(
            "SELECT value FROM tag_applications WHERE item_id = ?1 AND facet = ?2 LIMIT 1",
        )?
        .query_row((node.get(), FACET_PROMISE), |row| row.get(0))
        .optional()?;
    Ok(value
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(0.0))
}

/// The base resolution rollup every strategy starts from: read the outcome off the edges
/// that recorded it, so a unit cannot silently look live after being killed.
///
/// - an incoming `refutes` or `rules_out` ⇒ [`Resolution::DeadEnd`]
/// - an incoming `supersedes` ⇒ [`Resolution::Superseded`]
/// - an incoming `confirms` or `verifies` ⇒ [`Resolution::Success`]
/// - otherwise the stored resolution (NULL reading as [`Resolution::Unresolved`])
///
/// Death wins over confirmation deliberately: a unit that is both confirmed and refuted is
/// in dispute, and treating it as live-but-dead is safer than treating it as settled-good.
///
/// # Errors
/// Returns an error if a query fails.
pub fn default_rollup(conn: &Connection, node: ItemId) -> Result<Resolution> {
    let incoming = |types: &[EdgeType]| -> Result<bool> {
        let list = types
            .iter()
            .map(|t| format!("'{}'", t.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        // The type strings come from the closed `EdgeType` enum, never from user input.
        let sql =
            format!("SELECT 1 FROM edges WHERE dst_item_id = ?1 AND type IN ({list}) LIMIT 1");
        Ok(conn
            .prepare_cached(&sql)?
            .query_row([node.get()], |_| Ok(true))
            .optional()?
            .unwrap_or(false))
    };

    if incoming(&[EdgeType::Refutes, EdgeType::RulesOut])? {
        return Ok(Resolution::DeadEnd);
    }
    if incoming(&[EdgeType::Supersedes])? {
        return Ok(Resolution::Superseded);
    }
    if incoming(&[EdgeType::Confirms, EdgeType::Verifies])? {
        return Ok(Resolution::Success);
    }
    Ok(crate::item::get_resolution(conn, node)?.unwrap_or(Resolution::Unresolved))
}

#[cfg(test)]
mod tests {
    use super::{for_namespace, resolve, AVAILABLE, BASE_KINDS};
    use crate::{ns, Db};

    #[test]
    fn resolve_known_and_unknown_strategies() {
        for name in AVAILABLE {
            assert_eq!(resolve(name).unwrap().name(), *name);
        }
        let err = match resolve("evolutionary-search") {
            Ok(_) => panic!("expected an unknown-type error"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("unknown investigation type `evolutionary-search`"));
        // Deferred strategies are documented, not registered — the error must say so by
        // listing what this build does have.
        assert!(err.contains("debugging"), "{err}");
        assert!(err.contains("conjecture-attack"), "{err}");
    }

    #[test]
    fn every_strategy_inherits_the_base_kinds_and_declares_unique_verbs() {
        for name in AVAILABLE {
            let strategy = resolve(name).unwrap();
            for base in BASE_KINDS {
                assert!(
                    strategy.accepts_kind(base),
                    "{name} must accept the base kind {base}"
                );
            }
            assert!(!strategy.accepts_kind("not-a-kind"));

            let mut verbs: Vec<&str> = strategy.verbs().iter().map(|v| v.verb).collect();
            assert!(!verbs.is_empty(), "{name} declares no verbs");
            verbs.sort_unstable();
            let count = verbs.len();
            verbs.dedup();
            assert_eq!(verbs.len(), count, "{name} has duplicate verb names");

            // Every verb creates a kind the strategy itself accepts.
            for verb in strategy.verbs() {
                assert!(
                    strategy.accepts_kind(verb.kind),
                    "{name}'s `{}` creates the unaccepted kind {}",
                    verb.verb,
                    verb.kind
                );
            }
        }
    }

    #[test]
    fn a_namespace_resolves_its_strategy_by_inheritance_and_untyped_stays_untyped() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |conn, meta| {
            let root = ns::ensure(conn, "memory/jkb/bug")?;
            ns::set_type(conn, meta, root, "debugging")?;
            ns::ensure(conn, "memory/jkb/bug/hypotheses")?;
            ns::ensure(conn, "tasks/jkb")?;
            Ok(())
        })
        .unwrap();

        let (source, strategy) = db
            .read(|conn| for_namespace(conn, "memory/jkb/bug/hypotheses"))
            .unwrap()
            .unwrap();
        assert_eq!(source, "memory/jkb/bug");
        assert_eq!(strategy.name(), "debugging");

        // An ordinary namespace has no descriptor and is unaffected.
        assert!(db
            .read(|conn| for_namespace(conn, "tasks/jkb"))
            .unwrap()
            .is_none());
    }
}
