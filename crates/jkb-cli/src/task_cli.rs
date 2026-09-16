//! The task-mutate set's commands (tasks S6.2): `task add`/`set`/`edit`/`tag`/`depend`/`undepend`/
//! `place`/`unplace`/`bind`/`claim`/`release`, and `task why`.
//!
//! Each goes through [`Ops`], so through a `LocalBackend` on the host and `jkb serve` from the dev
//! container; the write itself is `jkb_api::tasks`'s, and only what cannot happen on the serving side
//! happens here — reading stdin, asking the terminal, choosing this process's owner id. The renderings
//! are the ones these commands printed before they were ported.

use anyhow::{bail, Context as _, Result};
use jkb_api::tasks::{AddAsk, TagMode};
use jkb_api::{Request, Response};

use super::ops_cli::{unexpected, Ops};
use super::{TaskCmd, TaskTagCmd};

/// Run one of the task writes [`super::ops_cli::handles`] names.
///
/// # Errors
/// The op's refusal, a local input error, or a verb this module does not handle.
#[allow(clippy::too_many_lines)] // a flat verb dispatcher: one arm per verb, as in `jkb_api`
pub fn run(ops: &Ops<'_>, cmd: TaskCmd) -> Result<()> {
    match cmd {
        // The session verbs served in both modes (tasks S6.4): their git work is done here, their
        // database work through the ops.
        TaskCmd::Start {
            uid,
            branch,
            onto,
            repo,
            owner,
        } => crate::session_cli::start(
            &crate::session_cli::Kb::from_ops(ops),
            &uid,
            crate::session_cli::StartWhere {
                branch,
                onto,
                repo,
                owner,
            },
            ops.json,
        ),
        TaskCmd::Gate {
            cmd: None,
            clear: false,
        } => crate::session_cli::gate(&crate::session_cli::Kb::from_ops(ops), ops.json),
        // On the host the old file store beside the database is listed for the operator
        // (`archive::Stores`); a client of `jkb serve` has none.
        TaskCmd::Work { uid, onto } => {
            let kb = crate::session_cli::Kb::from_ops(ops);
            let stores = crate::archive::Stores::new(kb, ops.db_path);
            crate::session_cli::work(&kb, &stores, &uid, onto.as_deref(), ops.json)
        }
        TaskCmd::Abandon {
            uid,
            force,
            delete_branch,
        } => {
            let kb = crate::session_cli::Kb::from_ops(ops);
            let stores = crate::archive::Stores::new(kb, ops.db_path);
            crate::session_cli::abandon(&kb, &stores, &uid, force, delete_branch, ops.json)
        }
        TaskCmd::Sessions => {
            let kb = crate::session_cli::Kb::from_ops(ops);
            let stores = crate::archive::Stores::new(kb, ops.db_path);
            crate::session_cli::sessions(&kb, &stores, ops.json)
        }
        TaskCmd::Add {
            text,
            backlog,
            sync,
            managed,
            under,
            home,
        } => add(
            ops,
            &text.join(" "),
            backlog,
            sync,
            managed,
            under.as_deref(),
            home.as_deref(),
        ),
        TaskCmd::Set {
            uid,
            status,
            priority,
            due,
        } => {
            applied(
                ops,
                Request::TaskSet {
                    uid: uid.clone(),
                    status,
                    priority,
                    due,
                },
            )?;
            report(ops.json, &uid, "updated");
            Ok(())
        }
        TaskCmd::Edit {
            uid,
            text,
            stdin,
            append,
        } => edit(ops, &uid, &text, stdin, append),
        TaskCmd::Tag { cmd } => {
            let (uid, facet_value, mode) = match cmd {
                TaskTagCmd::Add { uid, facet_value } => (uid, facet_value, TagMode::Add),
                TaskTagCmd::Set { uid, facet_value } => (uid, facet_value, TagMode::Set),
                TaskTagCmd::Rm { uid, facet_value } => (uid, facet_value, TagMode::Rm),
            };
            applied(
                ops,
                Request::TaskTag {
                    uid: uid.clone(),
                    facet_value,
                    mode,
                },
            )?;
            report(
                ops.json,
                &uid,
                if mode == TagMode::Rm {
                    "untagged"
                } else {
                    "tagged"
                },
            );
            Ok(())
        }
        TaskCmd::Depend { uid, dep } => {
            applied(
                ops,
                Request::TaskDepend {
                    uid: uid.clone(),
                    dep,
                },
            )?;
            report(ops.json, &uid, "depends_on set");
            Ok(())
        }
        TaskCmd::Undepend { uid, dep } => {
            applied(
                ops,
                Request::TaskUndepend {
                    uid: uid.clone(),
                    dep,
                },
            )?;
            report(ops.json, &uid, "depends_on removed");
            Ok(())
        }
        TaskCmd::Place { uid, ns, home } => {
            applied(
                ops,
                Request::TaskPlace {
                    uid: uid.clone(),
                    ns,
                    home,
                },
            )?;
            report(ops.json, &uid, "placed");
            Ok(())
        }
        TaskCmd::Unplace { uid, ns } => {
            let removed = match ops.call(Request::TaskUnplace {
                uid: uid.clone(),
                ns: ns.clone(),
            })? {
                Response::Unplaced { removed } => removed,
                other => return unexpected("task.unplace", &other),
            };
            if ops.json {
                println!("{}", serde_json::json!({ "uid": uid, "removed": removed }));
            } else {
                println!("unplaced {uid} from {ns} ({removed} mirror(s) removed)");
            }
            Ok(())
        }
        TaskCmd::Bind { uid, managed, sync } => {
            let sync = match (managed, sync) {
                (_, Some(uri)) => Some(uri),
                (true, None) => None,
                (false, None) => bail!("pass --managed or --sync <uri>"),
            };
            applied(
                ops,
                Request::TaskBind {
                    uid: uid.clone(),
                    sync,
                },
            )?;
            report(ops.json, &uid, "bound");
            Ok(())
        }
        TaskCmd::Claim { uid, owner } => claim(ops, &uid, owner, true),
        TaskCmd::Release { uid, owner } => claim(ops, &uid, owner, false),
        TaskCmd::Why { uid } => why(ops, &uid),
        _ => bail!("internal: a task subcommand the op set does not handle"),
    }
}

