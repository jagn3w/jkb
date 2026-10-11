//! `jkb role`, `jkb workflow` and `jkb attest` — roles, task workflows and harness attestation from
//! the command line (design D52, `openspec/changes/jkb-rbac-workflows/`).
//!
//! Every verb is an op through [`jkb_api::Backend`], so the host and the dev container run the same
//! code, and the daemon enforces the same rules on both.

use std::io::Read as _;
use std::path::PathBuf;

use anyhow::{bail, Context as _, Result};
use clap::Subcommand;
use jkb_api::{Backend, Request, Response};
use serde_json::{json, Value};

use crate::remote::{self, Purpose};

#[derive(Subcommand)]
pub enum RoleCmd {
    /// Mint a role grant and print its token — shown this once; only its hash is kept.
    Grant {
        /// operator, coordinator, designer, implementer, reviewer or `systemic_reviewer`.
        role: String,
        /// Scope it to this task: it may write only the task, its subtasks and its findings.
        #[arg(long)]
        task: Option<String>,
        /// Who it is for, recorded in every history row it writes.
        #[arg(long, default_value = "agent")]
        agent: String,
        /// Print `export JKB_AGENT_TOKEN=…` instead of the bare token.
        #[arg(long)]
        export: bool,
    },
    /// Revoke a grant, and every grant it minted.
    Revoke {
        /// The grant id (`jkb role ls`).
        id: i64,
    },
    /// List live grants and the agent-type map.
    Ls {
        /// Only grants scoped to this task.
        #[arg(long)]
        task: Option<String>,
        /// Include revoked grants, and grants their minter may no longer grant.
        #[arg(long)]
        all: bool,
    },
    /// Who this caller is: its roles and scope.
    Whoami,
    /// Bind this (attested) subagent to the task it works on. The first bind wins: a worker cannot
    /// move on to another task afterwards.
    Bind {
        /// The task.
        uid: String,
    },
    /// Map a Claude Code agent type (a subagent definition's `name`) to the role an attested call from
    /// it holds. Operator only.
    Map {
        /// The agent type.
        agent_type: String,
        /// The role; omit with --clear to remove the mapping.
        role: Option<String>,
        /// Remove the mapping.
        #[arg(long)]
        clear: bool,
    },
    /// Replace the dev container's credential (operator, on the host). Revokes the old one and
    /// everything it minted.
    RotateContainer {
        /// Write it to `~/.jkb-container/credential` (0600) instead of printing it.
        #[arg(long)]
        write: bool,
        /// Keep the credential already written there if it still names a live container grant, and
        /// rotate only when it does not — what `setup.sh` runs after every pull, where rotating would
        /// revoke every worker grant minted from the old one.
        #[arg(long, requires = "write")]
        keep_live: bool,
    },
    /// Print the role tables: which role runs which class of op, and who grants whom.
    Matrix,
}

#[derive(Subcommand)]
pub enum WorkflowCmd {
    /// A task's workflow: phase, strategy, who acts next, what the caller may do, history. With
    /// `--graph`, the machines instead: the strategy's workflow graph (who may fire each event) and
    /// the task lifecycle, from the compiled tables — the task's own strategy, `--strategy`'s, or the
    /// default.
    Show {
        /// The task; with `--graph` it may be left out.
        uid: Option<String>,
        /// Print the machines as data rather than the task's standing.
        #[arg(long)]
        graph: bool,
        /// With `--graph` and no task: this strategy's machines rather than the default's.
        #[arg(long, requires = "graph", conflicts_with = "uid")]
        strategy: Option<String>,
    },
    /// Who acts next on a task, and the one thing to do.
    Next {
        /// The task; omitted, the task whose branch the current directory is on.
        uid: Option<String>,
        /// Run as a Claude Code Stop hook: read the hook's JSON on stdin, and ask the session to keep
        /// going while the next step is one it drives.
        #[arg(long)]
        stop_hook: bool,
    },
    /// Fire an event: `submit_design`, `approve_design`, `reject_design`, `submit_work`, `submit_systemic`,
    /// `systemic_redesign`, rework, cancel, reopen, override.
    Fire {
        /// The task.
        uid: String,
        /// The event.
        event: String,
        /// Why — required for `submit_systemic`, `systemic_redesign` and override.
        #[arg(long)]
        reason: Option<String>,
        /// The phase an override names.
        #[arg(long)]
        to: Option<String>,
    },
    /// Observe the task and take the one step the facts call for (after a review round: landable,
    /// back to implementation, or on to a systemic review).
    Observe {
        /// The task.
        uid: String,
    },
    /// Pin a task to a strategy (operator).
    Set {
        /// The task.
        uid: String,
        /// A preset (design-reviewed, coordinated, autonomous) or a defined name.
        strategy: String,
    },
    /// Define or redefine a strategy (operator): a graph, toggles and attributes.
    Define {
        /// Its name; `default` makes it the default for tasks with none pinned.
        name: String,
        /// Start from this preset or definition.
        #[arg(long)]
        from: Option<String>,
        /// reviewed-design or direct.
        #[arg(long)]
        graph: Option<String>,
        /// Who approves designs, comma-separated (operator, coordinator).
        #[arg(long)]
        approves_design: Option<String>,
        /// Who lands, comma-separated (operator, coordinator).
        #[arg(long)]
        lands: Option<String>,
        /// When must-fixes repeat: `file:2`, `directory:3`.
        #[arg(long)]
        repeated_area: Option<String>,
    },
    /// List the presets and definitions, and the default.
    Strategies,
    /// Print a graph as Graphviz dot.
    Dot {
        /// reviewed-design or direct.
        graph: String,
    },
    /// The agent templates the coordinator sessions give themselves and their workers (design D53.7): packaged ones
    /// (read-only) and the operator's copies, which override a packaged template of their name.
    Agent {
        #[command(subcommand)]
        cmd: crate::agent_cli::AgentCmd,
    },
}

#[derive(Subcommand)]
pub enum AttestCmd {
    /// The Claude Code hook: read a `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `SubagentStop` or
    /// `SessionEnd` payload on stdin, mint or release the tool call's ticket, and on `PreToolUse` put it
    /// on the command. Always exits 0; failures go to `~/.jkb/logs/attest-hook.log`.
    Hook,
}

fn unexpected<T>(op: &str, r: &Response) -> Result<T> {
    bail!("{op}: unexpected answer {r:?}")
}

fn call(b: &dyn Backend, r: Request) -> Result<Response> {
    b.call(r).map_err(|e| anyhow::anyhow!("{}", e.message))
}

fn print(json_out: bool, v: &Value, human: impl FnOnce()) {
    if json_out {
        println!("{v}");
    } else {
        human();
    }
}

