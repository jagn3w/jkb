//! Investigation-coordination tests (design Dmem.0–9): two scripted investigations driven
//! end to end through the strategy verbs, plus the cross-strategy seam checks.
//!
//! These are written as *scripts* rather than unit assertions on purpose — the thing worth
//! testing is that an agent can drive a whole investigation through the public surface and
//! that the three buckets say the right thing at each stage.

use jkb_core::investigation::{self as inv, VerbCall};
use jkb_core::nstype::{self, conjecture, debugging};
use jkb_core::query::{self, Query, Scope};
use jkb_core::{edge, item, ns, Db};
use jkb_types::{EdgeType, ItemId, Resolution};

/// What `roll_up` would derive for one unit: the strategy's facts, read by its machine.
///
/// The two halves are asked separately on purpose — the strategy says what it *observed*, the
/// machine says what that *means* — so a test can be wrong about one without being wrong about
/// both.
fn derived(db: &Db, ns_path: &str, id: jkb_types::ItemId) -> Resolution {
    let ns_path = ns_path.to_owned();
    db.read(move |conn| {
        let (_, strategy) = nstype::for_namespace(conn, &ns_path)?.unwrap();
        let facts = strategy.unit_facts(conn, id)?;
        Ok(match strategy.unit_machine().reconcile(&facts) {
            jkb_fsm::Reconciliation::Fired(out) => out.state(),
            _ => facts.resolution,
        })
    })
    .unwrap()
}

/// Apply a verb and return the created unit's uid. Panics on failure — a script step that
/// cannot run is a test failure, not a branch to handle.
fn verb(db: &Db, ns_path: &str, call: VerbCall<'_>) -> String {
    let ns_path = ns_path.to_owned();
    let verb = call.verb.to_owned();
    let content = call.content.to_owned();
    let target = call.target_uid.map(str::to_owned);
    let weight = call.weight;
    let tags = call.tags.to_vec();
    db.write_txn("test", move |conn, meta| {
        let call = VerbCall {
            verb: &verb,
            content: &content,
            target_uid: target.as_deref(),
            weight,
            tags: &tags,
        };
        Ok(inv::apply_verb(conn, meta, &ns_path, &call)?.uid)
    })
    .unwrap_or_else(|e| panic!("verb `{}` failed: {e}", call.verb))
}

/// The same, but returning the error for the negative (gate) cases.
fn try_verb(db: &Db, ns_path: &str, call: VerbCall<'_>) -> Result<String, String> {
    let ns_path = ns_path.to_owned();
    let verb = call.verb.to_owned();
    let content = call.content.to_owned();
    let target = call.target_uid.map(str::to_owned);
    db.write_txn("test", move |conn, meta| {
        let call = VerbCall {
            verb: &verb,
            content: &content,
            target_uid: target.as_deref(),
            weight: None,
            tags: &[],
        };
        Ok(inv::apply_verb(conn, meta, &ns_path, &call)?.uid)
    })
    .map_err(|e| e.to_string())
}

fn id_of(db: &Db, uid: &str) -> ItemId {
    let owned = uid.to_owned();
    db.read(move |conn| item::id_for_uid(conn, &owned))
        .unwrap()
        .unwrap_or_else(|| panic!("no unit `{uid}`"))
}

fn frontier_uids(db: &Db, ns_path: &str) -> Vec<String> {
    let ns_path = ns_path.to_owned();
    db.read(move |conn| inv::frontier(conn, &ns_path, false, None))
        .unwrap()
        .into_iter()
        .map(|u| u.uid)
        .collect()
}

fn done_summary(db: &Db, ns_path: &str) -> (bool, String) {
    let ns_path = ns_path.to_owned();
    let state = db
        .read(move |conn| {
            let (_, strategy) = nstype::for_namespace(conn, &ns_path)?.unwrap();
            strategy.goal_predicate(conn, &ns_path)
        })
        .unwrap();
    (state.done, state.summary)
}

// ---- M5: a scripted `debugging` investigation ------------------------------

