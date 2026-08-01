//! Typed namespaces: the pluggable namespace-behaviour seam (design Dmem.1, D33).
//!
//! A namespace can declare a *type* (`namespaces.metadata.type`, written by
//! [`crate::ns::set_type`]) which resolves to a [`NamespaceType`] descriptor. A descriptor
//! plays one of two [roles](TypeRole):
//!
//! - **[`TypeRole::Investigation`]** — a coordination *strategy*: it declares the item
//!   kinds and edge types the investigation uses, the verbs an agent drives it with, how a
//!   unit's outcome rolls up from its edges, how the frontier is ranked, and the acceptance
//!   test that says the investigation is finished ([`debugging`], [`conjecture`]).
//! - **[`TypeRole::Contract`]** — nothing but a statement of what may live in the
//!   namespace, enforced at the writer boundary ([`tasks`], [`views`], [`journal`]). A
//!   contract type has no verbs, no frontier and no acceptance predicate, and says so
//!   rather than stubbing them.
//!
//! ## The guarantee
//! [`check_placement`] is called from `placement::place` — the single choke point through
//! which an item enters a namespace — so a namespace's declared contract holds for *every*
//! writer, not only for the engine that happens to know about it (design D33.2).
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
//! list — mirroring `jkb_sync::serializers::resolve`. Adding a type is one `static`
//! plus one match arm plus one entry in [`AVAILABLE`]; nothing else in the engine changes.
//! [`RESERVED_TYPES`] maps the reserved roots (design D32) to the contract each carries, so
//! they are typed on creation rather than by anyone remembering to.
//!
//! Untyped namespaces (every namespace that predates this) resolve to no descriptor and
//! behave exactly as before.

pub mod conjecture;
pub mod debugging;
pub mod journal;
pub mod tasks;
pub mod views;

use rusqlite::{Connection, OptionalExtension};

use jkb_types::{EdgeType, Error as TypeError, ItemId, NamespaceId, Resolution};

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

/// Every kind an investigation namespace accepts: [`BASE_KINDS`] **plus `task`**.
///
/// Ordinary work legitimately lives inside an investigation namespace — that is precisely
/// what makes `is:frontier` a strict generalization of `is:ready` (Dmem: for a task, with
/// its NULL resolution, the two select exactly the same rows). A strategy's vocabulary
/// therefore may not exclude tasks, or the writer-boundary check would break an invariant
/// the memory design already shipped.
///
/// Contract types override [`NamespaceType::base_kinds`] to `&[]` instead, which is what
/// keeps *them* exact.
pub const INVESTIGATION_KINDS: &[&str] = &[
    KIND_GOAL,
    KIND_NODE,
    KIND_ARTIFACT,
    KIND_REFLECTION,
    tasks::KIND_TASK,
];

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

/// What a namespace type governs (design D33.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeRole {
    /// A coordination protocol: verbs, a frontier, a ranking and an acceptance predicate.
    /// `jkb inv` drives these.
    Investigation,
    /// A statement of what may live in the namespace, and nothing more. No verbs, no
    /// frontier, no acceptance predicate — a contract type says so rather than stubbing
    /// them, so `jkb inv verbs`/`new` on one is a clean user error instead of an empty list.
    Contract,
}

/// A registered namespace type.
///
/// `Send + Sync` and `'static` so a resolved descriptor can be used from the writer
/// thread and held across calls.
pub trait NamespaceType: Send + Sync {
    /// The stable name stored in `namespaces.metadata.type`.
    fn name(&self) -> &'static str;

    /// One-line description of what this type is for.
    fn about(&self) -> &'static str;

    /// Which of the two roles this type plays. Defaults to [`TypeRole::Investigation`] —
    /// the role every type had before contract types existed.
    fn role(&self) -> TypeRole {
        TypeRole::Investigation
    }

    /// The item kinds this type uses, beyond [`NamespaceType::base_kinds`].
    fn node_kinds(&self) -> &'static [NodeKindSpec];

