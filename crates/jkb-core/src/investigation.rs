//! The investigation engine: create a typed investigation, add units and edges to it, and
//! read it back through the six retrieval primitives (design Dmem.0/Dmem.5).
//!
//! An **investigation** is a typed namespace holding a typed, scored graph of units. A
//! fresh agent resumes one by reading three status-partitioned buckets plus a digest:
//!
//! | Bucket | What it is | Read |
//! |---|---|---|
//! | **frontier** | live, unblocked work, ranked by promise | [`frontier`] |
//! | **confirmed core** | locked results — the current best model | [`confirmed_core`] |
//! | **tombstones** | dead ends + what killed each — the anti-retread set | [`tombstones`] |
//!
//! The tombstones bucket is the highest-value one and the easiest to get wrong: nothing is
//! ever hard-deleted here. A dead end is *retained* together with the edge that killed it,
//! because "we tried this and here is why it failed" is the memory that stops the next
//! agent — or the next run of the same agent — from spending a day re-deriving it.
//!
//! Everything is ordinary items, typed edges, and tags: no new tables, and every write goes
//! through the writer-actor so it is audited and undoable like any other mutation.

use rusqlite::{Connection, OptionalExtension};

use jkb_types::{EdgeType, Error as TypeError, ItemId, PlacementRole, Resolution};

use crate::nstype::{
    self, NamespaceType, TargetRule, VerbSpec, FACET_PROMISE, KIND_GOAL, KIND_REFLECTION,
};
use crate::query::{Query, Scope};
use crate::store::WriteMeta;
use crate::{dsl, edge, item, ns, placement, tag, Error, Result};
use std::fmt::Write as _;

/// The root under which investigations live (design D32's reserved semantic root).
pub const MEMORY_ROOT: &str = "memory";

/// The longest slug prefix carried into a minted unit uid before the suffix.
const UID_SLUG_MAX: usize = 32;

/// Mint a unit uid: `<kind>:<slug>-<nanos hex>`, mirroring [`crate::task::mint_uid`] so the
/// CLI, MCP, and file sync all derive the same shape from the same title.
#[must_use]
pub fn mint_uid(kind: &str, title: &str) -> String {
    let slug: String = dsl::slug(title).chars().take(UID_SLUG_MAX).collect();
    let slug = if slug.is_empty() { "unit" } else { &slug };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{kind}:{slug}-{nanos:x}")
}

/// A unit to add to an investigation.
#[derive(Debug, Clone)]
pub struct NewUnit {
    /// The item kind — must be one the strategy accepts.
    pub kind: String,
    /// The unit's body text (the claim, the observation, the reason).
    pub content: String,
    /// The namespace to home it under (inside the investigation).
    pub namespace: String,
    /// `facet=value` tags (`promise=`, `confidence=`, `commit-range=`, …).
    pub tags: Vec<(String, String)>,
    /// Edges to link from this unit: `(edge type, target uid, optional weight)`.
    pub edges: Vec<(EdgeType, String, Option<f64>)>,
    /// Edges to link *into* this unit from an existing unit — the blocking direction
    /// (`target depends_on this`).
    pub reverse_edges: Vec<(EdgeType, String, Option<f64>)>,
}

impl NewUnit {
    /// A unit of `kind` with `content`, homed at `namespace`, no tags and no edges.
    #[must_use]
    pub fn new(kind: impl Into<String>, content: impl Into<String>, namespace: &str) -> Self {
        Self {
            kind: kind.into(),
            content: content.into(),
            namespace: namespace.to_owned(),
            tags: Vec::new(),
            edges: Vec::new(),
            reverse_edges: Vec::new(),
        }
    }
}

/// Create a typed investigation namespace and seed its `goal` unit.
///
/// `path` is the investigation's namespace (conventionally `memory/<repo>/<name>`, or
/// `memory/<name>` for one that belongs to no repo). `type_name` must name a registered
/// strategy — an unknown one is rejected with the available list rather than silently
/// creating an untyped namespace. `goal_body` is the root intent **plus its acceptance
/// predicate**: the investigation terminates on that predicate, never on a timer, so an
/// investigation with a vague goal cannot be finished.
///
/// Returns the seeded goal unit's id — or the **existing** goal's id if the investigation
/// was already created, so the call is idempotent (re-running it does not pile up goals).
///
/// # Errors
/// Returns a validation error if `type_name` is not a registered strategy, if `path` is
/// malformed, or if `path` is already an investigation of a *different* type; otherwise a
/// database error.
pub fn create(
    conn: &Connection,
    meta: &WriteMeta,
    path: &str,
    type_name: &str,
    goal_kind: &str,
    goal_body: &str,
    tags: &[(String, String)],
) -> Result<ItemId> {
    // `resolve_strategy`, not `resolve`: a contract type (`tasks`, `views`, `journal`) has
    // no verbs and no acceptance predicate, so typing a namespace with one here would
    // produce an investigation that can never be driven or finished.
    let strategy = nstype::resolve_strategy(type_name)?;
    if !strategy.accepts_kind(goal_kind) {
        return Err(reject_kind(strategy, goal_kind));
    }
    let root = ns::ensure(conn, path)?;

    // Re-typing an existing investigation is refused, not silently applied. The units
    // already stored here were written under the old strategy's vocabulary: after a re-type
    // its `kind`s stop being accepted (so further writes against existing data fail) and
    // `goal_predicate` becomes a test the investigation was never built for. The namespace's
    // OWN type is what matters — a nested investigation inside a typed parent is fine.
    if let Some(existing) = ns::get_type_by_id(conn, root)? {
        if existing != type_name {
            return Err(Error::Types(TypeError::Validation(format!(
                "`{path}` is already a `{existing}` investigation; refusing to re-type it as \
                 `{type_name}` (its existing units were recorded under `{existing}`'s kinds). \
                 Create the new investigation at a different path."
            ))));
        }
    } else {
        ns::set_type(conn, meta, root, type_name)?;
    }
    save_bucket_views(conn, meta, path)?;

    // Idempotent: an investigation has one goal. Re-running `create` returns the goal it
    // already has rather than appending another (which would duplicate it on the frontier
    // and make `goals` ambiguous).
    if let Some(existing) = goals(conn, path)?.first() {
        return Ok(existing.id);
    }

    let mut unit = NewUnit::new(goal_kind, goal_body, path);
    unit.tags = tags.to_vec();
    add(conn, meta, &unit)
}

