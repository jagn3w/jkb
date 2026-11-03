use std::collections::BTreeMap;

use super::{
    agent_sets, copy, export, list, packaged, packaged_base, parse_file, placeholders, render,
    resolve, set, subagent_definitions, validate_name, versions, write_file, AgentPermissions,
    Edit, ExportBase, Isolation, Pick, Source, Writes, PACKAGED_JSON,
};
use crate::roles::Role;
use crate::Db;

/// The base of a copy built on `v` in a jkb that packages `v` too.
fn installed_at(v: Option<i64>) -> ExportBase {
    ExportBase {
        built_on: v,
        installed: v,
    }
}

/// An edit that makes a copy differ from the packaged text.
fn mine() -> Edit {
    Edit {
        describe: Some("the operator's".into()),
        ..Edit::default()
    }
}

fn db() -> Db {
    Db::open_in_memory().unwrap()
}

fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[test]
fn the_packaged_file_parses_validates_and_is_in_canonical_form() {
    let agents = packaged().unwrap();
    assert!(
        agents.len() >= 9,
        "the coordinators, their workers and the subagent types"
    );
    // Canonical form is what `export` writes, so a contribution's diff is only its own change.
    let file = parse_file(PACKAGED_JSON).unwrap();
    assert_eq!(write_file(&file).unwrap(), PACKAGED_JSON);
}

/// Every agent set the file names has agents, every agent names a set, and each set is described:
/// the four defects `parse_file` refuses, each on an otherwise-valid copy of the packaged file.
#[test]
fn the_packaged_file_refuses_an_undescribed_empty_duplicate_or_unnamed_agent_set() {
    let sets: Vec<_> = agent_sets()
        .unwrap()
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(sets, ["task-swarm", "code-review", "subagents"]);
    let original: serde_json::Value = serde_json::from_str(PACKAGED_JSON).unwrap();
    let refused = |change: &dyn Fn(&mut serde_json::Value), why: &str| {
        let mut v = original.clone();
        change(&mut v);
        let e = parse_file(&serde_json::to_string(&v).unwrap())
            .err()
            .unwrap_or_else(|| panic!("accepted a file where {why}"))
            .to_string();
        assert!(e.contains(why), "{why}: {e}");
    };
    refused(
        &|v| {
            v["agents"][0]["workflow"] = serde_json::json!("nowhere");
        },
        "belongs to agent set `nowhere`, which `agent_sets` does not name",
    );
    refused(
        &|v| {
            v["agent_sets"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({ "name": "idle", "describe": "Nobody here." }));
        },
        "agent set `idle` has no agents",
    );
    refused(
        &|v| {
            let first = v["agent_sets"][0].clone();
            v["agent_sets"].as_array_mut().unwrap().push(first);
        },
        "names agent set `task-swarm` twice",
    );
    refused(
        &|v| {
            v["agent_sets"][1]["describe"] = serde_json::json!(" ");
        },
        "agent set `code-review`: ",
    );
    refused(
        &|v| {
            v["agent_sets"][1]["describe"] = serde_json::json!("x".repeat(501));
        },
        "a description of 1 to 500 characters",
    );
    refused(
        &|v| {
            v.as_object_mut().unwrap().remove("agent_sets");
        },
        "it predates agent sets, so it is older than this jkb",
    );
}

/// Exporting into a packaged-templates file written before agent sets (a branch this change has not
/// reached) is a named refusal with its remedy, not serde's bare missing field, and adds nothing.
#[test]
fn exporting_into_a_file_without_agent_sets_names_the_skew() {
    let mut old: serde_json::Value = serde_json::from_str(PACKAGED_JSON).unwrap();
    old.as_object_mut().unwrap().remove("agent_sets");
    let old = serde_json::to_string_pretty(&old).unwrap() + "\n";
    let agent = packaged()
        .unwrap()
        .iter()
        .find(|a| a.name == "swarm-implementer")
        .unwrap()
        .clone();
    let mut edited = agent.clone();
    edited.def.describe = "The operator's.".into();
    let e = export(&old, &edited, installed_at(Some(agent.version)), false)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("has no `agent_sets`: it predates agent sets, so it is older than this jkb")
            && e.contains("Export from a jkb built from that file's commit"),
        "{e}"
    );
    assert!(!e.contains("missing field"), "{e}");
}

#[test]
fn every_packaged_hand_off_names_a_packaged_agent_of_the_same_workflow() {
    let agents = packaged().unwrap();
    for a in agents {
        for to in &a.def.hands_off_to {
            let target = agents
                .iter()
                .find(|b| b.name == *to)
                .unwrap_or_else(|| panic!("{} hands off to unknown {to}", a.name));
            assert_eq!(target.def.workflow, a.def.workflow, "{} -> {to}", a.name);
            assert!(!target.def.fragment, "{} hands off to a fragment", a.name);
        }
    }
}

#[test]
fn the_packaged_templates_are_what_the_coordinators_fill() {
    let agents = packaged().unwrap();
    let get = |n: &str| agents.iter().find(|a| a.name == n).unwrap();
    let imp = get("swarm-implementer");
    assert_eq!(imp.def.role, Role::Implementer);
    assert_eq!(imp.def.permissions.isolation, Isolation::Worktree);
    assert_eq!(imp.def.permissions.writes, Writes::Code);
    assert!(imp
        .def
        .template
        .starts_with("You are the IMPLEMENTER for ONE work-group in a task swarm"));
    let names = placeholders(&imp.def.template).unwrap();
    for p in ["repo", "task_list", "branch_step", "feedback_block"] {
        assert!(names.contains(&p.to_owned()), "{p} in {names:?}");
    }
    // The designer the coordinator starts for a task the design gate holds back: it writes the KB
    // (the task's design) and no files, so it needs no worktree.
    let des = get("swarm-designer");
    assert_eq!(des.def.role, Role::Designer);
    assert_eq!(des.def.permissions.isolation, Isolation::None);
    assert_eq!(des.def.permissions.writes, Writes::Kb);
    assert_eq!(
        placeholders(&des.def.template).unwrap(),
        ["feedback_block", "owner", "repo", "task_title", "task_uid"]
    );
    // The systemic reviewer the coordinator starts when review repeats an area: it records the cause
    // in the task (the KB) and reads the branch in a worktree of its own.
    for (name, isolation) in [
        ("swarm-systemic-reviewer", Isolation::Worktree),
        ("jkb-systemic-reviewer", Isolation::Worktree),
    ] {
        let sys = get(name);
        assert_eq!(sys.def.role, Role::SystemicReviewer, "{name}");
        assert_eq!(sys.def.permissions.isolation, isolation, "{name}");
        assert_eq!(sys.def.permissions.writes, Writes::Kb, "{name}");
    }
    assert_eq!(
        placeholders(&get("swarm-systemic-reviewer").def.template).unwrap(),
        [
            "branch",
            "integration",
            "repo",
            "rounds_block",
            "task_title",
            "task_uid"
        ]
    );
    // Each session's coordinator is a template of its own, acting as the coordinator.
    for c in ["swarm-coordinator", "review-coordinator"] {
        assert_eq!(get(c).def.role, Role::Coordinator, "{c}");
    }
    // `/task-swarm` fills the swarm's coordinator, the designer budget included.
    assert_eq!(get("swarm-coordinator").version, 5);
    assert_eq!(
        placeholders(&get("swarm-coordinator").def.template).unwrap(),
        [
            "designers",
            "group_cap",
            "integration",
            "integration_worktree",
            "owner",
            "repo",
            "retry_cap",
            "scope_block"
        ]
    );
    // The shared reviewer contract is a fragment the round's reviewer and its children include.
    assert!(get("review-contract").def.fragment);
    for r in ["review-reviewer", "review-area"] {
        assert!(
            placeholders(&get(r).def.template)
                .unwrap()
                .contains(&"contract".to_owned()),
            "{r} includes the contract"
        );
    }
}