/// The whole debugging loop: symptom -> repro -> two hypotheses -> experiments -> one
/// hypothesis refuted, one suspect area ruled out -> root cause confirmed -> fix ->
/// verified. Checks the frontier, the anti-retread set, and the two-stage terminal leg at
/// each step.
#[test]
#[allow(clippy::too_many_lines)] // one scripted investigation, start to finish
fn a_debugging_investigation_runs_from_symptom_to_verified_fix() {
    let db = Db::open_in_memory().unwrap();
    let path = "memory/jkb/flaky-sync";
    db.write_txn("test", |conn, meta| {
        inv::create(
            conn,
            meta,
            path,
            debugging::NAME,
            debugging::KIND_SYMPTOM,
            "jkb sync intermittently reports a conflict on an unchanged file",
            &[],
        )?;
        Ok(())
    })
    .unwrap();

    let symptom = db
        .read(move |conn| inv::goals(conn, "memory/jkb/flaky-sync"))
        .unwrap()
        .first()
        .expect("the goal unit is seeded by `create`")
        .uid
        .clone();

    // Only the symptom is live, so it is the whole frontier.
    assert_eq!(frontier_uids(&db, path), vec![symptom.clone()]);
    let (done, why) = done_summary(&db, path);
    assert!(!done);
    assert!(why.contains("no root cause"), "{why}");

    // Reproduce it, then open two competing hypotheses. Keeping both alive is the point:
    // an investigation that collapses to one explanation too early is how you miss the bug.
    let repro = verb(
        &db,
        path,
        VerbCall::new(
            "repro",
            "run sync twice in a row on a clean tree; fails ~1 in 5",
        )
        .on(&symptom),
    );
    let hash_race = verb(
        &db,
        path,
        VerbCall::new(
            "hypothesize",
            "the base-blob hash is read before the write lands, so disk_changed sees stale bytes",
        )
        .on(&symptom),
    );
    let clock_skew = verb(
        &db,
        path,
        VerbCall::new(
            "hypothesize",
            "mtime granularity makes two writes in the same second look unchanged",
        )
        .on(&symptom),
    );

    // Rank the more promising hypothesis up; the frontier is ordered by it.
    db.write_txn("test", {
        let hash_race = hash_race.clone();
        move |conn, meta| inv::set_promise(conn, meta, &hash_race, 5.0)
    })
    .unwrap();
    let ranked = frontier_uids(&db, path);
    assert_eq!(
        ranked.first(),
        Some(&hash_race),
        "the promised hypothesis leads the frontier: {ranked:?}"
    );

    // Experiment on the clock-skew hypothesis and refute it. It is RETAINED as a tombstone.
    let experiment = verb(
        &db,
        path,
        VerbCall::new(
            "experiment",
            "log mtimes at ns resolution across a failing run",
        )
        .on(&clock_skew),
    );
    verb(
        &db,
        path,
        VerbCall::new(
            "refute",
            "mtimes differ by 4ms across the two writes, so granularity is not involved",
        )
        .on(&clock_skew),
    );
    let skew_id = id_of(&db, &clock_skew);
    assert_eq!(
        db.read(move |conn| item::get_resolution(conn, skew_id))
            .unwrap(),
        Some(Resolution::DeadEnd)
    );
    assert!(
        !frontier_uids(&db, path).contains(&clock_skew),
        "a refuted hypothesis leaves the frontier"
    );

    // The tombstone carries WHY it died — that edge is the anti-retread payload.
    let tombs = db
        .read(|conn| inv::tombstones(conn, "memory/jkb/flaky-sync"))
        .unwrap();
    let tomb = tombs
        .iter()
        .find(|t| t.unit.uid == clock_skew)
        .expect("the refuted hypothesis is in the tombstones bucket");
    assert_eq!(tomb.killed_by.len(), 1);
    assert_eq!(tomb.killed_by[0].0, EdgeType::Refutes);
    assert!(tomb.killed_by[0].2.as_deref().unwrap().contains("4ms"));

    // Localize on the WHERE axis: two suspect areas, one ruled out.
    let watcher = verb(
        &db,
        path,
        VerbCall::new("suspect", "the notify debounce window"),
    );
    let engine = verb(
        &db,
        path,
        VerbCall::new("suspect", "engine::disk_changed hashing"),
    );
    verb(
        &db,
        path,
        VerbCall::new(
            "rule-out",
            "ran with the watcher disabled and a manual sync: still fails",
        )
        .on(&watcher),
    );
    assert!(!frontier_uids(&db, path).contains(&watcher));
    assert!(frontier_uids(&db, path).contains(&engine));

    // Evidence accumulates on the surviving hypothesis, weighted.
    let mut support = VerbCall::new(
        "support",
        "adding a fsync before the hash read makes 200 runs pass",
    )
    .on(&hash_race);
    support.weight = Some(3.0);
    verb(&db, path, support);
    let hash_race_id = id_of(&db, &hash_race);
    let evidence = db
        .read(move |conn| edge::evidence_for(conn, hash_race_id))
        .unwrap();
    assert!((evidence - 3.0).abs() < 1e-9, "got {evidence}");

    // Name and confirm the root cause.
    let root_cause = verb(
        &db,
        path,
        VerbCall::new(
            "root-cause",
            "engine::reconcile hashes the base blob before the export write is flushed",
        )
        .on(&symptom),
    );
    verb(
        &db,
        path,
        VerbCall::new(
            "confirm",
            "forcing the flush order deterministically fixes 500/500 runs",
        )
        .on(&root_cause),
    );

    // A confirmed root cause is a DIAGNOSIS, not a resolution: the symptom stays open until
    // a fix for it has been verified. This is the extra terminal leg.
    let (done, why) = done_summary(&db, path);
    assert!(!done, "diagnosing is not finishing: {why}");
    assert!(why.contains("no fix"), "{why}");
    let symptom_id = id_of(&db, &symptom);
    assert_eq!(
        derived(&db, "memory/jkb/flaky-sync", symptom_id),
        Resolution::Unresolved,
        "the symptom must not roll up to success on a diagnosis alone"
    );

    // Fix + verify closes it.
    let fix = verb(
        &db,
        path,
        VerbCall::new("fix", "flush the export write before hashing the base blob").on(&root_cause),
    );
    verb(
        &db,
        path,
        VerbCall::new(
            "verify",
            "the minimized repro passes 1000/1000 with the fix",
        )
        .on(&fix),
    );
    let (done, why) = done_summary(&db, path);
    assert!(done, "{why}");
    assert!(why.contains("verified"), "{why}");

    // Now the rollup promotes the symptom, and the confirmed core holds the settled model.
    let changed = db
        .write_txn("test", |conn, meta| {
            inv::roll_up(conn, meta, "memory/jkb/flaky-sync")
        })
        .unwrap();
    assert!(
        changed
            .iter()
            .any(|(uid, _, to)| uid == &symptom && *to == Resolution::Success),
        "the rollup promotes the symptom once the fix is verified: {changed:?}"
    );
    let core: Vec<String> = db
        .read(|conn| inv::confirmed_core(conn, "memory/jkb/flaky-sync"))
        .unwrap()
        .into_iter()
        .map(|u| u.uid)
        .collect();
    for settled in [&symptom, &root_cause, &fix] {
        assert!(
            core.contains(settled),
            "{settled} belongs to the core: {core:?}"
        );
    }
    // And nothing was deleted along the way: both dead ends are still queryable.
    for dead in [&clock_skew, &watcher] {
        assert!(id_of(&db, dead).get() > 0, "{dead} is retained");
    }
    let _ = (repro, experiment);
}

