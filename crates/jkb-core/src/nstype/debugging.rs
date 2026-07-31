//! The `debugging` strategy: find the cause of a hard bug in a large codebase
//! (design Dmem.6A).
//!
//! This is the v1 anchor because it is the *hardest* case for the substrate, not the
//! easiest. Two properties make it so:
//!
//! - **The system under investigation is mutable.** An observation is only true of the code
//!   that produced it, so observations carry a `commit-range=` and go `staleness=stale`
//!   when the code moves underneath them. A stale observation must not rank on the
//!   frontier or be cited as evidence — but it also must not be deleted, because "we saw
//!   this before the refactor" is real memory. (Contrast a cryptanalysis corpus, where an
//!   observation is true forever.)
//! - **There is an extra terminal leg.** Diagnosing is not finishing: the investigation
//!   ends at *fix + verify*, so a confirmed root cause leaves the symptom unresolved until
//!   a fix for it has been verified against the repro.
//!
//! Localization runs on two axes — **where** (`suspect-area`, narrowed by `rules_out`) and
//! **when** (`regression-window`, narrowed by `narrows`) — and the protocol is claim +
//! narrow with a small number of agents, not a wide fan-out.

use rusqlite::{Connection, OptionalExtension};

use jkb_types::{EdgeType, ItemId, Resolution};

use crate::nstype::{
    base_frontier, default_rollup, promise, BaseKind, DoneState, NamespaceType, NodeKindSpec,
    VerbSpec,
};
use crate::query::{Query, Scope, TagPred};
use crate::{edge, query::CmpOp, Result};

/// The strategy name stored in `namespaces.metadata.type`.
pub const NAME: &str = "debugging";

/// The tag facet marking an observation's validity against the moving codebase.
pub const FACET_STALENESS: &str = "staleness";
/// The `staleness=` value that takes an observation out of the live picture.
pub const STALE: &str = "stale";
/// The tag facet recording which commit range an observation was made against.
pub const FACET_COMMIT_RANGE: &str = "commit-range";
/// The tag facet recording how dependable a repro is: `flaky`/`deterministic`/`minimized`.
pub const FACET_RELIABILITY: &str = "reliability";

/// The `symptom` kind — the reported misbehaviour the investigation must explain.
pub const KIND_SYMPTOM: &str = "symptom";
/// The `repro` kind — a procedure that makes the symptom happen on demand.
pub const KIND_REPRO: &str = "repro";
/// The `hypothesis` kind — the pivot noun: a candidate explanation.
pub const KIND_HYPOTHESIS: &str = "hypothesis";
/// The `experiment` kind — a fabricated test (instrument / differential / bisection).
pub const KIND_EXPERIMENT: &str = "experiment";
/// The `observation` kind — what an experiment showed. Expires as the code moves.
pub const KIND_OBSERVATION: &str = "observation";
/// The `suspect-area` kind — the *where* axis: a component that might hold the cause.
pub const KIND_SUSPECT_AREA: &str = "suspect-area";
/// The `regression-window` kind — the *when* axis: the commit range that introduced it.
pub const KIND_REGRESSION_WINDOW: &str = "regression-window";
/// The `root-cause` kind — the confirmed causal location.
pub const KIND_ROOT_CAUSE: &str = "root-cause";
/// The `fix` kind — the change that removes the cause.
pub const KIND_FIX: &str = "fix";
/// The `invariant` kind — a property that must hold, learned along the way.
pub const KIND_INVARIANT: &str = "invariant";

/// The `debugging` strategy descriptor.
pub struct Debugging;