/// The swarm drives the task's workflow: the reviewer files and records its round (so it writes the
/// KB), and the coordinator fires `submit_work` before a review and observes after it, routing on the
/// phase. The calls are the ones `jkb-api`'s swarm-loop test makes, as the same principals.
#[test]
fn the_swarm_templates_drive_the_workflow() {
    let agents = packaged().unwrap();
    let get = |n: &str| agents.iter().find(|a| a.name == n).unwrap();
    let rev = get("swarm-reviewer");
    assert_eq!(rev.def.role, Role::Reviewer);
    assert_eq!(rev.def.permissions.writes, Writes::Kb);
    for call in [
        "jkb role bind {{lead_uid}}",
        "jkb task review file --findings {{findings_ns}}",
        "jkb task review record --branch {{branch}} --findings {{findings_ns}}",
    ] {
        assert!(rev.def.template.contains(call), "swarm-reviewer: {call}");
    }
    let coord = &get("swarm-coordinator").def.template;
    for call in [
        "jkb workflow fire <lead uid> submit_work",
        "jkb workflow observe <lead uid>",
    ] {
        assert!(coord.contains(call), "swarm-coordinator: {call}");
    }
    // It routes on every phase an observation after a round can leave the lead in, and the submit
    // comes before the reviewer starts, the observe after it reports.
    for phase in [
        "- implement:",
        "- systemic_review:",
        "- landable:",
        "- review ",
    ] {
        assert!(coord.contains(phase), "swarm-coordinator routes {phase}");
    }
    let at = |s: &str| coord.find(s).unwrap_or_else(|| panic!("{s}"));
    assert!(at("submit_work") < at("jkb workflow agent show swarm-reviewer"));
    assert!(at("jkb workflow agent show swarm-reviewer") < at("jkb workflow observe"));
    // The lead survives a compaction.
    assert!(coord.contains("swarm-lead=<lead uid>"));
    // Step 1 admits exactly the phases step 3 drives, plus `design` on the legacy path; the rest are
    // left out. `jkb-api`'s swarm-loop tests pin the same table against what jkb lets step 3 do.
    for row in [
        "  implement, review, landable   a WORKFLOW task (read A)",
        "  design                        a LEGACY task (read B, or read C with the gate OFF)",
        "  systemic_review               a SYSTEMIC task (read A)",
    ] {
        assert!(coord.contains(row), "step 1b's routing table: {row}");
    }
    assert!(coord.contains("never a mix"));
    // Whoever verifies a fix closes its finding: the coordinator, after a clean round.
    let closing = at("CLOSING FINDINGS: ");
    assert!(coord[closing..].contains("jkb task set <finding uid> --status done"));
    assert!(closing < at("5. THE MERGE QUEUE"));
    // No run builds a finding, and approval in jkb needs no tag.
    assert!(coord.contains("No run builds a finding"));
    assert!(coord.contains("-tag:finding=review"));
    assert!(coord.contains("so it needs no `design=approved`\n"));
    // An eject puts a `landable` lead back to `implement`, or the next submit is refused.
    let arm = &coord[at("- eject:")..];
    let rework = arm.find("jkb workflow fire <lead uid> rework").unwrap();
    assert!(
        rework < arm.find("Back to step 3").unwrap(),
        "the eject arm fires rework before going back"
    );
    // A refusal stops the group: nothing routes on the reviewer's reply instead of the phase.
    assert!(!coord.contains("route step 4 on the reviewer's VERDICT"));
    assert!(coord.contains("If jkb refuses a fire, stall the group"));
}

/// Under a swarm a designer writes the design and the operator approves it: the coordinator routes a
/// task the gate holds back to a `swarm-designer` started as a `jkb-designer`, and leaves a task
/// awaiting approval to the operator without tagging or settling it: no frontier read offers
/// `design_review`, so an approval or a rejection given mid-run reaches a later read by phase alone.
/// `jkb-api`'s swarm-loop test makes the same calls as the same principals.
#[test]
fn the_swarm_designs_what_the_gate_holds_back_and_leaves_it_to_the_operator() {
    let agents = packaged().unwrap();
    let get = |n: &str| agents.iter().find(|a| a.name == n).unwrap();
    let des = &get("swarm-designer").def.template;
    let at_in = |t: &str, s: &str| t.find(s).unwrap_or_else(|| panic!("{s}"));
    // Bound first, the design written into the task, then submitted; the result line the
    // coordinator reads.
    for call in [
        "jkb role bind {{task_uid}}",
        "jkb task edit {{task_uid}} --append --stdin",
        "jkb workflow fire {{task_uid}} submit_design",
        "RESULT: submitted {{task_uid}}",
        "RESULT: failed <reason>",
    ] {
        assert!(des.contains(call), "swarm-designer: {call}");
    }
    assert!(at_in(des, "jkb role bind") < at_in(des, "jkb task edit"));
    assert!(at_in(des, "jkb task edit") < at_in(des, "submit_design`"));
    assert!(
        des.contains("OPEN QUESTIONS"),
        "a headless designer asks in the design"
    );

    let coord = &get("swarm-coordinator").def.template;
    assert!(get("swarm-coordinator")
        .def
        .hands_off_to
        .iter()
        .any(|h| h == "swarm-designer"));
    let at = |s: &str| at_in(coord, s);
    // Read by phase, then routed, before anything is grouped.
    assert!(at("1. READ THE FRONTIER, BY PHASE") < at("1b. ROUTE BY PHASE"));
    assert!(at("1b. ROUTE BY PHASE") < at("GROUP what you build"));
    assert!(coord.contains("`jkb workflow show <uid> --json` gives each task's `phase`"));
    assert!(coord.contains("task the gate holds back (read C) goes to a DESIGNER"));
    let designing =
        &coord[at("\nDESIGNING A TASK, one designer per task")..at("GROUP what you build")];
    for step in [
        "jkb task claim <uid> --owner '{{owner}}'",
        "jkb task tag set <uid> swarm-designed={{owner}}",
        "jkb workflow agent show swarm-designer",
        "subagent type `jkb-designer`",
        "RESULT: submitted <uid>",
        "jkb task set <uid> --status open",
        "jkb task release <uid> --owner '{{owner}}'",
        "phase: design_review",
    ] {
        assert!(designing.contains(step), "DESIGNING A TASK: {step}");
    }
    assert!(
        at_in(designing, "jkb task claim") < at_in(designing, "swarm-designed=")
            && at_in(designing, "swarm-designed=") < at_in(designing, "jkb workflow agent show"),
        "the budget's count is recorded before the designer starts"
    );
    // A submitted design is the operator's: no tag, no settling, no re-check before each read. The
    // parked-tag machinery existed only because the query could not test a phase.
    for gone in [
        "swarm-parked",
        "PARKED",
        "RE-CHECK THE PARKED",
        "tag:swarm-parked",
        "holds only PARKED",
    ] {
        assert!(
            !coord.contains(gone),
            "the parked machinery is gone: {gone}"
        );
    }
    let continuing = &coord[at("\nCONTINUING\n")..at("\nREPORT\n")];
    // The stop rule: every read empty, read C only while the budget lasts.
    assert!(continuing.contains("reads A, R, B and C (C\nonly while DESIGNERS LEFT is above 0"));
    // After a compaction the designer's report may be gone: the phase says whether it submitted.
    assert!(continuing.contains("At `design_review` it was submitted"));
    let report = &coord[at("\nREPORT\n")..];
    assert!(
        report.contains("\"awaiting your design\napproval\""),
        "{report}"
    );
    assert!(report.contains("`jkb query --global --json 'kind:task phase:design_review <SCOPE>'`"));
    assert!(report.contains("marking those\nthis run's designers wrote"));
    // A redesign after a rejection supersedes the rejected one, and the implementer reads the last.
    assert!(des.contains("it replaces every earlier Design section above"));
    assert!(get("swarm-implementer")
        .def
        .template
        .contains("the LAST governs"));
    // The coordinator writes no design: the NEVER rule it is not held to by jkb.
    assert!(coord.contains("- Edit code or a task's design"));
}