/// The saved-view names for the investigation at `ns_path`: one per bucket
/// (`<path>-frontier`, `-core`, `-tombstones`).
#[must_use]
pub fn bucket_view_names(ns_path: &str) -> [String; 3] {
    [
        format!("{ns_path}-frontier"),
        format!("{ns_path}-core"),
        format!("{ns_path}-tombstones"),
    ]
}

/// Save a view per bucket so the three-bucket read is reachable from the *generic* surface
/// (`jkb view ls` / `jkb view run`), not only from `jkb inv`. An agent that has never heard
/// of investigations can still find the frontier.
///
/// The views are plain DSL queries, so they render the buckets without the strategy's
/// ranking — `jkb inv frontier` is still the ranked read.
fn save_bucket_views(conn: &Connection, meta: &WriteMeta, ns_path: &str) -> Result<()> {
    let [frontier_name, core_name, tombstones_name] = bucket_view_names(ns_path);
    for (name, dsl) in [
        (
            frontier_name,
            format!("is:frontier ns:{ns_path}/**,{ns_path}"),
        ),
        (
            core_name,
            format!("resolution:success ns:{ns_path}/**,{ns_path}"),
        ),
        (
            tombstones_name,
            format!("is:tombstone ns:{ns_path}/**,{ns_path}"),
        ),
    ] {
        crate::view::save(conn, meta, &name, &dsl)?;
    }
    Ok(())
}

/// Add a unit to an investigation: insert the item, home it, apply its tags, and link its
/// edges — all inside the caller's transaction, so a rejected edge rolls the whole unit
/// back rather than leaving a dangling node.
///
/// The unit's `kind` is validated against the strategy governing its namespace. An
/// **untyped** namespace accepts any kind: the engine does not require an investigation to
/// be typed before you can record something in it.
///
/// # Errors
/// Returns a validation error if the kind is not one the strategy accepts, if an edge names
/// a uid that does not exist, or if an edge would create a `depends_on` cycle; otherwise a
/// database error.
pub fn add(conn: &Connection, meta: &WriteMeta, unit: &NewUnit) -> Result<ItemId> {
    if let Some((_, strategy)) = nstype::for_namespace(conn, &unit.namespace)? {
        if !strategy.accepts_kind(&unit.kind) {
            return Err(reject_kind(strategy, &unit.kind));
        }
    }

    let uid = mint_uid(&unit.kind, &unit.content);
    let id = item::upsert(
        conn,
        meta,
        &item::NewItem {
            uid,
            kind: unit.kind.clone(),
            content: Some(unit.content.clone()),
            // Deliberately NOT content-addressed: two units can legitimately say the same
            // thing in different parts of an investigation (the same observation seen twice
            // is two observations), and dedup would silently merge them.
            content_hash: None,
            mime: None,
        },
    )?;
    let home = ns::ensure(conn, &unit.namespace)?;
    placement::place(conn, meta, id, home, PlacementRole::Primary, 0)?;
    for (facet, value) in &unit.tags {
        tag::apply(conn, meta, id, facet, value)?;
    }
    for (edge_type, target_uid, weight) in &unit.edges {
        let target = resolve_uid(conn, target_uid)?;
        edge::link_weighted(conn, meta, id, target, *edge_type, *weight, None)?;
    }
    for (edge_type, source_uid, weight) in &unit.reverse_edges {
        let source = resolve_uid(conn, source_uid)?;
        edge::link_weighted(conn, meta, source, id, *edge_type, *weight, None)?;
    }
    Ok(id)
}

/// One invocation of a strategy verb.
#[derive(Debug, Clone, Copy)]
pub struct VerbCall<'a> {
    /// The verb name as declared by the strategy (e.g. `rule-out`).
    pub verb: &'a str,
    /// The body of the unit the verb creates.
    pub content: &'a str,
    /// The uid of the unit it acts on, if the verb takes a target.
    pub target_uid: Option<&'a str>,
    /// The weight for a signed evidence edge (`supports`/`contradicts`).
    pub weight: Option<f64>,
    /// Extra `facet=value` tags, on top of the verb's own.
    pub tags: &'a [(String, String)],
}

impl<'a> VerbCall<'a> {
    /// A verb call with just a body — no target, no weight, no extra tags.
    #[must_use]
    pub fn new(verb: &'a str, content: &'a str) -> Self {
        Self {
            verb,
            content,
            target_uid: None,
            weight: None,
            tags: &[],
        }
    }

    /// Point the call at the unit `uid`.
    #[must_use]
    pub fn on(mut self, uid: &'a str) -> Self {
        self.target_uid = Some(uid);
        self
    }
}