/// Run a `jkb role` verb.
///
/// # Errors
/// A refused op, or a failure writing the credential.
#[allow(clippy::too_many_lines)] // one arm per verb
pub fn role(b: &dyn Backend, cmd: RoleCmd, json_out: bool) -> Result<()> {
    match cmd {
        RoleCmd::Grant {
            role,
            task,
            agent,
            export,
        } => match call(b, Request::RoleGrant { role, task, agent })? {
            Response::Granted { grant, token } => {
                let v = json!({ "grant": grant, "token": token });
                print(json_out, &v, || {
                    if export {
                        println!("export {}={token}", remote::AGENT_TOKEN_VAR);
                    } else {
                        eprintln!(
                            "granted {} (id {}) to {}{} — the token is shown once:",
                            grant.role,
                            grant.id,
                            grant.agent,
                            grant
                                .task
                                .as_deref()
                                .map(|t| format!(" on {t}"))
                                .unwrap_or_default()
                        );
                        println!("{token}");
                    }
                });
                Ok(())
            }
            other => unexpected("role.grant", &other),
        },
        RoleCmd::Revoke { id } => match call(b, Request::RoleRevoke { id })? {
            Response::Revoked { count } => {
                print(json_out, &json!({ "revoked": count }), || {
                    println!("revoked {count} grant(s)");
                });
                Ok(())
            }
            other => unexpected("role.revoke", &other),
        },
        RoleCmd::Ls { task, all } => match call(b, Request::RoleList { task, all })? {
            Response::Grants { listing } => {
                print(json_out, &json!(listing), || {
                    for g in &listing.grants {
                        println!(
                            "{:>4}  {:<17} {:<24} {}{}{}{}",
                            g.id,
                            g.role,
                            g.agent,
                            g.task.as_deref().unwrap_or("(unscoped)"),
                            g.parent
                                .map(|p| format!("  minted by {p}"))
                                .unwrap_or_default(),
                            g.revoked_at
                                .as_deref()
                                .map(|r| format!("  REVOKED {r}"))
                                .unwrap_or_default(),
                            // A revoked grant has nothing left to revoke.
                            if g.grantable || g.revoked_at.is_some() {
                                ""
                            } else {
                                "  NOT GRANTABLE (its minter may no longer grant it; revoke it)"
                            }
                        );
                    }
                    for (t, r) in &listing.agent_types {
                        println!("agent type {t} -> {r}");
                    }
                });
                Ok(())
            }
            other => unexpected("role.list", &other),
        },
        RoleCmd::Whoami => match call(b, Request::RoleWhoami {})? {
            Response::WhoAmI { whoami } => {
                print(json_out, &json!(whoami), || {
                    println!(
                        "{} — {}{}",
                        whoami.label,
                        if whoami.roles.is_empty() {
                            "no role".to_owned()
                        } else {
                            whoami.roles.join(", ")
                        },
                        whoami
                            .task
                            .as_deref()
                            .map(|t| format!(", on {t}"))
                            .unwrap_or_default()
                    );
                });
                Ok(())
            }
            other => unexpected("role.whoami", &other),
        },
        RoleCmd::Bind { uid } => match call(b, Request::RoleBind { uid })? {
            Response::Applied {} => Ok(()),
            other => unexpected("role.bind", &other),
        },
        RoleCmd::Map {
            agent_type,
            role,
            clear,
        } => {
            if role.is_none() && !clear {
                bail!("name a role, or --clear the mapping");
            }
            match call(b, Request::RoleMap { agent_type, role })? {
                Response::Applied {} => Ok(()),
                other => unexpected("role.map", &other),
            }
        }
        RoleCmd::RotateContainer { write, keep_live } => {
            let keep = keep_live
                .then(|| std::fs::read_to_string(remote::container_credential()).ok())
                .flatten()
                .map(|t| t.trim().to_owned())
                .filter(|t| !t.is_empty());
            match call(b, Request::RoleRotateContainer { keep: keep.clone() })? {
                Response::Granted { grant, token } if keep.as_deref() == Some(token.as_str()) => {
                    print(json_out, &json!({ "kept": grant }), || {
                        println!("container credential {} is live; kept", grant.id);
                    });
                    Ok(())
                }
                Response::Granted { grant, token } => {
                    if write {
                        let path = remote::container_credential();
                        jkb_daemon::token::write(&path, &token)
                            .with_context(|| format!("writing {}", path.display()))?;
                        print(json_out, &json!({ "grant": grant, "path": path }), || {
                            println!(
                                "container credential {} written to {}",
                                grant.id,
                                path.display()
                            );
                        });
                    } else {
                        print(json_out, &json!({ "grant": grant, "token": token }), || {
                            println!("{token}");
                        });
                    }
                    Ok(())
                }
                other => unexpected("role.rotate_container", &other),
            }
        }
        RoleCmd::Matrix => {
            use jkb_rbac::Grants as _;
            println!("## Which role runs which class of op\n");
            print!("{}", jkb_api::rbac::OP_GRANTS.matrix());
            println!("\n## Which role may grant which\n");
            print!("{}", jkb_core::roles::GRANTABLE.matrix());
            Ok(())
        }
    }
}

fn show(b: &dyn Backend, uid: String) -> Result<Box<jkb_api::rbac::WorkflowView>> {
    match call(b, Request::WorkflowShow { uid })? {
        Response::Workflow { workflow } => Ok(workflow),
        other => unexpected("workflow.show", &other),
    }
}

fn roles_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Run a `jkb workflow` verb.
///
/// # Errors
/// A refused op, or a malformed argument.
#[allow(clippy::too_many_lines)] // one arm per verb
pub fn workflow(b: &dyn Backend, cmd: WorkflowCmd, json_out: bool) -> Result<()> {
    match cmd {
        WorkflowCmd::Show {
            uid,
            graph: true,
            strategy,
        } => show_graph(b, uid, strategy, json_out),
        WorkflowCmd::Show { uid: None, .. } => {
            bail!("name a task, or pass --graph to print the machines")
        }
        WorkflowCmd::Show { uid: Some(uid), .. } => {
            let w = show(b, uid)?;
            print(json_out, &json!(w), || {
                println!("task:      {}", w.uid);
                println!("phase:     {}", w.phase);
                println!("strategy:  {}", w.source);
                println!("next:      {} — {}", w.next_role, w.next_step);
                if !w.may_fire.is_empty() {
                    println!("you may:   {}", w.may_fire.join(", "));
                }
                println!("\n{}", w.matrix);
                if !w.history.is_empty() {
                    println!("history:");
                    for h in &w.history {
                        println!("  {h}");
                    }
                }
            });
            Ok(())
        }
        WorkflowCmd::Next { uid, stop_hook } => {
            if stop_hook {
                stop(b, uid);
                return Ok(());
            }
            let uid = match uid {
                Some(u) => u,
                None => {
                    task_here(b)?.context("no task records this directory's branch; name one")?
                }
            };
            let w = show(b, uid)?;
            print(
                json_out,
                &json!({ "uid": w.uid, "phase": w.phase, "next_role": w.next_role,
                         "next_step": w.next_step, "caller_acts_next": w.caller_acts_next }),
                || {
                    println!(
                        "{} is in {}: {} — {}",
                        w.uid, w.phase, w.next_role, w.next_step
                    );
                },
            );
            Ok(())
        }
        WorkflowCmd::Fire {
            uid,
            event,
            reason,
            to,
        } => moved(
            call(
                b,
                Request::WorkflowFire {
                    uid,
                    event,
                    reason,
                    to,
                },
            )?,
            json_out,
        ),
        WorkflowCmd::Observe { uid } => moved(call(b, Request::WorkflowObserve { uid })?, json_out),
        WorkflowCmd::Set { uid, strategy } => {
            match call(b, Request::WorkflowSet { uid, strategy })? {
                Response::Applied {} => Ok(()),
                other => unexpected("workflow.set", &other),
            }
        }
        WorkflowCmd::Define {
            name,
            from,
            graph,
            approves_design,
            lands,
            repeated_area,
        } => {
            let mut spec = match &from {
                Some(base) => {
                    let (list, _) = strategies(b)?;
                    list.into_iter()
                        .find(|s| {
                            s.name == *base || s.name.split('@').next() == Some(base.as_str())
                        })
                        .with_context(|| format!("no strategy `{base}` to start from"))?
                        .spec
                }
                None => json!({ "graph": "reviewed-design" }),
            };
            if let Some(g) = graph {
                spec["graph"] = json!(g);
            }
            if let Some(r) = approves_design {
                spec["toggles"]["approves_design"] = json!(roles_list(&r));
            }
            if let Some(r) = lands {
                spec["toggles"]["lands"] = json!(roles_list(&r));
            }
            if let Some(ra) = repeated_area {
                let (scope, rounds) = ra
                    .split_once(':')
                    .context("--repeated-area as <file|directory>:<rounds>")?;
                let rounds: u32 = rounds.parse().context("--repeated-area rounds")?;
                spec["attributes"]["repeated_area"] = json!({ "scope": scope, "rounds": rounds });
            }
            match call(
                b,
                Request::WorkflowDefine {
                    name: name.clone(),
                    spec,
                },
            )? {
                Response::Defined { version } => {
                    print(
                        json_out,
                        &json!({ "name": name, "version": version }),
                        || {
                            println!("defined {name}@{version}");
                        },
                    );
                    Ok(())
                }
                other => unexpected("workflow.define", &other),
            }
        }
        WorkflowCmd::Strategies => {
            let (list, default) = strategies(b)?;
            print(
                json_out,
                &json!({ "strategies": list, "default": default }),
                || {
                    for s in &list {
                        let mark =
                            if s.name == default || s.name.starts_with(&format!("{default}@")) {
                                "*"
                            } else {
                                " "
                            };
                        println!("{mark} {:<20} {}  {}", s.name, s.describe, s.spec);
                    }
                },
            );
            Ok(())
        }
        WorkflowCmd::Agent { cmd } => crate::agent_cli::run(b, cmd, json_out),
        WorkflowCmd::Dot { graph } => {
            let g = jkb_core::workflow::GraphId::parse(&graph)
                .with_context(|| format!("no graph `{graph}` (reviewed-design, direct)"))?;
            print!("{}", g.machine().dot(g.as_str()));
            Ok(())
        }
    }
}

fn show_graph(
    b: &dyn Backend,
    uid: Option<String>,
    strategy: Option<String>,
    json_out: bool,
) -> Result<()> {
    let r = call(b, Request::WorkflowGraph { uid, strategy })?;
    let Response::WorkflowGraph { graph } = r else {
        return unexpected("workflow.graph", &r);
    };
    print(json_out, &json!(graph), || {
        println!("strategy:  {} ({})", graph.strategy, graph.graph);
        if let Some(task) = &graph.task {
            println!(
                "task:      {task} — phase {}, status {}",
                graph.phase.as_deref().unwrap_or("-"),
                graph.status.as_deref().unwrap_or("-")
            );
        }
        for (title, m) in [
            ("workflow", &graph.workflow),
            ("lifecycle", &graph.lifecycle),
        ] {
            println!("\n{title}:");
            for t in &m.transitions {
                println!(
                    "  {:<16} {:<20} -> {:<16} {}",
                    t.from,
                    t.event,
                    t.to.as_deref().unwrap_or("(stated)"),
                    t.fired_by
                );
            }
        }
    });
    Ok(())
}