fn applied(ops: &Ops<'_>, request: Request) -> Result<()> {
    let op = request.op();
    match ops.call(request)? {
        Response::Applied {} => Ok(()),
        other => unexpected(op, &other),
    }
}

/// `{"uid","action"}` under `--json`, else `action: uid`.
fn report(json: bool, uid: &str, action: &str) {
    if json {
        println!("{}", serde_json::json!({"uid": uid, "action": action}));
    } else {
        println!("{action}: {uid}");
    }
}

fn add(
    ops: &Ops<'_>,
    text: &str,
    backlog: bool,
    sync: bool,
    managed: bool,
    under: Option<&str>,
    home: Option<&str>,
) -> Result<()> {
    let ask = |global_backlog: bool| -> Result<Response> {
        ops.call(Request::TaskAdd(AddAsk {
            text: text.to_owned(),
            home: home.map(str::to_owned),
            under: under.map(str::to_owned),
            backlog,
            global_backlog,
            sync,
            managed,
            cwd: std::env::current_dir()?.to_string_lossy().into_owned(),
            client_home: std::env::var("HOME").unwrap_or_default(),
        }))
    };
    // The op decides whether `--backlog` needs the user's assent to use the global backlog, once
    // everything else about the request has validated; only the asking happens here.
    let added = match ask(false)? {
        Response::Added { added } => added,
        Response::NeedsGlobalBacklogAssent {} => {
            if !super::confirm_global_backlog()? {
                bail!("--backlog needs an ambient repo; run inside a mounted repo or use `+<ns>`");
            }
            match ask(true)? {
                Response::Added { added } => added,
                other => return unexpected("task.add", &other),
            }
        }
        other => return unexpected("task.add", &other),
    };
    if ops.json {
        println!(
            "{}",
            serde_json::json!({"id": added.id, "uid": added.uid, "home": added.home,
                "binding": added.binding.as_deref().unwrap_or("managed:")})
        );
    } else {
        println!(
            "added task {} (item {}) at {}",
            added.uid, added.id, added.home
        );
        if added.binding.is_some() {
            if ops.remote {
                println!("  synced binding — the host's sync writes it to the file");
            } else {
                println!("  synced binding — run `jkb sync` to write it to the file");
            }
        }
    }
    Ok(())
}