/// The outcome of applying a strategy verb.
#[derive(Debug, Clone)]
pub struct VerbOutcome {
    /// The unit the verb created.
    pub created: ItemId,
    /// Its minted uid.
    pub uid: String,
    /// The target it acted on, if any.
    pub target: Option<ItemId>,
    /// The resolution stamped on the target, if the verb resolves one.
    pub target_resolution: Option<Resolution>,
}

/// Apply a strategy verb inside the investigation at `ns_path` (design Dmem.1's
/// `cli_verbs`): create the verb's unit, link its edge to `target`, and stamp the target's
/// resolution if the verb resolves one.
///
/// Verbs are *data* on the descriptor, so a strategy adds one without touching this engine
/// or the CLI. The gates enforced here are the ones that make an investigation trustworthy:
/// a verb that requires a target gets one, and a verb that restricts its target's kind (the
/// gated `certify`, for example) is refused rather than silently mis-applied.
///
/// # Errors
/// Returns a validation error if the namespace is untyped, the verb is unknown to the
/// strategy, a required target is missing (or a forbidden one supplied), or the target's
/// kind is not one the verb accepts; otherwise a database error.
pub fn apply_verb(
    conn: &Connection,
    meta: &WriteMeta,
    ns_path: &str,
    call: &VerbCall,
) -> Result<VerbOutcome> {
    let VerbCall {
        verb: verb_name,
        content,
        target_uid,
        weight,
        tags: extra_tags,
    } = *call;
    let Some((_, strategy)) = nstype::for_namespace(conn, ns_path)? else {
        return Err(TypeError::Validation(format!(
            "`{ns_path}` is not an investigation namespace; create one with \
             `jkb inv new <type> <path>` (types: {})",
            nstype::AVAILABLE.join(", ")
        ))
        .into());
    };
    let verb = strategy
        .verbs()
        .iter()
        .find(|v| v.verb == verb_name)
        .ok_or_else(|| unknown_verb(strategy, verb_name))?;

    let target = match (verb.target, target_uid) {
        (TargetRule::Required, None) => {
            return Err(TypeError::Validation(format!(
                "`{}` needs a target: {}",
                verb.verb, verb.about
            ))
            .into())
        }
        (TargetRule::Forbidden, Some(uid)) => {
            return Err(TypeError::Validation(format!(
                "`{}` takes no target, but `{uid}` was given",
                verb.verb
            ))
            .into())
        }
        (_, Some(uid)) => Some(resolve_uid(conn, uid)?),
        (_, None) => None,
    };

    if let Some(target) = target {
        let target_kind = item::get(conn, target)?.map(|m| m.kind).unwrap_or_default();
        check_target_kind(verb, &target_kind)?;
    }

    let mut unit = NewUnit::new(verb.kind, content, ns_path);
    unit.tags = verb
        .tags
        .iter()
        .map(|(f, v)| ((*f).to_owned(), (*v).to_owned()))
        .chain(extra_tags.iter().cloned())
        .collect();
    if let (Some(edge_type), Some(target)) = (verb.edge, target) {
        let target_uid = uid_of(conn, target)?;
        if verb.reverse {
            unit.reverse_edges.push((edge_type, target_uid, weight));
        } else {
            unit.edges.push((edge_type, target_uid, weight));
        }
    }
    let created = add(conn, meta, &unit)?;

    // Resolve the target LAST: if anything above failed, the target keeps its old outcome.
    let mut target_resolution = None;
    if let (Some(target), Some(resolution)) = (target, verb.resolves_target) {
        item::set_resolution(conn, meta, target, resolution)?;
        target_resolution = Some(resolution);
    }

    Ok(VerbOutcome {
        created,
        uid: uid_of(conn, created)?,
        target,
        target_resolution,
    })
}

/// A verb's target-kind gate: refuse to apply a restricted verb to the wrong kind of unit.
///
/// # Errors
/// Returns a validation error naming the accepted kinds.
fn check_target_kind(verb: &VerbSpec, target_kind: &str) -> Result<()> {
    if verb.requires_target_kind.is_empty() || verb.requires_target_kind.contains(&target_kind) {
        return Ok(());
    }
    Err(TypeError::Validation(format!(
        "`{}` cannot act on a `{target_kind}`; its target must be one of {}",
        verb.verb,
        verb.requires_target_kind.join(", ")
    ))
    .into())
}

/// One unit in a bucket read, with everything needed to show it without a second query.
#[derive(Debug, Clone)]
pub struct UnitRow {
    /// The item id.
    pub id: ItemId,
    /// The stable uid.
    pub uid: String,
    /// The item kind.
    pub kind: String,
    /// The body text.
    pub content: Option<String>,
    /// The outcome axis (NULL reads as `unresolved`).
    pub resolution: Option<String>,
    /// The frontier rank the strategy computed (higher first).
    pub rank: f64,
    /// The signed-evidence balance (Σ supports − Σ contradicts).
    pub evidence: f64,
    /// The namespace it is homed under.
    pub namespace: Option<String>,
}

/// The **frontier** (primitive 1): live, unblocked units in the investigation at `ns_path`,
/// ranked by the strategy's `ranking` (highest first, ties by uid for stability).
///
/// `include_claimed` keeps units another agent is already working on — useful for a
/// coordinator taking stock, wrong for handing work out.
///
/// # Errors
/// Returns an error if the namespace is untyped or a query fails.
pub fn frontier(
    conn: &Connection,
    ns_path: &str,
    include_claimed: bool,
    limit: Option<usize>,
) -> Result<Vec<UnitRow>> {
    let strategy = require_strategy(conn, ns_path)?;
    let mut query = strategy.frontier(Scope::Subtree(ns_path.to_owned()));
    if include_claimed {
        query.claimed = None;
    }
    let ids = query.evaluate(conn)?;
    let mut rows = rows_for(conn, &ids, Some(strategy))?;
    rows.sort_by(|a, b| {
        b.rank
            .partial_cmp(&a.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.uid.cmp(&b.uid))
    });
    if let Some(limit) = limit {
        rows.truncate(limit);
    }
    Ok(rows)
}