/// The frontier is read by phase, approved work first: a broad scope's few approved tasks sorted past
/// `--limit 100` behind hundreds at `design`, so a gated run admitted nothing (review round 4). Every
/// read leaves out what the run settled and every review finding by tag, none offers
/// `design_review`, and the design read is limited to the designers the budget has left. A design
/// the operator rejects mid-run has a read of its own, at any budget, which B and C leave out: read
/// C alone would not offer it at 0, and at 1 could offer a new task first. Each read's terms are
/// ones jkb's own parser accepts.
#[test]
fn the_coordinator_reads_the_frontier_by_phase() {
    let agents = packaged().unwrap();
    let coord = &agents
        .iter()
        .find(|a| a.name == "swarm-coordinator")
        .unwrap()
        .def
        .template;
    let at = |s: &str| coord.find(s).unwrap_or_else(|| panic!("{s}"));
    let step = &coord[at("1. READ THE FRONTIER, BY PHASE")..at("1b. ROUTE BY PHASE")];
    let every = "-tag:swarm-settled={{owner}} -tag:finding=review -tag:swarm-landing=unrecorded";
    assert!(step.contains(&format!("\n  {every}\n")), "{step}");
    let reads = [
        (
            "  A, build      ",
            "phase:implement,review,landable,systemic_review",
            "--limit 100",
        ),
        (
            "  R, re-design  ",
            "phase:design tag:swarm-designed={{owner}}",
            "--limit 100",
        ),
        (
            "  B, legacy     ",
            "phase:design tag:design=approved -tag:swarm-designed={{owner}}",
            "--limit 100",
        ),
        (
            "  C, design     ",
            "phase:design -tag:design=approved -tag:swarm-designed={{owner}}",
            "--limit <DESIGNERS LEFT>",
        ),
    ];
    let mut last = 0;
    for (label, terms, limit) in reads {
        let line = step
            .lines()
            .find(|l| l.starts_with(label))
            .unwrap_or_else(|| panic!("read {label}"));
        assert!(line.contains(terms) && line.ends_with(limit), "{line}");
        assert!(step.find(label).unwrap() > last, "reads in order: {label}");
        last = step.find(label).unwrap();
        // jkb parses the read as the coordinator will send it.
        let q = crate::query::parse(&format!("{terms} {every}").replace("{{owner}}", "box:1"))
            .unwrap_or_else(|e| panic!("{terms}: {e}"));
        // R and the two it is carved out of never offer the same task.
        let designed =
            |t: &crate::query::TagPred| t.facet == "swarm-designed" && t.value == "box:1";
        if label.starts_with("  R") {
            assert!(q.tags.iter().any(designed), "{terms}");
        } else if label.starts_with("  B") || label.starts_with("  C") {
            assert!(q.exclude_tags.iter().any(designed), "{terms}");
        }
        assert!(
            !q.phases.iter().any(|p| p == "design_review"),
            "no read offers a design awaiting the operator: {terms}"
        );
        for facet in ["swarm-settled", "finding", "swarm-landing"] {
            assert!(
                q.exclude_tags.iter().any(|t| t.facet == facet),
                "{terms}: -tag:{facet}"
            );
        }
    }
    assert!(step.contains("Read A first, so approved work is never hidden"));
    assert!(step.contains("Read R next, at\nany budget"));
    assert!(step.contains("Read C only while DESIGNERS LEFT is above 0."));
    assert!(coord.contains(
        "Every task of\n                                read R is one: it goes to a designer"
    ));
    // The budget: a placeholder, counted by a tag so it survives a compaction, and its refusal the
    // only settling of a task the run did not build.
    assert!(
        coord.contains("  designer budget       {{designers}} tasks designed in this whole run")
    );
    assert!(coord.contains("'kind:task tag:swarm-designed={{owner}}'"));
    let routing = &coord[at("1b. ROUTE BY PHASE")..at("\nDESIGNING A TASK, one designer per task")];
    assert!(routing.contains("THE BUDGET REFUSES a designer when DESIGNERS LEFT is 0"));
    assert!(routing.contains("tag it SETTLED, or that read offers it again"));
    assert!(routing.contains("From read C\nwith the gate on, leave it untagged"));
    // Nothing else is settled without being built: a held-back design is out of every read already.
    assert!(!coord.contains("Tag every task you leave out SETTLED"));
    let report = &coord[at("\nREPORT\n")..];
    assert!(report.contains("of its budget of {{designers}}"));
    assert!(report.contains("a rejected design is not left for want\nof budget"));
    assert!(report.contains("in uid mode each task you named that no read took"));
    assert!(report.contains("<every read's terms> <read\nC's terms>"));
    assert!(report.contains("left undesigned\nfor want of budget"));
    // A paused group is still known by its claim and its lead's phase, and resumed before a read.
    let paused = &coord[at("RESUMING A PAUSED GROUP")..at("- systemic_review (nothing moved)")];
    assert!(paused.contains("before every frontier read"));
    assert!(paused.contains("a lead claimed by '{{owner}}' at `design_review`"));
}