const KINDS: &[NodeKindSpec] = &[
    NodeKindSpec {
        kind: KIND_SYMPTOM,
        base: BaseKind::Goal,
        about: "the reported misbehaviour to explain (the investigation's goal)",
    },
    NodeKindSpec {
        kind: KIND_REPRO,
        base: BaseKind::Artifact,
        about:
            "a procedure that reproduces the symptom; tag reliability=flaky|deterministic|minimized",
    },
    NodeKindSpec {
        kind: KIND_HYPOTHESIS,
        base: BaseKind::Node,
        about: "a candidate explanation — the pivot noun everything hangs off",
    },
    NodeKindSpec {
        kind: KIND_EXPERIMENT,
        base: BaseKind::Node,
        about: "a fabricated test of a hypothesis (instrument / differential / bisection)",
    },
    NodeKindSpec {
        kind: KIND_OBSERVATION,
        base: BaseKind::Node,
        about: "what an experiment showed; tag commit-range= and staleness= (it expires)",
    },
    NodeKindSpec {
        kind: KIND_SUSPECT_AREA,
        base: BaseKind::Node,
        about: "the WHERE axis: a component that may hold the cause",
    },
    NodeKindSpec {
        kind: KIND_REGRESSION_WINDOW,
        base: BaseKind::Node,
        about: "the WHEN axis: the commit range that introduced the bug",
    },
    NodeKindSpec {
        kind: KIND_ROOT_CAUSE,
        base: BaseKind::Node,
        about: "the confirmed causal location — a diagnosis, not yet a resolution",
    },
    NodeKindSpec {
        kind: KIND_FIX,
        base: BaseKind::Artifact,
        about: "the change that removes the cause; must be verified against the repro",
    },
    NodeKindSpec {
        kind: KIND_INVARIANT,
        base: BaseKind::Node,
        about: "a property that must hold, learned along the way",
    },
];

const EDGES: &[EdgeType] = &[
    EdgeType::DependsOn,
    EdgeType::DerivedFrom,
    EdgeType::References,
    EdgeType::ParentOf,
    EdgeType::Tests,
    EdgeType::Supports,
    EdgeType::Contradicts,
    EdgeType::Refutes,
    EdgeType::Narrows,
    EdgeType::RulesOut,
    EdgeType::Confirms,
    EdgeType::Answers,
    EdgeType::Fixes,
    EdgeType::Verifies,
    EdgeType::Supersedes,
    EdgeType::DiscoveredFrom,
];

const VERBS: &[VerbSpec] = &[
    VerbSpec::new(
        "symptom",
        KIND_SYMPTOM,
        "record the misbehaviour to explain",
    ),
    VerbSpec::on(
        "repro",
        KIND_REPRO,
        EdgeType::DerivedFrom,
        "record a reproduction of a symptom",
    ),
    VerbSpec::on(
        "hypothesize",
        KIND_HYPOTHESIS,
        EdgeType::DerivedFrom,
        "propose an explanation for a symptom or observation",
    )
    .optional_target()
    .tagged(&[("confidence", "unverified")]),
    VerbSpec::on(
        "experiment",
        KIND_EXPERIMENT,
        EdgeType::Tests,
        "design a test of a hypothesis",
    ),
    VerbSpec::on(
        "observe",
        KIND_OBSERVATION,
        EdgeType::DerivedFrom,
        "record what an experiment showed",
    )
    .optional_target(),
    VerbSpec::on(
        "support",
        KIND_OBSERVATION,
        EdgeType::Supports,
        "record evidence FOR a hypothesis (weighted with --weight)",
    ),
    VerbSpec::on(
        "contradict",
        KIND_OBSERVATION,
        EdgeType::Contradicts,
        "record evidence AGAINST a hypothesis (weighted with --weight)",
    ),
    VerbSpec::on(
        "refute",
        KIND_OBSERVATION,
        EdgeType::Refutes,
        "kill a hypothesis outright (it is retained as a tombstone)",
    )
    .resolving(Resolution::DeadEnd),
    VerbSpec::new(
        "suspect",
        KIND_SUSPECT_AREA,
        "name a component that may hold the cause (the WHERE axis)",
    ),
    VerbSpec::on(
        "rule-out",
        KIND_EXPERIMENT,
        EdgeType::RulesOut,
        "eliminate a suspect area — records what ruled it out (the pruning edge)",
    )
    .resolving(Resolution::DeadEnd),
    VerbSpec::on(
        "narrow",
        KIND_REGRESSION_WINDOW,
        EdgeType::Narrows,
        "narrow the regression window by bisection (the WHEN axis)",
    )
    .optional_target(),
    VerbSpec::on(
        "root-cause",
        KIND_ROOT_CAUSE,
        EdgeType::Answers,
        "state the causal location that explains a symptom",
    ),
    VerbSpec::on(
        "confirm",
        KIND_EXPERIMENT,
        EdgeType::Confirms,
        "promote a hypothesis or root cause to confirmed with a decisive experiment",
    )
    .resolving(Resolution::Success),
    VerbSpec::on(
        "fix",
        KIND_FIX,
        EdgeType::Fixes,
        "record the change that removes a root cause",
    ),
    VerbSpec::on(
        "verify",
        KIND_OBSERVATION,
        EdgeType::Verifies,
        "verify a fix against the repro — the terminal leg that ends the investigation",
    )
    .resolving(Resolution::Success),
    VerbSpec::new(
        "invariant",
        KIND_INVARIANT,
        "record a property that must hold",
    ),
    VerbSpec::on(
        "note",
        crate::nstype::KIND_NODE,
        EdgeType::References,
        "free-text note loosely attached to a unit (the escape hatch)",
    )
    .optional_target(),
];