/// The **confirmed core** (the current best model): units resolved `success` in the
/// investigation at `ns_path`. This is what a fresh agent should treat as settled.
///
/// # Errors
/// Returns an error if a query fails.
pub fn confirmed_core(conn: &Connection, ns_path: &str) -> Result<Vec<UnitRow>> {
    let ids = Query {
        resolution: Some(Resolution::Success.as_str().to_owned()),
        scope: Scope::Subtree(ns_path.to_owned()),
        ..Query::default()
    }
    .evaluate(conn)?;
    rows_for(conn, &ids, None)
}

/// A tombstone plus **what killed it** — the anti-retread record.
#[derive(Debug, Clone)]
pub struct Tombstone {
    /// The dead unit (retained, never deleted).
    pub unit: UnitRow,
    /// The units that killed it and how: `(edge type, killer uid, killer content)`. Empty
    /// when the resolution was set by hand with no edge recording why — which is exactly
    /// the case `jkb inv digest` should make visible, because a dead end without a reason
    /// teaches the next agent nothing.
    pub killed_by: Vec<(EdgeType, String, Option<String>)>,
}

/// The **tombstones** bucket (primitive 3 — the anti-retread set): every unit in the
/// investigation at `ns_path` that is resolved `dead_end`/`superseded` or carries an
/// incoming `refutes`/`rules_out` edge, each paired with what killed it.
///
/// Read this **before** starting work, not after. It is the single highest-value memory in
/// an investigation: without it a fresh agent re-treads, and re-treading is the failure mode
/// every system surveyed in the research hit.
///
/// # Errors
/// Returns an error if a query fails.
pub fn tombstones(conn: &Connection, ns_path: &str) -> Result<Vec<Tombstone>> {
    let ids = Query {
        tombstone: true,
        scope: Scope::Subtree(ns_path.to_owned()),
        ..Query::default()
    }
    .evaluate(conn)?;
    let rows = rows_for(conn, &ids, None)?;

    let mut out = Vec::with_capacity(rows.len());
    for unit in rows {
        let mut killed_by = Vec::new();
        for killer in edge::walk(
            conn,
            unit.id,
            &[
                EdgeType::Refutes,
                EdgeType::RulesOut,
                EdgeType::Supersedes,
                EdgeType::ExplainsFailure,
            ],
            1,
            edge::Direction::In,
        )? {
            if let Some(row) = item::get(conn, killer.item)? {
                killed_by.push((killer.via, row.uid, row.content));
            }
        }
        out.push(Tombstone { unit, killed_by });
    }
    Ok(out)
}

/// The **anti-retread check** (primitive 3, applied to one unit): the tombstones *related to*
/// `node`, found by walking the graph around it — so "has anyone tried this?" is one call
/// before any work starts.
///
/// Walks associative and lineage edges in both directions, then keeps whatever is a
/// tombstone. `depth` bounds the walk.
///
/// # Errors
/// Returns an error if a query fails.
pub fn anti_retread(conn: &Connection, node: ItemId, depth: usize) -> Result<Vec<UnitRow>> {
    let related = edge::walk(
        conn,
        node,
        &[
            EdgeType::References,
            EdgeType::DerivedFrom,
            EdgeType::MemberOf,
            EdgeType::ReducesTo,
            EdgeType::DependsOn,
            EdgeType::Narrows,
            EdgeType::Spawns,
            EdgeType::DiscoveredFrom,
        ],
        depth,
        edge::Direction::Both,
    )?;
    let ids: Vec<ItemId> = related.iter().map(|r| r.item).collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // Apply the tombstone predicate to exactly the reached ids. Restricting the query rather
    // than filtering its output afterwards matters: this read sits on the path an agent runs
    // before every unit, and the unrestricted form scanned every tombstone in the whole KB —
    // across every repo and investigation — to keep a handful. The SQL definition of
    // "tombstone" still lives in one place.
    let kept = Query {
        tombstone: true,
        ids,
        ..Query::default()
    }
    .evaluate(conn)?;
    rows_for(conn, &kept, None)
}