/// A task whose review keeps failing in one area goes to a systemic reviewer, never round the loop
/// again: the coordinator's step 4b starts a `swarm-systemic-reviewer` as a `jkb-systemic-reviewer`,
/// routes on the lead's phase afterwards, and on a redesign pauses the group, keeping its claim and
/// implementer, for a designer and the operator. `jkb-api`'s swarm-loop tests make the same calls as
/// the same principals.
#[test]
fn the_swarm_sends_a_repeated_area_to_a_systemic_reviewer() {
    let agents = packaged().unwrap();
    let get = |n: &str| agents.iter().find(|a| a.name == n).unwrap();
    let sys = &get("swarm-systemic-reviewer").def.template;
    let at_in = |t: &str, s: &str| t.find(s).unwrap_or_else(|| panic!("{s}"));
    // Bound first, on the branch, the cause recorded in the task, then exactly one exit with a
    // reason; the result and cause lines the coordinator reads.
    for call in [
        "jkb role bind {{task_uid}}",
        "git switch {{branch}}",
        "jkb task edit {{task_uid}} --append --stdin",
        "Systemic cause: ",
        "jkb workflow fire {{task_uid}} submit_systemic --reason",
        "jkb workflow fire {{task_uid}} systemic_redesign --reason",
        "RESULT: submit_systemic {{task_uid}}",
        "RESULT: systemic_redesign {{task_uid}}",
        "RESULT: failed <reason>",
        "CAUSE: ",
    ] {
        assert!(sys.contains(call), "swarm-systemic-reviewer: {call}");
    }
    assert!(at_in(sys, "jkb role bind") < at_in(sys, "git switch"));
    assert!(at_in(sys, "jkb task edit") < at_in(sys, "submit_systemic --reason"));
    assert!(sys.contains("Fire exactly one exit"));

    let swarm = get("swarm-coordinator");
    assert!(swarm
        .def
        .hands_off_to
        .iter()
        .any(|h| h == "swarm-systemic-reviewer"));
    let coord = &swarm.def.template;
    let at = |s: &str| at_in(coord, s);
    // Step 4's systemic arm goes to step 4b rather than settling the group.
    let arm = &coord[at("- systemic_review: ")..at("- landable: the round")];
    assert!(arm.contains("go to step 4b"), "{arm}");
    assert!(!arm.contains("SETTLED"), "{arm}");
    let step = &coord[at("4b. A SYSTEMIC REVIEW")..at("5. THE MERGE QUEUE")];
    for rule in [
        "jkb workflow agent show swarm-systemic-reviewer",
        "subagent type `jkb-systemic-reviewer`",
        "worktree isolation",
        "jkb ls --global <namespace>/must-fix",
        "never on the RESULT word",
        // jkb holds the coordinator to it: neither exit is the coordinator's.
        "jkb gives you neither `submit_systemic` nor\n`systemic_redesign`",
        "- implement (submit_systemic): a code-pattern fix. An attempt used.",
        "send the CAUSE and the last NOTES to the same implementer",
        "- design (systemic_redesign): the fix changes the design, so the group PAUSES.",
        "claim nothing",
        "it is EXEMPT from the\n  designer budget, so tag nothing `swarm-designed=` and start it whatever DESIGNERS LEFT says",
        "neither reopen nor release",
        "the lead is at `design_review`, still claimed by '{{owner}}'",
        "RESUMING A PAUSED GROUP",
        "- systemic_review (nothing moved)",
    ] {
        assert!(step.contains(rule), "step 4b: {rule}");
    }
    // A paused group is known by its claim and its lead's phase, and resumed rather than waiting for
    // a frontier that never offers it (it is claimed).
    assert!(step.contains(
        "for each PAUSED\n  group's lead (a lead claimed by '{{owner}}' at `design_review`)"
    ));
    // A design turned down goes to a designer whatever the gate says, so a redesigned task is never
    // built on its superseded design by a later run.
    let design_row =
        &coord[at("  design                        a LEGACY task")..at("  systemic_review ")];
    assert!(
        design_row.contains("`reject_design` or a\n                                `systemic_redesign`, whatever the gate says"),
        "{design_row}"
    );
    // A task an earlier run left at `systemic_review` is not left out (every later run would settle
    // it, and the stall would persist): step 1b takes it, as a group of one, straight to step 4b,
    // whose exits start an implementer on its branch since it has none of this run's.
    let row =
        &coord[at("  systemic_review               a SYSTEMIC task")..at("THE BUDGET REFUSES")];
    for rule in [
        "group of one",
        "start no implementer",
        "go straight to step 4b",
    ] {
        assert!(row.contains(rule), "step 1b's systemic row: {rule}");
    }
    // ...and nothing gives a paused group up for want of budget.
    assert!(!step.contains("redesign left for want of budget"), "{step}");
    assert!(coord.contains("a PAUSED group's redesign is exempt\n  (step 4b)"));
    for rule in [
        "NO IMPLEMENTER OF YOURS: a SYSTEMIC group never had one in this run",
        "the branch the lead's `branch=` tag names",
    ] {
        assert!(step.contains(rule), "step 4b: {rule}");
    }
    assert!(at("NO IMPLEMENTER OF YOURS") < at("- implement (submit_systemic)"));
    // A paused group waits on the operator: it does not hold the run open, and after a compaction
    // its lead's phase or tag names it.
    let continuing = &coord[at("\nCONTINUING\n")..at("\nREPORT\n")];
    assert!(continuing.contains("A PAUSED group is not in\nflight"));
    assert!(continuing.contains("A lead at `systemic_review` has a systemic reviewer at\nwork"));
    assert!(continuing.contains("A lead at `design_review` is a PAUSED group's lead."));
    let report = &coord[at("\nREPORT\n")..];
    assert!(report.contains("each systemic review (its exit and its CAUSE)"));
    assert!(report.contains("a PAUSED group's lead as a systemic redesign"));
    // A revised design is true of a redesign as well as a rejection: it does not claim the earlier
    // design was never approved.
    let des = &get("swarm-designer").def.template;
    assert!(!des.contains("which was not approved"), "{des}");
    assert!(!get("swarm-implementer")
        .def
        .template
        .contains("an earlier one\n   was rejected"));
}

/// A designer's failure outlives a report a compaction loses: `swarm-designer` tags its task
/// `design-failed=<run owner>` before it reports failed, the coordinator passes the owner, and the
/// compaction rebuild reads the tag as a failed designer instead of waiting for a report that already
/// came. `jkb-api`'s swarm tests make the designer's tag call as the designer.
#[test]
fn a_failed_designer_leaves_a_tag_the_rebuild_reads() {
    let agents = packaged().unwrap();
    let get = |n: &str| agents.iter().find(|a| a.name == n).unwrap();
    let des = &get("swarm-designer").def.template;
    let tag = des
        .find("`jkb task tag set {{task_uid}}\ndesign-failed={{owner}}`")
        .expect("the designer tags its failure");
    assert!(tag > des.find("jkb role bind {{task_uid}}").unwrap());
    assert!(tag < des.find("RESULT: failed <reason>").unwrap());
    let coord = &get("swarm-coordinator").def.template;
    assert!(coord.contains("--var owner={{owner}} --var feedback_block="));
    let continuing =
        &coord[coord.find("\nCONTINUING\n").unwrap()..coord.find("\nREPORT\n").unwrap()];
    assert!(
        continuing.contains(
            "Still at `design`\nand tagged `design-failed={{owner}}`, its designer failed."
        ),
        "{continuing}"
    );
    assert!(continuing.contains("unless the lead is tagged `design-failed={{owner}}`"));
}

/// CLOSING FINDINGS writes tasks outside SCOPE, so the NEVER rule that keeps the coordinator inside
/// SCOPE must carve out exactly the findings CLOSING FINDINGS closes — or a coordinator that keeps
/// both rules closes nothing. The two name the same set: the must-fixes under the lead's recorded
/// rounds.
#[test]
fn the_scope_rule_carves_out_exactly_the_findings_the_coordinator_closes() {
    let agents = packaged().unwrap();
    let coord = &agents
        .iter()
        .find(|a| a.name == "swarm-coordinator")
        .unwrap()
        .def
        .template;
    let never = &coord[coord.find("\nNEVER\n").unwrap()..];
    let scope_rule = never
        .lines()
        .skip_while(|l| !l.starts_with("- Touch a task outside SCOPE"))
        .take_while(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        scope_rule.contains("the group's own findings, which CLOSING FINDINGS"),
        "{scope_rule}"
    );
    for set in [
        "must-fixes",
        "rounds recorded against the group's lead",
        "`round=`",
    ] {
        assert!(
            scope_rule.contains(set),
            "the scope rule names {set}: {scope_rule}"
        );
    }
    assert!(scope_rule.contains("never another group's"), "{scope_rule}");
    let closing = &coord[coord.find("CLOSING FINDINGS: ").unwrap()..];
    let closing = &closing[..closing.find("\n\n").unwrap()];
    for set in ["must-fix", "`round=<namespace>`", "jkb task show <lead"] {
        assert!(
            closing.contains(set),
            "CLOSING FINDINGS names {set}: {closing}"
        );
    }
    // ...and the done rule carves out the same.
    assert!(never.contains("on any task but a finding you close as CLOSING FINDINGS says"));
}

