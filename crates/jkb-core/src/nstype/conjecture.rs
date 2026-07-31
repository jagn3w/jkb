//! The `conjecture-attack` strategy: resolve a hard conjecture by **proof or disproof**
//! (design Dmem.6B), grounded in the `OpenAI` Cycle-Double-Cover and Jacobian-Conjecture
//! prompts and the Dinitz–Garg–Goemans counterexample hunt.
//!
//! ## Why prove and disprove are ONE strategy
//! The frontier labs run them as a single coordination structure: the Jacobian prompt tells
//! its agents to "begin with a genuinely diverse portfolio of proof **and** counterexample
//! approaches" under one registry of approach families. The nodes, the edges, the family
//! registry, the graveyard, the adversarial audit — all identical. The *only* thing that
//! differs is the **acceptance predicate**: what counts as a complete resolution. So this is
//! one descriptor with two presets ([`ACCEPTANCE_PROVE`], [`ACCEPTANCE_DISPROVE`]), not two
//! strategies. (Contrast an evolutionary population, which really is a different protocol
//! and would earn its own descriptor — design Dmem.7.)
//!
//! ## What jkb contributes
//! The prompts hand-specify a coordination discipline that today's labs keep **in one
//! agent's context**: the approach-family registry, blocked-with-reason and gated reopen,
//! keeping incompatible routes alive, adversarial audit throughout, and a load-bearing
//! graveyard. jkb's contribution is *externalizing* that state so it is durable, queryable,
//! and survives past one context or run:
//!
//! - **approach-family registry** — `member_of` clustering plus [`family_pressure`], which
//!   answers "are too many routes converging on one idea?" as a query.
//! - **blocked-with-reason + gated reopen** — a route blocked at a theorem-strength missing
//!   lemma gets a `gap` unit it `depends_on`, so it leaves the frontier *with the reason
//!   attached*. [`reopen_gate`] then admits a reopen only on a materially new mechanism,
//!   invariant, construction, or obstruction — not on renewed enthusiasm.
//! - **anti-progress detection** — a route that reduces the conjecture to a lemma
//!   `equivalent_in_strength_to` it has made no progress however elegant the reduction.
//!   [`is_anti_progress`] makes that a derived flag rather than a judgement call.
//! - **the graveyard** — a refuted candidate and the `obstruction` that killed it are
//!   retained and linked, and a new candidate can be checked against the already
//!   `rules_out`-ed regimes before anyone spends time on it.
//! - **no partial results** — the acceptance test refuses to call an investigation done
//!   while any `gap` in a candidate's dependency closure is still open, which is the
//!   machine-checkable form of the prompts' "do not return a reduction, isolated missing
//!   lemma, or best-effort summary".
//!
//! *Honest limit:* jkb does not supply the prompts' 64 concurrent agents. This schema is the
//! north star; near-term it is driven at small fan-out. The value it adds today is
//! cross-run, larger-than-context durability, not parallelism.

use rusqlite::{Connection, OptionalExtension};

use jkb_types::{EdgeType, Error as TypeError, ItemId, Resolution};

use crate::nstype::{BaseKind, DoneState, NamespaceType, NodeKindSpec, VerbSpec, KIND_GOAL};
use crate::query::{Query, Scope};
use crate::{edge, item, Result};

/// The strategy name stored in `namespaces.metadata.type`.
pub const NAME: &str = "conjecture-attack";

/// The tag facet recording which acceptance preset the goal was seeded with
/// (`prove`/`disprove`/`either`).
pub const FACET_ACCEPTANCE: &str = "acceptance";
/// Acceptance preset: only an affirmative proof resolves this.
pub const ACCEPT_PROVE: &str = "prove";
/// Acceptance preset: only an explicit counterexample with a certificate resolves this.
pub const ACCEPT_DISPROVE: &str = "disprove";
/// Acceptance preset: either resolution counts (the labs' default posture — run proof and
/// counterexample portfolios side by side and take whichever completes).
pub const ACCEPT_EITHER: &str = "either";

/// Every acceptance preset this strategy offers, for `--help` and validation.
pub const ACCEPTANCE_PRESETS: &[&str] = &[ACCEPT_PROVE, ACCEPT_DISPROVE, ACCEPT_EITHER];