/// Recompute and store every unit's resolution from its edges (the strategy's
/// `resolution_rollup`), so an outcome recorded as an edge cannot be contradicted by a stale
/// column. Returns the units whose resolution changed, as `(uid, from, to)`.
///
/// **Tasks** in the namespace are skipped: a task mirrored or placed inside an investigation
/// keeps its lifecycle on `status`, and writing its `resolution` would split `is:frontier`
/// from `is:ready` (see [`resolve_unit`]).
///
/// # Errors
/// Returns an error if the namespace is untyped or a query fails.
pub fn roll_up(
    conn: &Connection,
    meta: &WriteMeta,
    ns_path: &str,
) -> Result<Vec<(String, Resolution, Resolution)>> {
    let strategy = require_strategy(conn, ns_path)?;
    let ids = Query {
        exclude_kinds: vec![KIND_TASK.to_owned()],
        scope: Scope::Subtree(ns_path.to_owned()),
        ..Query::default()
    }
    .evaluate(conn)?;

    let machine = strategy.unit_machine();
    let mut changed = Vec::new();
    for id in ids {
        // The strategy says what it observed; the machine says what that means. Those were one
        // function — a rollup that concluded — and the conclusion encoded the priority of
        // contradictory evidence in the order of its `if`s, where nothing could see it.
        //
        // `reconcile` also refuses ambiguity, so two conditions claiming one unit is reported
        // rather than resolved by whichever question the code asked first.
        let facts = strategy.unit_facts(conn, id)?;
        let current = facts.resolution;
        let outcome = match machine.reconcile(&facts) {
            jkb_fsm::Reconciliation::Settled => continue,
            jkb_fsm::Reconciliation::Fired(out) => out,
            jkb_fsm::Reconciliation::Ambiguous(events) => {
                return Err(Error::Types(TypeError::Validation(format!(
                    "`{}` carries evidence for two outcomes at once ({}), so `{}` will not \
                     choose between them",
                    uid_of(conn, id)?,
                    events
                        .iter()
                        .map(|e| jkb_fsm::Event::name(*e))
                        .collect::<Vec<_>>()
                        .join(" and "),
                    strategy.name(),
                ))))
            }
        };
        for effect in outcome.effects() {
            let nstype::lifecycle::UnitEffect::SetResolution(to) = effect;
            item::set_resolution(conn, meta, id, *to)?;
            changed.push((uid_of(conn, id)?, current, *to));
        }
    }
    Ok(changed)
}

/// A rendered snapshot of an investigation: the three buckets plus the acceptance verdict.
#[derive(Debug, Clone)]
pub struct Digest {
    /// The investigation's namespace.
    pub ns_path: String,
    /// The strategy governing it.
    pub type_name: &'static str,
    /// The acceptance verdict — done, or what is still missing.
    pub done: nstype::DoneState,
    /// The live, unblocked work, ranked.
    pub frontier: Vec<UnitRow>,
    /// The settled results.
    pub confirmed: Vec<UnitRow>,
    /// The dead ends and what killed them.
    pub tombstones: Vec<Tombstone>,
    /// How many units each bucket **elided** to stay under [`DIGEST_BUCKET_CAP`], as
    /// `(frontier, confirmed, tombstones)`. Rendered, never silent — see [`digest`].
    pub elided: (usize, usize, usize),
}

/// How many units of each bucket a digest renders. Bounded because the digest is the
/// **cold-start read**: an agent that has to page through 400 dead ends will skip it, and a
/// digest that gets skipped is worse than no digest (design Dmem.8, pitfall 4).
pub const DIGEST_BUCKET_CAP: usize = 12;

/// Build the state digest for the investigation at `ns_path` (primitive 6).
///
/// Each bucket is capped at [`DIGEST_BUCKET_CAP`], and every cap is **reported** in
/// [`Digest::elided`] and rendered. A silently truncated digest is worse than a long one:
/// the digest is what an agent reads *instead of* the full graph, so an unmarked cut reads
/// as "this is everything" — and on the tombstones bucket that is precisely the
/// re-treading failure the whole design exists to prevent. (Found by dogfooding this
/// change; see the `memory/jkb/digest-silent-cap` investigation.)
///
/// # Errors
/// Returns an error if the namespace is untyped or a query fails.
pub fn digest(conn: &Connection, ns_path: &str) -> Result<Digest> {
    let strategy = require_strategy(conn, ns_path)?;
    // Read each bucket in full, then cap — so the elided count is exact rather than a
    // "there might be more" guess.
    let mut live = frontier(conn, ns_path, true, None)?;
    let mut confirmed = confirmed_core(conn, ns_path)?;
    let mut tombs = tombstones(conn, ns_path)?;
    let elided = (
        live.len().saturating_sub(DIGEST_BUCKET_CAP),
        confirmed.len().saturating_sub(DIGEST_BUCKET_CAP),
        tombs.len().saturating_sub(DIGEST_BUCKET_CAP),
    );
    live.truncate(DIGEST_BUCKET_CAP);
    confirmed.truncate(DIGEST_BUCKET_CAP);
    tombs.truncate(DIGEST_BUCKET_CAP);
    Ok(Digest {
        ns_path: ns_path.to_owned(),
        type_name: strategy.name(),
        done: strategy.goal_predicate(conn, ns_path)?,
        frontier: live,
        confirmed,
        tombstones: tombs,
        elided,
    })
}

