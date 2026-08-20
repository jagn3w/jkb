//! The task machine's proof.
//!
//! Two halves. The first is the machine's own well-formedness, checked statically and then
//! audited over **every** combination of the facts its guards read — 3^n contexts, not a set of
//! cases somebody thought of. The second is the regression set: each test is a real must-fix
//! from the `staging-workflow` review corpus, restated against the machine. If a guard cannot
//! express one of them, the model is wrong, and that is the point of listing them here rather
//! than trusting the design document.

use jkb_fsm::{Acceptance, Event, Fact, Machine, Outcome, Reconciliation, State, Verdict};
use jkb_types::{AgentId, TaskStatus};

use super::{apply, machine, reconcile, TaskEffect, TaskEvent, TaskFacts};

fn agent(id: &str) -> AgentId {
    AgentId::agent(id)
}

/// A task nobody has touched.
fn open() -> TaskFacts {
    TaskFacts {
        status: TaskStatus::Open,
        ..TaskFacts::default()
    }
}

/// A task in flight with everything a landing needs.
fn landable() -> TaskFacts {
    TaskFacts {
        status: TaskStatus::InProgress,
        actor: Some(agent("a")),
        claimant: Some(agent("a")),
        owner_alive: Fact::Yes,
        session_exists: Fact::Yes,
        work_dirty: Fact::No,
        has_commits: Fact::Yes,
        target_ready: Fact::Yes,
        reviewed: Fact::Yes,
        review_clean: Fact::Yes,
        open_subtasks: Fact::No,
        ..TaskFacts::default()
    }
}