fn strategies(b: &dyn Backend) -> Result<(Vec<jkb_api::rbac::StrategyInfo>, String)> {
    match call(b, Request::WorkflowStrategies {})? {
        Response::Strategies {
            strategies,
            default,
        } => Ok((strategies, default)),
        other => unexpected("workflow.strategies", &other),
    }
}

fn moved(r: Response, json_out: bool) -> Result<()> {
    let Response::WorkflowMoved { outcome } = r else {
        return unexpected("workflow.fire", &r);
    };
    if let Some(why) = &outcome.refusal {
        if json_out {
            println!("{}", json!(outcome));
        }
        bail!("refused: {why}");
    }
    print(json_out, &json!(outcome), || match &outcome.event {
        Some(e) if outcome.moved => println!("{e}: now {}", outcome.phase),
        _ => println!("nothing to do: still {}", outcome.phase),
    });
    Ok(())
}

/// The task whose branch the current directory is checked out on, if a task records it.
fn task_here(b: &dyn Backend) -> Result<Option<String>> {
    let ctx = crate::repo::repo_ctx()?;
    let cwd = std::env::current_dir()?;
    let Some(branch) = crate::gitrepo::current_branch(&cwd)? else {
        return Ok(None);
    };
    let kb = crate::session_cli::Kb::new(b);
    let by_branch = kb.by_branch(&ctx.key)?;
    Ok(by_branch
        .get(&branch)
        .and_then(|ts| crate::session_cli::task_on(ts))
        .map(|t| t.uid.clone()))
}

/// The roles a coordinator drives: while one of them acts next, the coordinator's turn is not over.
const DRIVEN: &[&str] = &[
    "coordinator",
    "designer",
    "implementer",
    "reviewer",
    "systemic_reviewer",
];

/// The environment variable a session opts in to being driven with: `1` for the task its directory's
/// branch records, or a task uid.
pub const DRIVE_VAR: &str = "JKB_DRIVE";

/// Whether, and what, a Stop hook drives.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Drive {
    /// The session did not opt in: the hook never holds it, whatever phase its task is in.
    Off,
    /// The task the directory's branch records.
    Here,
    /// This task.
    Task(String),
}

/// What the Stop hook drives, from the session's opt-in (`JKB_DRIVE`) and the hook's own `uid`.
fn driven(opt_in: Option<&str>, uid: Option<String>) -> Drive {
    match (opt_in.map(str::trim), uid) {
        (None | Some("" | "0"), _) => Drive::Off,
        (Some("1"), Some(uid)) => Drive::Task(uid),
        (Some("1"), None) => Drive::Here,
        (Some(named), _) => Drive::Task(named.to_owned()),
    }
}

/// `jkb workflow next --stop-hook`: a Claude Code Stop hook. Blocks the stop — with the next step as
/// the reason, which Claude reads — while the task's next actor is one the coordinator drives; lets
/// it through when the operator acts next or the task is settled. Once per stop: a stop the hook
/// already sent back (`stop_hook_active`) is let through, so a session that cannot progress is not
/// held in a loop. Anything it cannot establish lets the stop through, silently.
///
/// **Only in a session that opted in** ([`DRIVE_VAR`], set where the coordinating session is
/// launched). A managed hook fires in every session in the container, and an interactive one working
/// in a task's worktree was told to "continue" implementing — every turn — work nobody had asked it
/// for.
fn stop(b: &dyn Backend, uid: Option<String>) {
    let uid = match driven(std::env::var(DRIVE_VAR).ok().as_deref(), uid) {
        Drive::Off => return,
        Drive::Here => None,
        Drive::Task(uid) => Some(uid),
    };
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let payload: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    if payload["stop_hook_active"].as_bool() == Some(true) {
        return;
    }
    if let Some(dir) = payload["cwd"].as_str() {
        let _ = std::env::set_current_dir(dir);
    }
    let uid = match uid {
        Some(u) => Some(u),
        None => task_here(b).ok().flatten(),
    };
    let Some(uid) = uid else { return };
    let Ok(w) = show(b, uid) else { return };
    if DRIVEN.contains(&w.next_role.as_str()) {
        println!(
            "{}",
            json!({
                "decision": "block",
                "reason": format!(
                    "The workflow for {} is in `{}`, and the next step is the {}'s: {}. Continue \
                     it (`jkb workflow show {}` for the whole picture).",
                    w.uid, w.phase, w.next_role, w.next_step, w.uid
                ),
            })
        );
    }
}

/// The attestation hook's log.
fn attest_log() -> PathBuf {
    std::env::var_os("HOME")
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join(".jkb/logs/attest-hook.log")
}

fn log_failure(what: &str) {
    use std::io::Write as _;
    let path = attest_log();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{}: {what}", std::process::id());
    }
}

/// The word-like tokens of `text` as bash could join them: a line continuation (`\` + newline) and
/// the quoting characters `'`, `"` and `\` removed first -- bash removes them inside a word, so
/// `j''kb`, `j'k'b` and `j\kb` all run `jkb` -- then split on every character that cannot be part
/// of a command word or path. Splitting on a NAMED list of separators missed whatever it did not
/// name (`jkb>out`, `'jkb'`), and splitting on the quotes instead of removing them read `j''kb` as
/// `j` and `kb` (review rounds 6 and 7).
fn tokens(text: &str) -> Vec<String> {
    text.replace("\\\n", "")
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '\\'))
        .collect::<String>()
        .split(|c: char| !(c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '~' | '+')))
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether `text` mentions `jkb` anywhere a shell could make a command of it -- the gate on whether
/// the hook looks at a Bash command at all. Deliberately wider than [`attestation`]'s model: what it
/// finds there, the model has to account for before a ticket is minted.
fn runs_jkb(command: &str) -> bool {
    tokens(command).iter().any(|w| is_jkb(w))
}

/// Commands that may share a line with `jkb` and still let the hook APPROVE the line -- an approval
/// that overrides the session's own permission rules for every command on it. So a member must
/// have no path to run other code, READ or WRITE a file, or change which program a later word names.
/// Matched on the command word exactly -- a path, an assignment prefix or a wrapper in front is
/// something else.
///
/// An ALLOWLIST, because every attempt to name the dangerous cases instead fell short: review
/// rounds 7 and 8 each found a jkb run out of sight -- `sh -c "jkb …"`, `xargs jkb`, `env -S`, a glob
/// inside quotes, a script file, `jkb ls | ./evil` -- that a denylist missed. Kept short on purpose, and
/// checked for exec AND write paths: `sort` is out (`--compress-program` runs a program), `printf`
/// is out (`printf -v PATH …` repoints the next `jkb`), `sed` and `awk` are out (`e`, `system()`),
/// and `uniq` is out -- `uniq IN OUT` writes OUT, and `uniq evil ~/.cargo/bin/jkb; jkb ls` replaced
/// the binary the next command ran, exec bit kept (review round 9, coreutils 9.4). `cd` is in only
/// because `PATH` holds no relative or empty entry, so the directory cannot change which `jkb`
/// runs; the measurement is recorded once, in D52.9 of docs/task-lifecycle.md.
///
/// No READER is on it either, by the user's decision (2026-10-02): `cat`, `grep`, `head`, `tail`,
/// `wc` and `jq` were, and approving `jkb ls; cat ~/repos/other/.env` then read a file past any
/// rule the person had for reads. Every member left is a shell builtin that reads no file, so
/// `jkb … | jq` is deferred -- under the auto posture still unprompted, under stricter rules theirs.
const HARMLESS: &[&str] = &["cd", "true", "false", ":", "echo"];

/// What the attestation hook does with a Bash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attestation {
    /// No `jkb` that could run with a ticket -- the line only MENTIONS jkb, in the arguments of a
    /// `HARMLESS` command (`echo jkb`) or not as a word at all: left alone, no ticket, no
    /// decision.
    Skip,
    /// Ticketed, and the line approved: every command on it is `jkb` itself or `HARMLESS`, so
    /// nothing on it but `jkb` can run or write anything, and no `jkb` on it reads a file the caller
    /// names or runs a command the caller wrote ([`remote::beyond_rbac`]). What each `jkb` may DO is the daemon's RBAC, held
    /// against the ticket on every request.
    Allow,
    /// Ticketed, and the permission decision left to whoever it belonged to: a line that runs
    /// something besides `jkb` and the `HARMLESS` commands, one the lexer cannot model, or one whose
    /// `jkb` does work RBAC cannot judge -- reads a file the caller names, runs a command the caller
    /// wrote ([`remote::beyond_rbac`]) -- or is refused by jkb's own parser. The hook
    /// returns no `permissionDecision`, so the session's own rules judge the call against the command
    /// as the model wrote it -- approving the line would approve the rest of it too, past whatever
    /// rule the person set for that.
    ///
    /// There is no third, forcing answer. The hook used to put `ask` on several classes -- first on
    /// everything that was not one plain invocation, later on `task land`/`task gate`, on lines it
    /// could not model and on lines where a jkb might run out of sight -- and a `PreToolUse` `ask`
    /// OVERRIDES an allow rule, so a mechanism meant to be invisible kept putting prompts in front of
    /// the person. Superseded by the user's rule, 2026-10-02: the ticket is authorization, and the
    /// daemon's RBAC decides what it may do. Each prompt was standing in for something already
    /// covered: who may land is `may_land`, asked before the graft, the gate or the session's
    /// disposal (`task.land_check`; an exclude entry, the lease and remote adoption come first); the gate
    /// a landing runs is `sh -c` inside the same Bash sandbox as every agent command; a stored gate
    /// cannot be set from the container at all; `--gate-on-host` is operator-only; and `main` changes
    /// only through a PR, whose CI is the verification that counts. A ticket inherited by another
    /// program on the line can do only what its role may.
    Defer,
}