/// A stale observation is excluded from the frontier and from ranking, but never deleted —
/// "we saw this before the refactor" stays on record with its edges intact.
#[test]
#[allow(clippy::too_many_lines)] // the staleness sweep and its consequences
fn observations_go_stale_when_the_code_moves_and_stop_confirming_things() {
    let db = Db::open_in_memory().unwrap();
    let path = "memory/jkb/stale";
    db.write_txn("test", |conn, meta| {
        inv::create(
            conn,
            meta,
            path,
            debugging::NAME,
            debugging::KIND_SYMPTOM,
            "panic in the writer thread under load",
            &[],
        )?;
        Ok(())
    })
    .unwrap();
    let symptom = db
        .read(|conn| inv::goals(conn, "memory/jkb/stale"))
        .unwrap()[0]
        .uid
        .clone();

    let hypothesis = verb(
        &db,
        path,
        VerbCall::new("hypothesize", "the retry loop re-enters the txn").on(&symptom),
    );
    // Two observations, each tagged with the commit range it was taken against.
    let old = verb(
        &db,
        path,
        VerbCall {
            tags: &[(
                debugging::FACET_COMMIT_RANGE.to_owned(),
                "abc123..def456".to_owned(),
            )],
            ..VerbCall::new("observe", "the panic happens inside retry()").on(&hypothesis)
        },
    );
    let current = verb(
        &db,
        path,
        VerbCall {
            tags: &[(
                debugging::FACET_COMMIT_RANGE.to_owned(),
                "def456..HEAD".to_owned(),
            )],
            ..VerbCall::new("observe", "the panic happens inside commit()").on(&hypothesis)
        },
    );
    // An observation with no recorded range: absence of provenance is not staleness.
    let unknown = verb(
        &db,
        path,
        VerbCall::new("observe", "seen once in CI, no commit recorded").on(&hypothesis),
    );

    assert!(frontier_uids(&db, path).contains(&old));

    // The code moved: everything not taken against the current window goes stale.
    let marked = db
        .write_txn("test", |conn, meta| {
            debugging::mark_stale_observations(conn, meta, "memory/jkb/stale", "def456..HEAD")
        })
        .unwrap();
    assert_eq!(marked, vec![old.clone()], "only the out-of-window one");

    let live = frontier_uids(&db, path);
    assert!(
        !live.contains(&old),
        "a stale observation leaves the frontier"
    );
    assert!(live.contains(&current));
    assert!(
        live.contains(&unknown),
        "an observation with no commit range is left alone, not silently invalidated"
    );

    // It is excluded, NOT deleted: still queryable, still holding its edge.
    let old_id = id_of(&db, &old);
    let hyp_id = id_of(&db, &hypothesis);
    let still_linked = db
        .read(move |conn| edge::edges_from(conn, old_id, EdgeType::DerivedFrom))
        .unwrap();
    assert_eq!(still_linked, vec![hyp_id], "the stale unit keeps its edges");

    // And a stale observation cannot confirm anything: the rollup refuses to settle a unit
    // on evidence about code that no longer exists.
    db.write_txn("test", move |conn, meta| {
        edge::link(conn, meta, old_id, hyp_id, EdgeType::Confirms, None)?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        derived(&db, "memory/jkb/stale", old_id),
        Resolution::Unresolved
    );
}

// ---- M6: a scripted `conjecture-attack` investigation ---------------------

/// The frontier-lab discipline, externalized: a diverse portfolio under an approach-family
/// registry, a route blocked on a gap with the reason attached, a ruled-out regime, an
/// adversarial audit, anti-progress detection, and both candidate directions.
#[test]
#[allow(clippy::too_many_lines)] // one scripted investigation, start to finish
fn a_conjecture_attack_keeps_incompatible_routes_alive_and_refuses_partial_results() {
    let db = Db::open_in_memory().unwrap();
    let path = "memory/cdc";
    db.write_txn("test", |conn, meta| {
        let goal = inv::create(
            conn,
            meta,
            path,
            conjecture::NAME,
            conjecture::KIND_CONJECTURE,
            "Every finite bridgeless loopless multigraph has a cycle double cover.",
            &[(
                conjecture::FACET_ACCEPTANCE.to_owned(),
                conjecture::ACCEPT_EITHER.to_owned(),
            )],
        )?;
        let _ = goal;
        Ok(())
    })
    .unwrap();
    let goal = db.read(|conn| inv::goals(conn, "memory/cdc")).unwrap()[0]
        .uid
        .clone();

    // A genuinely diverse portfolio, grouped by mathematical IDEA rather than wording.
    let flows = verb(
        &db,
        path,
        VerbCall::new("family", "nowhere-zero flow formulations"),
    );
    let structural = verb(
        &db,
        path,
        VerbCall::new("family", "structural induction on snarks"),
    );
    let route_a = verb(
        &db,
        path,
        VerbCall::new("approach", "lift a 6-flow to a double cover").on(&flows),
    );
    let route_b = verb(
        &db,
        path,
        VerbCall::new(
            "approach",
            "reduce to cubic, then Petersen-minor case analysis",
        )
        .on(&structural),
    );
    let route_c = verb(
        &db,
        path,
        VerbCall::new("approach", "transition systems via even subgraph parity").on(&flows),
    );

    // Family pressure answers "are too many routes converging on one idea?" as a query.
    let flows_id = id_of(&db, &flows);
    let structural_id = id_of(&db, &structural);
    let (crowded, thin) = db
        .read(move |conn| {
            Ok((
                conjecture::family_pressure(conn, flows_id)?,
                conjecture::family_pressure(conn, structural_id)?,
            ))
        })
        .unwrap();
    assert_eq!(
        (crowded, thin),
        (2, 1),
        "two live routes in the flow family"
    );

    // Incompatible routes coexist: nothing auto-prunes on conflict.
    let live = frontier_uids(&db, path);
    for route in [&route_a, &route_b, &route_c] {
        assert!(live.contains(route), "{route} stays alive: {live:?}");
    }

    // Route B stalls at a theorem-strength missing piece. Blocking it records the REASON as
    // a first-class `gap` the route depends on, so it leaves the frontier *explaining itself*.
    let gap = verb(
        &db,
        path,
        VerbCall::new(
            "gap",
            "needs: every bridgeless cubic graph has a cycle double cover — theorem-strength",
        )
        .on(&route_b),
    );
    let blocked = frontier_uids(&db, path);
    assert!(
        !blocked.contains(&route_b),
        "a route blocked on an open gap leaves the frontier"
    );
    assert!(
        blocked.contains(&gap),
        "but the GAP itself is on the frontier — that is the work now"
    );

    // A regime gets ruled out wholesale; the pruning edge stops it being re-proposed.
    let regime = verb(
        &db,
        path,
        VerbCall::new("regime", "girth <= 5 cubic graphs"),
    );
    verb(
        &db,
        path,
        VerbCall::new(
            "rule-out",
            "an exhaustive search to n=20 plus a parity obstruction closes this regime",
        )
        .on(&regime),
    );
    let tombs = db.read(|conn| inv::tombstones(conn, "memory/cdc")).unwrap();
    assert!(
        tombs.iter().any(|t| t.unit.uid == regime
            && t.killed_by.iter().any(|(e, _, _)| *e == EdgeType::RulesOut)),
        "the ruled-out regime is a tombstone linked to its obstruction"
    );

    // ANTI-PROGRESS: route A reduces the conjecture to a lemma that is equivalent in
    // strength to it. Elegant, and worth nothing — the check makes that a derived fact.
    let lemma = verb(
        &db,
        path,
        VerbCall::new(
            "lemma",
            "every bridgeless graph admits a nowhere-zero 5-flow",
        )
        .on(&route_a),
    );
    let route_a_id = id_of(&db, &route_a);
    assert!(
        !db.read(move |conn| conjecture::is_anti_progress(conn, route_a_id))
            .unwrap(),
        "not anti-progress until the equivalence is recorded"
    );
    db.write_txn("test", {
        let (lemma, goal) = (lemma.clone(), goal.clone());
        move |conn, meta| {
            inv::link(
                conn,
                meta,
                &lemma,
                EdgeType::EquivalentInStrengthTo,
                &goal,
                None,
            )
        }
    })
    .unwrap();
    assert!(
        db.read(move |conn| conjecture::is_anti_progress(conn, route_a_id))
            .unwrap(),
        "a route resting on an equivalent-strength lemma is anti-progress"
    );

    // Both directions are submitted under the SAME structure — this is one strategy.
    let candidate_proof = verb(
        &db,
        path,
        VerbCall::new("candidate-proof", "flow-lifting argument, full write-up").on(&goal),
    );
    let candidate_construction = verb(
        &db,
        path,
        VerbCall::new("candidate-construction", "a bridgeless graph with no CDC").on(&goal),
    );

    // Unaudited candidates do not finish the investigation.
    let (done, why) = done_summary(&db, path);
    assert!(!done, "{why}");
    assert!(why.contains("not yet through audit"), "{why}");

    // `certify` is GATED on the target kind: you cannot certify a route or a lemma.
    let err = try_verb(
        &db,
        path,
        VerbCall::new("certify", "looks fine to me").on(&route_c),
    )
    .unwrap_err();
    assert!(err.contains("cannot act on a `approach`"), "{err}");
    assert!(err.contains("candidate-proof"), "{err}");

    // The construction is refuted by an obstruction: retained, linked, and out of the way.
    verb(
        &db,
        path,
        VerbCall::new(
            "refute",
            "the claimed graph has a hidden 2-cycle cover via its parallel edges",
        )
        .on(&candidate_construction),
    );

    // The proof survives audit — but it still rests on the open gap, so it is a REDUCTION
    // WITH A RESIDUAL, not a resolution. This is the "no partial results" bar.
    db.write_txn("test", {
        let (candidate_proof, gap) = (candidate_proof.clone(), gap.clone());
        move |conn, meta| {
            inv::link(
                conn,
                meta,
                &candidate_proof,
                EdgeType::DependsOn,
                &gap,
                None,
            )
        }
    })
    .unwrap();
    verb(
        &db,
        path,
        VerbCall::new(
            "certify",
            "checked against the full audit checklist; no circularity",
        )
        .on(&candidate_proof),
    );
    let (done, why) = done_summary(&db, path);
    assert!(
        !done,
        "an audited candidate resting on an open gap is not a resolution: {why}"
    );
    assert!(why.contains("open gap"), "{why}");

    let proof_id = id_of(&db, &candidate_proof);
    assert!(
        db.read(move |conn| conjecture::survived_audit(conn, proof_id))
            .unwrap(),
        "the audit edge is recorded even while the gap stands"
    );
    let gaps = db
        .read(move |conn| conjecture::open_gaps_under(conn, proof_id))
        .unwrap();
    assert_eq!(gaps.len(), 1, "the open gap IS the handoff token");

    // Close the gap and the acceptance predicate is finally met.
    let gap_id = id_of(&db, &gap);
    db.write_txn("test", move |conn, meta| {
        item::set_resolution(conn, meta, gap_id, Resolution::Success)
    })
    .unwrap();
    let (done, why) = done_summary(&db, path);
    assert!(done, "{why}");
    assert!(why.contains("survived adversarial audit"), "{why}");
}

/// The acceptance preset is the *only* prove-vs-disprove difference: the same graph, the
/// same verbs, a different bar.
#[test]
fn the_acceptance_preset_is_the_only_prove_versus_disprove_difference() {
    let db = Db::open_in_memory().unwrap();
    for (path, preset, wanted) in [
        (
            "memory/jacobian-prove",
            conjecture::ACCEPT_PROVE,
            conjecture::KIND_CANDIDATE_PROOF,
        ),
        (
            "memory/jacobian-disprove",
            conjecture::ACCEPT_DISPROVE,
            conjecture::KIND_CANDIDATE_CONSTRUCTION,
        ),
    ] {
        let body = format!(
            "Resolve the Jacobian Conjecture.\n\n{}",
            conjecture::acceptance_text(preset).expect("a known preset has text")
        );
        db.write_txn("test", {
            let (path, preset, body) = (path.to_owned(), preset.to_owned(), body.clone());
            move |conn, meta| {
                inv::create(
                    conn,
                    meta,
                    &path,
                    conjecture::NAME,
                    conjecture::KIND_CONJECTURE,
                    &body,
                    &[(conjecture::FACET_ACCEPTANCE.to_owned(), preset)],
                )?;
                Ok(())
            }
        })
        .unwrap();

        // The preset decides which candidate kind can ever satisfy the goal.
        let accepted = db
            .read({
                let path = path.to_owned();
                move |conn| conjecture::accepted_kinds(conn, &path)
            })
            .unwrap();
        assert_eq!(accepted, vec![wanted], "{path}");

        // …and the not-done summary says which one is missing.
        let (done, why) = done_summary(&db, path);
        assert!(!done);
        assert!(why.contains(wanted), "{why}");

        // The seeded goal body carries the enumerated "insufficient" list, so any agent
        // picking this up reads the same bar.
        let goal_body = db
            .read({
                let path = path.to_owned();
                move |conn| inv::goals(conn, &path)
            })
            .unwrap()[0]
            .content
            .clone()
            .unwrap();
        assert!(goal_body.contains("INSUFFICIENT"), "{path}");
        assert!(
            goal_body.contains("Partial progress does not count"),
            "{path}"
        );
    }

    // No preset (or `either`) accepts whichever direction completes first — the labs'
    // default posture of running both portfolios side by side.
    db.write_txn("test", |conn, meta| {
        inv::create(
            conn,
            meta,
            "memory/open-either",
            conjecture::NAME,
            conjecture::KIND_CONJECTURE,
            "Resolve it, either way.",
            &[],
        )?;
        Ok(())
    })
    .unwrap();
    let accepted = db
        .read(|conn| conjecture::accepted_kinds(conn, "memory/open-either"))
        .unwrap();
    assert_eq!(accepted, conjecture::CANDIDATE_KINDS.to_vec());
}

/// A blocked route reopens only on a *materially new* mechanism, invariant, construction, or
/// obstruction — not on renewed enthusiasm. The gate is the thing that stops a swarm burning
/// rounds re-entering a route still stuck at the same lemma.
#[test]
fn reopening_a_blocked_route_is_gated_on_a_materially_new_mechanism() {
    let db = Db::open_in_memory().unwrap();
    let path = "memory/gated";
    db.write_txn("test", |conn, meta| {
        inv::create(
            conn,
            meta,
            path,
            conjecture::NAME,
            conjecture::KIND_CONJECTURE,
            "Resolve the conjecture.",
            &[],
        )?;
        Ok(())
    })
    .unwrap();
    let route = verb(
        &db,
        path,
        VerbCall::new("approach", "degree-growth bookkeeping"),
    );
    verb(
        &db,
        path,
        VerbCall::new("gap", "needs a uniform degree bound — theorem-strength").on(&route),
    );

    // A partial result is real progress to record, but it is NOT grounds to reopen.
    let partial = verb(
        &db,
        path,
        VerbCall::new("partial", "the bound holds for degree <= 4"),
    );
    let route_id = id_of(&db, &route);
    let partial_id = id_of(&db, &partial);
    let err = db
        .read(move |conn| Ok(conjecture::reopen_gate(conn, route_id, partial_id).err()))
        .unwrap()
        .expect("a partial-result must not reopen a blocked route")
        .to_string();
    assert!(
        err.contains("cannot reopen with a `partial-result`"),
        "{err}"
    );
    assert!(err.contains("mechanism"), "{err}");

    // A new mechanism is exactly the currency the prompts ask for.
    let mechanism = verb(
        &db,
        path,
        VerbCall::new(
            "mechanism",
            "a valuation-theoretic filtration nobody has applied here",
        ),
    );
    let mechanism_id = id_of(&db, &mechanism);
    let kind = db
        .read(move |conn| conjecture::reopen_gate(conn, route_id, mechanism_id))
        .unwrap();
    assert_eq!(kind, conjecture::KIND_MECHANISM);
}

// ---- M4/M9: the cross-strategy seam and the generic reads -----------------

/// The seam dispatches per namespace type, the generic reads work in either strategy, and an
/// ordinary untyped namespace is completely unaffected.
#[test]
#[allow(clippy::too_many_lines)] // the full cross-strategy seam check
fn the_seam_dispatches_per_namespace_and_untyped_namespaces_are_unaffected() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("test", |conn, meta| {
        inv::create(
            conn,
            meta,
            "memory/jkb/bug",
            debugging::NAME,
            debugging::KIND_SYMPTOM,
            "a bug",
            &[],
        )?;
        inv::create(
            conn,
            meta,
            "memory/math",
            conjecture::NAME,
            conjecture::KIND_CONJECTURE,
            "a conjecture",
            &[],
        )?;
        // An ordinary namespace with an ordinary task-shaped item.
        ns::ensure(conn, "repos/jkb/docs")?;
        Ok(())
    })
    .unwrap();

    // Each investigation resolves its own strategy, and its verbs are its own.
    let (_, debug_strategy) = db
        .read(|conn| nstype::for_namespace(conn, "memory/jkb/bug"))
        .unwrap()
        .unwrap();
    let (_, math_strategy) = db
        .read(|conn| nstype::for_namespace(conn, "memory/math"))
        .unwrap()
        .unwrap();
    assert_eq!(debug_strategy.name(), debugging::NAME);
    assert_eq!(math_strategy.name(), conjecture::NAME);
    assert!(debug_strategy.accepts_kind(debugging::KIND_REPRO));
    assert!(!debug_strategy.accepts_kind(conjecture::KIND_LEMMA));
    assert!(math_strategy.accepts_kind(conjecture::KIND_LEMMA));

    // A verb from the wrong strategy is rejected, listing the ones that do exist.
    let err = try_verb(
        &db,
        "memory/jkb/bug",
        VerbCall::new("family", "flow formulations"),
    )
    .unwrap_err();
    assert!(
        err.contains("is not a verb of the `debugging` strategy"),
        "{err}"
    );
    assert!(err.contains("hypothesize"), "{err}");

    // A kind from the wrong strategy is rejected too.
    let err = db
        .write_txn("test", |conn, meta| {
            inv::add(
                conn,
                meta,
                &inv::NewUnit::new(conjecture::KIND_GAP, "a gap", "memory/jkb/bug"),
            )
        })
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("is not a unit kind of the `debugging` strategy"),
        "{err}"
    );

    // Both investigations are listed with their type and unit count.
    let listed = db.read(inv::list).unwrap();
    let summary: Vec<(&str, &str, usize)> = listed
        .iter()
        .map(|r| (r.ns_path.as_str(), r.type_name, r.units))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("memory/jkb/bug", debugging::NAME, 1),
            ("memory/math", conjecture::NAME, 1),
        ],
        "one seeded goal unit each — the count is shown so an EMPTY investigation \
         (namespace and type survive an undo of its units) cannot masquerade as populated"
    );

    // An untyped namespace has no strategy, and the investigation reads refuse it with an
    // actionable message rather than half-working.
    assert!(db
        .read(|conn| nstype::for_namespace(conn, "repos/jkb/docs"))
        .unwrap()
        .is_none());
    let err = db
        .read(|conn| Ok(inv::frontier(conn, "repos/jkb/docs", false, None).err()))
        .unwrap()
        .unwrap()
        .to_string();
    assert!(err.contains("is not an investigation namespace"), "{err}");
    assert!(err.contains("jkb inv new"), "{err}");

    // …but `inv::add` into an untyped namespace still works with ANY kind: recording
    // something must never be blocked on having typed the namespace first.
    db.write_txn("test", |conn, meta| {
        inv::add(
            conn,
            meta,
            &inv::NewUnit::new("note", "just a note", "repos/jkb/docs"),
        )
    })
    .unwrap();

    // And the pre-existing task machinery is untouched: `is:ready` still works, and no
    // ordinary item acquired a resolution.
    let unresolved: usize = db
        .read(|conn| {
            Ok(Query {
                resolution: Some(Resolution::Unresolved.as_str().to_owned()),
                scope: Scope::Subtree("repos".to_owned()),
                ..Query::default()
            }
            .evaluate(conn)?
            .len())
        })
        .unwrap();
    assert_eq!(
        unresolved, 1,
        "the note reads as unresolved via its NULL column"
    );
    let with_resolution: i64 = db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM items WHERE resolution IS NOT NULL",
                [],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .unwrap();
    assert_eq!(
        with_resolution, 0,
        "nothing writes the resolution column unless asked"
    );
}