impl Digest {
    /// Render the digest as the markdown body of a `reflection` unit — the default
    /// cold-start read. Ordered frontier-first (what to do), then the settled model, then
    /// the graveyard (what not to redo).
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "# Investigation state — {}\n\nType: `{}`\nAcceptance: {} — {}\n",
            self.ns_path,
            self.type_name,
            if self.done.done { "MET" } else { "not met" },
            self.done.summary,
        );

        out.push_str("\n## Frontier (ranked — work here)\n");
        if self.frontier.is_empty() {
            out.push_str(
                "\n(empty — nothing live and unblocked. If the acceptance predicate is not \
                 met, this is the signal to open a new approach, not to stop.)\n",
            );
        } else {
            for unit in &self.frontier {
                let evidence = if unit.evidence.abs() < f64::EPSILON {
                    String::new()
                } else {
                    format!(" evidence {:+.2}", unit.evidence)
                };
                let _ = write!(
                    out,
                    "\n- `{}` [{}] rank {:.2}{evidence} — {}",
                    unit.uid,
                    unit.kind,
                    unit.rank,
                    one_line(unit.content.as_deref()),
                );
            }
            out.push_str(&elided_note(self.elided.0, "jkb inv frontier"));
            out.push('\n');
        }

        out.push_str("\n## Confirmed core (settled — build on this)\n");
        if self.confirmed.is_empty() {
            out.push_str("\n(nothing confirmed yet)\n");
        } else {
            for unit in &self.confirmed {
                let _ = write!(
                    out,
                    "\n- `{}` [{}] — {}",
                    unit.uid,
                    unit.kind,
                    one_line(unit.content.as_deref()),
                );
            }
            out.push_str(&elided_note(self.elided.1, "jkb inv core"));
            out.push('\n');
        }

        out.push_str("\n## Tombstones (do NOT re-tread)\n");
        if self.tombstones.is_empty() {
            out.push_str("\n(no dead ends recorded yet)\n");
        } else {
            for tomb in &self.tombstones {
                let _ = write!(
                    out,
                    "\n- `{}` [{}] {} — {}",
                    tomb.unit.uid,
                    tomb.unit.kind,
                    tomb.unit.resolution.as_deref().unwrap_or("unresolved"),
                    one_line(tomb.unit.content.as_deref()),
                );
                for (edge_type, uid, content) in &tomb.killed_by {
                    let _ = write!(
                        out,
                        "\n  - {} by `{}`: {}",
                        edge_type.as_str(),
                        uid,
                        one_line(content.as_deref()),
                    );
                }
                if tomb.killed_by.is_empty() {
                    out.push_str(
                        "\n  - (no edge records WHY — link the unit that killed it so this \
                         teaches instead of just blocking)",
                    );
                }
            }
            out.push_str(&elided_note(self.elided.2, "jkb inv tombstones"));
            out.push('\n');
        }
        out
    }
}

/// The line a capped bucket ends with. A digest is read *instead of* the full graph, so a
/// cut that says nothing reads as "this is everything" — and on the tombstones bucket that
/// is exactly how an agent re-treads a dead end somebody already paid for. Every cap names
/// the uncapped read that shows the rest.
fn elided_note(elided: usize, full_read: &str) -> String {
    if elided == 0 {
        return String::new();
    }
    format!("\n- … {elided} more not shown here — run `{full_read}` for the full bucket.")
}

/// The `uid` of the digest reflection unit for an investigation — stable, so re-running
/// `jkb inv digest` rewrites the same unit instead of piling up snapshots.
#[must_use]
pub fn digest_uid(ns_path: &str) -> String {
    format!("{KIND_REFLECTION}:digest:{ns_path}")
}

/// (Re)write the state-digest `reflection` unit for the investigation at `ns_path` and
/// return `(unit id, rendered body)`. Idempotent: one digest unit per investigation, updated
/// in place, so the cold-start read is always the current one.
///
/// # Errors
/// Returns an error if the namespace is untyped or a write fails.
pub fn write_digest(
    conn: &Connection,
    meta: &WriteMeta,
    ns_path: &str,
) -> Result<(ItemId, String)> {
    let body = digest(conn, ns_path)?.render();
    let uid = digest_uid(ns_path);
    let id = if let Some(id) = item::id_for_uid(conn, &uid)? {
        item::set_content(conn, meta, id, &body, None)?;
        id
    } else {
        let id = item::upsert(
            conn,
            meta,
            &item::NewItem {
                uid,
                kind: KIND_REFLECTION.to_owned(),
                content: Some(body.clone()),
                content_hash: None,
                mime: Some("text/markdown".to_owned()),
            },
        )?;
        let home = ns::ensure(conn, ns_path)?;
        placement::place(conn, meta, id, home, PlacementRole::Primary, 0)?;
        id
    };
    Ok((id, body))
}

/// Link two existing units — the escape hatch for edges no verb covers, including the
/// anti-progress edge (`equivalent_in_strength_to`) which is a *judgement about* two
/// existing statements rather than a new unit.
///
/// # Errors
/// Returns a validation error if either uid is unknown or the edge would create a
/// `depends_on` cycle; otherwise a database error.
pub fn link(
    conn: &Connection,
    meta: &WriteMeta,
    src_uid: &str,
    edge_type: EdgeType,
    dst_uid: &str,
    weight: Option<f64>,
) -> Result<()> {
    let src = resolve_uid(conn, src_uid)?;
    let dst = resolve_uid(conn, dst_uid)?;
    edge::link_weighted(conn, meta, src, dst, edge_type, weight, None)?;
    Ok(())
}

/// The item kind whose lifecycle lives on the **task** axis, not the outcome axis.
const KIND_TASK: &str = "task";

/// The strategy governing `ns_path`, required to be `expected`.
///
/// Strategy-specific verbs must not run against another strategy's investigation: the gate
/// they enforce is meaningless there (`reopen`'s accepted kinds do not exist in `debugging`),
/// and a silent no-op reads as a successful state change.
///
/// # Errors
/// Returns a validation error if the namespace is untyped or is governed by a different
/// strategy; otherwise a database error.
pub fn require_strategy_named(
    conn: &Connection,
    ns_path: &str,
    expected: &str,
) -> Result<&'static dyn NamespaceType> {
    let strategy = require_strategy(conn, ns_path)?;
    if strategy.name() != expected {
        return Err(Error::Types(TypeError::Validation(format!(
            "`{ns_path}` is a `{}` investigation; this command belongs to `{expected}`",
            strategy.name()
        ))));
    }
    Ok(strategy)
}

/// What [`reopen`] did — reported separately from "it worked" so a no-op cannot read as a
/// state change.
#[derive(Debug, Clone)]
pub struct Reopened {
    /// The kind of evidence that satisfied the gate.
    pub mechanism_kind: String,
    /// The uids of the gaps this unblocked. **Empty means nothing was blocking the route** —
    /// the mechanism was still recorded, but no reopen happened.
    pub superseded_gaps: Vec<String>,
}