/// The tag facet for the verification ladder (from the DGG episode):
/// `unverified` -> `screened` -> `machine-checked` -> `peer-reviewed`. A screening verdict
/// must never masquerade as a validation, which is why the *method* is tracked separately.
pub const FACET_CONFIDENCE: &str = "confidence";
/// The tag facet recording *how* a claim was checked (`flow-formulation`, `exhaustive-n8`,
/// `lean`, …) so a cheap screen is distinguishable from a real proof.
pub const FACET_METHOD: &str = "method";

/// The `conjecture` kind — the statement to resolve, carrying the acceptance predicate.
pub const KIND_CONJECTURE: &str = "conjecture";
/// The `approach-family` kind — a mathematical *idea*, the clustering unit.
pub const KIND_APPROACH_FAMILY: &str = "approach-family";
/// The `approach` kind — a concrete route within a family.
pub const KIND_APPROACH: &str = "approach";
/// The `reduction` kind — "it suffices to show X".
pub const KIND_REDUCTION: &str = "reduction";
/// The `lemma` kind — a statement a route needs proved.
pub const KIND_LEMMA: &str = "lemma";
/// The `construction` kind — an explicit object built along the way.
pub const KIND_CONSTRUCTION: &str = "construction";
/// The `invariant` kind — a quantity or property preserved by the structure.
pub const KIND_INVARIANT: &str = "invariant";
/// The `candidate-proof` kind — a claimed affirmative resolution.
pub const KIND_CANDIDATE_PROOF: &str = "candidate-proof";
/// The `candidate-construction` kind — a claimed counterexample.
pub const KIND_CANDIDATE_CONSTRUCTION: &str = "candidate-construction";
/// The `parameter-regime` kind — a region of the problem space (dimension, degree, girth…).
pub const KIND_PARAMETER_REGIME: &str = "parameter-regime";
/// The `obstruction` kind — a reason a whole regime or mechanism cannot work. The pruner.
pub const KIND_OBSTRUCTION: &str = "obstruction";
/// The `gap` kind — a first-class residual: exactly what is still missing. The resume token.
pub const KIND_GAP: &str = "gap";
/// The `partial-result` kind — the strongest rigorously proved statement so far; the
/// progress metric that distinguishes advancing from stalled.
pub const KIND_PARTIAL_RESULT: &str = "partial-result";
/// The `audit` kind — an adversarial check with an enumerated checklist.
pub const KIND_AUDIT: &str = "audit";
/// The `tool` kind — a harness built once and reused across rounds.
pub const KIND_TOOL: &str = "tool";
/// The `mechanism` kind — a materially new idea; the currency that reopens a blocked route.
pub const KIND_MECHANISM: &str = "mechanism";

/// The kinds that can *reopen* a blocked route. Straight from the prompts: "continue only
/// if someone proposes a materially new mechanism, invariant, construction, or obstruction."
/// Enthusiasm, a restatement, or a status report is not on this list.
pub const REOPENING_KINDS: &[&str] = &[
    KIND_MECHANISM,
    KIND_INVARIANT,
    KIND_CONSTRUCTION,
    KIND_OBSTRUCTION,
];

/// The kinds that can *resolve* the conjecture — one per direction.
pub const CANDIDATE_KINDS: &[&str] = &[KIND_CANDIDATE_PROOF, KIND_CANDIDATE_CONSTRUCTION];

/// The **prove** acceptance predicate: the enumerated "insufficient" list, transcribed from
/// the Jacobian / CDC prompts. Seeded into the `goal` node's body so every agent that picks
/// the investigation up reads the same bar.
pub const ACCEPTANCE_PROVE: &str = "\
Acceptance predicate (prove). A complete solution must prove the statement in full
generality, with no additional assumptions. Partial progress does not count unless it
implies exactly that resolution. The following are INSUFFICIENT:
  - proofs only in low dimension (e.g. dim 1 or 2) or for special classes
  - bounded-degree, homogeneous, or cubic reductions without completing the reduced case
  - formal power-series or local-analytic inverses; local rather than global arguments
  - hidden injectivity, surjectivity, birationality, or characteristic-zero assumptions
  - reduction to another unproved conjecture, or to a lemma equivalent in strength
  - computational verification through any fixed dimension, degree, or size