impl NamespaceType for Debugging {
    fn name(&self) -> &'static str {
        NAME
    }

    fn about(&self) -> &'static str {
        "localize and fix a hard bug: hypothesis -> experiment -> observation -> \
         root cause -> fix -> verify, over a mutable codebase"
    }

    fn node_kinds(&self) -> &'static [NodeKindSpec] {
        KINDS
    }

    fn edge_types(&self) -> &'static [EdgeType] {
        EDGES
    }

    fn verbs(&self) -> &'static [VerbSpec] {
        VERBS
    }

    /// The generalized frontier, minus **stale observations**. The system under
    /// investigation moves, so an observation taken against older code is no longer part
    /// of the live picture: it must not rank as work, and it must not be mistaken for
    /// current evidence. It is excluded, never deleted — "we saw this before the refactor"
    /// stays queryable via `resolution:`/`jkb related`.
    fn frontier(&self, scope: Scope) -> Query {
        // Built on `base_frontier` so this override inherits every base exclusion (e.g.
        // `reflection` units are not work) instead of re-deriving the query and drifting.
        let mut query = base_frontier(scope);
        query.exclude_tags.push(TagPred {
            facet: FACET_STALENESS.to_owned(),
            op: CmpOp::Eq,
            value: STALE.to_owned(),
        });
        query
    }

    /// The base rollup, plus the two debugging-specific corrections:
    ///
    /// 1. A **symptom**'s outcome is defined by *fix + verify*, not by explanation. Being
    ///    answered by a confirmed root cause is a diagnosis; the symptom resolves only once
    ///    a fix for it has been verified against the repro, and it resolves then even though
    ///    nothing `confirms` the symptom itself. Without this the frontier would go quiet
    ///    the moment somebody wrote down an answer.
    /// 2. A **stale observation** cannot confirm anything, so it does not roll a unit up to
    ///    success. Its `supports`/`confirms` edges stay on record; they just stop counting.
    fn resolution_rollup(&self, conn: &Connection, node: ItemId) -> Result<Resolution> {
        let base = default_rollup(conn, node)?;
        // Deaths and supersessions stand as-is: an obstruction does not go stale, and a
        // refuted symptom ("not actually a bug") stays refuted.
        if base.is_settled() && base != Resolution::Success {
            return Ok(base);
        }
        if kind_of(conn, node)?.as_deref() == Some(KIND_SYMPTOM) {
            return Ok(if has_verified_fix(conn, node)? {
                Resolution::Success
            } else {
                Resolution::Unresolved
            });
        }
        if base == Resolution::Success && is_stale(conn, node)? {
            return Ok(Resolution::Unresolved);
        }
        Ok(base)
    }

    /// A stale observation contributes no rank: it is not evidence about today's code.
    fn ranking(&self, conn: &Connection, node: ItemId) -> Result<f64> {
        if is_stale(conn, node)? {
            return Ok(f64::MIN);
        }
        Ok(promise(conn, node)? + edge::evidence_for(conn, node)?)
    }

    /// Done when a **root cause is confirmed** *and* a **fix for it has been verified**
    /// against the repro (design Dmem.6A). Diagnosing is not finishing: a confirmed root
    /// cause with no verified fix reports what is still missing, which keeps the frontier
    /// live.
    fn goal_predicate(&self, conn: &Connection, ns_path: &str) -> Result<DoneState> {
        let scope = Scope::Subtree(ns_path.to_owned());
        let confirmed_causes = Query {
            kind: Some(KIND_ROOT_CAUSE.to_owned()),
            resolution: Some(Resolution::Success.as_str().to_owned()),
            scope: scope.clone(),
            ..Query::default()
        }
        .evaluate(conn)?;
        if confirmed_causes.is_empty() {
            let open_causes = Query {
                kind: Some(KIND_ROOT_CAUSE.to_owned()),
                scope,
                ..Query::default()
            }
            .evaluate(conn)?;
            return Ok(DoneState::open(if open_causes.is_empty() {
                "no root cause proposed yet".to_owned()
            } else {
                format!(
                    "{} root cause(s) proposed, none confirmed by a decisive experiment",
                    open_causes.len()
                )
            }));
        }

        let verified: Vec<ItemId> = {
            let mut out = Vec::new();
            for cause in &confirmed_causes {
                for fix in edge::walk(conn, *cause, &[EdgeType::Fixes], 1, edge::Direction::In)? {
                    if is_verified(conn, fix.item)? {
                        out.push(fix.item);
                    }
                }
            }
            out
        };
        if verified.is_empty() {
            return Ok(DoneState::open(
                "root cause confirmed, but no fix has been verified against the repro yet"
                    .to_owned(),
            ));
        }
        Ok(DoneState::done(format!(
            "root cause confirmed and {} fix(es) verified against the repro",
            verified.len()
        )))
    }
}