fn edit(ops: &Ops<'_>, uid: &str, text: &[String], stdin: bool, append: bool) -> Result<()> {
    let text = if stdin {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .context("reading task content from stdin")?;
        buf.trim_end().to_owned()
    } else if text.is_empty() {
        bail!("provide new content as arguments, or pass --stdin");
    } else {
        text.join(" ")
    };
    let file_backed = match ops.call(Request::TaskEdit {
        uid: uid.to_owned(),
        text,
        append,
    })? {
        Response::Edited { file_backed } => file_backed,
        other => return unexpected("task.edit", &other),
    };
    report(ops.json, uid, if append { "appended" } else { "edited" });
    if file_backed && !ops.json {
        if ops.remote {
            eprintln!("note: this is a file-backed task; the host's sync propagates the edit to its file.");
        } else {
            eprintln!(
                "note: this is a file-backed task; run `jkb sync` to propagate the edit to its file."
            );
        }
    }
    Ok(())
}

fn claim(ops: &Ops<'_>, uid: &str, owner: Option<String>, acquire: bool) -> Result<()> {
    let owner = owner.unwrap_or_else(super::owner::preferred_owner);
    let ok = if acquire {
        match ops.call(Request::TaskClaim {
            uid: uid.to_owned(),
            owner: owner.clone(),
        })? {
            Response::Claimed { claimed } => {
                // A refusal is reported, not raised: the caller asked whether it could have the task,
                // and "no, because …" is an answer. The swarm reads the boolean.
                if let Some(why) = &claimed.refusal {
                    eprintln!("{why}");
                }
                claimed.acquired
            }
            other => return unexpected("task.claim", &other),
        }
    } else {
        match ops.call(Request::TaskRelease {
            uid: uid.to_owned(),
            owner: owner.clone(),
        })? {
            Response::Released { released } => released,
            other => return unexpected("task.release", &other),
        }
    };
    let key = if acquire { "acquired" } else { "released" };
    if ops.json {
        println!(
            "{}",
            serde_json::json!({"uid": uid, "owner": owner, key: ok})
        );
    } else {
        match (acquire, ok) {
            (true, true) => println!("claimed {uid} for {owner} (now in_progress)"),
            // The machine's own sentence has already gone to stderr.
            (true, false) => println!("{uid} was not claimed (see above)"),
            (false, true) => println!("released {uid} (was held by {owner})"),
            (false, false) => println!("{uid} was not claimed by {owner}"),
        }
    }
    Ok(())
}

fn why(ops: &Ops<'_>, uid: &str) -> Result<()> {
    let entries = match ops.call(Request::TaskWhy {
        uid: uid.to_owned(),
    })? {
        Response::History { entries, .. } => entries,
        other => return unexpected("task.why", &other),
    };
    if ops.json {
        let arr: Vec<_> = entries
            .iter()
            .map(|r| {
                serde_json::json!({
                    "at": r.at,
                    "txn": r.txn,
                    "event": r.event,
                    "from": r.from,
                    "to": r.to,
                    "agent": r.agent,
                    "branch": r.branch,
                    "onto": r.onto,
                    "pr": r.pr,
                    "evidence": r.evidence
                        .as_deref()
                        .and_then(|e| serde_json::from_str::<serde_json::Value>(e).ok()),
                })
            })
            .collect();
        println!("{}", serde_json::json!({"uid": uid, "history": arr}));
        return Ok(());
    }
    if entries.is_empty() {
        // Distinguished from "nothing happened": a task created before this history existed has none.
        println!(
            "no recorded transitions — this task predates the lifecycle history, or has \
                  not moved since"
        );
        return Ok(());
    }
    for r in &entries {
        let from = r.from.as_deref().unwrap_or("?");
        print!("{}  {from} -> {}  {}", r.at, r.to, r.event);
        if let Some(a) = &r.agent {
            print!("  by {a}");
        }
        if let Some(b) = &r.branch {
            print!("  on {b}");
        }
        if let Some(o) = &r.onto {
            print!("  onto {o}");
        }
        if let Some(n) = r.pr {
            print!("  #{n}");
        }
        println!();
        if let Some(e) = &r.evidence {
            // Only the facts that were established are worth printing.
            if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(e) {
                let shown: Vec<String> = map
                    .iter()
                    .filter(|(_, v)| !matches!(v.as_str(), None | Some("unknown")))
                    .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or("?")))
                    .collect();
                if !shown.is_empty() {
                    println!("      {}", shown.join(" "));
                }
            }
        }
    }
    Ok(())
}