Do not return a reduction, an isolated missing lemma, a best-effort summary, or an
explanation of why the problem is difficult. Return only a complete proof that survives
adversarial audit.";

/// The **disprove** acceptance predicate: the counterexample bar (Jacobian prompt + the DGG
/// episode). Note the verification asymmetry — a hit is cheap to check and hard to find,
/// which inverts proof search: hence "no candidate without a certificate".
pub const ACCEPTANCE_DISPROVE: &str = "\
Acceptance predicate (disprove). A complete disproof must exhibit an explicit object —
explicit parameters, explicit coordinates/structure — together with an exact computation of
the relevant invariant and a complete proof that it violates the statement. The following
are INSUFFICIENT:
  - a candidate without a complete impossibility/noninvertibility proof
  - numerical or floating-point evidence; screening verdicts presented as validation
  - a counterexample to a proposed sublemma rather than to the statement itself
  - constructions trivialized by a coordinate change, or importing characteristic-p
    phenomena into characteristic 0
Every candidate must be checked for a hidden inverse, an exact invariant computation, and
coordinate changes that trivialize it. Partial progress does not count unless it implies
exactly this resolution. Return only a complete explicit counterexample that survives
adversarial audit.";

/// The enumerated adversarial-audit checklist (design Dmem.6B: audit is a first-class
/// unit, not a vibe). Seeded into every `audit` node's body so the check is *recorded* as
/// performed against a fixed list rather than asserted.
pub const AUDIT_CHECKLIST: &str = "\
Adversarial audit checklist (mark each: pass / fail / n-a, with a reason):
  [ ] no confusion between formal and exact/polynomial objects
  [ ] no local-to-global leap (local or analytic invertibility != global algebraic)
  [ ] no hidden injectivity / surjectivity / birationality assumption
  [ ] no unjustified convergence or degree-bound claim; denominators accounted for
  [ ] every reduction is reversible, or its irreversibility is handled
  [ ] no circular use of a statement equivalent in strength to the conjecture
  [ ] exact invariant computation (not numerical); no floating-point artifacts
  [ ] special cases claimed 'routine' are actually proved
  [ ] the claimed object is not trivialized by a coordinate change
  [ ] every cited sublemma is proved here or is a standard named theorem";

/// The `conjecture-attack` strategy descriptor.
pub struct ConjectureAttack;

const KINDS: &[NodeKindSpec] = &[
    NodeKindSpec {
        kind: KIND_CONJECTURE,
        base: BaseKind::Goal,
        about: "the statement to resolve, carrying the acceptance predicate",
    },
    NodeKindSpec {
        kind: KIND_APPROACH_FAMILY,
        base: BaseKind::Node,
        about: "a mathematical IDEA — group routes by idea, not by wording",
    },
    NodeKindSpec {
        kind: KIND_APPROACH,
        base: BaseKind::Node,
        about: "a concrete route within a family",
    },
    NodeKindSpec {
        kind: KIND_REDUCTION,
        base: BaseKind::Node,
        about: "'it suffices to show X' — check it is not equivalent in strength",
    },
    NodeKindSpec {
        kind: KIND_LEMMA,
        base: BaseKind::Node,
        about: "a statement a route needs proved",
    },
    NodeKindSpec {
        kind: KIND_CONSTRUCTION,
        base: BaseKind::Artifact,
        about: "an explicit object built along the way",
    },
    NodeKindSpec {
        kind: KIND_INVARIANT,
        base: BaseKind::Node,
        about: "a preserved quantity or property (also reopens blocked routes)",
    },
    NodeKindSpec {
        kind: KIND_CANDIDATE_PROOF,
        base: BaseKind::Artifact,
        about: "a claimed affirmative resolution — must survive audit",
    },
    NodeKindSpec {
        kind: KIND_CANDIDATE_CONSTRUCTION,
        base: BaseKind::Artifact,
        about: "a claimed counterexample — needs an exact certificate",
    },
    NodeKindSpec {
        kind: KIND_PARAMETER_REGIME,
        base: BaseKind::Node,
        about: "a region of the problem space; can be ruled out wholesale",
    },
    NodeKindSpec {
        kind: KIND_OBSTRUCTION,
        base: BaseKind::Node,
        about: "a reason a regime or mechanism cannot work — the pruning unit",
    },
    NodeKindSpec {
        kind: KIND_GAP,
        base: BaseKind::Node,
        about: "a first-class residual: exactly what is still missing (the resume token)",
    },
    NodeKindSpec {
        kind: KIND_PARTIAL_RESULT,
        base: BaseKind::Artifact,
        about: "the strongest proved statement so far — the progress metric",
    },
    NodeKindSpec {
        kind: KIND_AUDIT,
        base: BaseKind::Node,
        about: "an adversarial check against the enumerated checklist",
    },
    NodeKindSpec {
        kind: KIND_TOOL,
        base: BaseKind::Artifact,
        about: "a harness built once and reused across rounds",
    },
    NodeKindSpec {
        kind: KIND_MECHANISM,
        base: BaseKind::Node,
        about: "a materially new idea — the currency that reopens a blocked route",
    },
];