/// The three characters this hook cannot reason about wherever they appear: `$` and `` ` `` are
/// substituted inside double quotes, and `\\` escapes the quoting itself. With none of them present,
/// [`shell_commands`] reads quotes exactly as the shell does, which is what lets every other
/// metacharacter be judged by whether it is quoted. Refused outright, quoted or not, by
/// [`shell_commands`] itself so that no caller can proceed on words it could not model.
const UNQUOTABLE: &[char] = &['$', '`', '\\'];

/// Characters that END one command and BEGIN another when the shell sees them bare.
///
/// Splitting on these is what lets a command LIST be judged command by command instead of refused
/// whole: `cd repo && jkb workflow next` is two commands, `cd` and `jkb`, both approvable, so the
/// line is approved. A repeated one (`&&`, `||`) just leaves an empty command between them, which
/// is dropped. Inside either kind of quote they are ordinary text, like every character below.
const SEPARATORS: &[char] = &[';', '&', '|', '\n'];

/// Characters that leave nothing this can model, bare, so a line carrying one is never approved --
/// it is deferred to the session's own rules: grouping (a subshell, or process substitution
/// `<(…)`), brace expansion, globs, redirects and a comment.
///
/// Redirects and `#` were once read as word breaks instead, to approve `jkb … 2>&1`, and review
/// round 6 measured bash running those lines differently from how they were read. A redirect
/// operator spelled with `&` or `|` (`2>&1`, `>&2`, `&>`, `>|`) was split as a command separator,
/// and a quote inside a comment or a here-doc body is text to bash but opened a quote here,
/// swallowing the line after it. Approving a redirect safely needs redirect operators, here-docs and
/// comments lexed as bash lexes them; until that is written, such a line is deferred, not approved.
/// Pinned by `run_through_bash_a_redirect_comment_or_heredoc_is_never_approved`.
///
/// Quoted, they are argument text, like every character in [`BREAKS`] -- which is the whole point,
/// because jkb's own quick-add syntax (`?`, …) is spelled in them, and refusing them quoted put a
/// permission prompt on the most ordinary `jkb` calls there are.
const FATAL: &[char] = &['<', '>', '(', ')', '{', '}', '#', '*', '?', '[', ']'];

/// Characters that make a command more than one plain invocation, bare, but cannot start an
/// operator, a command or a quote span: a bare `!` (pipeline negation; history expansion is off)
/// and a carriage return. Read as word breaks, and a line carrying one is never approved, only
/// deferred. Redirects and `#` were here too, and are not: see [`FATAL`].
///
/// `\r` is not syntax to bash at all -- it is word text (measured: `a\rb` is passed as the single
/// word `a\rb`) -- and is here because a carriage return in a command is a line ending that got
/// through something; deferring it costs nothing.
const BREAKS: &[char] = &['!', '\r'];

/// A character bash's **lexer** breaks a command line on: a blank (space or tab) or a newline.
///
/// Not [`char::is_whitespace`], which is Unicode-wide: a non-breaking space is ordinary word text
/// to bash. Reading `jkb\u{a0}task show x` as four words made it one plain invocation here while
/// bash looked for a command named `jkb\u{a0}task`.
///
/// And not `IFS`, which an earlier version of this comment named. `IFS` splits the RESULT of an
/// expansion, not the command line, so the caller cannot change this set (measured on GNU bash:
/// under `IFS=x`, `p axb c` still passes `axb` and `c`; under `IFS=` it still passes `a` and `b`).
/// The distinction matters because the wrong reason points at an unsafe correction -- a reader who
/// believed this held only "at IFS's default value" and made it consult `$IFS` would, under
/// `IFS=x`, split words bash keeps whole and revive exactly the defect above.
fn is_blank(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n')
}

/// The commands a command line is, each as the words the shell will run it as — quotes removed, as
/// the shell removes them — and whether the line is PLAIN (no bare [`BREAKS`] character), or
/// `None` for any command this cannot model faithfully: an [`UNQUOTABLE`] character anywhere, a
/// bare [`FATAL`] character, an unbalanced quote, or a word whose value the shell decides
/// (a bare leading `~`, unless the word also carries a `/`).
///
/// The soundness precondition is enforced here rather than stated for the caller to honour. It was
/// prose before ("sound only for a command with none of [`UNQUOTABLE`] in it"), which made it a
/// rule every call site had to remember, and the one thing this must never do is return words that
/// are not what the shell will pass.
fn shell_commands(command: &str) -> Option<(Vec<Vec<String>>, bool)> {
    // `$` and a backtick substitute inside double quotes and a backslash escapes the quoting
    // itself, so with any of them present the quote tracking below is not a model of anything.
    if command.contains(UNQUOTABLE) {
        return None;
    }
    let mut commands: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word: Option<String> = None;
    let mut quote: Option<char> = None;
    // The word being read opens with a bare `~`, so the shell, not this reader, decides its value.
    let mut expands = false;
    let mut plain = true;
    for c in command.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => {
                quote = Some(c);
                word.get_or_insert_with(String::new);
            }
            (None, c) if FATAL.contains(&c) => return None,
            // Before the blank arm: a line break is both, and it ends a command first.
            (None, c) if SEPARATORS.contains(&c) => {
                if let Some(w) = word.take() {
                    words.push(modelled(w, expands)?);
                }
                expands = false;
                if !words.is_empty() {
                    commands.push(std::mem::take(&mut words));
                }
            }
            (None, c) if is_blank(c) || BREAKS.contains(&c) => {
                plain &= !BREAKS.contains(&c);
                if let Some(w) = word.take() {
                    words.push(modelled(w, expands)?);
                }
                expands = false;
            }
            // Inside quotes or out, anything else is part of the word. An unquoted `~` is the one
            // character whose VALUE the shell chooses rather than passes through -- ANYWHERE in the
            // word, not only at its start: bash also expands one after an assignment's `=` and after
            // a `:` in the value (measured: under `HOME=land`, `a=~` is passed as `a=land`).
            (_, c) => {
                if quote.is_none() && c == '~' {
                    expands = true;
                }
                word.get_or_insert_with(String::new).push(c);
            }
        }
    }
    if let Some(w) = word {
        words.push(modelled(w, expands)?);
    }
    if !words.is_empty() {
        commands.push(words);
    }
    quote.is_none().then_some((commands, plain))
}

/// Whether a word names the `jkb` binary, by name or by a path ending in it.
fn is_jkb(word: &str) -> bool {
    word == "jkb" || word.ends_with("/jkb")
}

/// One finished word, or `None` when the shell will choose its value and this cannot say what it
/// will be. A word with an unquoted `~` in it but carrying a `/` is safe to keep unexpanded:
/// whatever the tilde expands to, the `/` survives, so the word can never come out equal to a bare
/// word like `jkb` or `land` -- which is the only thing the words are ever compared against. A bare
/// `~`, `~+`, `~-` or `a=~` can be anything at all.
fn modelled(word: String, expands: bool) -> Option<String> {
    (!expands || word.contains('/')).then_some(word)
}

/// Classify a Bash command for [`attest`].
fn attestation(command: &str) -> Attestation {
    if !runs_jkb(command) {
        return Attestation::Skip;
    }
    let command = command.trim_matches(is_blank);
    // A line this cannot model is not approved -- only an `allow` overrides the session's own
    // rules, so only an approved line has to be understood -- but it is ticketed, so whatever jkb
    // it runs can act, as far as its role lets it.
    let Some((commands, plain)) = shell_commands(command) else {
        return Attestation::Defer;
    };
    // Every command on the line is one of three things. `jkb` itself, by that exact command word.
    // A [`HARMLESS`] command, whose arguments are data however they mention jkb (`echo jkb`).
    // Or OTHER -- anything that might run or write something, which the session's own rules judge.
    // A jkb doing work RBAC cannot judge counts as OTHER too ([`beyond_rbac`]).
    let mut jkb = 0_usize;
    let mut other = false;
    let mut mentions = false;
    for words in &commands {
        match words.first().map(String::as_str) {
            Some("jkb") => {
                jkb += 1;
                other |= beyond_rbac(words);
            }
            Some(w) if HARMLESS.contains(&w) => {}
            _ => {
                other = true;
                mentions |= words.iter().any(|w| runs_jkb(w));
            }
        }
    }
    // No jkb to ticket and none that might run: leave the line alone (`echo jkb`).
    if jkb == 0 && !mentions {
        return Attestation::Skip;
    }
    // Something on the line is not jkb and not harmless: approving the line would approve it too,
    // past whatever rule the person set for it. Ticketed, and left to those rules.
    if other || !plain {
        return Attestation::Defer;
    }
    // Every command is jkb or harmless: approved, so `jkb task show x` and `cd repo && jkb workflow
    // next` never prompt. What each jkb may DO is the daemon's RBAC, held against the ticket on
    // every request -- not this hook's.
    Attestation::Allow
}