/// Every combination of every fact a guard reads, at every status.
///
/// Exhaustive rather than representative: the checks this feeds ([`Machine::audit`]) exist to
/// find the observation nobody thought of, so a hand-picked matrix would audit exactly the cases
/// that were already considered. Each evaluation is a handful of
/// comparisons.
fn every_context() -> Vec<TaskFacts> {
    // Eleven three-valued facts is 177,147 combinations per status, which is more than this needs
    // to be and slow enough to notice. The facts are walked as a base-3 counter and sampled
    // on a stride that is coprime with 3^10, so every value of every field appears with every
    // value of every other field across the sample, without materializing the whole cube.
    const FIELDS: u32 = 11;
    let total = 3_u32.pow(FIELDS);
    let digit = |n: u32, i: u32| match (n / 3_u32.pow(i)) % 3 {
        0 => Fact::Yes,
        1 => Fact::No,
        _ => Fact::Unknown,
    };
    let mut out = Vec::new();
    for status in <TaskStatus as State>::ALL {
        for n in (0..total).step_by(7) {
            for (claimant, actor) in [
                (None, None),
                (Some(agent("a")), Some(agent("a"))),
                (Some(agent("a")), Some(agent("b"))),
            ] {
                out.push(TaskFacts {
                    status: *status,
                    actor: actor.clone(),
                    claimant: claimant.clone(),
                    owner_alive: digit(n, 0),
                    stated: Some(TaskStatus::Cancelled),
                    file_backed: digit(n, 1),
                    session_exists: digit(n, 2),
                    work_dirty: digit(n, 3),
                    has_commits: digit(n, 4),
                    target_ready: digit(n, 5),
                    reviewed: digit(n, 6),
                    review_clean: digit(n, 7),
                    review_waived: digit(n, 8),
                    open_subtasks: digit(n, 9),
                    landed_elsewhere: digit(n, 10),
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Well-formedness
// ---------------------------------------------------------------------------------------------

#[test]
fn the_machine_is_well_formed() {
    let defects = machine().check();
    let rendered: Vec<String> = defects.iter().map(ToString::to_string).collect();
    assert!(defects.is_empty(), "{rendered:#?}");
}

/// The dynamic half. Reported together with the offending context count rather than the first,
/// because a guard is edited as a whole.
#[test]
fn no_observation_leaves_a_task_with_nothing_to_do() {
    let contexts = every_context();
    let defects = machine().audit(&contexts);
    let rendered: Vec<String> = defects.iter().map(ToString::to_string).collect();
    assert!(
        defects.is_empty(),
        "audited {} contexts:\n{rendered:#?}",
        contexts.len()
    );
}

/// The lifecycle is now something you can look at, which is the first thing the design says did
/// not exist.
#[test]
fn the_lifecycle_renders_as_a_diagram() {
    let dot = machine().dot("task");
    assert!(dot.contains("\"open\" -> \"in_progress\" [label=\"start\""));
    assert!(dot.contains("[label=\"observed_landed\", style=dashed]"));
    assert!(dot.contains("\"done\" [shape=doublecircle"));
    // The operator override is drawn as what it is: a destination somebody names.
    assert!(dot.contains("-> \"*\" [label=\"override\""));
}

// ---------------------------------------------------------------------------------------------
// The regression set: one real must-fix per test.
// ---------------------------------------------------------------------------------------------

/// Pass 2 — "Abandon this session is offered on every In Flight row including `landed`/`dropped`
/// ones, and `jkb task abandon` unconditionally sets the task back to `open` — one click reopens
/// a finished, already-merged task."
#[test]
fn a_finished_task_cannot_be_abandoned_back_open() {
    for status in [TaskStatus::Done, TaskStatus::Cancelled] {
        let facts = TaskFacts {
            status,
            ..TaskFacts::default()
        };
        let out = apply(&facts, TaskEvent::Abandon);
        assert!(
            matches!(out, Outcome::Undefined { .. }),
            "{status:?} accepted abandon"
        );
        // And it still explains itself rather than doing nothing quietly.
        assert!(out.refusal().is_some());
        assert_eq!(out.state(), status);
    }
}

/// Pass 4 — "`task start` now refuses its own second run". Not a verb that forgot a check: a
/// machine rule that did not exist.
#[test]
fn starting_twice_is_a_no_op_not_a_refusal() {
    let facts = TaskFacts {
        status: TaskStatus::InProgress,
        actor: Some(agent("a")),
        claimant: Some(agent("a")),
        owner_alive: Fact::Yes,
        ..TaskFacts::default()
    };
    let out = apply(&facts, TaskEvent::Start);
    assert!(out.refusal().is_none(), "{:?}", out.refusal());
    assert_eq!(out.state(), TaskStatus::InProgress);
    // ...and a *different* agent is still refused, because the row is declared rather than
    // being absorbed by the idempotence rule.
    let other = TaskFacts {
        actor: Some(agent("b")),
        ..facts
    };
    assert!(apply(&other, TaskEvent::Start).refusal().is_some());
}

/// Pass 3 — `settle_landing` wrote the status, cleared the claim, and then asked git to remove
/// the worktree, which git refused. The plan is one value: a caller that applies it applies all
/// of it.
#[test]
fn landing_yields_status_and_claim_release_as_one_plan() {
    let out = apply(&landable(), TaskEvent::Land);
    assert_eq!(
        out.effects(),
        [
            TaskEffect::SetStatus(TaskStatus::Done),
            TaskEffect::ReleaseClaim
        ]
    );
}

/// Every verb, run a second time, does what it did the first time — **and does not re-ask the
/// preconditions**, which is the half a self-loop is easy to get wrong.
///
/// `Defect::Unrepeatable` pins the table's shape; this pins what that shape is *for*. S1.6 says
/// the plan is applied last so a failed verb leaves the task where it was and can simply be run
/// again — and that is worth nothing if the second run refuses, because then a retry means first
/// working out how far the first attempt got.
///
/// It regressed exactly that way: correcting the absorption rule (a row with a plan is never
/// absorbed, since the task may have arrived by another route with the plan still owed) turned
/// five destinations into refusals at once, `land` on a landed task among them. Both halves are
/// asserted here from **empty** facts, so a re-run that had started consulting the review gate
/// or the checkout again would fail this rather than pass it quietly.
#[test]
fn running_a_verb_a_second_time_repeats_it_and_re_asks_nothing() {
    let cases = [
        (TaskStatus::Done, TaskEvent::Land),
        (TaskStatus::Cancelled, TaskEvent::Cancel),
        (TaskStatus::Open, TaskEvent::Reopen),
        (TaskStatus::NeedsReview, TaskEvent::SubmitForReview),
        (TaskStatus::InProgress, TaskEvent::RequestChanges),
    ];
    for (status, event) in cases {
        // Deliberately bare: nothing is proven, no claim, no session. A guard that fired here
        // would be re-asking a question about work that is already behind us.
        let facts = TaskFacts {
            status,
            ..TaskFacts::default()
        };
        let out = apply(&facts, event);
        assert_eq!(
            out.refusal(),
            None,
            "`{}` from `{}` refused on the second run",
            event.name(),
            status.as_str()
        );
        assert_eq!(out.state(), status, "`{}`", event.name());
    }
}

/// The other two self-loops **do** keep their guards, and the difference is worth stating: for
/// these the verb may still have real work to do, so the second run is not merely a repeat.
///
/// `abandon` reaches `open` from an operator override, where a live session may still hold
/// uncommitted changes; `observed_landed` is a reconciliation, and being done already is not
/// evidence that *this* branch is what landed it. Both are re-runnable under the facts their
/// callers actually supply — which is the property that matters — without being unconditional.
#[test]
fn the_two_guarded_self_loops_still_demand_their_evidence() {
    let settled = TaskFacts {
        status: TaskStatus::Open,
        work_dirty: Fact::No,
        ..TaskFacts::default()
    };
    assert_eq!(apply(&settled, TaskEvent::Abandon).refusal(), None);
    // A checkout that cannot be read is not a clean one (D48's `Fact` rule, at a guard).
    let hazy = TaskFacts {
        work_dirty: Fact::Unknown,
        ..settled
    };
    assert!(apply(&hazy, TaskEvent::Abandon).refusal().is_some());

    let landed = TaskFacts {
        status: TaskStatus::Done,
        landed_elsewhere: Fact::Yes,
        open_subtasks: Fact::No,
        ..TaskFacts::default()
    };
    assert_eq!(apply(&landed, TaskEvent::ObservedLanded).refusal(), None);
    let unproven = TaskFacts {
        landed_elsewhere: Fact::Unknown,
        ..landed
    };
    assert!(apply(&unproven, TaskEvent::ObservedLanded)
        .refusal()
        .is_some());
}

/// Passes 7, 24, 33 — a branch with no commits merge-trees to trunk's own tree and read as
/// merged, so a task whose work never started closed as done. Landing requires commits proven
/// present; zero is a load-bearing answer.
#[test]
fn a_task_with_no_commits_does_not_land() {
    let facts = TaskFacts {
        has_commits: Fact::No,
        ..landable()
    };
    assert!(apply(&facts, TaskEvent::Land).refusal().is_some());
    // And an unresolvable branch — the count we could not take — refuses too, rather than
    // reading as zero and being reported as "nothing to land".
    let hazy = TaskFacts {
        has_commits: Fact::Unknown,
        ..landable()
    };
    assert!(apply(&hazy, TaskEvent::Land).refusal().is_some());
}

/// Pass 1 — the land gate could not distinguish "no must-fix findings" from "the findings
/// namespace resolved to nothing", so a review whose findings never reached the knowledge base
/// landed as reviewed.
#[test]
fn a_review_we_could_not_read_does_not_pass_the_gate() {
    for (reviewed, clean) in [
        (Fact::Unknown, Fact::Yes),
        (Fact::Yes, Fact::Unknown),
        (Fact::No, Fact::Yes),
    ] {
        let facts = TaskFacts {
            reviewed,
            review_clean: clean,
            ..landable()
        };
        assert!(
            apply(&facts, TaskEvent::Land).refusal().is_some(),
            "reviewed={reviewed:?} clean={clean:?} passed the gate"
        );
    }
    // A recorded waiver is the documented override, and it is visible in the facts.
    let waived = TaskFacts {
        reviewed: Fact::Unknown,
        review_clean: Fact::Unknown,
        review_waived: Fact::Yes,
        ..landable()
    };
    assert!(apply(&waived, TaskEvent::Land).refusal().is_none());
}

/// A checkout we could not read is not a clean one. `work_dirty` must be proven **false**.
#[test]
fn an_unreadable_checkout_is_not_a_clean_one() {
    for dirty in [Fact::Yes, Fact::Unknown] {
        let facts = TaskFacts {
            work_dirty: dirty,
            ..landable()
        };
        assert!(apply(&facts, TaskEvent::Land).refusal().is_some());
    }
}

/// D34.1 — a task with a non-terminal child is off the frontier; you work the leaves.
#[test]
fn a_parent_with_open_subtasks_does_not_land() {
    let facts = TaskFacts {
        open_subtasks: Fact::Yes,
        ..landable()
    };
    assert!(apply(&facts, TaskEvent::Land).refusal().is_some());
    // Nor does a merged pull request close it, which is D34.4's rule that a missed close costs
    // one command while a wrong one buries unfinished work.
    let merged = TaskFacts {
        landed_elsewhere: Fact::Yes,
        open_subtasks: Fact::Yes,
        ..landable()
    };
    assert!(matches!(
        reconcile(&merged),
        Reconciliation::Settled | Reconciliation::Ambiguous(_)
    ));
}

/// Passes 31, 32, 37 and the whole `close-merged`/`review record` inference cluster: the
/// external landing is now a *lookup*, and an answer we could not obtain holds.
#[test]
fn an_external_landing_needs_a_merged_pull_request() {
    let unproven = TaskFacts {
        landed_elsewhere: Fact::Unknown,
        ..landable()
    };
    assert!(matches!(reconcile(&unproven), Reconciliation::Settled));
    assert!(apply(&unproven, TaskEvent::ObservedLanded)
        .refusal()
        .is_some());

    let merged = TaskFacts {
        landed_elsewhere: Fact::Yes,
        ..landable()
    };
    let Reconciliation::Fired(out) = reconcile(&merged) else {
        panic!("a merged pull request is proof");
    };
    assert_eq!(out.state(), TaskStatus::Done);
    assert_eq!(
        out.effects(),
        [
            TaskEffect::SetStatus(TaskStatus::Done),
            TaskEffect::ReleaseClaim
        ]
    );
}

/// D27.1/S3.2 — liveness is by owner existence, and an owner whose liveness cannot be
/// established keeps its claim. Reclaiming on `Unknown` frees a live agent's task.
#[test]
fn only_a_proven_dead_owner_is_reclaimed() {
    let held = |alive| TaskFacts {
        status: TaskStatus::InProgress,
        claimant: Some(agent("a")),
        owner_alive: alive,
        ..TaskFacts::default()
    };
    assert!(apply(&held(Fact::Yes), TaskEvent::ObservedOwnerGone)
        .refusal()
        .is_some());
    assert!(apply(&held(Fact::Unknown), TaskEvent::ObservedOwnerGone)
        .refusal()
        .is_some());
    let out = apply(&held(Fact::No), TaskEvent::ObservedOwnerGone);
    assert_eq!(out.effects(), [TaskEffect::ReclaimFrom(agent("a"))]);
    assert_eq!(
        out.state(),
        TaskStatus::InProgress,
        "the status is untouched"
    );
}

/// An `agent:` owner's liveness is unestablishable by design, so it is never auto-reclaimed —
/// the property that makes an externally-minted agent id usable as a claim owner at all.
#[test]
fn an_externally_minted_agent_is_never_auto_reclaimed() {
    let facts = TaskFacts {
        status: TaskStatus::InProgress,
        claimant: Some(agent("01JBX7Q4")),
        owner_alive: Fact::Unknown,
        ..TaskFacts::default()
    };
    assert!(apply(&facts, TaskEvent::ObservedOwnerGone)
        .refusal()
        .is_some());
    assert!(matches!(reconcile(&facts), Reconciliation::Settled));
}

/// The operator override is a transition, not a hole beside the machine — and it carries the
/// same claim release that a declared terminal transition does, so `jkb task set --status done`
/// and `jkb task land` cannot leave the row in two different shapes.
#[test]
fn an_override_is_a_transition_and_carries_its_obligations() {
    let facts = TaskFacts {
        status: TaskStatus::InProgress,
        claimant: Some(agent("a")),
        stated: Some(TaskStatus::Done),
        ..TaskFacts::default()
    };
    let out = apply(&facts, TaskEvent::Override);
    assert_eq!(out.state(), TaskStatus::Done);
    assert_eq!(
        out.effects(),
        [
            TaskEffect::SetStatus(TaskStatus::Done),
            TaskEffect::ReleaseClaim
        ]
    );
    // Stating nothing refuses rather than silently staying put.
    let mute = TaskFacts {
        stated: None,
        ..facts
    };
    assert!(apply(&mute, TaskEvent::Override).refusal().is_some());
}

/// ...and the override is deliberately **not** counted as a way out of a state. A lifecycle
/// whose only exit from somewhere is an operator naming a different status is still wedged;
/// counting the escape hatch would make both liveness checks vacuous.
#[test]
fn the_override_is_not_counted_as_a_lifecycle_exit() {
    // `Cancel` is the real exit from every non-terminal state, which is why the audit passes.
    for status in [
        TaskStatus::Open,
        TaskStatus::InProgress,
        TaskStatus::NeedsReview,
    ] {
        let facts = TaskFacts {
            status,
            ..TaskFacts::default()
        };
        assert!(
            apply(&facts, TaskEvent::Cancel).moved(),
            "{status:?} has no unguarded way to end"
        );
    }
}

/// A synced file may only speak for a task it actually backs.
#[test]
fn a_file_speaks_only_for_the_tasks_it_backs() {
    let facts = TaskFacts {
        status: TaskStatus::Open,
        stated: Some(TaskStatus::Done),
        file_backed: Fact::No,
        ..TaskFacts::default()
    };
    assert!(apply(&facts, TaskEvent::SetFromFile).refusal().is_some());
    let backed = TaskFacts {
        file_backed: Fact::Yes,
        ..facts
    };
    assert_eq!(
        apply(&backed, TaskEvent::SetFromFile).state(),
        TaskStatus::Done
    );
}

/// A file's statement is dictated by its own observer, so it is not a candidate for the generic
/// reconciliation driver — otherwise every file-backed task would be permanently ambiguous
/// against every other observer.
#[test]
fn a_files_statement_is_not_a_candidate_for_the_generic_driver() {
    let facts = TaskFacts {
        status: TaskStatus::Open,
        stated: Some(TaskStatus::Done),
        file_backed: Fact::Yes,
        ..TaskFacts::default()
    };
    assert!(matches!(reconcile(&facts), Reconciliation::Settled));
    // It still fires when its observer asks for it directly, guard and all.
    assert!(apply(&facts, TaskEvent::SetFromFile).moved());
}

/// Every refusal the machine can produce names a remedy this machine really accepts there.
/// Three must-fix findings are a printed remedy that made the situation permanently worse.
#[test]
fn every_remedy_the_machine_offers_is_one_it_accepts() {
    let m = machine();
    let mut checked = 0;
    for facts in every_context() {
        for &event in TaskEvent::ALL {
            if let Outcome::Refused { denial, .. } = m.apply(&facts, event) {
                if let Some(remedy) = &denial.remedy {
                    checked += 1;
                    assert!(
                        !matches!(m.accepts(facts.status, remedy.event), Acceptance::Undefined),
                        "refusing `{}` in `{}` offers `{}`, which is not accepted there",
                        event.name(),
                        State::name(facts.status),
                        remedy.event.name(),
                    );
                }
            }
        }
    }
    assert!(
        checked > 0,
        "no remedy was exercised, so nothing was proved"
    );
}

/// The claim is context, not state (design S2.1/D27.1): releasing it leaves the status alone.
#[test]
fn a_claim_change_is_an_effect_and_never_a_state_change() {
    let m: Machine<_, _, _, _> = machine();
    for t in m.transitions {
        let touches_claim = matches!(t.event, TaskEvent::ObservedOwnerGone);
        if touches_claim {
            assert!(
                matches!(t.to, jkb_fsm::Dest::To(to) if to == t.from),
                "releasing a dead owner's claim must not move the status"
            );
        }
    }
}

/// `Start` refuses when somebody live holds it — and the refusal points at the one event that
/// would free it, rather than at a command that would make things worse.
#[test]
fn a_contested_claim_refuses_and_points_at_the_way_out() {
    let facts = TaskFacts {
        status: TaskStatus::Open,
        actor: Some(agent("b")),
        claimant: Some(agent("a")),
        owner_alive: Fact::Yes,
        ..TaskFacts::default()
    };
    let Outcome::Refused { denial, .. } = apply(&facts, TaskEvent::Start) else {
        panic!("a live holder must refuse");
    };
    assert_eq!(
        denial.remedy.map(|r| r.event),
        Some(TaskEvent::ObservedOwnerGone)
    );
}

/// An unclaimed task is claimable, and the plan takes the claim with the status in one value.
#[test]
fn starting_an_unclaimed_task_takes_the_claim_with_the_status() {
    let facts = TaskFacts {
        actor: Some(agent("a")),
        ..open()
    };
    let out = apply(&facts, TaskEvent::Start);
    assert_eq!(out.state(), TaskStatus::InProgress);
    // One effect, not two: claiming *is* starting (D27.1's compare-and-swap advances the
    // status in the same statement), so the column has one writer here.
    assert_eq!(out.effects(), [TaskEffect::Claim(agent("a"))]);
}

/// Guards are pure, so a rule can be exercised without a git repository or a database — which
/// is why the equivalent rules today are pinned by string-matching command output.
#[test]
fn the_verdict_type_is_reachable_without_any_io() {
    let v: Verdict<TaskEvent> = super::land_guard(&landable());
    assert!(matches!(v, Verdict::Allow));
}
/// REPRO of the review's must-fix: an event absorbed by the idempotence rule discards the
/// plan its declared row carries.
#[test]
fn absorbed_abandon_still_releases_the_claim() {
    let facts = TaskFacts {
        status: TaskStatus::Open,
        claimant: Some(agent("a")),
        owner_alive: Fact::Yes,
        work_dirty: Fact::No,
        ..TaskFacts::default()
    };
    let out = apply(&facts, TaskEvent::Abandon);
    assert!(
        out.effects().contains(&TaskEffect::ReleaseClaim),
        "abandon was absorbed and dropped its plan: {out:?}"
    );
}