const EDGES: &[EdgeType] = &[
    EdgeType::DependsOn,
    EdgeType::DerivedFrom,
    EdgeType::References,
    EdgeType::ParentOf,
    EdgeType::ReducesTo,
    EdgeType::EquivalentInStrengthTo,
    EdgeType::MemberOf,
    EdgeType::Refutes,
    EdgeType::RulesOut,
    EdgeType::Supports,
    EdgeType::Contradicts,
    EdgeType::Verifies,
    EdgeType::Supersedes,
    EdgeType::ExplainsFailure,
    EdgeType::Informs,
    EdgeType::Answers,
    EdgeType::Spawns,
    EdgeType::DiscoveredFrom,
];

const VERBS: &[VerbSpec] = &[
    VerbSpec::new(
        "family",
        KIND_APPROACH_FAMILY,
        "register an approach family (a mathematical idea, not a wording)",
    ),
    VerbSpec::on(
        "approach",
        KIND_APPROACH,
        EdgeType::MemberOf,
        "open a route inside an approach family",
    )
    .optional_target(),
    VerbSpec::on(
        "reduce",
        KIND_REDUCTION,
        EdgeType::ReducesTo,
        "record 'it suffices to show X' — then check it is not equivalent in strength",
    ),
    VerbSpec::blocking(
        "lemma",
        KIND_LEMMA,
        EdgeType::DependsOn,
        "state a lemma a route needs — the route blocks on it until it lands",
    ),
    VerbSpec::blocking(
        "gap",
        KIND_GAP,
        EdgeType::DependsOn,
        "block a route at a theorem-strength missing piece, with the reason attached",
    ),
    VerbSpec::new(
        "mechanism",
        KIND_MECHANISM,
        "record a materially new idea (the currency that reopens blocked routes)",
    ),
    VerbSpec::new(
        "invariant",
        KIND_INVARIANT,
        "record a preserved quantity or property",
    ),
    VerbSpec::new(
        "regime",
        KIND_PARAMETER_REGIME,
        "name a region of the problem space",
    ),
    VerbSpec::new(
        "obstruction",
        KIND_OBSTRUCTION,
        "record why something cannot work",
    ),
    VerbSpec::on(
        "rule-out",
        KIND_OBSTRUCTION,
        EdgeType::RulesOut,
        "eliminate a whole regime — the pruning edge that stops re-treading it",
    )
    .resolving(Resolution::DeadEnd),
    VerbSpec::on(
        "construct",
        KIND_CONSTRUCTION,
        EdgeType::MemberOf,
        "build an explicit object",
    )
    .optional_target(),
    VerbSpec::on(
        "candidate-proof",
        KIND_CANDIDATE_PROOF,
        EdgeType::Answers,
        "submit a claimed affirmative resolution of the conjecture",
    )
    .optional_target()
    .tagged(&[(FACET_CONFIDENCE, "unverified")]),
    VerbSpec::on(
        "candidate-construction",
        KIND_CANDIDATE_CONSTRUCTION,
        EdgeType::Answers,
        "submit a claimed counterexample (needs an exact certificate)",
    )
    .optional_target()
    .tagged(&[(FACET_CONFIDENCE, "unverified")]),
    VerbSpec::on(
        "audit",
        KIND_AUDIT,
        EdgeType::References,
        "open an adversarial audit of a candidate against the enumerated checklist",
    ),
    VerbSpec::on(
        "certify",
        KIND_AUDIT,
        EdgeType::Verifies,
        "record that a candidate SURVIVED adversarial audit",
    )
    .resolving(Resolution::Success)
    .target_kinds(CANDIDATE_KINDS)
    .tagged(&[(FACET_CONFIDENCE, "peer-reviewed")]),
    VerbSpec::on(
        "refute",
        KIND_OBSTRUCTION,
        EdgeType::Refutes,
        "kill a candidate or lemma — retained as a tombstone with the reason",
    )
    .resolving(Resolution::DeadEnd),
    VerbSpec::on(
        "explain-failure",
        KIND_OBSTRUCTION,
        EdgeType::ExplainsFailure,
        "record WHY a dead route died (so the graveyard teaches, not just blocks)",
    ),
    VerbSpec::new(
        "partial",
        KIND_PARTIAL_RESULT,
        "record the strongest proved statement so far (the progress metric)",
    ),
    VerbSpec::new(
        "tool",
        KIND_TOOL,
        "record a reusable harness built once and used across rounds",
    ),
    VerbSpec::on(
        "note",
        crate::nstype::KIND_NODE,
        EdgeType::References,
        "free-text note loosely attached to a unit (the escape hatch)",
    )
    .optional_target(),
];