/// Whether one `jkb` command's words do work RBAC cannot judge -- [`remote::beyond_rbac`], asked of
/// the words as the binary itself would parse them, so a flag or an `=` spelling cannot read
/// differently here. Words the parser refuses count too: what they would do is not known, so such a
/// line is not approved and the session's own rules judge it. Help and the version are not refusals
/// -- clap reports them as errors, but they print and exit having read nothing.
fn beyond_rbac(words: &[String]) -> bool {
    use clap::{error::ErrorKind, Parser as _};
    match crate::Cli::try_parse_from(words) {
        Ok(cli) => remote::beyond_rbac(&cli.command),
        Err(e) => !matches!(
            e.kind(),
            ErrorKind::DisplayHelp
                | ErrorKind::DisplayVersion
                | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        ),
    }
}

/// The `PreToolUse` answer for one classified command.
///
/// The rewrite always goes out: it is how the ticket reaches `jkb`, and it is a real per-tool-call
/// secret (measured — each Bash tool call runs in its own PID namespace, so no other call can read
/// this one's `/proc/*/environ`, which is why the ticket cannot instead be left in a file for a
/// sibling call to steal). Only the DECISION is conditional, and for [`Attestation::Defer`] there
/// is none: the session's own rules judge the call, as they did before this hook existed.
///
/// `JKB_ATTEST_DECISION=ask` forces the prompt on every ticketed line without a rebuild — the
/// hook binary is pinned and root-owned, so a rollback that needs one is not a rollback.
fn pre_tool_use(class: Attestation, forced_ask: bool, input: &Value) -> Value {
    let mut out = json!({
        "hookEventName": "PreToolUse",
        "updatedInput": input,
    });
    // No default arm, so a new class has to say what it emits.
    let decision = match (class, forced_ask) {
        (Attestation::Skip | Attestation::Allow | Attestation::Defer, true) => Some("ask"),
        (Attestation::Allow, false) => Some("allow"),
        (Attestation::Defer | Attestation::Skip, false) => None,
    };
    if let Some(d) = decision {
        out["permissionDecision"] = json!(d);
    }
    json!({ "hookSpecificOutput": out })
}