    /// The base kinds this type inherits. Investigation strategies get
    /// [`INVESTIGATION_KINDS`] (the four universal roles, plus `task`); a contract type
    /// overrides this to `&[]`, which is what makes its [`NamespaceType::accepts_kind`]
    /// *exact* — the `tasks` contract accepts `task` and nothing else, not `task` plus five
    /// investigation kinds.
    fn base_kinds(&self) -> &'static [&'static str] {
        INVESTIGATION_KINDS
    }

    /// The subset of the global edge vocabulary this type uses. Advisory: it drives
    /// `--help` and `jkb inv kinds`, and is *not* enforced on writes — an investigation
    /// must always be able to record an association it has no vocabulary for yet
    /// (design Dmem.8, pitfall 1: no over-structuring).
    fn edge_types(&self) -> &'static [EdgeType] {
        &[]
    }

    /// The verbs `jkb inv do` dispatches for this type. Empty for a contract type.
    fn verbs(&self) -> &'static [VerbSpec] {
        &[]
    }

    /// Whether `kind` is a valid item kind here: one of [`NamespaceType::base_kinds`] or one
    /// of [`NamespaceType::node_kinds`].
    fn accepts_kind(&self, kind: &str) -> bool {
        self.base_kinds().contains(&kind) || self.node_kinds().iter().any(|k| k.kind == kind)
    }

    /// Every kind this type accepts, in declaration order — the actionable half of a
    /// rejection message.
    fn accepted_kinds(&self) -> Vec<&'static str> {
        self.base_kinds()
            .iter()
            .copied()
            .chain(self.node_kinds().iter().map(|k| k.kind))
            .collect()
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
    /// The default is only reachable for a [`TypeRole::Contract`] type, where "this has no
    /// acceptance test" is the truthful answer — so it errors saying that, rather than
    /// returning a [`DoneState`] the caller would have to distrust.
    ///
    /// # Errors
    /// Returns an error if a query fails, or a validation error if this is a contract type.
    fn goal_predicate(&self, conn: &Connection, ns_path: &str) -> Result<DoneState> {
        let _ = (conn, ns_path);
        Err(not_an_investigation(self.name()))
    }

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

/// The investigation **strategies** registered in this build — the types `jkb inv new`
/// accepts. A subset of [`AVAILABLE`].
pub const STRATEGIES: &[&str] = &[debugging::NAME, conjecture::NAME];

/// Every namespace type registered in this build, strategies and contracts alike.
pub const AVAILABLE: &[&str] = &[
    debugging::NAME,
    conjecture::NAME,
    tasks::NAME,
    views::NAME,
    journal::NAME,
];

/// The contract each reserved root carries (design D33.4). Applied from two directions so a
/// reserved namespace is never left untyped: migration `V008` back-fills existing
/// databases, and [`crate::ns::ensure`] stamps a reserved path as it creates it.
///
/// ## A type is not a location marker
/// The direction here is one-way: a **reserved root** (design D32's fixed layout — `tasks`,
/// `repos`, `media`, `references`, `memory`, `_sys`) is told which contract it carries. The
/// inverse — asking "which namespace carries the `tasks` contract?" to *find* the tasks root
/// — is deliberately not supported. `tasks/` is the tasks root because the layout reserves
/// it, full stop; the contract only says what may live there.
///
/// These roots are a handful of declared special cases, and generalizing them into a
/// location-discovery mechanism made a type mean two things at once (a constraint, which is
/// naturally many-to-many, and a location, which is singular) — which then needed a
/// uniqueness guard, an escape hatch, and a re-seeding guard to hold together. The layout
/// belongs in the layout.
pub const RESERVED_TYPES: &[(&str, &str)] = &[
    ("tasks", tasks::NAME),
    ("_sys/views", views::NAME),
    ("_sys/sync", journal::NAME),
    ("_sys/transactions", journal::NAME),
    ("_sys/ingestions", journal::NAME),
];