impl NamespaceType for ConjectureAttack {
    fn name(&self) -> &'static str {
        NAME
    }

    fn about(&self) -> &'static str {
        "resolve a hard conjecture by proof OR disproof under one structure: diverse \
         approach families, blocked-with-reason routes, adversarial audit, retained graveyard"
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

    /// The prove / disprove / either presets — the *only* thing that differs between
    /// resolving this conjecture affirmatively and refuting it (design Dmem.6B).
    fn acceptance_presets(&self) -> &'static [&'static str] {
        ACCEPTANCE_PRESETS
    }

    fn acceptance_text(&self, preset: &str) -> Option<&'static str> {
        acceptance_text(preset)
    }

    /// Done only when a candidate of the direction the goal accepts **survives adversarial
    /// audit** and leaves **no open gap** behind it (design Dmem.6B).
    ///
    /// The three conditions are the prompts' bar, made machine-checkable:
    /// 1. a `candidate-proof` (prove) or `candidate-construction` (disprove) exists — the
    ///    goal's `acceptance=` tag decides which counts;
    /// 2. it is `resolution = success` **and** carries an incoming `verifies` edge from an
    ///    `audit` unit — "survives adversarial audit", not "its author is confident";
    /// 3. nothing in its `depends_on` closure is an unresolved `gap` — this is the
    ///    machine-checkable form of "do not return a reduction, isolated missing lemma, or
    ///    best-effort summary".
    ///
    /// When it is not done, the summary names the open residual, which is exactly the
    /// handoff token a fresh agent needs: best partial result + open gap.
    fn goal_predicate(&self, conn: &Connection, ns_path: &str) -> Result<DoneState> {
        let scope = Scope::Subtree(ns_path.to_owned());
        let accepted = accepted_kinds(conn, ns_path)?;

        let candidates = Query {
            kinds: accepted.iter().map(|k| (*k).to_owned()).collect(),
            scope: scope.clone(),
            ..Query::default()
        }
        .evaluate(conn)?;
        if candidates.is_empty() {
            return Ok(DoneState::open(format!(
                "no {} submitted yet",
                accepted.join(" or ")
            )));
        }

        let mut audited_but_gapped = 0_usize;
        let mut unaudited = 0_usize;
        for candidate in candidates {
            if item::get_resolution(conn, candidate)?.unwrap_or(Resolution::Unresolved)
                != Resolution::Success
                || !survived_audit(conn, candidate)?
            {
                unaudited += 1;
                continue;
            }
            let gaps = open_gaps_under(conn, candidate)?;
            if gaps.is_empty() {
                return Ok(DoneState::done(
                    "a candidate survived adversarial audit with no open gap in its \
                     dependency closure"
                        .to_owned(),
                ));
            }
            audited_but_gapped += 1;
        }
        Ok(DoneState::open(format!(
            "{unaudited} candidate(s) not yet through audit, {audited_but_gapped} audited but \
             still resting on an open gap — a reduction with a residual is not a resolution"
        )))
    }
}