/// `/task-swarm` reads the frontier by phase, as the coordinator does: the scout makes the same
/// three reads, `scope_block` says how to make one and whether the gate is on, the designer budget
/// is a flag passed to the template, and nothing parks a task by tag any more.
#[test]
fn the_slash_command_reads_the_frontier_by_phase_with_a_designer_budget() {
    let md = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.claude/commands/task-swarm.md"),
    )
    .unwrap();
    assert!(
        !md.contains("<GATE>"),
        "a frontier query still gates on the tag"
    );
    assert!(md.contains(
        "Make each read with `jkb task next --global --json\n  '<SCOPE> <the read's terms>' --limit <the read's limit>`"
    ));
    for read in [
        "'<SCOPE> -tag:finding=review -tag:swarm-landing=unrecorded phase:implement,review,landable,systemic_review' --limit 100",
        "'<SCOPE> -tag:finding=review -tag:swarm-landing=unrecorded phase:design tag:design=approved' --limit 100",
        "'kind:task is:ready <SCOPE> -tag:finding=review -tag:swarm-landing=unrecorded phase:design -tag:design=approved'",
        "'kind:task <SCOPE> phase:design_review'",
    ] {
        assert!(md.contains(read), "the scout's read: {read}");
    }
    assert!(md.contains("The design gate is ON"));
    assert!(md.contains("The design gate is OFF"));
    assert!(md.contains("no run builds a finding"));
    assert!(md.contains("tagged `finding=review`, which `jkb task review file` sets"));
    // A finding named by uid is refused and said to be one (review round 4's nit), not previewed as
    // legacy and then dropped.
    assert!(md.contains("a finding named by uid is refused and reported as one"));
    assert!(md.contains("leave it and report it as a finding\n  you named"));
    // The budget: parsed, previewed, and passed to the template.
    assert!(md.contains("[--designers <n>]"));
    assert!(md.contains("- `--designers <n>` → `DESIGNERS=<n>`"));
    assert!(md.contains("Default 3; `0` designs nothing."));
    assert!(md.contains("A paused group's redesign (below) is\n  exempt"));
    assert!(md.contains("(tagged `swarm-designed=<owner>`; a re-design uses no\n  budget, so this read is never limited by it)"));
    assert!(md.contains("(always\n  started, whatever the designer budget has left)"));
    assert!(md.contains("--var designers=\"${DESIGNERS:-3}\""));
    assert!(md.contains(
        "the designer budget (`DESIGNERS`), how many tasks at `design` the gate holds back"
    ));
    // No parking by tag, no re-check.
    for gone in [
        "swarm-parked",
        "parked",
        "re-checks",
        "DSL cannot test a workflow phase",
    ] {
        assert!(!md.contains(gone), "{gone}");
    }
    // The step it names is the coordinator's routing step, which starts the designers.
    assert!(md.contains("step 1b (ROUTE BY PHASE)"));
    assert!(md.contains("`jkb-designer`"));
    assert!(md.contains("awaiting your design approval"));
    // The systemic reviewer, and what its two exits do to the group.
    assert!(md.contains("`jkb-systemic-reviewer`"));
    assert!(md.contains("`systemic_review`\ngoes to a systemic reviewer"));
    assert!(!md.contains("`systemic_review` stops the group"));
    // A turned-down design goes to a designer on every path, and the slash command says so wherever
    // it names the legacy path for a hand-pick or an ungated run.
    assert!(md.contains("Neither the tag, a hand-pick nor\n`--no-design-gate` builds a task whose design was turned down"));
    assert!(md.contains("legacy path, unless its design was turned down.\""));
    assert!(md.contains("on the legacy path, unless its\n  design was turned down.\""));
    assert!(md.contains("go on the legacy path unless their design was\n  turned down"));
}

/// The workers' subagent types: rendered from the packaged file, prefixed like everything the
/// installer writes, and holding nothing a coordinator would have to fill.
#[test]
fn every_subagent_template_renders_as_an_agent_definition() {
    let defs = subagent_definitions().unwrap();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "jkb-designer",
            "jkb-implementer",
            "jkb-reviewer",
            "jkb-systemic-reviewer"
        ]
    );
    for d in &defs {
        assert!(
            d.markdown
                .starts_with(&format!("---\nname: {}\ndescription: \"", d.name)),
            "{}",
            d.markdown
        );
        assert!(!d.markdown.contains("{{"), "{} holds a placeholder", d.name);
    }
    // A designer, a reviewer and a systemic reviewer read and run; no type can start a worker, which
    // would attest as its own role.
    let tools = |n: &str| {
        defs.iter()
            .find(|d| d.name == n)
            .unwrap()
            .markdown
            .lines()
            .find(|l| l.starts_with("tools:"))
            .map(str::to_owned)
    };
    for kb in ["jkb-designer", "jkb-reviewer", "jkb-systemic-reviewer"] {
        assert_eq!(
            tools(kb).as_deref(),
            Some("tools: Read, Grep, Glob, Bash"),
            "{kb}"
        );
    }
    let imp = tools("jkb-implementer").unwrap();
    assert!(imp.contains("Edit"), "{imp}");
    for d in &defs {
        let t = tools(&d.name).unwrap();
        assert!(!t.contains("Agent"), "{} may start workers: {t}", d.name);
    }
}

/// Every placeholder a coordinator template tells it to pass is one the target template has, and
/// every placeholder the target has is passed: `show --var` refuses either mismatch, so a drift
/// here is a coordinator that cannot start its workers.
#[test]
fn every_show_a_coordinator_template_spells_matches_its_target() {
    let agents = packaged().unwrap();
    for a in agents {
        let t = &a.def.template;
        let mut from = 0;
        while let Some(at) = t[from..].find("jkb workflow agent show ") {
            let start = from + at + "jkb workflow agent show ".len();
            let target: String = t[start..]
                .chars()
                .take_while(|c| c.is_ascii_lowercase() || *c == '-')
                .collect();
            // The command runs to the first line that does not continue it with `\`.
            let mut end = start;
            for line in t[start..].split_inclusive('\n') {
                end += line.len();
                if !line.trim_end().ends_with('\\') {
                    break;
                }
            }
            let cmd = &t[start..end];
            let passed: std::collections::BTreeSet<String> = cmd
                .split("--var ")
                .skip(1)
                .map(|v| v.split('=').next().unwrap().trim().to_owned())
                .collect();
            let target_def = agents
                .iter()
                .find(|b| b.name == target)
                .unwrap_or_else(|| panic!("{} shows unknown `{target}`", a.name));
            let wanted: std::collections::BTreeSet<String> = placeholders(&target_def.def.template)
                .unwrap()
                .into_iter()
                .collect();
            assert_eq!(passed, wanted, "{} fills `{target}`", a.name);
            from = end;
        }
    }
}

#[test]
fn placeholders_are_found_and_malformed_ones_refused() {
    assert_eq!(
        placeholders("a {{x}} b {{y_2}} {{x}}").unwrap(),
        vec!["x".to_owned(), "y_2".to_owned()]
    );
    assert!(placeholders("no braces } here }}").unwrap().is_empty());
    assert!(placeholders("open {{x").is_err());
    assert!(placeholders("{{Not}}").is_err());
    assert!(placeholders("{{a b}}").is_err());
    assert!(placeholders("{{}}").is_err());
}

#[test]
fn render_fills_every_placeholder_and_refuses_holes_and_strays() {
    let t = "Hi {{who}}, in {{repo}}; bye {{who}}.";
    assert_eq!(
        render(t, &vars(&[("who", "ann"), ("repo", "/r")])).unwrap(),
        "Hi ann, in /r; bye ann."
    );
    let missing = render(t, &vars(&[("who", "ann")])).unwrap_err().to_string();
    assert!(missing.contains("no value for repo"), "{missing}");
    let stray = render(t, &vars(&[("who", "a"), ("repo", "r"), ("x", "1")]))
        .unwrap_err()
        .to_string();
    assert!(stray.contains("no placeholder named x"), "{stray}");
    // A value is inserted as text: braces in it are not read as placeholders.
    assert_eq!(render("{{a}}", &vars(&[("a", "{{b}}")])).unwrap(), "{{b}}");
}

