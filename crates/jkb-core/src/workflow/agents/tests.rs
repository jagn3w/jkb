use std::collections::BTreeMap;

use super::{
    copy, export, list, packaged, packaged_base, parse_file, placeholders, render, resolve, set,
    validate_name, versions, write_file, AgentPermissions, Edit, ExportBase, Isolation, Pick,
    Source, Writes, PACKAGED_JSON,
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
    db.write_txn("test", |c, m| copy(c, m, "swarm-status", false, None, None))
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
    db.write_txn("test", |c, m| copy(c, m, "swarm-status", false, None, None))
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
    db.write_txn("test", |c, m| {
        copy(c, m, "swarm-claim", false, None, Some(mine()))
    })
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
    db.write_txn("test", |c, m| copy(c, m, "swarm-status", false, None, None))
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
    db.write_txn("test", |c, m| copy(c, m, "swarm-status", false, None, None))
        .unwrap();
    let before = packaged()
        .unwrap()
        .iter()
        .find(|a| a.name == "swarm-status")
        .unwrap()
        .version;
    let base = db.read(|c| packaged_base(c, "swarm-status")).unwrap();
    assert_eq!(base, Some(before));
    let unchanged = db
        .read(|c| resolve(c, "swarm-status", Pick::Effective))
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
                "swarm-status",
                Edit {
                    template: Some("Set {{status}}.\nThen stop.".into()),
                    ..Edit::default()
                },
            )
        })
        .unwrap();
    // An edit follows the version before it: still built on the same packaged version.
    assert_eq!(
        db.read(|c| packaged_base(c, "swarm-status")).unwrap(),
        Some(before)
    );
    let (text, version) = export(PACKAGED_JSON, &edited, base, false).unwrap();
    assert_eq!(version, before + 1);
    let file = parse_file(&text).unwrap();
    let entry = file
        .agents
        .iter()
        .find(|e| e.name == "swarm-status")
        .unwrap();
    assert_eq!(entry.template, vec!["Set {{status}}.", "Then stop."]);
    assert_eq!(entry.version, before + 1);
    // Only that entry moved: every other one is byte-for-byte what it was, at the same position.
    let original = parse_file(PACKAGED_JSON).unwrap();
    assert_eq!(file.agents.len(), original.agents.len());
    for (i, (now, was)) in file.agents.iter().zip(&original.agents).enumerate() {
        assert_eq!(now.name, was.name, "entry {i} moved");
        if now.name != "swarm-status" {
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
            copy(c, m, "swarm-status", false, Some("my-status"), None)
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
            "swarm-status",
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
        .read(|c| resolve(c, "swarm-status", Pick::Effective))
        .unwrap();
    let built_on = db.read(|c| packaged_base(c, "swarm-status")).unwrap();
    let pv = built_on.unwrap();
    let base = installed_at(built_on);
    // Upstream has moved on: the target file holds the next version of the entry.
    let mut upstream = parse_file(PACKAGED_JSON).unwrap();
    let e = upstream
        .agents
        .iter_mut()
        .find(|e| e.name == "swarm-status")
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
        behind_only.contains("copy swarm-status --packaged") && !behind_only.contains("update jkb"),
        "{behind_only}"
    );
    // A deliberate revert is allowed, and writes the version after the file's.
    let (_, version) = export(&upstream, &copy_of, base, true).unwrap();
    assert_eq!(version, pv + 2);

    // With no copy at all, the installed packaged template is the base: a file it is not is refused.
    let installed = db
        .read(|c| resolve(c, "swarm-claim", Pick::Packaged))
        .unwrap();
    let mut other = parse_file(PACKAGED_JSON).unwrap();
    other
        .agents
        .iter_mut()
        .find(|e| e.name == "swarm-claim")
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
        .find(|a| a.name == "swarm-claim")
        .unwrap()
        .version;
    db.write_txn("test", |c, m| {
        copy(c, m, "swarm-claim", false, None, Some(mine()))
    })
    .unwrap();
    db.write_txn("test", move |c, _| {
        c.execute(
            "UPDATE workflow_agents SET based_on = ?1 WHERE name = 'swarm-claim'",
            [format!("packaged:swarm-claim@{}", pv - 1)],
        )?;
        Ok(())
    })
    .unwrap();
    // Copying the copy onto its own name records `swarm-claim@1`; the text is still built on the
    // older packaged version, and the listing still says so.
    let again = db
        .write_txn("test", |c, m| copy(c, m, "swarm-claim", false, None, None))
        .unwrap();
    assert_eq!(again.based_on.as_deref(), Some("swarm-claim@1"));
    // ...and through another name and back.
    db.write_txn("test", |c, m| {
        copy(c, m, "swarm-claim", false, Some("my-claim"), None)
    })
    .unwrap();
    assert_eq!(db.read(|c| packaged_base(c, "my-claim")).unwrap(), None);
    let row = |db: &crate::Db| {
        db.read(list)
            .unwrap()
            .into_iter()
            .find(|l| l.agent.name == "swarm-claim")
            .unwrap()
    };
    let listed = row(&db);
    assert!(listed.behind_packaged);
    assert_eq!(listed.packaged_base, Some(pv - 1));
    // Back to the packaged text: current again, and nothing in it to contribute.
    db.write_txn("test", |c, m| copy(c, m, "swarm-claim", true, None, None))
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
        .find(|a| a.name == "swarm-status")
        .unwrap()
        .version;
    db.write_txn("test", |c, m| copy(c, m, "swarm-status", false, None, None))
        .unwrap();
    // As if the copy had been taken from the version before, edited, contributed, merged as the
    // version this jkb packages, and installed: its text is the packaged text, its chain is old.
    db.write_txn("test", move |c, _| {
        c.execute(
            "UPDATE workflow_agents SET based_on = ?1 WHERE name = 'swarm-status'",
            [format!("packaged:swarm-status@{}", pv - 1)],
        )?;
        Ok(())
    })
    .unwrap();
    let row = |db: &Db| {
        db.read(list)
            .unwrap()
            .into_iter()
            .find(|l| l.agent.name == "swarm-status")
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
                "swarm-status",
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
    db.write_txn("test", |c, m| copy(c, m, "swarm-status", true, None, None))
        .unwrap();
    db.write_txn("test", move |c, _| {
        c.execute(
            "UPDATE workflow_agents SET based_on = ?1 WHERE name = 'swarm-status'",
            [format!("packaged:swarm-status@{}", pv - 1)],
        )?;
        Ok(())
    })
    .unwrap();
    let again = db
        .write_txn("test", |c, m| copy(c, m, "swarm-status", false, None, None))
        .unwrap();
    assert_eq!(
        again.based_on.as_deref(),
        Some(format!("packaged:swarm-status@{pv}").as_str())
    );
}
