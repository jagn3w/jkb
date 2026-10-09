//! `design.*` through the backend: the wire shapes, the version-token edit, and who may approve.

use std::sync::Arc;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use jkb_core::Db;
use serde_json::{json, Value};

use crate::rbac::{Caller, Tickets};
use crate::{ApiError, Backend, ErrorCode, LocalBackend, Response};

fn call(b: &LocalBackend, r: Value) -> Result<Response, ApiError> {
    b.call(serde_json::from_value(r).expect("request parses"))
}

#[allow(clippy::needless_pass_by_value)] // every call site builds its request inline
fn ok(b: &LocalBackend, r: Value) -> Response {
    call(b, r.clone()).unwrap_or_else(|e| panic!("{r}: {e:?}"))
}

struct Kb {
    db: Db,
    op: LocalBackend,
    tickets: Arc<Tickets>,
}

impl Kb {
    fn new() -> Self {
        let db = Db::open_in_memory().unwrap();
        let tickets = Arc::new(Tickets::default());
        Self {
            op: LocalBackend::new(db.clone()).with_tickets(Arc::clone(&tickets)),
            db,
            tickets,
        }
    }

    fn as_role(&self, role: &str) -> LocalBackend {
        let token = match ok(
            &self.op,
            json!({ "op": "role.grant", "role": role, "agent": format!("{role}-agent") }),
        ) {
            Response::Granted { token, .. } => token,
            other => panic!("{other:?}"),
        };
        LocalBackend::new(self.db.clone())
            .with_tickets(Arc::clone(&self.tickets))
            .with_caller(Caller::Token(token))
    }
}

fn create(b: &LocalBackend, body: &str) -> String {
    match ok(
        b,
        json!({ "op": "design.create", "repo": "jkb", "title": "Factory", "body": body }),
    ) {
        Response::DesignCreated { design } => {
            assert_eq!(design.namespace.as_deref(), Some("designs/jkb"));
            assert!(design.topic.starts_with("design/"));
            design.uid
        }
        other => panic!("{other:?}"),
    }
}

fn cat(b: &LocalBackend, uid: &str) -> crate::designs::DesignDoc {
    match ok(b, json!({ "op": "design.cat", "uid": uid })) {
        Response::DesignText { design } => *design,
        other => panic!("{other:?}"),
    }
}

fn written(r: Response) -> crate::designs::Written {
    match r {
        Response::DesignWritten { written } => written,
        other => panic!("{other:?}"),
    }
}