/// `jkb related`'s engine: the anti-retread check surfaces the dead ends *around* a unit
/// before any work starts, and the digest renders all three buckets.
#[test]
fn anti_retread_surfaces_neighbouring_dead_ends_and_the_digest_renders_the_buckets() {
    let db = Db::open_in_memory().unwrap();
    let path = "memory/retread";
    db.write_txn("test", |conn, meta| {
        inv::create(
            conn,
            meta,
            path,
            conjecture::NAME,
            conjecture::KIND_CONJECTURE,
            "Resolve it.",
            &[],
        )?;
        Ok(())
    })
    .unwrap();

    let family = verb(&db, path, VerbCall::new("family", "counting arguments"));
    let dead_route = verb(
        &db,
        path,
        VerbCall::new("approach", "double counting over vertices").on(&family),
    );
    verb(
        &db,
        path,
        VerbCall::new("refute", "the count is off by the number of bridges").on(&dead_route),
    );
    let new_route = verb(
        &db,
        path,
        VerbCall::new("approach", "double counting over edges").on(&family),
    );

    // Before working the new route, ask what already died around it. Two hops: new route ->
    // family -> the dead sibling.
    let new_id = id_of(&db, &new_route);
    let prior = db
        .read(move |conn| inv::anti_retread(conn, new_id, 2))
        .unwrap();
    let uids: Vec<&str> = prior.iter().map(|u| u.uid.as_str()).collect();
    assert!(
        uids.contains(&dead_route.as_str()),
        "the refuted sibling in the same family must surface: {uids:?}"
    );
    // Depth 1 reaches only the family, which is not itself a tombstone.
    assert!(db
        .read(move |conn| inv::anti_retread(conn, new_id, 1))
        .unwrap()
        .is_empty());

    // The digest renders the three buckets and the acceptance verdict.
    let body = db
        .write_txn("test", |conn, meta| {
            Ok(inv::write_digest(conn, meta, "memory/retread")?.1)
        })
        .unwrap();
    assert!(body.contains("## Frontier"), "{body}");
    assert!(body.contains("## Confirmed core"), "{body}");
    assert!(body.contains("## Tombstones (do NOT re-tread)"), "{body}");
    assert!(
        body.contains(&new_route),
        "the live route is on the frontier"
    );
    assert!(
        body.contains(&dead_route),
        "the dead route is in the graveyard"
    );
    assert!(
        body.contains("refutes by"),
        "the digest says WHY it died: {body}"
    );

    // Re-running it updates ONE reflection unit rather than piling up snapshots.
    db.write_txn("test", |conn, meta| {
        Ok(inv::write_digest(conn, meta, "memory/retread")?.0)
    })
    .unwrap();
    let digests = db
        .read(|conn| query::parse("kind:reflection ns:memory/retread/**")?.evaluate(conn))
        .unwrap();
    assert_eq!(digests.len(), 1, "the digest is idempotent");
    // It is a normal item, so every ordinary read finds it.
    let uid = inv::digest_uid("memory/retread");
    assert!(db
        .read(move |conn| item::id_for_uid(conn, &uid))
        .unwrap()
        .is_some());
}

