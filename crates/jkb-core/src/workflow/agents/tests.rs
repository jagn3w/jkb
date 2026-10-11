use std::collections::BTreeMap;

use super::{
    copy, export, list, packaged, packaged_base, parse_file, placeholders, render, resolve, set,
    subagent_definitions, validate_name, versions, write_file, AgentPermissions, Edit, ExportBase,
    Isolation, Pick, Source, Writes, PACKAGED_JSON,
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
    // Each session's coordinator is a template of its own, acting as the coordinator.
    for c in ["swarm-coordinator", "review-coordinator"] {
        assert_eq!(get(c).def.role, Role::Coordinator, "{c}");
    }
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

/// The workers' subagent types: rendered from the packaged file, prefixed like everything the
/// installer writes, and holding nothing a coordinator would have to fill.
#[test]
fn every_subagent_template_renders_as_an_agent_definition() {
    let defs = subagent_definitions().unwrap();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["jkb-implementer", "jkb-reviewer"]);
    for d in &defs {
        assert!(
            d.markdown
                .starts_with(&format!("---\nname: {}\ndescription: \"", d.name)),
            "{}",
            d.markdown
        );
        assert!(!d.markdown.contains("{{"), "{} holds a placeholder", d.name);
    }
    // A reviewer reads and runs; neither type can start a worker, which would attest as its own role.
    let tools = |n: &str| {
        defs.iter()
            .find(|d| d.name == n)
            .unwrap()
            .markdown
            .lines()
            .find(|l| l.starts_with("tools:"))
            .map(str::to_owned)
    };
    assert_eq!(
        tools("jkb-reviewer").as_deref(),
        Some("tools: Read, Grep, Glob, Bash")
    );
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