static DEBUGGING: debugging::Debugging = debugging::Debugging;
static CONJECTURE: conjecture::ConjectureAttack = conjecture::ConjectureAttack;
static TASKS: tasks::Tasks = tasks::Tasks;
static VIEWS: views::Views = views::Views;
static JOURNAL: journal::Journal = journal::Journal;

/// Resolve a namespace type by name, rejecting unknown names with an actionable error
/// listing what *is* available (mirroring `jkb_sync::serializers::resolve`).
///
/// # Errors
/// Returns a validation error if `name` is not a type in this build.
pub fn resolve(name: &str) -> Result<&'static dyn NamespaceType> {
    match name {
        debugging::NAME => Ok(&DEBUGGING),
        conjecture::NAME => Ok(&CONJECTURE),
        tasks::NAME => Ok(&TASKS),
        views::NAME => Ok(&VIEWS),
        journal::NAME => Ok(&JOURNAL),
        other => Err(TypeError::Validation(format!(
            "unknown namespace type `{other}`; available: {}",
            AVAILABLE.join(", ")
        ))
        .into()),
    }
}

/// Resolve a name that must be an investigation **strategy**, rejecting a contract type
/// with an error that says why rather than letting `jkb inv new` type a namespace with
/// something that has no verbs and no acceptance predicate.
///
/// # Errors
/// Returns a validation error if `name` is unknown or is not a strategy.
pub fn resolve_strategy(name: &str) -> Result<&'static dyn NamespaceType> {
    // An unknown name is reported against the STRATEGY list, not the full one: offering
    // `tasks` or `journal` as alternatives to a misspelled strategy would be a worse answer
    // than the misspelling.
    let Ok(ty) = resolve(name) else {
        return Err(TypeError::Validation(format!(
            "unknown investigation type `{name}`; available: {}",
            STRATEGIES.join(", ")
        ))
        .into());
    };
    if ty.role() != TypeRole::Investigation {
        return Err(TypeError::Validation(format!(
            "`{name}` is a contract type ({}), not an investigation strategy; strategies: {}",
            ty.about(),
            STRATEGIES.join(", ")
        ))
        .into());
    }
    Ok(ty)
}

/// The error a [`TypeRole::Contract`] type answers investigation questions with.
fn not_an_investigation(name: &str) -> crate::Error {
    TypeError::Validation(format!(
        "`{name}` is a contract type: it constrains what may live in the namespace and has \
         no verbs, frontier or acceptance predicate"
    ))
    .into()
}

/// Reject an item whose `kind` the namespace's type does not accept, naming the namespace,
/// the type, and what it *does* accept.
fn reject_kind(ns_path: &str, ty: &dyn NamespaceType, kind: &str) -> crate::Error {
    let accepted = ty.accepted_kinds();
    let accepted = if accepted.is_empty() {
        "nothing (it surfaces a system table and holds no items)".to_owned()
    } else {
        accepted.join(", ")
    };
    TypeError::Validation(format!(
        "namespace `{ns_path}` is typed `{}`, which does not accept items of kind `{kind}`; \
         it accepts: {accepted}",
        ty.name()
    ))
    .into()
}