// ---- code-review regressions (20260730-003611-jkb-memory-1) ----------------

/// The digest is synthesized memory *about* an investigation, not a unit of it, so it must
/// never appear as frontier work — and must not render itself inside its own Frontier
/// section. Without the exclusion the digest is unresolved/unblocked/unclaimed like anything
/// else, and (ranks tie at 0, ties break by uid) `reflection:digest:…` sorts ahead of
/// `symptom:`, making the summary an agent just read the first thing it is told to do.
#[test]
fn the_digest_reflection_is_never_frontier_work() {
    let db = Db::open_in_memory().unwrap();
    let path = "memory/self-ref";
    db.write_txn("test", |conn, meta| {
        inv::create(
            conn,
            meta,
            path,
            debugging::NAME,
            debugging::KIND_SYMPTOM,
            "a symptom",
            &[],
        )?;
        Ok(())
    })
    .unwrap();

    let body = db
        .write_txn("test", |conn, meta| {
            Ok(inv::write_digest(conn, meta, "memory/self-ref")?.1)
        })
        .unwrap();
    let digest_uid = inv::digest_uid(path);

    // Not in the frontier…
    let live = frontier_uids(&db, path);
    assert!(
        !live.contains(&digest_uid),
        "the digest must not be offered as work: {live:?}"
    );
    assert_eq!(live.len(), 1, "only the symptom is work");

    // …and not inside its own rendering.
    assert!(
        !body.contains(&digest_uid),
        "the digest must not render itself: {body}"
    );

    // It is still an ordinary, findable item — excluded from *work*, not hidden.
    assert!(id_of(&db, &digest_uid).get() > 0);

    // The exclusion survives a strategy that overrides `frontier` (debugging adds its
    // staleness filter on top of `base_frontier`), which is the drift this guards against.
    let (_, strategy) = db
        .read(|conn| nstype::for_namespace(conn, "memory/self-ref"))
        .unwrap()
        .unwrap();
    assert_eq!(strategy.name(), debugging::NAME);
    assert!(
        strategy
            .frontier(Scope::Subtree(path.to_owned()))
            .exclude_kinds
            .contains(&"reflection".to_owned()),
        "an overriding strategy must inherit the base exclusions"
    );
}