/// The candidate kinds that satisfy the goal at `ns_path`, from its `acceptance=` tag:
/// `prove` accepts only a `candidate-proof`, `disprove` only a `candidate-construction`,
/// and anything else (including no tag) accepts either — the labs' default posture of
/// running both portfolios and taking whichever completes.
///
/// # Errors
/// Returns an error if a query fails.
pub fn accepted_kinds(conn: &Connection, ns_path: &str) -> Result<Vec<&'static str>> {
    let goals = Query {
        kinds: vec![KIND_CONJECTURE.to_owned(), KIND_GOAL.to_owned()],
        scope: Scope::Subtree(ns_path.to_owned()),
        ..Query::default()
    }
    .evaluate(conn)?;
    for goal in goals {
        let preset: Option<String> = conn
            .prepare_cached(
                "SELECT value FROM tag_applications WHERE item_id = ?1 AND facet = ?2 LIMIT 1",
            )?
            .query_row((goal.get(), FACET_ACCEPTANCE), |r| r.get(0))
            .optional()?;
        match preset.as_deref() {
            Some(ACCEPT_PROVE) => return Ok(vec![KIND_CANDIDATE_PROOF]),
            Some(ACCEPT_DISPROVE) => return Ok(vec![KIND_CANDIDATE_CONSTRUCTION]),
            _ => {}
        }
    }
    Ok(CANDIDATE_KINDS.to_vec())
}

/// The acceptance-predicate text for a preset name, or `None` for an unknown one.
#[must_use]
pub fn acceptance_text(preset: &str) -> Option<&'static str> {
    match preset {
        ACCEPT_PROVE => Some(ACCEPTANCE_PROVE),
        ACCEPT_DISPROVE => Some(ACCEPTANCE_DISPROVE),
        ACCEPT_EITHER => Some(ACCEPTANCE_BOTH),
        _ => None,
    }
}

/// The `either` preset: both bars apply, whichever direction completes first.
pub const ACCEPTANCE_BOTH: &str = "\
Acceptance predicate (either direction). Resolve the statement completely, by proof OR by
explicit counterexample; run both portfolios in parallel and keep incompatible routes alive.
Whichever direction completes must clear its own bar in full — see the prove and disprove
predicates. Partial progress does not count unless it implies exactly one resolution.";