/// Whether `node` carries `staleness=stale`.
fn is_stale(conn: &Connection, node: ItemId) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT 1 FROM tag_applications
             WHERE item_id = ?1 AND facet = ?2 AND value = ?3 LIMIT 1",
        )?
        .query_row((node.get(), FACET_STALENESS, STALE), |_| Ok(true))
        .optional()?
        .unwrap_or(false))
}

/// `node`'s item kind, or `None` if it no longer exists.
fn kind_of(conn: &Connection, node: ItemId) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached("SELECT kind FROM items WHERE id = ?1")?
        .query_row([node.get()], |r| r.get::<_, String>(0))
        .optional()?)
}

/// Whether something `verifies` `fix` with a non-stale observation.
fn is_verified(conn: &Connection, fix: ItemId) -> Result<bool> {
    for verifier in edge::walk(conn, fix, &[EdgeType::Verifies], 1, edge::Direction::In)? {
        if !is_stale(conn, verifier.item)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `symptom` has a verified fix: some `fix` that `fixes` either the symptom
/// directly or a `root-cause` that `answers` it, and that fix is itself verified.
fn has_verified_fix(conn: &Connection, symptom: ItemId) -> Result<bool> {
    let mut candidates = vec![symptom];
    for answer in edge::walk(conn, symptom, &[EdgeType::Answers], 1, edge::Direction::In)? {
        candidates.push(answer.item);
    }
    for candidate in candidates {
        for fix in edge::walk(conn, candidate, &[EdgeType::Fixes], 1, edge::Direction::In)? {
            if is_verified(conn, fix.item)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Mark every `observation` in the investigation at `ns_path` whose `commit-range=` tag is
/// **not** `current_window` as `staleness=stale` — the "the code moved underneath us" sweep
/// (design Dmem.6A). Observations with no recorded commit range are left alone: absence of
/// provenance is not evidence of staleness, and silently invalidating them would erase real
/// memory. Returns the uids that were newly marked.
///
/// Nothing is deleted: a stale observation is excluded from the frontier and from ranking,
/// but stays queryable and keeps its edges.
///
/// # Errors
/// Returns a validation error if `ns_path` is not a `debugging` investigation (staleness is
/// this strategy's concept — running it elsewhere would silently do nothing and report
/// success), or an error if a query or tag write fails.
pub fn mark_stale_observations(
    conn: &Connection,
    meta: &crate::WriteMeta,
    ns_path: &str,
    current_window: &str,
) -> Result<Vec<String>> {
    crate::investigation::require_strategy_named(conn, ns_path, NAME)?;
    let observations = Query {
        kind: Some(KIND_OBSERVATION.to_owned()),
        scope: Scope::Subtree(ns_path.to_owned()),
        ..Query::default()
    }
    .evaluate(conn)?;

    let mut marked = Vec::new();
    for node in observations {
        let window: Option<String> = conn
            .prepare_cached(
                "SELECT value FROM tag_applications
                 WHERE item_id = ?1 AND facet = ?2 LIMIT 1",
            )?
            .query_row((node.get(), FACET_COMMIT_RANGE), |r| r.get(0))
            .optional()?;
        let Some(window) = window else { continue };
        if window == current_window || is_stale(conn, node)? {
            continue;
        }
        crate::tag::apply(conn, meta, node, FACET_STALENESS, STALE)?;
        if let Some(uid) = conn
            .prepare_cached("SELECT uid FROM items WHERE id = ?1")?
            .query_row([node.get()], |r| r.get::<_, String>(0))
            .optional()?
        {
            marked.push(uid);
        }
    }
    Ok(marked)
}