/// `create` is idempotent, and re-typing an existing investigation is refused rather than
/// silently applied — a re-type would leave stored units whose kinds the new strategy does
/// not accept, and swap in a "done" test the investigation was never built for.
#[test]
fn create_is_idempotent_and_refuses_to_retype_an_investigation() {
    let db = Db::open_in_memory().unwrap();
    let path = "memory/once";
    let first = db
        .write_txn("test", |conn, meta| {
            inv::create(
                conn,
                meta,
                path,
                debugging::NAME,
                debugging::KIND_SYMPTOM,
                "the original symptom",
                &[],
            )
        })
        .unwrap();

    // Re-running with the same type returns the EXISTING goal — no second goal appears.
    let again = db
        .write_txn("test", |conn, meta| {
            inv::create(
                conn,
                meta,
                path,
                debugging::NAME,
                debugging::KIND_SYMPTOM,
                "a different body that must NOT be seeded",
                &[],
            )
        })
        .unwrap();
    assert_eq!(
        again, first,
        "re-running create must not seed a second goal"
    );
    let goals = db.read(|conn| inv::goals(conn, "memory/once")).unwrap();
    assert_eq!(goals.len(), 1);
    assert_eq!(
        goals[0].content.as_deref(),
        Some("the original symptom"),
        "the existing goal is left alone"
    );

    // Re-typing is refused, naming both types and what to do instead.
    let err = db
        .write_txn("test", |conn, meta| {
            inv::create(
                conn,
                meta,
                path,
                conjecture::NAME,
                conjecture::KIND_CONJECTURE,
                "a conjecture",
                &[],
            )
        })
        .unwrap_err()
        .to_string();
    assert!(err.contains("already a `debugging` investigation"), "{err}");
    assert!(err.contains("different path"), "{err}");
    // And the type really did not change.
    assert_eq!(
        db.read(|conn| ns::get_type(conn, "memory/once"))
            .unwrap()
            .as_deref(),
        Some(debugging::NAME)
    );

    // A nested investigation inside a typed parent is still allowed — the guard is on the
    // namespace's OWN type, not an inherited one.
    db.write_txn("test", |conn, meta| {
        inv::create(
            conn,
            meta,
            "memory/once/sub",
            conjecture::NAME,
            conjecture::KIND_CONJECTURE,
            "a nested conjecture",
            &[],
        )
    })
    .unwrap();
}