/// Whether `candidate` carries an incoming `verifies` edge from an `audit` unit — the
/// "survives adversarial audit" test. A candidate verified by anything *other* than an audit
/// does not count: self-assessment is not an audit.
///
/// # Errors
/// Returns an error if a query fails.
pub fn survived_audit(conn: &Connection, candidate: ItemId) -> Result<bool> {
    for verifier in edge::walk(
        conn,
        candidate,
        &[EdgeType::Verifies],
        1,
        edge::Direction::In,
    )? {
        let kind: Option<String> = conn
            .prepare_cached("SELECT kind FROM items WHERE id = ?1")?
            .query_row([verifier.item.get()], |r| r.get(0))
            .optional()?;
        if kind.as_deref() == Some(KIND_AUDIT) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Every unresolved `gap` in `node`'s transitive `depends_on` closure — what still stands
/// between this unit and a complete resolution. An empty result is what "no partial
/// results" means operationally; a non-empty one *is* the handoff token.
///
/// # Errors
/// Returns an error if a query fails.
pub fn open_gaps_under(conn: &Connection, node: ItemId) -> Result<Vec<ItemId>> {
    // `depends_on` is cycle-guarded, so the closure terminates; `walk` also dedups.
    let reachable = edge::walk(
        conn,
        node,
        &[EdgeType::DependsOn],
        MAX_CLOSURE_DEPTH,
        edge::Direction::Out,
    )?;
    let mut gaps = Vec::new();
    for hop in reachable {
        let row: Option<(String, Option<String>)> = conn
            .prepare_cached("SELECT kind, resolution FROM items WHERE id = ?1")?
            .query_row([hop.item.get()], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((kind, resolution)) = row else {
            continue;
        };
        let unresolved = Resolution::from_str_opt(resolution.as_deref().unwrap_or_default())
            .is_none_or(|r| !r.is_settled());
        if kind == KIND_GAP && unresolved {
            gaps.push(hop.item);
        }
    }
    Ok(gaps)
}

/// How deep a `depends_on` closure is followed. Generous, and bounded so a pathological
/// graph cannot make a read run away.
const MAX_CLOSURE_DEPTH: usize = 64;

/// Whether `node` is **anti-progress**: it reduces to (or is) a statement
/// `equivalent_in_strength_to` something else — verbatim from the prompts, "a route that
/// ends at a lemma equivalent in strength to the original conjecture is not close to
/// completion unless it supplies a genuinely new proof of that lemma".
///
/// More edges is not more progress: this is the check that stops an elegant reduction from
/// dominating a portfolio on aesthetics.
///
/// # Errors
/// Returns an error if a query fails.
pub fn is_anti_progress(conn: &Connection, node: ItemId) -> Result<bool> {
    // Either the unit itself is declared equivalent in strength to something, or a unit it
    // reduces to / depends on is.
    let mut suspects = vec![node];
    for hop in edge::walk(
        conn,
        node,
        &[EdgeType::ReducesTo, EdgeType::DependsOn],
        MAX_CLOSURE_DEPTH,
        edge::Direction::Out,
    )? {
        suspects.push(hop.item);
    }
    for suspect in suspects {
        let hit: Option<i64> = conn
            .prepare_cached(
                "SELECT 1 FROM edges
                 WHERE (src_item_id = ?1 OR dst_item_id = ?1)
                   AND type = 'equivalent_in_strength_to' LIMIT 1",
            )?
            .query_row([suspect.get()], |r| r.get(0))
            .optional()?;
        if hit.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// How many live routes sit in an approach family — the **family pressure** the prompts ask
/// an orchestrator to watch ("if many agents converge to one family, redirect some of
/// them"). Counts `member_of` sources that are still unresolved, so a family whose routes
/// all died reads as empty rather than crowded.
///
/// # Errors
/// Returns an error if a query fails.
pub fn family_pressure(conn: &Connection, family: ItemId) -> Result<i64> {
    Ok(conn
        .prepare_cached(
            "SELECT count(*) FROM edges e JOIN items i ON i.id = e.src_item_id
             WHERE e.dst_item_id = ?1 AND e.type = 'member_of'
               AND (i.resolution IS NULL OR i.resolution = 'unresolved')",
        )?
        .query_row([family.get()], |r| r.get(0))?)
}

/// The **gated reopen** (design Dmem.6B): admit `evidence` as grounds to reopen the blocked
/// `route` only if it is a materially new mechanism, invariant, construction, or obstruction
/// ([`REOPENING_KINDS`]). Returns the evidence kind on success.
///
/// This is deliberately a gate rather than a suggestion. The prompts' failure mode is
/// re-entering a route that is still blocked at the same theorem-strength lemma, burning
/// rounds on it, and calling the churn progress.
///
/// # Errors
/// Returns [`jkb_types::Error::NotFound`] if either unit is missing, or a validation error
/// naming the accepted kinds if `evidence` is not one of them.
pub fn reopen_gate(conn: &Connection, route: ItemId, evidence: ItemId) -> Result<String> {
    for (label, id) in [("route", route), ("evidence", evidence)] {
        if item::get(conn, id)?.is_none() {
            return Err(TypeError::NotFound(format!("{label} item {id}")).into());
        }
    }
    let kind = item::get(conn, evidence)?
        .map(|m| m.kind)
        .unwrap_or_default();
    if !REOPENING_KINDS.contains(&kind.as_str()) {
        return Err(TypeError::Validation(format!(
            "cannot reopen with a `{kind}`: a blocked route reopens only on a materially new \
             {} — see the acceptance predicate on the goal",
            REOPENING_KINDS.join(", ")
        ))
        .into());
    }
    Ok(kind)
}