#[test]
fn claude_edits_by_quote_against_the_token_it_read_and_both_edits_survive() {
    let kb = Kb::new();
    let uid = create(&kb.op, "The app reads. It writes nothing.");
    let read = cat(&kb.op, &uid);
    assert_eq!(read.text, "The app reads. It writes nothing.");
    // The operator's edit lands first…
    let r = ok(
        &kb.op,
        json!({ "op": "design.edit", "uid": uid, "base": read.version,
                "edit": { "how": "insert_after", "find": "The app", "text": " (desktop)" } }),
    );
    assert!(
        r.announces_a_send(),
        "a stored update wakes the design's subscribers"
    );
    // …and Claude's, against the version it read, merges with it.
    let w = written(ok(
        &kb.op,
        json!({ "op": "design.edit", "uid": uid, "base": read.version,
                "edit": { "how": "replace", "find": "writes nothing", "with": "writes via ops" } }),
    ));
    assert_eq!(w.seq, Some(3));
    assert_eq!(
        cat(&kb.op, &uid).text,
        "The app (desktop) reads. It writes via ops."
    );
    let e = call(
        &kb.op,
        json!({ "op": "design.edit", "uid": uid, "base": read.version,
                "edit": { "how": "replace", "find": "nowhere", "with": "x" } }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
}

#[test]
fn an_editor_syncs_by_state_and_apply_in_base64() {
    let kb = Kb::new();
    let uid = create(&kb.op, "abc");
    let (update, version) = match ok(&kb.op, json!({ "op": "design.state", "uid": uid })) {
        Response::DesignUpdate {
            update, version, ..
        } => (update, version),
        other => panic!("{other:?}"),
    };
    let peer = jkb_core::design::crdt::Crdt::new();
    peer.apply(&STANDARD.decode(update).unwrap()).unwrap();
    let ((), mine) = peer
        .change(|txn, body, _| {
            yrs::Text::insert(body, txn, 3, "def");
            Ok(())
        })
        .unwrap();
    let w = written(ok(
        &kb.op,
        json!({ "op": "design.apply", "uid": uid, "update": STANDARD.encode(mine.unwrap()) }),
    ));
    assert!(w.seq.is_some());
    assert_ne!(w.version, version);
    assert_eq!(cat(&kb.op, &uid).text, "abcdef");
    let e = call(
        &kb.op,
        json!({ "op": "design.apply", "uid": uid, "update": "%%%" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
}

/// D53.5 through D52 RBAC: writing a design is a designer's; a span naming the operator is approved by
/// the operator alone, one naming Claude by a Claude session (recorded by its label).
#[test]
fn a_span_is_approved_only_by_the_reviewer_it_names() {
    let kb = Kb::new();
    let designer = kb.as_role("designer");
    let implementer = kb.as_role("implementer");
    let uid = create(&designer, "one. two.");
    let e = call(
        &implementer,
        json!({ "op": "design.create", "repo": "jkb", "title": "x" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden, "{e:?}");
    let version = cat(&designer, &uid).version;
    let mine = written(ok(
        &designer,
        json!({ "op": "design.span", "uid": uid, "base": version, "find": "one." }),
    ))
    .span
    .unwrap();
    let claudes = written(ok(
        &designer,
        json!({ "op": "design.span", "uid": uid, "base": version, "find": "two.",
                "reviewer": "claude" }),
    ))
    .span
    .unwrap();
    let e = call(
        &designer,
        json!({ "op": "design.approve", "span": mine, "base": cat(&kb.op, &uid).version }),
    )
    .unwrap_err();
    assert!(e.message.contains("only the operator"), "{e:?}");
    match ok(
        &designer,
        json!({ "op": "design.approve", "span": claudes, "base": cat(&kb.op, &uid).version }),
    ) {
        Response::DesignSpan { span } => {
            assert_eq!(span.state, "APPROVED");
            assert!(span.approved_by.unwrap().starts_with("grant:"));
        }
        other => panic!("{other:?}"),
    }
    match ok(
        &kb.op,
        json!({ "op": "design.approve", "span": mine, "base": cat(&kb.op, &uid).version }),
    ) {
        Response::DesignSpan { span } => {
            assert_eq!(span.approved_by.as_deref(), Some("operator"));
        }
        other => panic!("{other:?}"),
    }
    let marked = cat(&kb.op, &uid).marked;
    assert_eq!(
        marked,
        format!("⟦{mine} APPROVED⟧one.⟦/{mine}⟧ ⟦{claudes} APPROVED⟧two.⟦/{claudes}⟧")
    );
    // Compaction is the operator's.
    let e = call(&designer, json!({ "op": "design.compact", "uid": uid })).unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden);
    match ok(&kb.op, json!({ "op": "design.compact", "uid": uid })) {
        Response::DesignCompacted { through, .. } => assert_eq!(through, 3),
        other => panic!("{other:?}"),
    }
}

/// D55.5–6 over the wire: a designer records a doc target and sources, the listing carries both,
/// and `design.export` answers the generated file — for one design, or every one with a target.
#[test]
fn a_design_records_its_doc_target_and_sources_and_exports_its_approved_text() {
    let kb = Kb::new();
    let designer = kb.as_role("designer");
    let implementer = kb.as_role("implementer");
    let uid = create(&designer, "Draft. Decided.");
    create(&designer, "no target");
    let e = call(
        &implementer,
        json!({ "op": "design.target", "uid": uid, "path": "docs/f.md" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden, "{e:?}");
    let hash = "a".repeat(64);
    for (r, check_sources) in [
        (
            json!({ "op": "design.target", "uid": uid, "path": "docs/f.md" }),
            false,
        ),
        (
            json!({ "op": "design.sources", "uid": uid,
                    "sources": [{ "path": "openspec/x/design.md", "blake3": hash }] }),
            true,
        ),
    ] {
        match ok(&designer, r) {
            Response::DesignMeta { design } => {
                assert_eq!(design.doc_target.as_deref(), Some("docs/f.md"));
                assert_eq!(design.sources.len(), usize::from(check_sources));
            }
            other => panic!("{other:?}"),
        }
    }
    let e = call(
        &designer,
        json!({ "op": "design.target", "uid": uid, "path": "../f.md" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    match ok(&kb.op, json!({ "op": "design.list", "repo": "jkb" })) {
        Response::Designs { designs } => {
            let d = designs.iter().find(|d| d.uid == uid).unwrap();
            let v = serde_json::to_value(d).unwrap();
            assert_eq!(v["doc_target"], "docs/f.md");
            assert_eq!(v["sources"][0]["path"], "openspec/x/design.md");
            assert_eq!(v["sources"][0]["blake3"], hash);
        }
        other => panic!("{other:?}"),
    }

    let version = cat(&designer, &uid).version;
    let span = written(ok(
        &designer,
        json!({ "op": "design.span", "uid": uid, "base": version, "find": "Decided." }),
    ))
    .span
    .unwrap();
    ok(
        &kb.op,
        json!({ "op": "design.approve", "span": span, "base": cat(&kb.op, &uid).version }),
    );
    let exports = |r: Value| match ok(&implementer, r) {
        Response::DesignExports { exports } => exports,
        other => panic!("{other:?}"),
    };
    let one = exports(json!({ "op": "design.export", "uid": uid }));
    assert_eq!(one.len(), 1);
    let doc = cat(&kb.op, &uid);
    assert_eq!(one[0].version, doc.version);
    assert_eq!(
        one[0].text,
        format!(
            "<!-- generated from jkb design {uid}, edit there (version {}) -->\nDecided.\n",
            doc.version
        )
    );
    assert_eq!(
        exports(json!({ "op": "design.export", "repo": "jkb" })),
        one,
        "every design with a target, and only those"
    );
    assert!(exports(json!({ "op": "design.export", "repo": "other" })).is_empty());
}

#[allow(clippy::needless_pass_by_value)] // every call site builds its ask inline
fn prompt_of(b: &LocalBackend, ask: Value) -> crate::designs::Prompt {
    match ok(b, json!({ "op": "design.prompt", "ask": ask })) {
        Response::DesignPrompt { prompt } => *prompt,
        other => panic!("{other:?}"),
    }
}

/// *Discuss* (D53.5): the app sends the selection as offsets into the version it showed, and the
/// prompt names it the way Claude edits — the quote, its occurrence and that version's token — so
/// the edit the prompt describes lands on the selected words even after a later edit.
#[test]
fn a_discuss_prompt_names_the_selection_as_the_edit_that_reaches_it() {
    let kb = Kb::new();
    let uid = create(&kb.op, "one cat, two cat");
    let read = cat(&kb.op, &uid);
    written(ok(
        &kb.op,
        json!({ "op": "design.edit", "uid": uid, "base": read.version,
                "edit": { "how": "insert_after", "find": "one", "text": " small" } }),
    ));
    let designer = kb.as_role("designer");
    let prompt = prompt_of(
        &designer,
        json!({ "kind": "discuss", "uid": uid, "base": read.version, "start": 13, "end": 16 }),
    );
    assert_eq!(prompt.kind, "discuss");
    assert_eq!(prompt.quote, "cat");
    assert_eq!(prompt.occurrence, Some(2));
    assert_eq!(prompt.version, read.version);
    for needle in [
        format!("jkb design cat {uid}"),
        format!(
            "--base {} --find=<the passage above> --occurrence 2`. That occurrence holds only \
             with `--base {}`",
            read.version, read.version
        ),
        "```\ncat\n```".to_owned(),
        "Exactly, as a JSON string: \"cat\"".to_owned(),
        "PROPOSED".to_owned(),
    ] {
        assert!(
            prompt.prompt.contains(&needle),
            "{needle:?} not in:\n{}",
            prompt.prompt
        );
    }
    // The occurrence counts matches in the version selected in, so it is never offered beside
    // another token: a re-read version may hold a new earlier match, and the pairing would edit it.
    assert_eq!(
        prompt.prompt.matches("--occurrence").count(),
        1,
        "{}",
        prompt.prompt
    );
    written(ok(
        &kb.op,
        json!({ "op": "design.edit", "uid": uid, "base": prompt.version,
                "edit": { "how": "replace", "find": prompt.quote, "occurrence": prompt.occurrence, "with": "dog" } }),
    ));
    assert_eq!(cat(&kb.op, &uid).text, "one small cat, two dog");
    // A selection the version cannot hold is the engine's refusal, as `invalid`.
    let e = call(
        &kb.op,
        json!({ "op": "design.prompt", "ask": { "kind": "discuss", "uid": uid, "start": 5, "end": 500 } }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    // An unknown kind is a bad request, not a guess.
    assert!(serde_json::from_value::<crate::Request>(
        json!({ "op": "design.prompt", "ask": { "kind": "summarize", "uid": uid } })
    )
    .is_err());
    // …and so is a known kind with another kind's fields.
    assert!(serde_json::from_value::<crate::Request>(
        json!({ "op": "design.prompt", "ask": { "kind": "play", "uid": uid } })
    )
    .is_err());
}

/// A passage with a line break at its edge: the fence cannot show it, but the quote and its
/// occurrence count it, so the prompt gives the exact quote as a JSON string and says so — copying
/// the visible `foo` would match three times, and occurrence 2 of those is the middle line.
#[test]
fn a_discussed_passage_with_an_edge_line_break_is_quoted_exactly() {
    let kb = Kb::new();
    let uid = create(&kb.op, "foo bar\nfoo\nfoo\n");
    let prompt = prompt_of(
        &kb.op,
        json!({ "kind": "discuss", "uid": uid, "start": 12, "end": 16 }),
    );
    assert_eq!(prompt.quote, "foo\n");
    assert_eq!(prompt.occurrence, Some(2));
    for needle in [
        "Exactly, as a JSON string: \"foo\\n\"",
        "begins or ends with whitespace or a line break",
        "--find=$'foo\\n'",
        "an apostrophe is written `\\'`",
    ] {
        assert!(
            prompt.prompt.contains(needle),
            "{needle:?} not in:\n{}",
            prompt.prompt
        );
    }
    // A passage with no edge whitespace says nothing of it.
    let plain = prompt_of(
        &kb.op,
        json!({ "kind": "discuss", "uid": uid, "start": 4, "end": 7 }),
    );
    assert!(!plain.prompt.contains("begins or ends"), "{}", plain.prompt);
}

/// A quote holding backticks is fenced by a longer run, so the passage cannot close its own fence.
#[test]
fn a_discussed_passage_with_backticks_is_fenced_by_a_longer_run() {
    let kb = Kb::new();
    let uid = create(&kb.op, "run ```cargo``` now");
    let prompt = prompt_of(
        &kb.op,
        json!({ "kind": "discuss", "uid": uid, "start": 4, "end": 15 }),
    );
    assert!(
        prompt.prompt.contains("````\n```cargo```\n````"),
        "{}",
        prompt.prompt
    );
}

// ---- execution plans and *Play* (D53.6) --------------------------------------------------------

fn plan_of(r: Response) -> crate::designs::plans::Plan {
    match r {
        Response::DesignPlan { plan } => *plan,
        other => panic!("{other:?}"),
    }
}

fn plans_of(b: &LocalBackend, uid: &str) -> crate::designs::plans::PlanList {
    match ok(b, json!({ "op": "design.plans", "uid": uid })) {
        Response::DesignPlans { list } => *list,
        other => panic!("{other:?}"),
    }
}

#[allow(clippy::needless_pass_by_value)] // every call site builds its ask inline
fn work_prompt(b: &LocalBackend, ask: Value) -> crate::designs::plans::WorkPrompt {
    match ok(b, json!({ "op": "design.prompt", "ask": ask })) {
        Response::DesignWorkPrompt { prompt } => *prompt,
        other => panic!("{other:?}"),
    }
}

/// A design with a plan whose first step stages an approved span and holds one task.
struct Staged {
    kb: Kb,
    uid: String,
    plan: String,
    step: String,
    span: String,
    task: String,
}

/// A plan is a designer's write (not an implementer's); its steps take spans and tasks.
fn staged() -> Staged {
    let kb = Kb::new();
    let uid = create(&kb.op, "Scaffold the app. Ship it.");
    let ask = json!({ "op": "design.plan_create", "uid": uid, "title": "First cut",
                      "steps": ["scaffold", "deploy"] });
    let e = call(&kb.as_role("implementer"), ask.clone()).unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden, "{e:?}");
    let plan = plan_of(ok(&kb.as_role("designer"), ask));
    assert_eq!(plan.design, uid);
    assert_eq!(plan.steps.len(), 2);
    let step = plan.steps[0].uid.clone();
    let plan = plan_of(ok(
        &kb.op,
        json!({ "op": "design.plan_step", "plan": plan.uid, "text": "monitor" }),
    ));
    assert_eq!(plan.steps[2].text, "monitor");

    let read = cat(&kb.op, &uid);
    let span = written(ok(
        &kb.op,
        json!({ "op": "design.span", "uid": uid, "base": read.version, "find": "Scaffold the app." }),
    ))
    .span
    .unwrap();
    ok(
        &kb.op,
        json!({ "op": "design.approve", "span": span, "base": cat(&kb.op, &uid).version }),
    );
    ok(
        &kb.op,
        json!({ "op": "design.stage", "span": span, "step": step }),
    );
    let task = match ok(
        &kb.op,
        json!({ "op": "task.add", "text": "Build the scaffold", "under": step, "managed": true }),
    ) {
        Response::Added { added } => added.uid,
        other => panic!("{other:?}"),
    };
    Staged {
        kb,
        uid,
        plan: plan.uid,
        step,
        span,
        task,
    }
}

/// The listing carries each step's staged spans and tasks, in the wire shape the app decodes.
#[test]
fn a_plan_lists_its_steps_spans_and_tasks() {
    let Staged {
        kb,
        uid,
        plan,
        span,
        task,
        ..
    } = staged();
    // The wire shape `@jkb/core`'s `decodePlanList` and `decodeWorkPrompt` read (`ui/core/src/plan.ts`).
    let wire =
        serde_json::to_value(ok(&kb.op, json!({ "op": "design.plans", "uid": uid }))).unwrap();
    assert_eq!(wire["result"], "design_plans");
    let wire_task = &wire["list"]["plans"][0]["steps"][0]["tasks"][0];
    for key in [
        "uid",
        "title",
        "status",
        "priority",
        "depth",
        "claimed_by",
        "strategy",
    ] {
        assert!(wire_task.get(key).is_some(), "{key}: {wire_task}");
    }
    let wire = serde_json::to_value(ok(
        &kb.op,
        json!({ "op": "design.prompt", "ask": { "kind": "task", "uid": task } }),
    ))
    .unwrap();
    assert_eq!(wire["result"], "design_work_prompt");
    assert_eq!(wire["prompt"]["kind"], "task");

    let listed = plans_of(&kb.op, &uid);
    assert_eq!(listed.hidden, 0);
    assert!(
        listed.tasks.is_empty(),
        "the task is the step's, not a one-off"
    );
    let shown = &listed.plans[0];
    assert_eq!(shown.uid, plan);
    assert_eq!(shown.steps[0].spans[0].uid, span);
    assert_eq!(shown.steps[0].spans[0].state, "STAGED");
    assert_eq!(shown.steps[0].tasks[0].uid, task);
    assert_eq!(shown.steps[0].tasks[0].strategy, "default:design-reviewed");
    assert_eq!(
        plan_of(ok(
            &kb.op,
            json!({ "op": "design.plan", "plan": shown.uid })
        )),
        *shown
    );
}

/// *Play* names the plan, its steps, spans and tasks and the strategy the work runs under; a task's
/// own *Play* names where it sits and the strategy it runs.
#[test]
fn play_prompts_name_the_plan_and_the_strategy_its_work_runs_under() {
    let Staged {
        kb,
        uid,
        plan,
        step,
        span,
        task,
    } = staged();
    let prompt = work_prompt(
        &kb.op,
        json!({ "kind": "play", "plan": plan, "strategy": "coordinated" }),
    );
    assert_eq!(prompt.kind, "play");
    assert_eq!(prompt.strategy.as_deref(), Some("coordinated"));
    assert_eq!(prompt.design.as_deref(), Some(uid.as_str()));
    for needle in [
        plan.as_str(),
        step.as_str(),
        span.as_str(),
        task.as_str(),
        "Workflow strategy: coordinated",
        "\"Scaffold the app.\"",
        "Tasks: none yet.",
        "jkb task add",
        "--under <step uid>",
    ] {
        assert!(
            prompt.prompt.contains(needle),
            "{needle}: {}",
            prompt.prompt
        );
    }
    let e = call(
        &kb.op,
        json!({ "op": "design.prompt", "ask": { "kind": "play", "plan": plan, "strategy": "lax" } }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");

    // With no strategy chosen the prompt claims no choice: each task runs its own.
    let unchosen = work_prompt(&kb.op, json!({ "kind": "play", "plan": plan }));
    assert_eq!(
        unchosen.strategy, None,
        "no pick is reported as none, not the default"
    );
    assert!(
        unchosen.prompt.contains("no choice for this plan"),
        "{}",
        unchosen.prompt
    );
    assert!(
        !unchosen.prompt.contains("The operator chose it"),
        "{}",
        unchosen.prompt
    );

    // The strategy a task's *Play* names is the one it runs: pinned by the operator.
    ok(
        &kb.op,
        json!({ "op": "workflow.set", "uid": task, "strategy": "autonomous" }),
    );
    let one = work_prompt(&kb.op, json!({ "kind": "task", "uid": task }));
    assert_eq!(one.kind, "task");
    assert_eq!(one.strategy.as_deref(), Some("autonomous"));
    for needle in [
        task.as_str(),
        uid.as_str(),
        step.as_str(),
        span.as_str(),
        "jkb task work",
        "Workflow strategy: autonomous",
    ] {
        assert!(one.prompt.contains(needle), "{needle}: {}", one.prompt);
    }
    assert_eq!(
        plans_of(&kb.op, &uid).plans[0].steps[0].tasks[0].strategy,
        "autonomous"
    );
    let e = call(
        &kb.op,
        json!({ "op": "design.prompt", "ask": { "kind": "task", "uid": step } }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "a step is not a task: {e:?}");
}

const SESSION: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

fn recorded(r: Response) -> crate::designs::prompts::DesignPrompt {
    match r {
        Response::DesignPromptRecorded { prompt, .. } => *prompt,
        other => panic!("{other:?}"),
    }
}

/// A launch records its session before Claude starts — a designer's write, which the container's
/// coordinator credential holds and an implementer does not — and the Prompts pane lists them in
/// the wire shape `@jkb/core`'s `decodeDesignPrompts` reads (`ui/core/src/prompts.ts`).
#[test]
fn a_launch_records_its_session_and_the_design_lists_it() {
    let Staged { kb, uid, task, .. } = staged();
    let ask_again = |cwd: &str| {
        json!({ "op": "design.prompt_record", "uid": uid, "session": SESSION, "cwd": cwd,
                "launch": "task", "subject": task, "title": "x" })
    };
    let ask = json!({ "op": "design.prompt_record", "uid": uid, "session": SESSION,
                      "cwd": "/home/vscode/repos/jkb/.jkb/work/build", "launch": "task",
                      "subject": task, "title": "Play · Build the scaffold" });
    let e = call(&kb.as_role("implementer"), ask.clone()).unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden, "{e:?}");
    let wire = serde_json::to_value(ok(&kb.as_role("coordinator"), ask)).unwrap();
    assert_eq!(wire["result"], "design_prompt_recorded");
    for key in [
        "uid",
        "design",
        "session",
        "cwd",
        "launch",
        "subject",
        "title",
        "created_at",
    ] {
        assert!(wire["prompt"].get(key).is_some(), "{key}: {wire}");
    }
    assert_eq!(wire["prompt"]["uid"], format!("prompt:{SESSION}"));
    assert_eq!(wire["prompt"]["launch"], "task");
    assert_eq!(wire["wrote"], true);
    let again = ok(&kb.op, ask_again("/home/vscode/repos/jkb/.jkb/work/build"));
    assert!(
        !again.announces_a_send(),
        "the same place again wrote nothing, so wakes no poller"
    );

    // The same session again, from the other side of the toggle: the same prompt, moved.
    let moved = ok(&kb.op, ask_again("/Users/me/repos/jkb"));
    assert!(moved.announces_a_send(), "a move is announced");
    let moved = recorded(moved);
    assert_eq!(moved.cwd, "/Users/me/repos/jkb");
    assert_eq!(moved.subject.as_deref(), Some(task.as_str()));

    let wire =
        serde_json::to_value(ok(&kb.op, json!({ "op": "design.prompts", "uid": uid }))).unwrap();
    assert_eq!(wire["result"], "design_prompts");
    assert_eq!(wire["uid"], json!(uid));
    assert_eq!(wire["prompts"].as_array().map(Vec::len), Some(1));
    assert_eq!(wire["prompts"][0]["session"], SESSION);
    // Anyone who reads may list them.
    assert!(matches!(
        ok(
            &kb.as_role("implementer"),
            json!({ "op": "design.prompts", "uid": uid })
        ),
        Response::DesignPrompts { .. }
    ));

    let e = call(
        &kb.op,
        json!({ "op": "design.prompt_record", "uid": uid, "session": SESSION, "cwd": "/r",
                "launch": "resume", "title": "x" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    let e = call(
        &kb.op,
        json!({ "op": "design.prompts", "uid": "design:nope" }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound, "{e:?}");

    // A session resolves back to its design (the Sessions tab's *Jump to context*, D53.9), in the
    // wire shape `@jkb/core`'s `decodeSessionPrompt` reads (`ui/core/src/sessions.ts`); a session no
    // launch recorded answers without a prompt, to anyone who reads.
    let wire = serde_json::to_value(ok(
        &kb.as_role("implementer"),
        json!({ "op": "design.prompt_of", "session": SESSION.to_ascii_uppercase() }),
    ))
    .unwrap();
    assert_eq!(wire["result"], "design_prompt_of");
    assert_eq!(wire["prompt"]["design"], json!(uid));
    assert_eq!(wire["prompt"]["cwd"], "/Users/me/repos/jkb");
    let wire = serde_json::to_value(ok(
        &kb.op,
        json!({ "op": "design.prompt_of", "session": "abc" }),
    ))
    .unwrap();
    assert_eq!(
        wire,
        json!({ "result": "design_prompt_of", "session": "abc" })
    );
}

/// *New prompt* starts a session with the operator's words, fenced, after how to read the design.
#[test]
fn a_new_prompt_carries_the_operators_words_and_how_to_read_the_design() {
    let kb = Kb::new();
    let uid = create(&kb.op, "x");
    let new = |text: &str| match ok(
        &kb.op,
        json!({ "op": "design.prompt", "ask": { "kind": "new", "uid": uid, "text": text } }),
    ) {
        Response::DesignNewPrompt { prompt } => *prompt,
        other => panic!("{other:?}"),
    };
    let p = new("  Tighten the ```intro``` please.\n");
    assert_eq!(p.kind, "new");
    assert_eq!(p.uid, uid);
    assert_eq!(p.title, "Factory");
    for needle in [
        format!("jkb design cat {uid}"),
        format!("jkb design plan ls {uid}"),
        "\n````\nTighten the ```intro``` please.\n````\n".to_owned(),
        // The one spelling of an edit every prompt teaches (`edit_usage`), the same as Discuss's.
        format!("jkb design edit {uid} --base <token> --find=<quote> --replace=<text>"),
        "Quote enough of the surrounding text that the quote occurs once".to_owned(),
    ] {
        assert!(p.prompt.contains(&needle), "{needle}: {}", p.prompt);
    }
    assert!(!p.prompt.contains("--find <quote>"), "{}", p.prompt);
    let blank = new("");
    assert!(
        blank.prompt.contains("has not said what they want yet"),
        "{}",
        blank.prompt
    );
    let wire = serde_json::to_value(ok(
        &kb.op,
        json!({ "op": "design.prompt", "ask": { "kind": "new", "uid": uid } }),
    ))
    .unwrap();
    assert_eq!(wire["result"], "design_new_prompt");

    let long = "x".repeat(crate::designs::prompts::MAX_NEW_PROMPT_BYTES + 1);
    let e = call(
        &kb.op,
        json!({ "op": "design.prompt", "ask": { "kind": "new", "uid": uid, "text": long } }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid, "{e:?}");
    let e = call(
        &kb.op,
        json!({ "op": "design.prompt", "ask": { "kind": "new", "uid": "design:nope" } }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound, "{e:?}");
}

/// The plan's *Play* says the pick covers its open tasks only when each is pinned to it — compared
/// by the identity a pinned task reports (`name@version` for a definition) — and names the others.
#[test]
fn a_plan_play_names_the_open_tasks_not_on_the_chosen_strategy() {
    let Staged { kb, plan, task, .. } = staged();
    let claims = |p: &str| p.contains("its open tasks run under it");
    let off = work_prompt(
        &kb.op,
        json!({ "kind": "play", "plan": plan, "strategy": "coordinated" }),
    );
    assert!(!claims(&off.prompt), "{}", off.prompt);
    assert!(
        off.prompt.contains(&format!(
            "not pinned to it and run their own until the operator pins them: {task} \
             (default:design-reviewed)"
        )),
        "{}",
        off.prompt
    );
    ok(
        &kb.op,
        json!({ "op": "workflow.set", "uid": task, "strategy": "coordinated" }),
    );
    let on = work_prompt(
        &kb.op,
        json!({ "kind": "play", "plan": plan, "strategy": "coordinated" }),
    );
    assert!(claims(&on.prompt), "{}", on.prompt);

    // A definition: the pick resolves to its newest version, and a task pinned to an older one is
    // not on it.
    let define = || {
        ok(
            &kb.op,
            json!({ "op": "workflow.define", "name": "mine",
                    "spec": { "graph": "direct", "toggles": { "lands": ["operator", "coordinator"] } } }),
        )
    };
    define();
    ok(
        &kb.op,
        json!({ "op": "workflow.set", "uid": task, "strategy": "mine" }),
    );
    let pinned = work_prompt(
        &kb.op,
        json!({ "kind": "play", "plan": plan, "strategy": "mine" }),
    );
    assert_eq!(pinned.strategy.as_deref(), Some("mine@1"));
    assert!(claims(&pinned.prompt), "{}", pinned.prompt);
    define();
    let newer = work_prompt(
        &kb.op,
        json!({ "kind": "play", "plan": plan, "strategy": "mine" }),
    );
    assert_eq!(newer.strategy.as_deref(), Some("mine@2"));
    assert!(!claims(&newer.prompt), "{}", newer.prompt);
    assert!(
        newer.prompt.contains(&format!("{task} (mine@1)")),
        "{}",
        newer.prompt
    );
}

/// D53.5 through D52.9: a `reviewer`-typed subagent, attested by a harness ticket, approves a span
/// naming `claude` — the path roles.rs documents for a reviewer. Unbound, so held to one task for
/// shared writes: an approval is not one (the engine holds it to the reviewer the span names), and
/// editing the design still is.
#[test]
fn an_attested_reviewer_subagent_approves_a_span_naming_claude() {
    let kb = Kb::new();
    let uid = create(&kb.op, "one. two.");
    let version = cat(&kb.op, &uid).version;
    let span_of = |find: &str, reviewer: &str| {
        written(ok(
            &kb.op,
            json!({ "op": "design.span", "uid": uid, "base": version, "find": find,
                    "reviewer": reviewer }),
        ))
        .span
        .unwrap()
    };
    let operators = span_of("one.", "operator");
    let claudes = span_of("two.", "claude");
    let container = match ok(&kb.op, json!({ "op": "role.rotate_container" })) {
        Response::Granted { token, .. } => token,
        other => panic!("{other:?}"),
    };
    ok(
        &kb.op,
        json!({ "op": "role.map", "agent_type": "reviewer", "role": "reviewer" }),
    );
    let as_token = |token: String| {
        LocalBackend::new(kb.db.clone())
            .with_tickets(Arc::clone(&kb.tickets))
            .with_caller(Caller::Token(token))
    };
    let ticket = match ok(
        &as_token(container),
        json!({ "op": "attest.mint", "session": "s", "agent_id": "r1",
                "agent_type": "reviewer", "tool_use_id": "t" }),
    ) {
        Response::Ticket { token } => token,
        other => panic!("{other:?}"),
    };
    let reviewer = as_token(ticket);
    let base = cat(&kb.op, &uid).version;
    match ok(
        &reviewer,
        json!({ "op": "design.approve", "span": claudes, "base": base }),
    ) {
        Response::DesignSpan { span } => assert_eq!(span.state, "APPROVED"),
        other => panic!("{other:?}"),
    }
    let e = call(
        &reviewer,
        json!({ "op": "design.approve", "span": operators, "base": base }),
    )
    .unwrap_err();
    assert!(e.message.contains("only the operator"), "{e:?}");
    let e = call(
        &reviewer,
        json!({ "op": "design.edit", "uid": uid, "base": base,
                "edit": { "how": "replace", "find": "one.", "with": "1." } }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::Forbidden, "{e:?}");
}