/// Reopen a blocked `conjecture-attack` route on the strength of a materially new mechanism
/// (design Dmem.6B).
///
/// The gate runs first ([`nstype::conjecture::reopen_gate`]), so nothing is written unless
/// the evidence qualifies. On success the mechanism `informs` the route and every open `gap`
/// the route was blocked on is superseded — the route is unblocked *by* the new idea, on the
/// record.
///
/// # Errors
/// Returns a validation error if the route's investigation is not a `conjecture-attack`, if
/// either uid is unknown, or if the evidence is not a reopening kind; otherwise a database
/// error.
pub fn reopen(
    conn: &Connection,
    meta: &WriteMeta,
    route_uid: &str,
    mechanism_uid: &str,
) -> Result<Reopened> {
    let route = resolve_uid(conn, route_uid)?;
    let mechanism = resolve_uid(conn, mechanism_uid)?;

    // The route's own namespace decides which strategy governs this, so the command cannot be
    // pointed at a `debugging` investigation whose kinds the gate knows nothing about.
    let ns_path = primary_namespace(conn, route)?.ok_or_else(|| {
        Error::Types(TypeError::Validation(format!(
            "`{route_uid}` is not placed in any namespace"
        )))
    })?;
    require_strategy_named(conn, &ns_path, nstype::conjecture::NAME)?;

    let mechanism_kind = nstype::conjecture::reopen_gate(conn, route, mechanism)?;
    edge::link(conn, meta, mechanism, route, EdgeType::Informs, None)?;

    let mut superseded_gaps = Vec::new();
    for dep in edge::edges_from(conn, route, EdgeType::DependsOn)? {
        let Some(blocker) = item::get(conn, dep)? else {
            continue;
        };
        if blocker.kind != nstype::conjecture::KIND_GAP {
            continue;
        }
        if item::get_resolution(conn, dep)?.is_some_and(Resolution::is_settled) {
            continue;
        }
        edge::link(conn, meta, mechanism, dep, EdgeType::Supersedes, None)?;
        item::set_resolution(conn, meta, dep, Resolution::Superseded)?;
        superseded_gaps.push(blocker.uid);
    }
    Ok(Reopened {
        mechanism_kind,
        superseded_gaps,
    })
}

/// The path of `item`'s primary namespace (falling back to any placement), if it has one.
fn primary_namespace(conn: &Connection, item: ItemId) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached(
            "SELECT n.path FROM placements p JOIN namespaces n ON n.id = p.namespace_id
             WHERE p.item_id = ?1
             ORDER BY (p.role = 'primary') DESC, p.position LIMIT 1",
        )?
        .query_row([item.get()], |r| r.get(0))
        .optional()?)
}

/// Set an investigation unit's [`Resolution`] by uid, refusing to do it to a **task**.
///
/// A task's lifecycle is its `status`; `resolution` is the orthogonal axis for investigation
/// units, and the two are only equivalent because a task's `resolution` is always NULL.
/// Writing one breaks that: `is:frontier` would drop a task that `is:ready` still returns,
/// and a dependent would unblock under one and stay blocked under the other. So the guard
/// lives here rather than at the CLI edge — every caller gets it.
///
/// # Errors
/// Returns [`jkb_types::Error::NotFound`] if `uid` names nothing, a validation error if it
/// names a task or an unknown resolution; otherwise a database error.
pub fn resolve_unit(
    conn: &Connection,
    meta: &WriteMeta,
    uid: &str,
    resolution: &str,
) -> Result<ItemId> {
    let id = resolve_uid(conn, uid)?;
    let kind = item::get(conn, id)?.map(|m| m.kind).unwrap_or_default();
    if kind == KIND_TASK {
        return Err(Error::Types(TypeError::Validation(format!(
            "`{uid}` is a task: its lifecycle is `status`, not `resolution` — use \
             `jkb task set {uid} --status …`. (Writing a task's resolution would split \
             `is:frontier` from `is:ready`.)"
        ))));
    }
    item::set_resolution_str(conn, meta, id, resolution)?;
    Ok(id)
}

/// Set a unit's `promise=` rank (the frontier ordering knob).
///
/// # Errors
/// Returns an error if the uid is unknown or a write fails.
pub fn set_promise(conn: &Connection, meta: &WriteMeta, uid: &str, promise: f64) -> Result<()> {
    if !promise.is_finite() {
        return Err(TypeError::Validation(format!("promise must be finite, got {promise}")).into());
    }
    let id = resolve_uid(conn, uid)?;
    // One promise per unit: replace rather than accumulate values on the facet.
    for (facet, value) in tag::applications(conn, id)? {
        if facet == FACET_PROMISE {
            tag::remove(conn, meta, id, &facet, &value)?;
        }
    }
    tag::apply(conn, meta, id, FACET_PROMISE, &format!("{promise}"))
}

/// One row of [`list`]: an investigation namespace, its strategy, and how many units it
/// holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvestigationRow {
    /// The investigation's namespace path.
    pub ns_path: String,
    /// The strategy governing it, or `"unknown"` if this build has no such strategy.
    pub type_name: &'static str,
    /// How many units it holds, across its whole subtree. Shown because an investigation
    /// can legitimately be **empty** — the namespace and its type survive an `undo` that
    /// removed the units — and a listing that hid that would advertise state that isn't
    /// there. (Found by dogfooding; see `memory/jkb/digest-silent-cap`.)
    pub units: usize,
}