#[test]
fn names_are_checked() {
    assert!(validate_name("swarm-implementer").is_ok());
    for bad in ["", "Swarm", "1st", "a_b", "a b", &"x".repeat(65)] {
        assert!(validate_name(bad).is_err(), "{bad}");
    }
}

#[test]
fn a_packaged_template_is_read_only_and_a_copy_overrides_it() {
    let db = db();
    let refused = db
        .write_txn("test", |c, m| {
            set(
                c,
                m,
                "swarm-implementer",
                Edit {
                    describe: Some("mine".into()),
                    ..Edit::default()
                },
            )
        })
        .unwrap_err()
        .to_string();
    assert!(refused.contains("read-only"), "{refused}");

    let copied = db
        .write_txn("test", |c, m| {
            copy(c, m, "swarm-implementer", false, None, None)
        })
        .unwrap();
    assert_eq!(copied.source, Source::Operator);
    assert_eq!(copied.version, 1);
    let pv = packaged()
        .unwrap()
        .iter()
        .find(|a| a.name == "swarm-implementer")
        .unwrap()
        .version;
    assert_eq!(
        copied.based_on.as_deref(),
        Some(format!("packaged:swarm-implementer@{pv}").as_str())
    );

    let (edited, wrote) = db
        .write_txn("test", |c, m| {
            set(
                c,
                m,
                "swarm-implementer",
                Edit {
                    template: Some("Build {{what}}.".into()),
                    ..Edit::default()
                },
            )
        })
        .unwrap();
    assert!(wrote);
    assert_eq!(edited.version, 2);
    // An edit of a copy that was the packaged text records that packaged version as its base.
    assert_eq!(
        edited.based_on.as_deref(),
        Some(format!("packaged:swarm-implementer@{pv}").as_str())
    );

    // What the script reads by name is now the operator's text; the packaged one is still there.
    let effective = db
        .read(|c| resolve(c, "swarm-implementer", Pick::Effective))
        .unwrap();
    assert_eq!(effective.def.template, "Build {{what}}.");
    let pkg = db
        .read(|c| resolve(c, "swarm-implementer", Pick::Packaged))
        .unwrap();
    assert_eq!(pkg.source, Source::Packaged);
    assert!(pkg.def.template.starts_with("You are the IMPLEMENTER"));
    let v1 = db
        .read(|c| resolve(c, "swarm-implementer", Pick::Version(1)))
        .unwrap();
    assert_eq!(v1.def.template, pkg.def.template);

    let listing = db.read(list).unwrap();
    let row = listing
        .iter()
        .find(|l| l.agent.name == "swarm-implementer")
        .unwrap();
    assert!(row.overrides_packaged);
    assert!(!row.behind_packaged);
    assert_eq!(row.agent.version, 2);
    assert_eq!(row.packaged_version, Some(pv));

    // Back to the packaged text: a copy of it, appended.
    let back = db
        .write_txn("test", |c, m| {
            copy(c, m, "swarm-implementer", true, None, None)
        })
        .unwrap();
    assert_eq!(back.version, 3);
    assert_eq!(back.def, pkg.def);
    assert_eq!(
        db.read(|c| versions(c, "swarm-implementer")).unwrap().len(),
        3
    );
}

#[test]
fn an_edit_that_changes_nothing_writes_nothing() {
    let db = db();
    db.write_txn("test", |c, m| {
        copy(c, m, "swarm-coordinator", false, None, None)
    })
    .unwrap();
    let (same, wrote) = db
        .write_txn("test", |c, m| {
            set(
                c,
                m,
                "swarm-coordinator",
                Edit {
                    role: Some(Role::Coordinator),
                    ..Edit::default()
                },
            )
        })
        .unwrap();
    assert!(!wrote);
    assert_eq!(same.version, 1);
    let empty = db
        .write_txn("test", |c, m| {
            set(c, m, "swarm-coordinator", Edit::default())
        })
        .unwrap_err()
        .to_string();
    assert!(empty.contains("nothing to change"), "{empty}");
}

#[test]
fn a_copy_under_another_name_is_the_operators_own_but_never_another_packaged_name() {
    let db = db();
    let mine = db
        .write_txn("test", |c, m| {
            copy(c, m, "swarm-reviewer", false, Some("strict-reviewer"), None)
        })
        .unwrap();
    assert_eq!(mine.name, "strict-reviewer");
    let listing = db.read(list).unwrap();
    let row = listing
        .iter()
        .find(|l| l.agent.name == "strict-reviewer")
        .unwrap();
    assert_eq!(row.packaged_version, None);
    assert!(!row.overrides_packaged);
    // Packaged ones first, in the file's order; the operator's own after.
    assert_eq!(listing.last().unwrap().agent.name, "strict-reviewer");

    let onto = db
        .write_txn("test", |c, m| {
            copy(
                c,
                m,
                "swarm-reviewer",
                false,
                Some("swarm-implementer"),
                None,
            )
        })
        .unwrap_err()
        .to_string();
    assert!(onto.contains("is a packaged template"), "{onto}");
    let none = db
        .write_txn("test", |c, m| copy(c, m, "nope", false, None, None))
        .unwrap_err()
        .to_string();
    assert!(none.contains("no agent template `nope`"), "{none}");
    let not_packaged = db
        .read(|c| resolve(c, "strict-reviewer", Pick::Packaged))
        .unwrap_err()
        .to_string();
    assert!(not_packaged.contains("no packaged"), "{not_packaged}");
}

#[test]
fn an_invalid_edit_is_refused_and_writes_nothing() {
    let db = db();
    db.write_txn("test", |c, m| {
        copy(c, m, "swarm-reviewer", false, None, None)
    })
    .unwrap();
    for edit in [
        Edit {
            template: Some("open {{brace".into()),
            ..Edit::default()
        },
        Edit {
            template: Some("  ".into()),
            ..Edit::default()
        },
        Edit {
            hands_off_to: Some(vec!["a".into(), "a".into()]),
            ..Edit::default()
        },
        Edit {
            permissions: Some(AgentPermissions {
                isolation: Isolation::None,
                model: Some("bad model".into()),
                writes: Writes::Kb,
            }),
            ..Edit::default()
        },
    ] {
        assert!(db
            .write_txn("test", move |c, m| set(c, m, "swarm-reviewer", edit))
            .is_err());
    }
    assert_eq!(db.read(|c| versions(c, "swarm-reviewer")).unwrap().len(), 1);
}

#[test]
fn a_copy_taken_from_an_older_packaged_version_is_behind() {
    let db = db();
    let pv = packaged()
        .unwrap()
        .iter()
        .find(|a| a.name == "review-area")
        .unwrap()
        .version;
    db.write_txn("test", |c, m| {
        copy(c, m, "review-area", false, None, Some(mine()))
    })
    .unwrap();
    // As if this copy had been taken before the packaged text moved on.
    db.write_txn("test", move |c, _| {
        c.execute(
            "UPDATE workflow_agents SET based_on = ?1 WHERE name = 'review-area'",
            [format!("packaged:review-area@{}", pv - 1)],
        )?;
        Ok(())
    })
    .unwrap();
    let listing = db.read(list).unwrap();
    assert!(
        listing
            .iter()
            .find(|l| l.agent.name == "review-area")
            .unwrap()
            .behind_packaged
    );
}