/// Validate that `item` may be placed under `namespace` (design D33.2).
///
/// Called from `crate::placement::place` — the single choke point through which an item
/// enters a namespace — so a typed namespace's contract holds for every writer, not only
/// for the engine that knows about it. An **untyped** namespace accepts anything, which is
/// every namespace that predates typing.
///
/// # Errors
/// Returns a validation error if the namespace's effective type rejects the item's kind;
/// otherwise a database error.
pub fn check_placement(conn: &Connection, item: ItemId, namespace: NamespaceId) -> Result<()> {
    let Some(path): Option<String> = conn
        .prepare_cached("SELECT path FROM namespaces WHERE id = ?1")?
        .query_row([namespace.get()], |row| row.get(0))
        .optional()?
    else {
        // A placement into a namespace that does not exist fails on the foreign key a
        // moment later; there is no contract to check.
        return Ok(());
    };
    let Some((_, ty)) = for_namespace(conn, &path)? else {
        return Ok(());
    };
    let kind: Option<String> = conn
        .prepare_cached("SELECT kind FROM items WHERE id = ?1")?
        .query_row([item.get()], |row| row.get(0))
        .optional()?;
    let Some(kind) = kind else { return Ok(()) };
    if ty.accepts_kind(&kind) {
        return Ok(());
    }
    Err(reject_kind(&path, ty, &kind))
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
    use super::{
        check_placement, for_namespace, resolve, resolve_strategy, TypeRole, AVAILABLE, BASE_KINDS,
        RESERVED_TYPES, STRATEGIES,
    };
    use crate::{item, ns, placement, Db};
    use jkb_types::PlacementRole;

    #[test]
    fn resolve_known_and_unknown_types() {
        for name in AVAILABLE {
            assert_eq!(resolve(name).unwrap().name(), *name);
        }
        let err = match resolve("no-such-type") {
            Ok(_) => panic!("expected an unknown-type error"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("unknown namespace type `no-such-type`"),
            "{err}"
        );
        for name in AVAILABLE {
            assert!(err.contains(name), "{err} must list {name}");
        }
    }

    #[test]
    fn resolve_strategy_refuses_unknown_names_and_contract_types() {
        for name in STRATEGIES {
            assert_eq!(resolve_strategy(name).unwrap().name(), *name);
        }

        // An unknown name is reported against the STRATEGY list — offering `journal` as an
        // alternative to a misspelled strategy would be a worse answer than the misspelling.
        let err = match resolve_strategy("evolutionary-search") {
            Ok(_) => panic!("expected an unknown-type error"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("unknown investigation type `evolutionary-search`"));
        // Deferred strategies are documented, not registered — the error must say so by
        // listing what this build does have.
        assert!(err.contains("debugging"), "{err}");
        assert!(err.contains("conjecture-attack"), "{err}");
        assert!(
            !err.contains("journal"),
            "contracts are not strategies: {err}"
        );

        // A registered contract type is refused with *why*, not with "unknown".
        let err = match resolve_strategy(super::tasks::NAME) {
            Ok(_) => panic!("expected a contract-type rejection"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("is a contract type"), "{err}");
    }

    #[test]
    fn every_strategy_inherits_the_base_kinds_and_declares_unique_verbs() {
        for name in STRATEGIES {
            let strategy = resolve(name).unwrap();
            assert_eq!(strategy.role(), TypeRole::Investigation);
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

    /// A contract type is *exact*: it must not inherit the four investigation base kinds,
    /// or `tasks` would silently accept a `goal` and the contract would mean nothing.
    #[test]
    fn a_contract_type_accepts_only_its_own_kinds_and_answers_no_investigation_questions() {
        for name in AVAILABLE.iter().filter(|n| !STRATEGIES.contains(n)) {
            let ty = resolve(name).unwrap();
            assert_eq!(ty.role(), TypeRole::Contract, "{name}");
            assert!(ty.verbs().is_empty(), "{name} is a contract with verbs");
            for base in BASE_KINDS {
                assert!(
                    !ty.accepts_kind(base),
                    "the {name} contract must not inherit the base kind {base}"
                );
            }
            // The acceptance predicate says "I am a contract" rather than faking a verdict.
            let db = Db::open_in_memory().unwrap();
            let err = db
                .read(move |conn| {
                    Ok(ty
                        .goal_predicate(conn, "tasks")
                        .err()
                        .map(|e| e.to_string()))
                })
                .unwrap()
                .unwrap_or_default();
            assert!(err.contains("is a contract type"), "{name}: {err}");
        }

        let tasks = resolve(super::tasks::NAME).unwrap();
        assert!(tasks.accepts_kind("task"));
        assert!(!tasks.accepts_kind("view"));
        // `journal` accepts nothing at all — the whole point.
        assert!(resolve(super::journal::NAME)
            .unwrap()
            .accepted_kinds()
            .is_empty());
    }

    #[test]
    fn a_namespace_resolves_its_type_by_inheritance_and_untyped_stays_untyped() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |conn, meta| {
            let root = ns::ensure(conn, "memory/jkb/bug")?;
            ns::set_type(conn, meta, root, "debugging")?;
            ns::ensure(conn, "memory/jkb/bug/hypotheses")?;
            ns::ensure(conn, "repos/jkb/docs")?;
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
            .read(|conn| for_namespace(conn, "repos/jkb/docs"))
            .unwrap()
            .is_none());
    }

    /// The reserved roots are typed as they are created, without anyone applying the type
    /// (design D33.4) — and the type reaches the whole subtree by inheritance.
    #[test]
    fn ensure_stamps_the_reserved_roots_and_the_subtree_inherits() {
        let db = Db::open_in_memory().unwrap();
        db.write_txn("t", |conn, _meta| {
            for (path, _) in RESERVED_TYPES {
                ns::ensure(conn, path)?;
            }
            ns::ensure(conn, "tasks/jkb/.backlog")?;
            Ok(())
        })
        .unwrap();

        for (path, expected) in RESERVED_TYPES {
            let got = db.read(move |conn| ns::get_type(conn, path)).unwrap();
            assert_eq!(got.as_deref(), Some(*expected), "{path}");
        }
        let (source, ty) = db
            .read(|conn| for_namespace(conn, "tasks/jkb/.backlog"))
            .unwrap()
            .unwrap();
        assert_eq!((source.as_str(), ty.name()), ("tasks", super::tasks::NAME));
    }

    /// The guarantee this whole change exists for: a raw `item::upsert` + `placement::place`
    /// into a typed namespace is checked, not just the investigation engine's own writes.
    #[test]
    fn placement_enforces_the_namespace_contract_for_any_writer() {
        let db = Db::open_in_memory().unwrap();
        let err = db
            .write_txn("t", |conn, meta| {
                let ns_id = ns::ensure(conn, "tasks/jkb")?;
                let note = item::upsert(
                    conn,
                    meta,
                    &item::NewItem {
                        uid: "note:stray".to_owned(),
                        kind: "note".to_owned(),
                        content: Some("filed in the wrong place".to_owned()),
                        content_hash: None,
                        mime: None,
                    },
                )?;
                // The check must fire here, not in `investigation::add`, which this path
                // never touches.
                Ok(check_placement(conn, note, ns_id)
                    .err()
                    .map(|e| e.to_string()))
            })
            .unwrap()
            .unwrap_or_default();
        assert!(err.contains("typed `tasks`"), "{err}");
        assert!(err.contains("kind `note`"), "{err}");
        assert!(err.contains("accepts: task"), "{err}");

        // …and it fires through `placement::place` itself, rolling the transaction back.
        let outcome = db.write_txn("t", |conn, meta| {
            let ns_id = ns::ensure(conn, "_sys/views")?;
            let note = item::upsert(
                conn,
                meta,
                &item::NewItem {
                    uid: "note:stray2".to_owned(),
                    kind: "note".to_owned(),
                    content: None,
                    content_hash: None,
                    mime: None,
                },
            )?;
            placement::place(conn, meta, note, ns_id, PlacementRole::Primary, 0)
        });
        assert!(
            outcome.is_err(),
            "a note must not be placed under _sys/views"
        );

        // An untyped namespace still takes anything — every namespace that predates typing.
        db.write_txn("t", |conn, meta| {
            let ns_id = ns::ensure(conn, "references/web")?;
            let note = item::upsert(
                conn,
                meta,
                &item::NewItem {
                    uid: "note:fine".to_owned(),
                    kind: "note".to_owned(),
                    content: None,
                    content_hash: None,
                    mime: None,
                },
            )?;
            placement::place(conn, meta, note, ns_id, PlacementRole::Primary, 0)
        })
        .unwrap();
    }
}