/// Every investigation: the typed namespaces under `memory/`, with their strategy and unit
/// count. The "what was I working on?" read.
///
/// # Errors
/// Returns an error if a query fails.
pub fn list(conn: &Connection) -> Result<Vec<InvestigationRow>> {
    let mut out = Vec::new();
    // Propagated, not swallowed: a failure here must not render as "(no investigations yet)"
    // with a success exit code, which is indistinguishable from a genuinely empty KB.
    // `ns::subtree` already returns an empty vec when `memory/` simply does not exist.
    for (id, path) in ns::subtree(conn, MEMORY_ROOT)? {
        let Some(name) = ns::get_type_by_id(conn, id)? else {
            continue;
        };
        // An investigation typed by a build that had a strategy this one lacks is listed
        // rather than hidden, so it is visible instead of mysteriously absent.
        let type_name = nstype::resolve(&name).map_or("unknown", NamespaceType::name);
        let units = Query {
            scope: Scope::Subtree(path.clone()),
            ..Query::default()
        }
        .evaluate(conn)?
        .len();
        out.push(InvestigationRow {
            ns_path: path,
            type_name,
            units,
        });
    }
    out.sort_by(|a, b| a.ns_path.cmp(&b.ns_path));
    Ok(out)
}

// ---- internals ------------------------------------------------------------

/// The strategy governing `ns_path`, or an actionable error naming how to create one.
fn require_strategy(conn: &Connection, ns_path: &str) -> Result<&'static dyn NamespaceType> {
    match nstype::for_namespace(conn, ns_path)? {
        Some((_, strategy)) => Ok(strategy),
        None => Err(TypeError::Validation(format!(
            "`{ns_path}` is not an investigation namespace; create one with \
             `jkb inv new <type> <path>` (types: {})",
            nstype::AVAILABLE.join(", ")
        ))
        .into()),
    }
}

/// Load display rows for `ids`, computing the rank with `strategy` when one is given.
fn rows_for(
    conn: &Connection,
    ids: &[ItemId],
    strategy: Option<&'static dyn NamespaceType>,
) -> Result<Vec<UnitRow>> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(row) = item::get(conn, *id)? else {
            continue;
        };
        let namespace: Option<String> = conn
            .prepare_cached(
                "SELECT n.path FROM placements p JOIN namespaces n ON n.id = p.namespace_id
                 WHERE p.item_id = ?1
                 ORDER BY (p.role = 'primary') DESC, p.position LIMIT 1",
            )?
            .query_row([id.get()], |r| r.get(0))
            .optional()?;
        let rank = match strategy {
            Some(strategy) => strategy.ranking(conn, *id)?,
            None => 0.0,
        };
        out.push(UnitRow {
            id: row.id,
            uid: row.uid,
            kind: row.kind,
            content: row.content,
            resolution: row.resolution,
            rank,
            evidence: edge::evidence_for(conn, *id)?,
            namespace,
        });
    }
    Ok(out)
}

/// Resolve a unit uid to its id, or a `NotFound` naming the uid.
fn resolve_uid(conn: &Connection, uid: &str) -> Result<ItemId> {
    item::id_for_uid(conn, uid)?
        .ok_or_else(|| Error::Types(TypeError::NotFound(format!("unit `{uid}`"))))
}

/// The uid of an item id (which must exist — it was just written or read).
fn uid_of(conn: &Connection, id: ItemId) -> Result<String> {
    conn.prepare_cached("SELECT uid FROM items WHERE id = ?1")?
        .query_row([id.get()], |r| r.get(0))
        .map_err(Into::into)
}

/// An actionable "that kind is not part of this strategy" error.
fn reject_kind(strategy: &'static dyn NamespaceType, kind: &str) -> Error {
    Error::Types(TypeError::Validation(format!(
        "`{kind}` is not a unit kind of the `{}` strategy; kinds: {}",
        strategy.name(),
        strategy.accepted_kinds().join(", ")
    )))
}

/// An actionable "that verb is not part of this strategy" error.
fn unknown_verb(strategy: &'static dyn NamespaceType, verb: &str) -> Error {
    let verbs: Vec<&str> = strategy.verbs().iter().map(|v| v.verb).collect();
    Error::Types(TypeError::Validation(format!(
        "`{verb}` is not a verb of the `{}` strategy; verbs: {}",
        strategy.name(),
        verbs.join(", ")
    )))
}

/// The first non-empty line of a body, trimmed for a one-line digest entry.
fn one_line(content: Option<&str>) -> String {
    let line = content
        .unwrap_or("")
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut out: String = line.chars().take(100).collect();
    if line.chars().count() > 100 {
        out.push('…');
    }
    out
}

/// The `goal`/`conjecture` unit(s) of the investigation at `ns_path` — the root intent and
/// its acceptance predicate, which is what an agent should read first.
///
/// # Errors
/// Returns an error if a query fails.
pub fn goals(conn: &Connection, ns_path: &str) -> Result<Vec<UnitRow>> {
    let mut kinds = vec![KIND_GOAL.to_owned()];
    if let Some((_, strategy)) = nstype::for_namespace(conn, ns_path)? {
        kinds.extend(
            strategy
                .node_kinds()
                .iter()
                .filter(|k| k.base == nstype::BaseKind::Goal)
                .map(|k| k.kind.to_owned()),
        );
    }
    kinds.sort_unstable();
    kinds.dedup();
    let ids = Query {
        kinds,
        scope: Scope::Subtree(ns_path.to_owned()),
        ..Query::default()
    }
    .evaluate(conn)?;
    rows_for(conn, &ids, None)
}