/// A task's lifecycle is `status`; `resolution` is the orthogonal axis for investigation
/// units. Writing one onto a task would split `is:frontier` from `is:ready` — so both the
/// explicit setter and the bulk rollup refuse to.
#[test]
fn a_task_inside_an_investigation_never_gets_a_resolution() {
    let db = Db::open_in_memory().unwrap();
    let path = "memory/mixed";
    db.write_txn("test", |conn, meta| {
        inv::create(
            conn,
            meta,
            path,
            debugging::NAME,
            debugging::KIND_SYMPTOM,
            "a symptom",
            &[],
        )?;
        // A task placed inside the investigation namespace (a mirror, or someone filing work
        // alongside the investigation).
        let task = jkb_core::task::create(
            conn,
            meta,
            &jkb_core::task::NewTask {
                home: path.to_owned(),
                ..jkb_core::task::NewTask::new("task:in-inv", "do the thing")
            },
        )?;
        // Give it an incoming `verifies` edge, which the base rollup reads as success.
        let symptom = inv::goals(conn, path)?[0].id;
        edge::link(conn, meta, symptom, task, EdgeType::Verifies, None)?;
        Ok(())
    })
    .unwrap();

    // The rollup walks the namespace but must skip the task.
    let changed = db
        .write_txn("test", |conn, meta| {
            inv::roll_up(conn, meta, "memory/mixed")
        })
        .unwrap();
    assert!(
        !changed.iter().any(|(uid, _, _)| uid == "task:in-inv"),
        "the rollup must not touch a task: {changed:?}"
    );
    let task_id = id_of(&db, "task:in-inv");
    assert_eq!(
        db.read(move |conn| item::get_resolution(conn, task_id))
            .unwrap(),
        Some(Resolution::Unresolved),
        "the task's resolution column stays NULL"
    );

    // The explicit setter refuses, pointing at the right command.
    let err = db
        .write_txn("test", |conn, meta| {
            inv::resolve_unit(conn, meta, "task:in-inv", "dead_end")
        })
        .unwrap_err()
        .to_string();
    assert!(err.contains("is a task"), "{err}");
    assert!(err.contains("jkb task set"), "{err}");

    // …so the documented equivalence still holds for that task.
    let ready = db
        .read(|conn| query::parse("kind:task is:ready")?.evaluate(conn))
        .unwrap();
    let frontier = db
        .read(|conn| query::parse("kind:task is:frontier")?.evaluate(conn))
        .unwrap();
    assert_eq!(ready, frontier, "is:ready and is:frontier must not diverge");

    // A non-task unit in the same namespace resolves normally.
    db.write_txn("test", |conn, meta| {
        let uid = inv::goals(conn, "memory/mixed")?[0].uid.clone();
        inv::resolve_unit(conn, meta, &uid, "abandoned")
    })
    .unwrap();
}

/// Acceptance presets belong to the strategy, so one strategy's predicate can never be
/// stamped onto another's goal.
#[test]
fn acceptance_presets_are_owned_by_the_strategy() {
    let conjecture_strategy = nstype::resolve(conjecture::NAME).unwrap();
    assert_eq!(
        conjecture_strategy.acceptance_presets(),
        conjecture::ACCEPTANCE_PRESETS
    );
    assert!(conjecture_strategy
        .acceptance_text(conjecture::ACCEPT_PROVE)
        .is_some_and(|t| t.contains("INSUFFICIENT")));
    assert!(conjecture_strategy.acceptance_text("maybe").is_none());

    // `debugging`'s "done" test is not parameterized, so it offers none — which is what lets
    // the CLI refuse `--accept` for it instead of appending an unrelated predicate.
    let debug_strategy = nstype::resolve(debugging::NAME).unwrap();
    assert!(debug_strategy.acceptance_presets().is_empty());
    assert!(debug_strategy
        .acceptance_text(conjecture::ACCEPT_PROVE)
        .is_none());
}

/// Strategy-specific commands refuse to run against another strategy's investigation, and a
/// reopen that unblocked nothing reports that distinctly instead of claiming a state change.
#[test]
fn strategy_specific_commands_refuse_the_wrong_strategy_and_report_no_ops() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("test", |conn, meta| {
        inv::create(
            conn,
            meta,
            "memory/bug",
            debugging::NAME,
            debugging::KIND_SYMPTOM,
            "a symptom",
            &[],
        )?;
        inv::create(
            conn,
            meta,
            "memory/math",
            conjecture::NAME,
            conjecture::KIND_CONJECTURE,
            "a conjecture",
            &[],
        )?;
        Ok(())
    })
    .unwrap();

    // `stale` is a debugging concept: against a conjecture investigation it must error, not
    // quietly report "(no observations went stale)".
    let err = db
        .write_txn("test", |conn, meta| {
            debugging::mark_stale_observations(conn, meta, "memory/math", "abc..HEAD")
        })
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("is a `conjecture-attack` investigation"),
        "{err}"
    );
    assert!(err.contains("debugging"), "{err}");

    // `reopen` is a conjecture concept, and its gate names kinds `debugging` does not have.
    let hypothesis = verb(&db, "memory/bug", VerbCall::new("hypothesize", "a guess"));
    let invariant = verb(&db, "memory/bug", VerbCall::new("invariant", "a property"));
    let err = db
        .write_txn("test", move |conn, meta| {
            inv::reopen(conn, meta, &hypothesis, &invariant)
        })
        .unwrap_err()
        .to_string();
    assert!(err.contains("is a `debugging` investigation"), "{err}");

    // In a conjecture investigation with nothing blocking the route, reopening is a NO-OP —
    // reported as an empty gap list so the caller cannot present it as a reopen.
    let route = verb(&db, "memory/math", VerbCall::new("approach", "a route"));
    let mechanism = verb(&db, "memory/math", VerbCall::new("mechanism", "a new idea"));
    let outcome = db
        .write_txn("test", {
            let (route, mechanism) = (route.clone(), mechanism.clone());
            move |conn, meta| inv::reopen(conn, meta, &route, &mechanism)
        })
        .unwrap();
    assert_eq!(outcome.mechanism_kind, conjecture::KIND_MECHANISM);
    assert!(
        outcome.superseded_gaps.is_empty(),
        "nothing was blocking it, so nothing was reopened"
    );

    // With a gap blocking it, the same call really does reopen.
    verb(
        &db,
        "memory/math",
        VerbCall::new("gap", "a missing lemma").on(&route),
    );
    let outcome = db
        .write_txn("test", move |conn, meta| {
            inv::reopen(conn, meta, &route, &mechanism)
        })
        .unwrap();
    assert_eq!(outcome.superseded_gaps.len(), 1);
}