#[test]
fn a_stored_permission_this_jkb_does_not_know_is_refused_not_dropped() {
    let db = db();
    db.write_txn("test", |c, m| {
        copy(c, m, "swarm-reviewer", false, None, None)
    })
    .unwrap();
    db.write_txn("test", |c, _| {
        c.execute(
            "UPDATE workflow_agents SET permissions = \
             '{\"isolation\":\"none\",\"writes\":\"kb\",\"network\":false}'",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let err = db
        .read(|c| resolve(c, "swarm-reviewer", Pick::Effective))
        .unwrap_err()
        .to_string();
    assert!(err.contains("newer jkb"), "{err}");
}

#[test]
fn export_replaces_an_entry_with_the_next_version_or_appends_a_new_one() {
    let db = db();
    db.write_txn("test", |c, m| {
        copy(c, m, "swarm-reviewer", false, None, None)
    })
    .unwrap();
    let before = packaged()
        .unwrap()
        .iter()
        .find(|a| a.name == "swarm-reviewer")
        .unwrap()
        .version;
    let base = db.read(|c| packaged_base(c, "swarm-reviewer")).unwrap();
    assert_eq!(base, Some(before));
    let unchanged = db
        .read(|c| resolve(c, "swarm-reviewer", Pick::Effective))
        .unwrap();
    let base = installed_at(base);
    let same = export(PACKAGED_JSON, &unchanged, base, false)
        .unwrap_err()
        .to_string();
    assert!(same.contains("nothing to contribute"), "{same}");

    let (edited, _) = db
        .write_txn("test", |c, m| {
            set(
                c,
                m,
                "swarm-reviewer",
                Edit {
                    template: Some("Set {{status}}.\nThen stop.".into()),
                    ..Edit::default()
                },
            )
        })
        .unwrap();
    // An edit follows the version before it: still built on the same packaged version.
    assert_eq!(
        db.read(|c| packaged_base(c, "swarm-reviewer")).unwrap(),
        Some(before)
    );
    let (text, version) = export(PACKAGED_JSON, &edited, base, false).unwrap();
    assert_eq!(version, before + 1);
    let file = parse_file(&text).unwrap();
    let entry = file
        .agents
        .iter()
        .find(|e| e.name == "swarm-reviewer")
        .unwrap();
    assert_eq!(entry.template, vec!["Set {{status}}.", "Then stop."]);
    assert_eq!(entry.version, before + 1);
    // Only that entry moved: every other one is byte-for-byte what it was, at the same position.
    let original = parse_file(PACKAGED_JSON).unwrap();
    assert_eq!(file.agents.len(), original.agents.len());
    for (i, (now, was)) in file.agents.iter().zip(&original.agents).enumerate() {
        assert_eq!(now.name, was.name, "entry {i} moved");
        if now.name != "swarm-reviewer" {
            assert_eq!(
                serde_json::to_string(now).unwrap(),
                serde_json::to_string(was).unwrap(),
                "entry {i} ({}) changed",
                now.name
            );
        }
    }

    let mine = db
        .write_txn("test", |c, m| {
            copy(c, m, "swarm-reviewer", false, Some("my-status"), None)
        })
        .unwrap();
    // Built on no packaged `my-status`, and the file has none: appended as v1.
    assert_eq!(db.read(|c| packaged_base(c, "my-status")).unwrap(), None);
    let (text, version) = export(PACKAGED_JSON, &mine, ExportBase::default(), false).unwrap();
    assert_eq!(version, 1);
    let file = parse_file(&text).unwrap();
    assert_eq!(file.agents.last().unwrap().name, "my-status");
}

#[test]
fn export_refuses_a_copy_built_on_another_version_than_the_file_holds() {
    let db = db();
    db.write_txn("test", |c, m| {
        copy(
            c,
            m,
            "swarm-reviewer",
            false,
            None,
            Some(Edit {
                describe: Some("one line changed".into()),
                ..Edit::default()
            }),
        )
    })
    .unwrap();
    let copy_of = db
        .read(|c| resolve(c, "swarm-reviewer", Pick::Effective))
        .unwrap();
    let built_on = db.read(|c| packaged_base(c, "swarm-reviewer")).unwrap();
    let pv = built_on.unwrap();
    let base = installed_at(built_on);
    // Upstream has moved on: the target file holds the next version of the entry.
    let mut upstream = parse_file(PACKAGED_JSON).unwrap();
    let e = upstream
        .agents
        .iter_mut()
        .find(|e| e.name == "swarm-reviewer")
        .unwrap();
    e.version = pv + 1;
    e.template.push("A line added upstream.".into());
    let upstream = write_file(&upstream).unwrap();

    let refused = export(&upstream, &copy_of, base, false)
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains(&format!("built on packaged v{pv}"))
            && refused.contains(&format!("the file packages v{}", pv + 1)),
        "{refused}"
    );
    // It is this jkb that is behind the file, so re-copying would loop: it says to update jkb.
    assert!(refused.contains("update jkb"), "{refused}");
    // Where only the copy is behind (this jkb packages the file's version), re-copying is the cure.
    let behind_only = export(
        &upstream,
        &copy_of,
        ExportBase {
            built_on: Some(pv),
            installed: Some(pv + 1),
        },
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(
        behind_only.contains("copy swarm-reviewer --packaged")
            && !behind_only.contains("update jkb"),
        "{behind_only}"
    );
    // A deliberate revert is allowed, and writes the version after the file's.
    let (_, version) = export(&upstream, &copy_of, base, true).unwrap();
    assert_eq!(version, pv + 2);

    // With no copy at all, the installed packaged template is the base: a file it is not is refused.
    let installed = db
        .read(|c| resolve(c, "review-area", Pick::Packaged))
        .unwrap();
    let mut other = parse_file(PACKAGED_JSON).unwrap();
    other
        .agents
        .iter_mut()
        .find(|e| e.name == "review-area")
        .unwrap()
        .version = installed.version + 1;
    let refused = export(
        &write_file(&other).unwrap(),
        &installed,
        installed_at(Some(installed.version)),
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(
        refused.contains("the file packages v") && refused.contains("update jkb"),
        "{refused}"
    );
    // A name the file has, against a template built on none of it.
    let refused = export(
        PACKAGED_JSON,
        &copy_of,
        ExportBase {
            built_on: None,
            installed: Some(pv),
        },
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(
        refused.contains("built on packaged no version"),
        "{refused}"
    );
}

#[test]
fn a_copy_with_a_refused_edit_leaves_no_copy_and_one_with_an_edit_is_one_version() {
    let db = db();
    let refused = db
        .write_txn("test", |c, m| {
            copy(
                c,
                m,
                "swarm-implementer",
                false,
                None,
                Some(Edit {
                    template: Some("{{ repo }}".into()),
                    ..Edit::default()
                }),
            )
        })
        .unwrap_err()
        .to_string();
    assert!(refused.contains("not a placeholder"), "{refused}");
    assert!(db
        .read(|c| versions(c, "swarm-implementer"))
        .unwrap()
        .is_empty());
    let effective = db
        .read(|c| resolve(c, "swarm-implementer", Pick::Effective))
        .unwrap();
    assert_eq!(effective.source, Source::Packaged, "nothing overrides it");

    let made = db
        .write_txn("test", |c, m| {
            copy(
                c,
                m,
                "swarm-implementer",
                false,
                None,
                Some(Edit {
                    template: Some("Build {{what}}.".into()),
                    ..Edit::default()
                }),
            )
        })
        .unwrap();
    assert_eq!(made.version, 1);
    assert_eq!(made.def.template, "Build {{what}}.");
    assert!(made
        .based_on
        .as_deref()
        .is_some_and(|b| b.starts_with("packaged:swarm-implementer@")));
}

#[test]
fn a_recopy_of_a_copy_keeps_its_packaged_base() {
    let db = db();
    let pv = packaged()
        .unwrap()
        .iter()
        .find(|a| a.name == "review-area")
        .unwrap()
        .version;
    db.write_txn("test", |c, m| {
        copy(c, m, "review-area", false, None, Some(mine()))
    })
    .unwrap();
    db.write_txn("test", move |c, _| {
        c.execute(
            "UPDATE workflow_agents SET based_on = ?1 WHERE name = 'review-area'",
            [format!("packaged:review-area@{}", pv - 1)],
        )?;
        Ok(())
    })
    .unwrap();
    // Copying the copy onto its own name records `review-area@1`; the text is still built on the
    // older packaged version, and the listing still says so.
    let again = db
        .write_txn("test", |c, m| copy(c, m, "review-area", false, None, None))
        .unwrap();
    assert_eq!(again.based_on.as_deref(), Some("review-area@1"));
    // ...and through another name and back.
    db.write_txn("test", |c, m| {
        copy(c, m, "review-area", false, Some("my-claim"), None)
    })
    .unwrap();
    assert_eq!(db.read(|c| packaged_base(c, "my-claim")).unwrap(), None);
    let row = |db: &crate::Db| {
        db.read(list)
            .unwrap()
            .into_iter()
            .find(|l| l.agent.name == "review-area")
            .unwrap()
    };
    let listed = row(&db);
    assert!(listed.behind_packaged);
    assert_eq!(listed.packaged_base, Some(pv - 1));
    // Back to the packaged text: current again, and nothing in it to contribute.
    db.write_txn("test", |c, m| copy(c, m, "review-area", true, None, None))
        .unwrap();
    let listed = row(&db);
    assert!(!listed.behind_packaged);
    assert_eq!(listed.packaged_base, Some(pv));
    assert!(listed.matches_packaged);
}

#[test]
fn a_placeholder_refusal_states_the_leading_character_rule() {
    let why = placeholders("{{1st_task}}").unwrap_err().to_string();
    assert!(why.contains("starting with a letter or `_`"), "{why}");
}

#[test]
fn a_copy_that_a_merged_contribution_made_current_is_built_on_the_installed_version() {
    let db = db();
    let pv = packaged()
        .unwrap()
        .iter()
        .find(|a| a.name == "swarm-reviewer")
        .unwrap()
        .version;
    db.write_txn("test", |c, m| {
        copy(c, m, "swarm-reviewer", false, None, None)
    })
    .unwrap();
    // As if the copy had been taken from the version before, edited, contributed, merged as the
    // version this jkb packages, and installed: its text is the packaged text, its chain is old.
    db.write_txn("test", move |c, _| {
        c.execute(
            "UPDATE workflow_agents SET based_on = ?1 WHERE name = 'swarm-reviewer'",
            [format!("packaged:swarm-reviewer@{}", pv - 1)],
        )?;
        Ok(())
    })
    .unwrap();
    let row = |db: &Db| {
        db.read(list)
            .unwrap()
            .into_iter()
            .find(|l| l.agent.name == "swarm-reviewer")
            .unwrap()
    };
    let listed = row(&db);
    assert!(!listed.behind_packaged, "current, not behind");
    assert_eq!(listed.packaged_base, Some(pv));
    // The next edit is built on the installed version, and exports against a file holding it.
    let (edited, _) = db
        .write_txn("test", |c, m| {
            set(
                c,
                m,
                "swarm-reviewer",
                Edit {
                    describe: Some("the next edit".into()),
                    ..Edit::default()
                },
            )
        })
        .unwrap();
    let listed = row(&db);
    assert_eq!(listed.packaged_base, Some(pv));
    assert!(!listed.behind_packaged);
    let (_, version) = export(
        PACKAGED_JSON,
        &edited,
        ExportBase {
            built_on: listed.packaged_base,
            installed: listed.packaged_version,
        },
        false,
    )
    .unwrap();
    assert_eq!(version, pv + 1);
    // ...and a re-copy of that copy onto its own name keeps it too.
    db.write_txn("test", |c, m| {
        copy(c, m, "swarm-reviewer", true, None, None)
    })
    .unwrap();
    db.write_txn("test", move |c, _| {
        c.execute(
            "UPDATE workflow_agents SET based_on = ?1 WHERE name = 'swarm-reviewer'",
            [format!("packaged:swarm-reviewer@{}", pv - 1)],
        )?;
        Ok(())
    })
    .unwrap();
    let again = db
        .write_txn("test", |c, m| {
            copy(c, m, "swarm-reviewer", false, None, None)
        })
        .unwrap();
    assert_eq!(
        again.based_on.as_deref(),
        Some(format!("packaged:swarm-reviewer@{pv}").as_str())
    );
}

/// A copy of a subagent type would be listed as in effect and never installed, so none is made:
/// not under its own name, and not under a new one. (An edit cannot move a template between
/// workflows, so `copy` is the only way in, and `append` refuses it there.)
#[test]
fn a_subagent_template_cannot_be_copied() {
    let db = db();
    for as_name in [None, Some("my-reviewer")] {
        let refused = db
            .write_txn("test", move |c, m| {
                copy(c, m, "jkb-reviewer", false, as_name, None)
            })
            .unwrap_err()
            .to_string();
        assert!(refused.contains("never run"), "{refused}");
    }
    assert!(db.read(|c| versions(c, "jkb-reviewer")).unwrap().is_empty());
    assert!(db.read(|c| versions(c, "my-reviewer")).unwrap().is_empty());
}

/// The role a subagent type's calls act as is mapped by `setup.sh`, apart from this file: a type
/// added here with no line there would attest as no role at all, and a line naming another role
/// would attest as the wrong one. Read from the script, so the two cannot drift.
#[test]
fn setup_maps_every_subagent_type_to_its_templates_role() {
    let setup = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/setup.sh"),
    )
    .unwrap();
    let defs = packaged().unwrap();
    let subagents: Vec<_> = defs
        .iter()
        .filter(|a| a.def.workflow == super::SUBAGENTS_WORKFLOW)
        .collect();
    assert!(!subagents.is_empty());
    // No loop maps a type to a role spelled by the same variable: that shape cannot be read here.
    assert!(
        !setup.contains(r#"role map "$t" "$t""#),
        "scripts/setup.sh maps types through a loop variable, which this test cannot read"
    );
    let lines: Vec<&str> = setup.lines().collect();
    let clear_at = lines
        .iter()
        .position(|l| l.contains(r#"role map "$t" --clear"#))
        .expect("scripts/setup.sh has no clear loop");
    let cleared: Vec<String> = lines[clear_at - 1]
        .trim()
        .strip_prefix("for t in ")
        .and_then(|l| l.strip_suffix("; do"))
        .expect("the clear loop's `for t in …; do` line")
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    for a in subagents {
        let line = format!("role map {} {} ", a.name, a.def.role.as_str());
        assert!(
            setup.contains(&line),
            "scripts/setup.sh does not map `{}` to `{}` (`{line}`)",
            a.name,
            a.def.role.as_str()
        );
        // ...and the bare type it replaces is cleared, not mapped: a repository's own `reviewer`
        // agent must not attest as the reviewer. Read from the clear loop's word list, because a
        // literal search misses a mapping spelled through a loop variable — which is how the bare
        // types were mapped (`for t in designer implementer reviewer; … role map "$t" "$t"`).
        let bare = a.name.trim_start_matches(super::SUBAGENT_PREFIX);
        assert!(
            !setup.contains(&format!("role map {bare} ")),
            "scripts/setup.sh still maps the bare `{bare}`"
        );
        assert!(
            cleared.iter().any(|w| w == bare),
            "scripts/setup.sh does not clear the bare `{bare}` (cleared: {cleared:?})"
        );
    }
}