/// `jkb attest hook`. Never fails: every failure is logged, and the tool call proceeds without a
/// ticket — which its `jkb` calls then refuse. Failing closed is the daemon's job, not the hook's.
pub fn attest(cmd: &AttestCmd) {
    let AttestCmd::Hook = cmd;
    let mut raw = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut raw) {
        log_failure(&format!("reading the payload: {e}"));
        return;
    }
    let p: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            log_failure(&format!("parsing the payload: {e}"));
            return;
        }
    };
    let s = |k: &str| p[k].as_str().map(str::to_owned);
    let Some(session) = s("session_id") else {
        return;
    };
    let backend = match remote::client(&remote::daemon_url(), Purpose::Hook) {
        Ok(b) => b,
        Err(e) => {
            log_failure(&format!("client: {}", e.message));
            return;
        }
    };
    let request = match s("hook_event_name").as_deref() {
        Some("PreToolUse") => {
            let command = p["tool_input"]["command"].as_str().unwrap_or_default();
            let class = attestation(command);
            if s("tool_name").as_deref() != Some("Bash") || class == Attestation::Skip {
                return;
            }
            let Some(tool_use_id) = s("tool_use_id") else {
                return;
            };
            let token = match backend.call(Request::AttestMint {
                session,
                agent_id: s("agent_id"),
                agent_type: s("agent_type"),
                tool_use_id,
            }) {
                Ok(Response::Ticket { token }) => token,
                Ok(other) => {
                    log_failure(&format!("attest.mint: unexpected {other:?}"));
                    return;
                }
                Err(e) => {
                    log_failure(&format!("attest.mint: {}", e.message));
                    return;
                }
            };
            let mut input = p["tool_input"].clone();
            input["command"] = json!(format!("export {}={token}; {command}", remote::ATTEST_VAR));
            let forced_ask = std::env::var("JKB_ATTEST_DECISION").as_deref() == Ok("ask");
            println!("{}", pre_tool_use(class, forced_ask, &input));
            return;
        }
        // Only a call this hook ticketed has anything to release: asking the daemon after every
        // `ls` put it on every Bash call's path.
        Some("PostToolUse" | "PostToolUseFailure") => {
            let command = p["tool_input"]["command"].as_str().unwrap_or_default();
            if attestation(command) == Attestation::Skip {
                return;
            }
            Request::AttestRelease {
                session,
                agent_id: None,
                tool_use_id: s("tool_use_id"),
            }
        }
        Some("SubagentStop") => match s("agent_id") {
            Some(agent) => Request::AttestRelease {
                session,
                agent_id: Some(agent),
                tool_use_id: None,
            },
            None => return,
        },
        Some("SessionEnd") => Request::AttestRelease {
            session,
            agent_id: None,
            tool_use_id: None,
        },
        _ => return,
    };
    if let Err(e) = backend.call(request) {
        log_failure(&format!("attest.release: {}", e.message));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        attestation, driven, is_blank, pre_tool_use, runs_jkb, shell_commands, Attestation, Drive,
        BREAKS, FATAL, HARMLESS, SEPARATORS, UNQUOTABLE,
    };
    use serde_json::json;

    #[test]
    fn the_stop_hook_holds_only_a_session_that_opted_in() {
        assert_eq!(driven(None, None), Drive::Off, "not opted in: never held");
        assert_eq!(driven(Some(""), Some("task:x".into())), Drive::Off);
        assert_eq!(driven(Some("0"), None), Drive::Off);
        assert_eq!(driven(Some("1"), None), Drive::Here, "the directory's task");
        assert_eq!(
            driven(Some("1"), Some("task:x".into())),
            Drive::Task("task:x".into())
        );
        assert_eq!(driven(Some("task:y"), None), Drive::Task("task:y".into()));
    }

    /// What the hook actually puts on stdout, which is the thing the harness acts on. The ticket
    /// rewrite must go out for EVERY ticketed class -- dropping it for the deferred class would turn
    /// a permission prompt into a hard `Unauthorized` on every compound `jkb` command -- while the
    /// decision is emitted only when there is one to make.
    #[test]
    fn the_rewrite_goes_out_always_and_the_decision_only_when_there_is_one() {
        let input = json!({ "command": "export JKB_ATTEST=t; jkb task show x" });
        let decision = |class, forced| {
            let o = pre_tool_use(class, forced, &input);
            let h = o["hookSpecificOutput"].clone();
            assert_eq!(h["hookEventName"], "PreToolUse");
            assert_eq!(h["updatedInput"], input, "the ticket must reach jkb: {h}");
            h.get("permissionDecision").cloned()
        };

        assert_eq!(decision(Attestation::Allow, false), Some(json!("allow")));
        // The whole of the over-prompting was the hook answering "ask", which overrides an allow
        // rule. Answering nothing leaves the call to the rules that were already there.
        assert_eq!(
            decision(Attestation::Defer, false),
            None,
            "the deferred class must not answer the permission question at all"
        );
        // The rollback forces the prompt on every ticketed class.
        assert_eq!(decision(Attestation::Allow, true), Some(json!("ask")));
        assert_eq!(decision(Attestation::Defer, true), Some(json!("ask")));
    }

    /// Every command the classifier approves. `attestation` is asserted against them below, and
    /// `every_approved_command_is_the_jkb_on_path_and_nothing_else` runs each one through a real
    /// bash to check the words it approved are the words bash actually passes.
    const ALLOWED: &[&str] = &[
        "jkb task show x",
        "jkb --json workflow next",
        "  jkb role whoami  ",
        "jkb task add 'a subtask' --under task:x",
        // A quoted metacharacter is an argument, not syntax. jkb's own quick-add syntax is
        // spelled in these characters, so refusing them put a prompt on the most ordinary
        // `jkb` calls there are -- which is what this arm exists to approve.
        "jkb query 'status!=done'",
        "jkb task add 'Fix the sweep !p1 @2026-10-01 +tasks/inbox #area=container'",
        "jkb search 'what changed?'",
        "jkb task edit x \"a # hash, a ! bang and a [bracket]\"",
        "jkb grep '*.rs' repos/jkb",
        // Tilde expansion yields one word and always a path, and has no quoted spelling that
        // still expands, so it is judged bare.
        "jkb --db ~/.jkb/jkb.db task show x",
        // A line break is a separator bare and text quoted, like every other character here;
        // the bare spelling is in the deferred list below.
        "jkb task add 'line one\nline two'",
        // Split on IFS, so a non-breaking space is ordinary word text, quoted or not --
        // which is exactly what bash passes. Only a word SEPARATOR has to match.
        "jkb task add 'a\u{a0}b'",
        "jkb task add x\u{3000}y",
        // Help and the version are clap "errors" that read nothing: still approved.
        "jkb task show --help",
        "jkb --version",
        "jkb task",
    ];

    /// Lines approved as a whole: every command on them is `jkb` itself or HARMLESS, so nothing on
    /// them can run or write anything but jkb -- the over-prompting the hook was changed to remove.
    /// Some were once forced to a prompt (`land`, `gate`): what a jkb may do is the daemon's RBAC,
    /// the gate runs in the same sandbox as every agent command, and a stored gate cannot be set
    /// from the container at all.
    const APPROVED_LINES: &[&str] = &[
        "cd repo && jkb workflow next",
        "cd repo; jkb ls; echo done",
        "jkb task show x\njkb task show y",
        "jkb task land task:x",
        "jkb task gate",
        "jkb task add 'ok' && jkb task land task:x",
        // A jkb that reads no file the caller names: a page to render, or a review result typed
        // on the line itself.
        "jkb ingest https://example.com/page",
        "jkb --json ingest --ns inbox https://example.com/page",
        "echo '{}' | jkb task review file --findings repos/x/codereviews/y --from -",
        // A design op names no local file: the export writes the target the design holds, and
        // the drift check reads only the checkout's `docs/`.
        "jkb design export design:x",
        "jkb design export --check",
        "jkb design export --check --against-db",
    ];

    /// Lines the classifier cannot model, so it does not approve them -- only an approval overrides
    /// the session's own rules -- but tickets them, and those rules judge them.
    const UNMODELLED: &[&str] = &[
        "jkb task show ~",
        "jkb task show ~+",
        "jkb task edit x --text \"$(cat /etc/passwd)\"",
        "jkb task add 'x' `whoami`",
        "jkb task add \"x\" \\; rm -rf /",
        "jkb task add '$(whoami)'",
        "jkb task add 'unbalanced",
        "jkb ls > /tmp/out",
        "jkb task list --json 2>&1",
        "jkb task show x 2>/dev/null",
        "jkb task show x # a note",
        "jkb ls *",
        "jkb task show {a,b}",
        // A bare `!` is understood, but makes the line more than plain invocations.
        "jkb task show x!y",
    ];

    /// Lines with something that is neither `jkb` nor HARMLESS on them, or `jkb` reached other than
    /// by its command word: approving the line would approve the rest past the person's own rule.
    const OTHER: &[&str] = &[
        // Readers are not harmless (the user's decision): approving the line would read past the
        // person's own rules. Deferred -- under the auto posture still unprompted.
        "jkb task show x | jq .status",
        "jkb task list --json | head -20",
        "jkb ls; cat ~/repos/other/.env",
        "grep -rn jkb src",
        // An inline gate is a command the caller wrote, run with `sh -c`: approving it would run
        // and read past the person's own rules ([`remote::beyond_rbac`]).
        "jkb --json task land task:x --gate true",
        "jkb task 'land' task:x --gate 'sh /tmp/p.sh'",
        "jkb task land task:x --gate='cat ~/repos/other/.env'",
        // A jkb that reads a file the caller NAMES ([`remote::beyond_rbac`]): the daemon sees
        // only the text, so approving it would read past the person's own rules like `cat` -- the
        // same decision. However the flags are spelled, since the binary's own parser reads them.
        "jkb ingest ~/repos/other/.env",
        "jkb ingest notes.md && jkb cat x",
        "jkb --json ingest --ns inbox ./notes.md",
        "jkb ingest --ns=inbox notes.md",
        "jkb ingest file:///home/vscode/repos/other/.env",
        "jkb ingest HTTPS://example.com",
        "jkb mcp",
        "echo '{}' | jkb mcp",
        // `design source` hashes files the caller names; `export --to` writes to one.
        "jkb design source design:x secrets/.env",
        "jkb --json design source design:x README.md docs/a.md",
        "jkb design export design:x --to docs/a.md",
        "jkb design export design:x --to=../elsewhere.md",
        "jkb task review file --findings repos/x/codereviews/y --from result.json",
        "jkb task review file --findings=n --from=result.json",
        "cd repo && jkb task review file --from r.json --findings n",
        // Words jkb's parser refuses are not approved either: what they would do is not known.
        "jkb --no-such-flag ls",
        "jkb no-such-command",
        "jkb ls && rm -rf ~/repos/other",
        "jkb ls\nrm -rf /",
        "jkb task show x | ./evil.sh",
        "jkb ls; sh f",
        "jkb ls; make",
        "jkb ls; sh -c '~/.cargo/bin/jk? task land x --gate evil'",
        "jkb ls; sh -c 'jkb task show x'",
        "sh -c 'jkb task show x'",
        "echo x | xargs jkb task show",
        // `jkb` reached other than by its command word: a path is any file called jkb, and a
        // prefix or wrapper can change what runs with the ticket.
        "./jkb ls",
        "~/.cargo/bin/jkb ls",
        "FOO=1 jkb ls",
        "LD_PRELOAD=/tmp/x.so jkb ls",
        "PATH=/tmp/evil jkb ls",
        "timeout 300 jkb ls",
        // Repointing `jkb` for the rest of the line, with no character the lexer asks about.
        "PATH=/tmp/evil; jkb ls",
        "hash -p /tmp/evil jkb; jkb ls",
        "printf -v PATH /tmp/evil; jkb ls",
        "jkb ls | sort --compress-program=/tmp/evil",
        // Bash splits on blanks alone, so the command word here is `jkb\u{a0}task`, not `jkb`.
        "jkb\u{a0}task show x",
        "\u{a0}jkb task show x",
        // Measured: this ran an arbitrary program with only a writable cwd. A command word
        // containing `/` is never searched on PATH, so `jkb\u{a0}./x` is the relative path
        // `jkb\u{a0}.` / `x` -- no PATH entry needed, and no character from either list used.
        "jkb\u{a0}./x --gate 'sh /tmp/p.sh'",
        "jkb\u{a0}evil",
        "sudo jkb task land task:x",
    ];

    /// Lines that only MENTION jkb -- in a HARMLESS command's arguments, or not as a word at all.
    const MENTIONS: &[&str] = &["ls", "cargo build -p jkb-cli", "echo jkb-core", "echo jkb"];

    #[test]
    fn a_line_of_only_jkb_and_harmless_commands_is_approved_and_the_rest_is_deferred() {
        for allow in ALLOWED {
            assert_eq!(attestation(allow), Attestation::Allow, "{allow}");
        }
        for allow in APPROVED_LINES {
            assert_eq!(attestation(allow), Attestation::Allow, "{allow}");
        }
        for defer in UNMODELLED.iter().chain(OTHER) {
            assert_eq!(attestation(defer), Attestation::Defer, "{defer}");
        }
        // Skipped -- no ticket, no decision.
        for skip in MENTIONS {
            assert_eq!(attestation(skip), Attestation::Skip, "{skip}");
        }
    }

    /// The words bash passes for a command, and how many times it ran `jkb`. `jkb` is installed as
    /// a shell FUNCTION: bash resolves a command word function -> builtin -> `PATH`, so the function
    /// fires only when the word is exactly `jkb`, and -- unless `executable` -- nothing is executed.
    /// With `executable`, a fake `jkb` program and `/usr/bin:/bin` are on `PATH`, and a fixture runs
    /// real system binaries (`sh`, `xargs`, `env`): its safety is the literal fixture list. It proves the
    /// command word is literally `jkb` with no path component -- NOT that the binary came from
    /// `PATH`, which a `jkb` function or alias in the invoking shell would defeat anyway.
    fn bash_argv(
        cmd: &str,
        env: &[(&str, &str)],
        cwd: &std::path::Path,
        executable: bool,
    ) -> (usize, Vec<String>, Vec<String>) {
        use std::process::Command;
        // The function reports on fd 9, which the command under test never names. It reported on
        // stdout once, and a fixture that redirects stdout (`jkb task land>out x`) sent the report
        // into the file: `args` came back empty, `.any(|a| a == "land")` was trivially false, and a
        // model that dropped the word before a redirect passed (measured, by that mutation).
        let capture = tempfile::NamedTempFile::new().expect("capture file");
        // And every simple command bash runs is logged by its command word, on fd 8, through a
        // DEBUG trap -- which fires once per simple command, pipeline members included, and not
        // inside function bodies (measured; `set -T` would descend into them). Quote characters are
        // stripped from the word, as bash removes them. Every HARMLESS command is a builtin, so an
        // approved line runs under the empty PATH without any binary.
        let ran = tempfile::NamedTempFile::new().expect("command log");
        let script = format!(
            "exec 9>'{}'\n\
             exec 8>'{}'\n\
             jkb() {{ printf '\u{2}' >&9; for a in \"$@\"; do printf '%s\u{1}' \"$a\" >&9; done; }}\n\
             __ran() {{ local w=${{BASH_COMMAND%% *}}; w=${{w//\\'/}}; w=${{w//\\\"/}}; printf '%s\u{1}' \"$w\" >&8; }}\n\
             trap __ran DEBUG\n\
             {cmd}",
            capture.path().display(),
            ran.path().display()
        );
        // With `executable`, a `jkb` program is on `PATH` as well as the function, reporting the same
        // way: a function is visible only to this shell, so `sh -c "jkb …"`, `xargs jkb` and
        // `env -S "jkb …"` never reached the oracle, and the hidden jkb they run was invisible to
        // every test (review round 7). Its fd 9 is inherited from this shell.
        let bin = tempfile::TempDir::new().expect("bin dir");
        if executable {
            let fake = bin.path().join("jkb");
            std::fs::write(
                &fake,
                "#!/bin/sh\nprintf '\\002' >&9\nfor a in \"$@\"; do printf '%s\\001' \"$a\" >&9; done\n",
            )
            .expect("fake jkb");
            std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .expect("chmod fake jkb");
            // And one in the working directory, for a row that reaches jkb by a relative glob.
            std::fs::copy(&fake, cwd.join("jkb")).expect("fake jkb in cwd");
        }
        let mut c = Command::new("/bin/bash");
        c.arg("-c").arg(&script).current_dir(cwd);
        // Nothing is inherited, so -- unless `executable` -- "nothing is executed" is a property of
        // this function rather than of whatever `ALLOWED` happens to hold. `bash -c` sources `$BASH_ENV`, which would let the
        // developer's shell configure the oracle for a security classifier; an inherited `PATH`
        // would let a row that is one bad edit away from approving too much actually run something;
        // and an inherited `GIT_DIR`/`GIT_WORK_TREE` is the measured damage the crate's spawn guard
        // exists to prevent. With an empty `PATH` a mis-approved row reaches no binary at all.
        c.env_clear();
        // Empty, so a mis-approved row reaches no binary -- except, with `executable`, the fake and
        // the system directories `sh`, `xargs` and `env` live in, for the shapes that need them.
        if executable {
            c.env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()));
        } else {
            c.env("PATH", "");
        }
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().expect("bash is required to run this test");
        let report = std::fs::read_to_string(capture.path()).expect("the capture is utf-8");
        let calls = report.matches('\u{2}').count();
        let mut args: Vec<String> = report
            .replace('\u{2}', "")
            .split('\u{1}')
            .map(str::to_owned)
            .collect();
        args.pop(); // the empty tail after the final separator
        let mut commands: Vec<String> = std::fs::read_to_string(ran.path())
            .expect("the command log is utf-8")
            .split('\u{1}')
            .map(str::to_owned)
            .collect();
        commands.pop();
        (calls, args, commands)
    }

    /// The table above asserts what the classifier decides; this asserts the decision was about the
    /// command bash actually runs. Every approved command goes through a real bash and must come
    /// back as exactly one `jkb` call, passing exactly the words the model said, with no argument
    /// equal to `land`.
    ///
    /// It exists because a table row could not have caught either defect it was added for. The word
    /// reader split on `char::is_whitespace` while bash splits on `IFS` alone, so `jkb\u{a0}./x`
    /// was approved as the two words `jkb ./x` while bash executed the single relative path
    /// `jkb\u{a0}./x`. And a bare `~` was approved as the literal word `~` while bash expanded it
    /// to `$HOME` -- so the run under a hostile `HOME`/`OLDPWD` below is the half a word comparison
    /// against the ambient environment cannot see. Measured against GNU bash in the dev container.
    #[test]
    fn every_approved_command_runs_one_jkb_and_passes_the_modelled_words() {
        // A real `land` directory, so the hostile `OLDPWD` below survives bash's own validation.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let cwd = dir.path();
        std::fs::create_dir(cwd.join("land")).expect("a `land` directory to point OLDPWD at");
        for cmd in ALLOWED {
            let (calls, args, _) = bash_argv(cmd, &[], cwd, false);
            assert_eq!(
                calls, 1,
                "bash did not run exactly one `jkb`: {cmd:?} -> {args:?}"
            );
            let (modelled, _) =
                shell_commands(cmd.trim_matches(is_blank)).expect("approved, so modelled");
            let [only] = modelled.as_slice() else {
                panic!("an approved command is one command: {cmd:?}")
            };
            let model = &only[1..];

            // Compared per word, not per command: a tilde word is the one thing left to expand, and
            // keying the exemption on the whole command would stop comparing every OTHER word in it.
            // The length is always compared -- tilde expansion is not word-split (measured:
            // `HOME='/x y'` makes `~/z` one word), so a word appearing or vanishing is a defect.
            assert_eq!(args.len(), model.len(), "word count for {cmd:?}: {args:?}");
            for (got, want) in args.iter().zip(model) {
                assert!(
                    got == want || want.starts_with('~'),
                    "word mismatch for {cmd:?}: {args:?} vs {model:?}"
                );
            }
            assert!(
                !args.iter().any(|a| a == "land"),
                "bash passed the word `land` from an approved command: {cmd:?} -> {args:?}"
            );

            // The expansions a tilde reads are ordinary variables, so approving a tilde word means
            // approving whatever they hold. Nothing here may become a bare subcommand.
            //
            // `OLDPWD` is set to a directory that EXISTS: bash discards an inherited `OLDPWD`
            // naming a missing one (measured -- it was silently empty here, so `~-` stayed literal
            // and this half of the check proved nothing). The call count is asserted too, so an
            // empty `hostile` cannot pass `.any()` trivially.
            let (calls, hostile, _) = bash_argv(
                cmd,
                &[("HOME", "land"), ("OLDPWD", "land"), ("PWD", "land")],
                cwd,
                false,
            );
            assert_eq!(calls, 1, "the hostile run did not reach `jkb`: {cmd:?}");
            assert!(
                !hostile.iter().any(|a| a == "land"),
                "a hostile HOME made an approved command pass `land`: {cmd:?} -> {hostile:?}"
            );
        }
    }

    /// The one property an approval rests on, measured in bash rather than read off the model: on a
    /// line the classifier APPROVES, bash runs nothing but `jkb` and HARMLESS commands. Every row of
    /// the classification table goes through, approved or not, so a classifier that wrongly
    /// approved a line with anything else on it fails here. The approved lists were first checked
    /// only against the model, and the bash oracle took single commands only (review of the
    /// never-ask change).
    #[test]
    fn run_through_bash_an_approved_line_runs_nothing_but_jkb_and_harmless_commands() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let cwd = dir.path();
        std::fs::create_dir(cwd.join("repo")).expect("a `repo` directory for `cd repo`");
        let mut approved = 0;
        let rows = ALLOWED
            .iter()
            .chain(APPROVED_LINES)
            .chain(UNMODELLED)
            .chain(OTHER)
            .chain(MENTIONS);
        for line in rows {
            if attestation(line) != Attestation::Allow {
                continue;
            }
            approved += 1;
            let (calls, _, ran) = bash_argv(line, &[], cwd, false);
            assert!(calls >= 1, "bash ran no jkb in approved {line:?}: {ran:?}");
            for word in &ran {
                assert!(
                    word == "jkb" || HARMLESS.contains(&word.as_str()),
                    "approved {line:?} ran `{word}` ({ran:?})"
                );
            }
        }
        assert_eq!(
            approved,
            ALLOWED.len() + APPROVED_LINES.len(),
            "the approved rows are exactly the approved lists"
        );
    }

    /// Run through bash rather than trusted: a line carrying a bare redirect, comment, here-doc, `!`
    /// or carriage return is never approved -- bash may run it differently from how the model reads
    /// it (review round 6 measured a redirect operator spelled with `&` or `|` read as a separator,
    /// and a quote in a comment or here-doc body opening a quote bash never saw). Each row really
    /// reaches a `jkb` in bash, so the line is one the hook tickets and must not approve.
    #[test]
    fn run_through_bash_a_redirect_comment_or_heredoc_is_never_approved() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let cwd = dir.path();
        for cmd in [
            "jkb task show x 2>&1",
            "jkb task show x 2>/dev/null",
            "jkb task show x > out",
            "jkb task show x < /dev/null",
            "jkb task show x # land",
            "jkb task show x #; jkb task land y",
            "jkb task >out land x",
            "jkb>out task land x",
            "jkb task land>out x",
            "jkb task land 2>&1",
            "jkb task show x &> out",
            "jkb task show x >> out",
            "! jkb task land x",
            "jkb task la#nd x",
            // Review round 6, both measured: a redirect operator spelled with `&` or `|` was split
            // as a command separator, leaving the `land` in a "command" with no jkb word ...
            "jkb task 2>&1 land x",
            "jkb task >&2 land x",
            "jkb task &>out land x",
            "jkb task &>>out land x",
            "jkb task >|out land x",
            // ... and a quote inside a comment or a here-doc body is text to bash but opened a quote
            // in the model, which then swallowed the real `land` line after it.
            "jkb task show x # it's\njkb task land y # '",
            "cat <<A\n'\nA\njkb task land y\ncat <<B\n'\nB",
            "jkb task show x\r",
        ] {
            assert_eq!(
                attestation(cmd),
                Attestation::Defer,
                "{cmd:?} may run differently from how it is read, so it is not approved"
            );
            let (calls, _, _) = bash_argv(cmd, &[], cwd, false);
            // At least one: a comment or here-doc fixture spans lines and can run jkb on each. A row
            // bash never ran jkb in is not a line the hook would ticket, and checks nothing.
            assert!(
                calls >= 1,
                "bash must reach `jkb` in {cmd:?}, or this checks nothing"
            );
        }
    }

    /// A line that runs a jkb out of the model's sight -- through `sh -c`, `bash -c`, `xargs`,
    /// `env -S`, a glob -- is never approved: the session's own rules judge it. Each row really runs
    /// a `jkb` program in bash (round 7's oracle could not see these, because its only `jkb` was a
    /// shell function, which a child shell or `xargs` never calls). What that jkb may then DO with
    /// the inherited ticket is RBAC's to decide, as for any jkb.
    #[test]
    fn run_through_bash_a_line_that_runs_jkb_out_of_sight_is_never_approved() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let cwd = dir.path();
        for cmd in [
            "sh -c 'jkb task land x'",
            "bash -c 'jkb task land x'",
            "sh -c \"j''kb task land x --gate id\"",
            "echo land y | xargs jkb task",
            "echo x | xargs jkb task land",
            "env -S 'jkb task land z'",
            "jkb task show x; sh -c 'jkb task land y'",
            "jkb task show x && echo land | xargs jkb task",
            // (`sudo jkb …` cannot run here at all; the static table pins it as deferred.)
            "time -p jkb task land x",
            // Round 8: a glob inside quotes is not a jkb word to the lexer, and is to `sh`. Aimed
            // at the fake in the working directory: with `~`, and no HOME in the child, the glob
            // matched nothing and the row could never reach a jkb (round 9).
            "jkb task show x; sh -c './jk? task land x'",
        ] {
            let (calls, args, _) = bash_argv(cmd, &[], cwd, true);
            // Every row is built so that some jkb really runs, with `land` in its argv: one that did
            // not would check nothing, whatever the classifier said (round 9 found a row that could not).
            assert!(
                calls >= 1 && args.iter().any(|a| a == "land"),
                "bash never ran a jkb with `land` in {cmd:?} ({args:?}), so this row checks nothing"
            );
            assert_eq!(
                attestation(cmd),
                Attestation::Defer,
                "{cmd:?} runs a jkb the model does not see, so it is not approved"
            );
        }
        // Mentioning jkb where nothing runs it needs no ticket at all.
        assert_eq!(attestation("echo jkb"), Attestation::Skip);
        // Spelled with quotes inside the word, bash still runs `jkb`, and the line is approved like
        // the plain spelling: quote removal is modelled.
        assert_eq!(attestation("j''kb task land x"), Attestation::Allow);
    }

    /// Every character in either list is the SOLE reason its command is not approved. The template
    /// is approved as it stands, so the character is the only thing that changes the verdict, and a
    /// character leaving a list fails here by construction.
    ///
    /// This exists because the tables could not enforce it: `;`, `|`, `<`, `(`, `)`, bare `!`, `?`,
    /// `[`, `]` and `\r` had no fixture that turned on them -- `curl … | sh; echo jkb` is not
    /// approved because its first word is `curl`, and `jkb task show x # land` because it mentions
    /// `land` -- so deleting one of them from the lists failed no test. That is how `~` was removed
    /// from a list with the whole suite green, taking a real hole with it.
    ///
    /// The lists answer differently on purpose. A SEPARATOR leaves commands behind that can each
    /// still be judged -- `jkb …; true` is two commands, both approvable. A FATAL or UNQUOTABLE
    /// character leaves nothing to judge, so the line is not approved, and the session's own rules
    /// decide it.
    #[test]
    fn every_listed_character_is_the_only_reason_its_command_is_not_approved() {
        // Spelled out here rather than read from the constants. A loop over a constant cannot
        // notice a character LEAVING that constant -- it just stops testing it -- which is the very
        // way `~` was dropped with the suite green. Measured: with the loop reading the constant,
        // deleting `;` from it failed no test.
        const SPLITS: &[char] = &[';', '&', '|', '\n'];
        const STOPS: &[char] = &['<', '>', '(', ')', '{', '}', '#', '*', '?', '[', ']'];
        const BREAK: &[char] = &['!', '\r'];
        const NEVER: &[char] = &['$', '`', '\\'];
        assert_eq!(
            SEPARATORS, SPLITS,
            "SEPARATORS changed: change this list too, deliberately"
        );
        assert_eq!(
            FATAL, STOPS,
            "FATAL changed: change this list too, deliberately"
        );
        assert_eq!(
            UNQUOTABLE, NEVER,
            "UNQUOTABLE changed: change this list too, deliberately"
        );
        assert_eq!(
            BREAKS, BREAK,
            "BREAKS changed: change this list too, deliberately"
        );
        // Growing HARMLESS is the dangerous direction -- each member may share a line with a
        // ticketed jkb -- so it is pinned like the character lists.
        assert_eq!(
            HARMLESS,
            &["cd", "true", "false", ":", "echo"],
            "HARMLESS changed: check the new member for an exec, a file-read AND a file-write path, \
             then change this too"
        );
        assert_eq!(
            attestation("jkb task show ab"),
            Attestation::Allow,
            "template"
        );
        for &c in SPLITS {
            assert_eq!(
                attestation(&format!("jkb task show a{c}true")),
                Attestation::Allow,
                "bare {c:?} ends the command; `true` after it is harmless, so the list is approved"
            );
            assert_eq!(
                attestation(&format!("jkb task show a{c}rm b")),
                Attestation::Defer,
                "bare {c:?} ends the command; `rm` after it is not harmless"
            );
            assert_eq!(
                attestation(&format!("jkb task show 'a{c}b'")),
                Attestation::Allow,
                "quoted {c:?} is argument text"
            );
        }
        for &c in STOPS {
            assert_eq!(
                attestation(&format!("jkb task show a{c}b")),
                Attestation::Defer,
                "bare {c:?} leaves nothing to judge, so the line is not approved"
            );
            assert_eq!(
                attestation(&format!("jkb task show 'a{c}b'")),
                Attestation::Allow,
                "quoted {c:?} is argument text"
            );
        }
        for &c in BREAK {
            assert_eq!(
                attestation(&format!("jkb task show a{c}b")),
                Attestation::Defer,
                "bare {c:?} makes it more than a plain invocation, but cannot hide a `land`"
            );
            assert_eq!(
                attestation(&format!("jkb task show 'a{c}b'")),
                Attestation::Allow,
                "quoted {c:?} is argument text"
            );
        }
        for &c in NEVER {
            for spelling in [
                format!("jkb task show a{c}b"),
                format!("jkb task show 'a{c}b'"),
            ] {
                assert_eq!(
                    attestation(&spelling),
                    Attestation::Defer,
                    "{c:?} is never approved, wherever it appears"
                );
            }
        }
        // `~` is in no list, so it is pinned here rather than by the loops above.
        for bare in ["jkb task show ~", "jkb task show ~+", "jkb task show ~-"] {
            assert_eq!(attestation(bare), Attestation::Defer, "{bare}");
        }
        assert_eq!(
            attestation("jkb --db ~/.jkb/jkb.db task show x"),
            Attestation::Allow,
            "a tilde word carrying a `/` keeps it however it expands"
        );
    }

    #[test]
    fn only_a_command_that_visibly_runs_jkb_gets_a_ticket() {
        for yes in [
            "jkb task show x",
            "cd repo && jkb workflow next",
            "~/.cargo/bin/jkb role whoami",
            "echo hi; jkb ls",
            "(jkb ls)",
            // Glued to a redirect, or quoted: bash still runs `jkb`.
            "jkb>out task land x",
            "jkb<in ls",
            "'jkb' task show x",
            "\"jkb\" ls",
            "j''kb task land x",
            "j'k'b task land x",
            "! jkb ls",
        ] {
            assert!(runs_jkb(yes), "{yes}");
        }
        for no in [
            "echo jkb-core",
            "cargo build -p jkb-cli",
            "ls ~/.jkb",
            "grep jkbx f",
        ] {
            assert!(!runs_jkb(no), "{no}");
        }
    }
}