/// The anti-retread read is restricted to the units it actually reached: a tombstone in an
/// unrelated investigation must never surface, however many there are.
#[test]
fn anti_retread_does_not_reach_into_unrelated_investigations() {
    let db = Db::open_in_memory().unwrap();
    for path in ["memory/mine", "memory/theirs"] {
        db.write_txn("test", move |conn, meta| {
            inv::create(
                conn,
                meta,
                path,
                conjecture::NAME,
                conjecture::KIND_CONJECTURE,
                "a conjecture",
                &[],
            )?;
            Ok(())
        })
        .unwrap();
    }
    // A pile of dead ends in someone else's investigation.
    for i in 0..5 {
        let regime_text = format!("their regime {i}");
        let regime = verb(&db, "memory/theirs", VerbCall::new("regime", &regime_text));
        let obstruction_text = format!("their obstruction {i}");
        verb(
            &db,
            "memory/theirs",
            VerbCall::new("rule-out", &obstruction_text).on(&regime),
        );
    }
    // …and one in mine, reachable from the unit I am about to work on.
    let family = verb(&db, "memory/mine", VerbCall::new("family", "my family"));
    let dead = verb(
        &db,
        "memory/mine",
        VerbCall::new("approach", "my dead route").on(&family),
    );
    verb(
        &db,
        "memory/mine",
        VerbCall::new("refute", "did not work").on(&dead),
    );
    let live = verb(
        &db,
        "memory/mine",
        VerbCall::new("approach", "my new route").on(&family),
    );

    let prior = db
        .read({
            let live = live.clone();
            move |conn| {
                let id = item::id_for_uid(conn, &live)?.unwrap();
                inv::anti_retread(conn, id, 3)
            }
        })
        .unwrap();
    let uids: Vec<&str> = prior.iter().map(|u| u.uid.as_str()).collect();
    assert_eq!(
        uids,
        vec![dead.as_str()],
        "only the reachable dead end in MY investigation: {uids:?}"
    );
}

/// `list` reports an empty KB as empty and a real failure as a failure — it must not turn a
/// database error into "(no investigations yet)" with a success exit code.
#[test]
fn list_distinguishes_an_empty_memory_root_from_a_failure() {
    let db = Db::open_in_memory().unwrap();
    // No `memory/` namespace at all: an empty list, not an error.
    assert!(db.read(inv::list).unwrap().is_empty());

    // An untyped `memory/` subtree is still empty of *investigations*.
    db.write_txn("test", |conn, _m| {
        ns::ensure(conn, "memory/scratch")?;
        Ok(())
    })
    .unwrap();
    assert!(db.read(inv::list).unwrap().is_empty());
}

/// A digest bucket that hits its cap must SAY so. The digest is read instead of the full
/// graph, so an unmarked cut reads as "this is everything" — and on the tombstones bucket
/// that is exactly how an agent re-treads a dead end somebody already paid for.
///
/// Regression test for the defect this change's own dogfood investigation found
/// (`memory/jkb/digest-silent-cap`): 15 dead ends recorded, 12 rendered, nothing said.
#[test]
fn a_capped_digest_bucket_reports_what_it_elided() {
    let db = Db::open_in_memory().unwrap();
    let path = "memory/cap";
    db.write_txn("test", |conn, meta| {
        inv::create(
            conn,
            meta,
            path,
            conjecture::NAME,
            conjecture::KIND_CONJECTURE,
            "Resolve it.",
            &[],
        )?;
        Ok(())
    })
    .unwrap();

    // Three more dead ends than the digest will render.
    let overflow = inv::DIGEST_BUCKET_CAP + 3;
    for i in 0..overflow {
        let regime_text = format!("regime {i}");
        let regime = verb(&db, path, VerbCall::new("regime", &regime_text));
        let obstruction_text = format!("obstruction {i} closes it");
        verb(
            &db,
            path,
            VerbCall::new("rule-out", &obstruction_text).on(&regime),
        );
    }

    // The uncapped read has everything…
    let all = db.read(|conn| inv::tombstones(conn, "memory/cap")).unwrap();
    assert_eq!(all.len(), overflow);

    // …and the digest renders the cap's worth, having *counted* what it left out.
    let digest = db.read(|conn| inv::digest(conn, "memory/cap")).unwrap();
    assert_eq!(digest.tombstones.len(), inv::DIGEST_BUCKET_CAP);
    assert_eq!(digest.elided.2, 3);
    let body = digest.render();
    assert!(
        body.contains("… 3 more not shown here"),
        "the cut must be visible: {body}"
    );
    assert!(
        body.contains("jkb inv tombstones"),
        "and must name the uncapped read: {body}"
    );

    // A bucket under the cap says nothing — no noise when nothing was dropped.
    assert_eq!(digest.elided.1, 0);
    assert!(
        !body.contains("more not shown here — run `jkb inv core`"),
        "{body}"
    );
}

/// Investigations are repo-scoped under `memory/<repo>/…` and reachable from the *generic*
/// surface: the ordinary query DSL scopes across repos, and each investigation gets a saved
/// view per bucket so an agent that never heard of `jkb inv` can still find the frontier.
#[test]
fn investigations_are_repo_scoped_and_reachable_from_the_generic_surface() {
    let db = Db::open_in_memory().unwrap();
    for (path, type_name, kind) in [
        ("memory/jkb/leak", debugging::NAME, debugging::KIND_SYMPTOM),
        (
            "memory/other/crash",
            debugging::NAME,
            debugging::KIND_SYMPTOM,
        ),
        ("memory/cdc", conjecture::NAME, conjecture::KIND_CONJECTURE),
    ] {
        db.write_txn("test", move |conn, meta| {
            inv::create(conn, meta, path, type_name, kind, "the goal", &[])?;
            Ok(())
        })
        .unwrap();
    }

    // Cross-repo recall: `ns:memory/**` spans every investigation…
    let all = db
        .read(|conn| query::parse("ns:memory/**")?.evaluate(conn))
        .unwrap();
    assert_eq!(all.len(), 3, "one goal unit per investigation");
    // …and a repo scope narrows to just that repo's.
    let jkb_only = db
        .read(|conn| query::parse("ns:memory/jkb/**")?.evaluate(conn))
        .unwrap();
    assert_eq!(jkb_only.len(), 1);

    // Each investigation has three saved views, runnable through the generic `view` surface.
    let views = db.read(jkb_core::view::list).unwrap();
    for name in inv::bucket_view_names("memory/jkb/leak") {
        assert!(
            views.iter().any(|(n, _)| *n == name),
            "missing view {name}: {views:?}"
        );
    }
    let frontier_view = inv::bucket_view_names("memory/jkb/leak")[0].clone();
    let via_view = db
        .read(move |conn| jkb_core::view::run(conn, &frontier_view))
        .unwrap();
    assert_eq!(via_view.len(), 1, "the goal unit is the whole frontier");
    // The saved view sees exactly what the ranked read sees.
    let via_inv: Vec<ItemId> = db
        .read(|conn| inv::frontier(conn, "memory/jkb/leak", false, None))
        .unwrap()
        .into_iter()
        .map(|u| u.id)
        .collect();
    assert_eq!(via_view, via_inv);
}
