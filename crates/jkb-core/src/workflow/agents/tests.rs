use std::collections::{BTreeMap, BTreeSet};

use super::{
    copy, export, list, packaged, parse_file, placeholders, render, resolve, set, validate_name,
    versions, write_file, AgentPermissions, Edit, Isolation, Pick, Source, Writes, PACKAGED_JSON,
};
use crate::roles::Role;
use crate::Db;

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
    assert!(agents.len() >= 20, "the swarm's and the reviewer's agents");
    // Canonical form is what `export` writes, so a contribution's diff is only its own change.
    let file = parse_file(PACKAGED_JSON).unwrap();
    assert_eq!(write_file(&file).unwrap(), PACKAGED_JSON);
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
fn the_packaged_templates_are_the_scripts_prompts() {
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
    for p in ["repo", "task_list", "branch_step", "jkb", "db"] {
        assert!(names.contains(&p.to_owned()), "{p} in {names:?}");
    }
    // A mechanical step runs on the small model, as the script runs it.
    assert_eq!(
        get("swarm-merge-runner").def.permissions.model.as_deref(),
        Some("haiku")
    );
    // The shared reviewer contract is a fragment the finders include.
    assert!(get("review-preamble").def.fragment);
    assert!(get("review-lens-input")
        .def
        .template
        .starts_with("{{preamble}}\n\nYOUR QUESTION: **what values break this?**"));
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
        .write_txn("test", |c, m| copy(c, m, "swarm-implementer", false, None))
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
    assert_eq!(edited.based_on, None);

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
        .write_txn("test", |c, m| copy(c, m, "swarm-implementer", true, None))
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
    db.write_txn("test", |c, m| copy(c, m, "swarm-status", false, None))
        .unwrap();
    let (same, wrote) = db
        .write_txn("test", |c, m| {
            set(
                c,
                m,
                "swarm-status",
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
        .write_txn("test", |c, m| set(c, m, "swarm-status", Edit::default()))
        .unwrap_err()
        .to_string();
    assert!(empty.contains("nothing to change"), "{empty}");
}

#[test]
fn a_copy_under_another_name_is_the_operators_own_but_never_another_packaged_name() {
    let db = db();
    let mine = db
        .write_txn("test", |c, m| {
            copy(c, m, "swarm-reviewer", false, Some("strict-reviewer"))
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
            copy(c, m, "swarm-reviewer", false, Some("swarm-implementer"))
        })
        .unwrap_err()
        .to_string();
    assert!(onto.contains("is a packaged template"), "{onto}");
    let none = db
        .write_txn("test", |c, m| copy(c, m, "nope", false, None))
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
    db.write_txn("test", |c, m| copy(c, m, "swarm-status", false, None))
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
            .write_txn("test", move |c, m| set(c, m, "swarm-status", edit))
            .is_err());
    }
    assert_eq!(db.read(|c| versions(c, "swarm-status")).unwrap().len(), 1);
}

#[test]
fn a_copy_taken_from_an_older_packaged_version_is_behind() {
    let db = db();
    let pv = packaged()
        .unwrap()
        .iter()
        .find(|a| a.name == "swarm-claim")
        .unwrap()
        .version;
    db.write_txn("test", |c, m| copy(c, m, "swarm-claim", false, None))
        .unwrap();
    // As if this copy had been taken before the packaged text moved on.
    db.write_txn("test", move |c, _| {
        c.execute(
            "UPDATE workflow_agents SET based_on = ?1 WHERE name = 'swarm-claim'",
            [format!("packaged:swarm-claim@{}", pv - 1)],
        )?;
        Ok(())
    })
    .unwrap();
    let listing = db.read(list).unwrap();
    assert!(
        listing
            .iter()
            .find(|l| l.agent.name == "swarm-claim")
            .unwrap()
            .behind_packaged
    );
}

#[test]
fn a_stored_permission_this_jkb_does_not_know_is_refused_not_dropped() {
    let db = db();
    db.write_txn("test", |c, m| copy(c, m, "swarm-status", false, None))
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
        .read(|c| resolve(c, "swarm-status", Pick::Effective))
        .unwrap_err()
        .to_string();
    assert!(err.contains("newer jkb"), "{err}");
}

#[test]
fn export_replaces_an_entry_with_the_next_version_or_appends_a_new_one() {
    let db = db();
    db.write_txn("test", |c, m| copy(c, m, "swarm-status", false, None))
        .unwrap();
    let unchanged = db
        .read(|c| resolve(c, "swarm-status", Pick::Effective))
        .unwrap();
    let same = export(PACKAGED_JSON, &unchanged).unwrap_err().to_string();
    assert!(same.contains("nothing to contribute"), "{same}");

    let (edited, _) = db
        .write_txn("test", |c, m| {
            set(
                c,
                m,
                "swarm-status",
                Edit {
                    template: Some("Set {{status}}.\nThen stop.".into()),
                    ..Edit::default()
                },
            )
        })
        .unwrap();
    let before = packaged()
        .unwrap()
        .iter()
        .find(|a| a.name == "swarm-status")
        .unwrap()
        .version;
    let (text, version) = export(PACKAGED_JSON, &edited).unwrap();
    assert_eq!(version, before + 1);
    let file = parse_file(&text).unwrap();
    let entry = file
        .agents
        .iter()
        .find(|e| e.name == "swarm-status")
        .unwrap();
    assert_eq!(entry.template, vec!["Set {{status}}.", "Then stop."]);
    assert_eq!(entry.version, before + 1);
    // Only that entry moved: every other one is byte-for-byte where it was.
    let names = |f: &super::PackagedFile| -> BTreeSet<String> {
        f.agents.iter().map(|e| e.name.clone()).collect()
    };
    assert_eq!(names(&file), names(&parse_file(PACKAGED_JSON).unwrap()));

    let mine = db
        .write_txn("test", |c, m| {
            copy(c, m, "swarm-status", false, Some("my-status"))
        })
        .unwrap();
    let (text, version) = export(PACKAGED_JSON, &mine).unwrap();
    assert_eq!(version, 1);
    let file = parse_file(&text).unwrap();
    assert_eq!(file.agents.last().unwrap().name, "my-status");
}
