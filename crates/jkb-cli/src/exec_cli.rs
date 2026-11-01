//! `jkb exec`: a session asks the operator to run one command for it, somewhere its own sandbox
//! cannot reach, and waits for the answer (the Executions design).
//!
//! The request is a record ([`jkb_core::exec`]), made through the `exec.*` ops: the argv as a list
//! (run without a shell), the directory's real path, the target, a reason, and the tree pin of the
//! directory's repository, computed here with jkb's hooks-off git ([`crate::gitrepo::tree_pin`]).
//! Nothing here runs the command — the operator's runner does, after reading exactly what would run.
//!
//! `jkb exec request` waits by default: it follows the request's topic (`exec.follow`), writes the
//! output to stdout as it arrives, and exits with the run's status. The topic is a hint; the answer is always
//! confirmed with `exec.status` before it is acted on.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use clap::{Subcommand, ValueEnum};
use jkb_api::exec::{ExecAsk, ExecView, Pin};
use jkb_api::{ApiError, Backend, ErrorCode, Request, Response};

/// Exit status for a declined request.
pub const EXIT_DECLINED: i32 = 77;
/// Exit status for a refused request.
pub const EXIT_REFUSED: i32 = 78;
/// Exit status for an expired request.
pub const EXIT_EXPIRED: i32 = 79;
/// Exit status for a run that timed out or was interrupted (as `timeout(1)`).
pub const EXIT_TIMED_OUT: i32 = 124;

/// Where a request runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TargetArg {
    /// On the host.
    Host,
    /// In the dev container, outside the agent's sandbox.
    Container,
}

impl TargetArg {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Container => "container",
        }
    }
}

/// `jkb exec …`.
#[derive(Debug, Subcommand)]
pub enum ExecCmd {
    /// Ask the operator to run one command, and wait for its output and exit status.
    ///
    /// The command is argv, run without a shell: a pipeline names `sh -c '<script>'` explicitly, so
    /// the operator sees a shell is involved. Exits with the command's status (128+n for a signal,
    /// 124 for a timeout); 77 if the operator declined (the note is printed), 78 if it was refused,
    /// 79 if it expired.
    Request {
        /// Where it runs: `host`, or `container` (outside the agent's sandbox).
        #[arg(long, value_enum)]
        target: TargetArg,
        /// The directory it runs in.
        #[arg(long)]
        dir: PathBuf,
        /// How long it may run: `90s`, `15m`, `2h` (default 15m, at most 2h).
        #[arg(long, value_parser = parse_timeout)]
        timeout: Option<i64>,
        /// One sentence for the operator: why the session needs this.
        #[arg(long, default_value = "")]
        reason: String,
        /// Print the request id and return at once; read the outcome with `jkb exec status`.
        #[arg(long)]
        no_wait: bool,
        /// The program and its arguments, after `--`.
        #[arg(last = true, required = true, num_args = 1..)]
        argv: Vec<String>,
    },
    /// A request's state, and its output once it ran.
    Status {
        /// `exec-<n>`.
        id: String,
    },
    /// Decline a pending request, with a note saying what to ask for instead (the operator's).
    Decline {
        /// `exec-<n>`.
        id: String,
        /// What the session should ask for instead.
        #[arg(long)]
        note: String,
    },
    /// Run requests: every session's for the operator, a session's own for it.
    List {
        /// Only requests in this state (`pending`, `running`, `finished`, …).
        #[arg(long)]
        state: Option<String>,
    },
    /// The tree pin of the repository containing a directory — what a request would record — and the
    /// `.gitignore` files it applied. Opens no database.
    Pin {
        /// The directory.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// A clean checkout of main (the host's own clone) whose `.gitignore` set the pinned tree's
        /// must equal: any difference is printed and exits 1, because the ignore rules decide what
        /// the pin leaves out and must be main's.
        #[arg(long)]
        against: Option<PathBuf>,
    },
    /// The base a request's diff is read against: the default branch of the directory's `origin`,
    /// verified on the host with `git ls-remote` — or, when that fails, read from `--clone`, a clone
    /// of the same remote the container cannot write. Opens no database. With `--json`, one object:
    /// `{dir, base: {sha, ref, source, url}}`, `{dir, unverified: {message}}`, or `{dir, repo: null}`.
    Base {
        /// The directory.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// A clean clone of the same remote, consulted when the remote cannot be asked. Its remote
        /// is trusted.
        #[arg(long)]
        clone: Option<PathBuf>,
        /// A remote URL the operator trusts (Code Factory passes `~/.jkb/trusted-remotes`). The
        /// directory's `origin` must be one of these or the clone's, or the base is unverified and
        /// nothing is asked of the network.
        #[arg(long)]
        trust: Vec<String>,
    },
    /// The tree a request's pin covers, diffed against a verified base commit (`jkb exec base`):
    /// tracked changes and untracked files alike, read as the pin reads them. Opens no database.
    /// With `--json`, one object: `{dir, top, base, pin, files, files_truncated, patch, truncated}`,
    /// `{dir, repo: null}`, or `{dir, refused: {reason, message}}` (`tree_changed`, `unpinnable`).
    Diff {
        /// The directory.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// The base commit: a full object id.
        #[arg(long)]
        base: String,
        /// Another repository to copy the base's objects from when the directory's has none (the
        /// clean clone the base came from).
        #[arg(long)]
        objects_from: Vec<PathBuf>,
        /// The most bytes of patch printed; past it the patch is cut and `truncated` is set.
        #[arg(long, default_value_t = DIFF_MAX_BYTES)]
        max: usize,
        /// A program path to ask about: `program_pinned` says whether the pin covers it.
        #[arg(long)]
        program: Option<PathBuf>,
    },
}

/// `jkb exec diff`'s default cap: 2 MiB of patch.
pub const DIFF_MAX_BYTES: usize = 2 * 1024 * 1024;

/// `90s`, `15m`, `2h`, or bare seconds, as milliseconds.
fn parse_timeout(s: &str) -> Result<i64, String> {
    let (digits, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => s.split_at(i),
        None => (s, "s"),
    };
    let n: i64 = digits
        .parse()
        .map_err(|_| format!("{s:?} is not a duration like 90s, 15m or 2h"))?;
    let scale = match unit {
        "s" => 1000,
        "m" => 60 * 1000,
        "h" => 60 * 60 * 1000,
        _ => return Err(format!("{s:?}: the unit is s, m or h")),
    };
    n.checked_mul(scale)
        .ok_or_else(|| format!("{s:?} is too long"))
}

/// Whether this process runs in the dev container — so the paths it names are the container's.
fn dir_side() -> TargetArg {
    if crate::session::in_container() {
        TargetArg::Container
    } else {
        TargetArg::Host
    }
}

/// Refuse a host run of a container directory the host cannot see: only `~/repos` is shared.
fn check_visible(dir: &Path, side: TargetArg, target: TargetArg) -> Result<()> {
    if side != TargetArg::Container || target != TargetArg::Host {
        return Ok(());
    }
    let repos = Path::new(crate::session::CONTAINER_REPOS);
    if !dir.starts_with(repos) {
        bail!(
            "{} is not under {}: the host sees only the container's ~/repos, so it cannot run a \
             command in this directory — ask for --target container, or use a directory under ~/repos",
            dir.display(),
            repos.display()
        );
    }
    Ok(())
}

fn call(backend: &dyn Backend, request: Request) -> Result<Response> {
    backend.call(request).map_err(|e| anyhow::anyhow!("{e}"))
}

fn expect_view(r: Response) -> Result<ExecView> {
    match r {
        Response::ExecRequest { request } => Ok(request),
        other => bail!("unexpected response: {other:?}"),
    }
}

/// Run `cmd`.
///
/// # Errors
/// A refused op, an unreadable directory, or a pin that cannot be computed.
#[allow(clippy::too_many_lines)] // one arm per verb, like `mq_cli::run`
pub fn run(backend: &dyn Backend, cmd: ExecCmd, json: bool) -> Result<()> {
    match cmd {
        ExecCmd::Pin { dir, against } => pin(&dir, against.as_deref(), json),
        ExecCmd::Base { dir, clone, trust } => base(&dir, clone.as_deref(), &trust, json),
        ExecCmd::Diff {
            dir,
            base,
            objects_from,
            max,
            program,
        } => diff(&dir, &base, &objects_from, max, program.as_deref(), json),
        ExecCmd::Request {
            target,
            dir,
            timeout,
            reason,
            no_wait,
            argv,
        } => {
            let dir = std::fs::canonicalize(&dir)
                .with_context(|| format!("resolving {}", dir.display()))?;
            let side = dir_side();
            check_visible(&dir, side, target)?;
            let pin = crate::gitrepo::tree_pin(&dir)?;
            let dir_text = dir
                .to_str()
                .with_context(|| format!("{} is not valid UTF-8", dir.display()))?
                .to_owned();
            let ask = ExecAsk {
                target: target.as_str().to_owned(),
                dir: dir_text,
                dir_side: side.as_str().to_owned(),
                argv,
                reason,
                pin: pin.map(Pin::from),
                timeout_ms: timeout,
            };
            let (view, folded) = match call(backend, Request::ExecRequest(ask))? {
                Response::ExecRequested { request, folded } => (request, folded),
                other => bail!("unexpected response: {other:?}"),
            };
            if no_wait {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({ "request": view, "folded": folded })
                    );
                } else {
                    println!("{}{}", view.id, if folded { " (folded)" } else { "" });
                }
                return Ok(());
            }
            eprintln!(
                "jkb exec: asked the operator to run this ({}{}); waiting for the answer",
                view.id,
                if folded {
                    ", folded into an identical pending request"
                } else {
                    ""
                }
            );
            let stdout = std::io::stdout();
            let stderr = std::io::stderr();
            let code = wait(
                backend,
                &view,
                &mut stdout.lock(),
                &mut stderr.lock(),
                Duration::from_millis(500),
            )
            .with_context(|| {
                format!(
                    "stopped waiting for {id}; it is still recorded — read its outcome with `jkb \
                     exec status {id}`",
                    id = view.id
                )
            })?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        ExecCmd::Status { id } => {
            let view = expect_view(call(backend, Request::ExecStatus { id })?)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&view)?);
            } else {
                print_view(&view, &mut std::io::stdout().lock())?;
            }
            Ok(())
        }
        ExecCmd::Decline { id, note } => {
            let view = expect_view(call(backend, Request::ExecDecline { id, note })?)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&view)?);
            } else {
                println!("{} declined", view.id);
            }
            Ok(())
        }
        ExecCmd::List { state } => {
            let mut after = None;
            let mut all = Vec::new();
            loop {
                match call(
                    backend,
                    Request::ExecList {
                        state: state.clone(),
                        after,
                        newest_first: false,
                    },
                )? {
                    Response::ExecRequests { requests, next } => {
                        all.extend(requests);
                        match next {
                            Some(n) => after = Some(n),
                            None => break,
                        }
                    }
                    other => bail!("unexpected response: {other:?}"),
                }
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&all)?);
            } else if all.is_empty() {
                println!("no run requests");
            } else {
                for v in &all {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        v.id,
                        v.state,
                        v.target,
                        v.session,
                        v.argv
                            .iter()
                            .map(|a| format!("{a:?}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    );
                }
            }
            Ok(())
        }
    }
}

/// Why `jkb exec pin` could not pin a directory, when the cause is identified, for a runner that
/// must answer the request ([`pin_refusal`]). The first two are `exec.refuse` reasons of the same
/// names; an unpinnable tree is a pin the host cannot reproduce, which refuses the request too
/// (`tree_changed`). A fault that is none of these — a permission error, git not running, a full
/// temporary directory — is **not** classified: the runner reports it and refuses nothing, because a
/// refusal consumes the request under a reason that would be false.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinRefusal {
    /// The directory does not exist (`ENOENT`), or a component of it is not a directory (`ENOTDIR`).
    DirGone,
    /// The hooks-off git audit's own finding: a key or a layout it refuses, or a config git cannot
    /// read at all ([`crate::gitrepo::ConfigUnreadable`]).
    AuditRefused,
    /// [`crate::gitrepo::NotPinnable`]: a tree the pin refuses to cover, or a repository with no
    /// commit.
    Unpinnable,
}

impl PinRefusal {
    /// The wire name, in `jkb exec pin --json`'s `refused.reason`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DirGone => "dir_gone",
            Self::AuditRefused => "audit_refused",
            Self::Unpinnable => "unpinnable",
        }
    }
}

/// Whether an error of type `T` is anywhere in `e`'s chain.
fn in_chain<T: std::error::Error + 'static>(e: &anyhow::Error) -> bool {
    e.chain().any(<dyn std::error::Error>::is::<T>)
}

/// Whether the audit's error `e` is its own finding rather than a fault on the way. The rule
/// `git_audit`'s `kind_of` reads the same chain by: an `io::Error` in it is git not running or a
/// path not readable, which judged nothing — except [`crate::gitrepo::ConfigUnreadable`], a config
/// git could not parse, which is no more trustworthy than a planted one. The findings themselves are
/// built with no `io::Error` beneath them.
fn is_audit_finding(e: &anyhow::Error) -> bool {
    in_chain::<crate::gitrepo::ConfigUnreadable>(e) || !in_chain::<std::io::Error>(e)
}

/// `dir` resolved, audited, and pinned — or why not: the [`PinRefusal`] when its cause is
/// identified, `None` for a fault. The audit is asked on its own first, though every git call
/// [`crate::gitrepo::pin_tree`] makes asks it again, so that its refusal is told apart from a tree
/// that cannot be pinned.
fn pin_refusal(
    dir: &Path,
) -> Result<(PathBuf, Option<crate::gitrepo::PinnedTree>), (Option<PinRefusal>, anyhow::Error)> {
    let dir = std::fs::canonicalize(dir).map_err(|e| {
        let gone = matches!(
            e.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        );
        (
            gone.then_some(PinRefusal::DirGone),
            anyhow::Error::new(e).context(format!("resolving {}", dir.display())),
        )
    })?;
    crate::gitrepo::audit_repo_config(&dir)
        .map_err(|e| (is_audit_finding(&e).then_some(PinRefusal::AuditRefused), e))?;
    let tree = crate::gitrepo::pin_tree(&dir).map_err(|e| {
        let unpinnable = in_chain::<crate::gitrepo::NotPinnable>(&e);
        (unpinnable.then_some(PinRefusal::Unpinnable), e)
    })?;
    Ok((dir, tree))
}

/// The one object `jkb exec pin --json` prints when it could not pin `dir`: `{"dir", "refused":
/// {"reason", "message"}}` for an identified cause, `{"dir", "error": {"message"}}` for a fault. The
/// runner refuses a request only on `refused` (Code Factory's `parsePinOutput`).
fn pin_failure_json(
    dir: &Path,
    reason: Option<PinRefusal>,
    e: &anyhow::Error,
) -> serde_json::Value {
    let message = format!("{e:#}");
    match reason {
        Some(r) => serde_json::json!({
            "dir": dir,
            "refused": { "reason": r.as_str(), "message": message },
        }),
        None => serde_json::json!({ "dir": dir, "error": { "message": message } }),
    }
}

/// `jkb exec pin`: run before any database or daemon is reached.
///
/// With `--json`, a directory that cannot be pinned still prints one object on stdout
/// ([`pin_failure_json`]) before the command fails, so a runner reads why from the output rather
/// than from the wording of an error ([`PinRefusal`]).
///
/// # Errors
/// An unreadable directory, or a pin that cannot be computed.
pub fn pin(dir: &Path, against: Option<&Path>, json: bool) -> Result<()> {
    let (dir, tree) = match pin_refusal(dir) {
        Ok(v) => v,
        Err((reason, e)) => {
            if json {
                println!("{}", pin_failure_json(dir, reason, &e));
            }
            return Err(e);
        }
    };
    // The `.gitignore` files that decided what the pin leaves out — the set the host holds to main's.
    let ignores: Vec<String> = tree
        .as_ref()
        .map(|t| {
            t.gitignores
                .keys()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect()
        })
        .unwrap_or_default();
    let drift: Option<Vec<String>> = match (against, tree.as_ref()) {
        (Some(main), Some(t)) => {
            let main = std::fs::canonicalize(main)
                .with_context(|| format!("resolving {}", main.display()))?;
            let main_tree = crate::gitrepo::pin_tree(&main)?
                .with_context(|| format!("{} is in no repository", main.display()))?;
            Some(
                crate::gitrepo::ignore_rules_drift(&t.gitignores, &main_tree.gitignores)
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            )
        }
        _ => None,
    };
    let pin = tree.map(|t| Pin::from(t.pin));
    if json {
        println!(
            "{}",
            serde_json::json!({ "dir": dir, "pin": pin, "gitignores": ignores, "ignore_drift": drift })
        );
    } else {
        match pin {
            Some(p) => {
                println!("head {} (a label only)\nchanges {}", p.head, p.changes);
                for g in &ignores {
                    println!("ignore rules {g}");
                }
            }
            None => println!("{} is in no repository: no pin", dir.display()),
        }
        for d in drift.iter().flatten() {
            println!("this tree {d} relative to main");
        }
    }
    if drift.is_some_and(|d| !d.is_empty()) {
        bail!("this request changes the ignore rules; land that change first");
    }
    Ok(())
}

/// The `jkb exec` verbs that open no database — `pin`, `base`, `diff` — run here, before any database
/// or daemon is reached, in any mode; `None` for any other verb.
#[must_use]
pub fn no_database(cmd: &ExecCmd, json: bool) -> Option<Result<()>> {
    Some(match cmd {
        ExecCmd::Pin { dir, against } => pin(dir, against.as_deref(), json),
        ExecCmd::Base { dir, clone, trust } => base(dir, clone.as_deref(), trust, json),
        ExecCmd::Diff {
            dir,
            base,
            objects_from,
            max,
            program,
        } => diff(dir, base, objects_from, *max, program.as_deref(), json),
        _ => return None,
    })
}

/// `jkb exec base`: run before any database or daemon is reached.
///
/// # Errors
/// An unreadable directory, the audit's refusal, or git not running. A base that cannot be verified
/// is an answer (`unverified`), not an error.
pub fn base(dir: &Path, clone: Option<&Path>, trust: &[String], json: bool) -> Result<()> {
    let dir = std::fs::canonicalize(dir).with_context(|| format!("resolving {}", dir.display()))?;
    let clone = clone.map(std::fs::canonicalize).transpose()?;
    match crate::gitrepo::trusted_base(&dir, clone.as_deref(), trust) {
        Ok(Some(b)) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "dir": dir, "base": {
                        "sha": b.sha, "ref": b.ref_name, "source": b.source.as_str(), "url": b.url,
                    } })
                );
            } else {
                println!(
                    "{} {} (from {}, {})",
                    b.sha,
                    b.ref_name,
                    b.url,
                    b.source.as_str()
                );
            }
            Ok(())
        }
        Ok(None) => {
            if json {
                println!("{}", serde_json::json!({ "dir": dir, "repo": null }));
            } else {
                println!("{} is in no repository: no base", dir.display());
            }
            Ok(())
        }
        Err(e) if e.is::<crate::gitrepo::BaseUnverified>() => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "dir": dir, "unverified": { "message": e.to_string() } })
                );
            } else {
                println!("base unverified: {e}");
            }
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// `jkb exec diff`: run before any database or daemon is reached.
///
/// # Errors
/// An unreadable directory, a malformed base, a base with no objects here, the audit's refusal, or
/// git failing. With `--json` an identified refusal (the tree moved, or cannot be pinned) is printed
/// as one object before the command fails.
pub fn diff(
    dir: &Path,
    base: &str,
    objects_from: &[PathBuf],
    max: usize,
    program: Option<&Path>,
    json: bool,
) -> Result<()> {
    let dir = std::fs::canonicalize(dir).with_context(|| format!("resolving {}", dir.display()))?;
    let objects_from = objects_from
        .iter()
        .map(std::fs::canonicalize)
        .collect::<std::io::Result<Vec<_>>>()
        .context("resolving --objects-from")?;
    let d = match crate::gitrepo::tree_diff(&dir, base, &objects_from, max, program) {
        Ok(Some(d)) => d,
        Ok(None) => {
            if json {
                println!("{}", serde_json::json!({ "dir": dir, "repo": null }));
            } else {
                println!("{} is in no repository: no diff", dir.display());
            }
            return Ok(());
        }
        Err(e) => {
            let reason = if in_chain::<crate::gitrepo::TreeMoved>(&e) {
                Some("tree_changed")
            } else if in_chain::<crate::gitrepo::NotPinnable>(&e) {
                Some("unpinnable")
            } else {
                None
            };
            if let (true, Some(r)) = (json, reason) {
                println!(
                    "{}",
                    serde_json::json!({ "dir": dir, "refused": { "reason": r, "message": format!("{e:#}") } })
                );
            }
            return Err(e);
        }
    };
    if json {
        let files: Vec<serde_json::Value> = d
            .files
            .iter()
            .map(|(status, path)| serde_json::json!({ "status": status, "path": path }))
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "dir": dir, "top": d.top, "base": d.base, "pin": Pin::from(d.pin),
                "files": files, "files_truncated": d.files_truncated,
                "patch": d.patch, "truncated": d.truncated, "program_pinned": d.program_pinned,
            })
        );
    } else {
        for (status, path) in &d.files {
            println!("{status}\t{path:?}");
        }
        print!("{}", escape_for_terminal(&d.patch));
        if d.program_pinned == Some(false) {
            println!("[jkb: the program is outside the pin: it can change before Run]");
        }
        if d.truncated {
            println!("\n[jkb: the diff is longer than {max} bytes and was cut here]");
        }
    }
    Ok(())
}

/// Text from a repository as a terminal may be handed it: every control character but a newline
/// and a tab (C0, DEL, C1), and every bidi or invisible formatting character, as `\u{…}`, so file
/// content cannot move the cursor, erase lines or reorder what the operator reads.
fn escape_for_terminal(text: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let invisible = matches!(
            c,
            '\u{00ad}'
                | '\u{061c}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
        );
        if (c.is_control() && c != '\n' && c != '\t') || invisible {
            let _ = write!(out, "\\u{{{:04x}}}", u32::from(c));
        } else {
            out.push(c);
        }
    }
    out
}

/// A request, for a person: every argument on its own line, escaped.
fn print_view(v: &ExecView, out: &mut dyn Write) -> Result<()> {
    writeln!(out, "{}  {}", v.id, v.state)?;
    writeln!(out, "target   {} (dir is the {}'s)", v.target, v.dir_side)?;
    writeln!(out, "dir      {:?}", v.dir)?;
    writeln!(out, "argv")?;
    for a in &v.argv {
        writeln!(out, "  {a:?}")?;
    }
    if !v.reason.is_empty() {
        writeln!(out, "reason   {:?}", v.reason)?;
    }
    writeln!(
        out,
        "asked by {} ({}{}){}",
        v.session,
        v.role,
        v.agent_type
            .as_deref()
            .map(|t| format!(", {t}"))
            .unwrap_or_default(),
        v.task
            .as_deref()
            .map(|t| format!(" for {t}"))
            .unwrap_or_default()
    )?;
    match &v.pin {
        Some(p) => writeln!(out, "pin      {} + {}", p.head, p.changes)?,
        None => writeln!(out, "pin      none (the directory is in no repository)")?,
    }
    writeln!(out, "timeout  {}s", v.timeout_ms / 1000)?;
    if let Some(r) = &v.refuse_reason {
        writeln!(out, "refused  {r}")?;
    }
    if let Some(n) = &v.note {
        writeln!(out, "note     {n}")?;
    }
    if let Some(o) = &v.outcome {
        writeln!(
            out,
            "outcome  {o}{}",
            v.exit_status.map(|s| format!(" {s}")).unwrap_or_default()
        )?;
    }
    if let Some(o) = &v.output {
        if let Some(p) = &o.log_path {
            writeln!(out, "log      {p}")?;
        }
        let tail = jkb_api::exec::bytes("tail", &o.tail_b64).unwrap_or_default();
        if !tail.is_empty() {
            writeln!(
                out,
                "output{}",
                if o.truncated {
                    " (the last 256 KiB)"
                } else {
                    ""
                }
            )?;
            out.write_all(&tail)?;
            if !tail.ends_with(b"\n") {
                writeln!(out)?;
            }
        }
    }
    Ok(())
}

/// Whether `v` ended by `settle`'s age rule alone ([`jkb_core::exec::SETTLED_INTERRUPT`]) — an
/// ending the recorded runner's own late report replaces, so a waiter does not act on it while the
/// runner can still answer.
fn is_provisional(v: &ExecView) -> bool {
    v.state == "finished"
        && v.outcome.as_deref() == Some("interrupted")
        && v.note.as_deref() == Some(jkb_core::exec::SETTLED_INTERRUPT)
}

/// The exit status a terminal request makes `jkb exec request` exit with.
#[must_use]
pub fn exit_code(v: &ExecView) -> Option<i32> {
    let status = || v.exit_status.and_then(|s| i32::try_from(s).ok());
    match v.state.as_str() {
        "finished" => Some(match v.outcome.as_deref() {
            Some("exited") => status().unwrap_or(1),
            Some("signalled") => 128 + status().unwrap_or(0),
            _ => EXIT_TIMED_OUT,
        }),
        "declined" => Some(EXIT_DECLINED),
        "refused" => Some(EXIT_REFUSED),
        "expired" => Some(EXIT_EXPIRED),
        _ => None,
    }
}

/// Whether a refusal is worth waiting out.
fn transient(e: &ApiError) -> bool {
    matches!(e.code, ErrorCode::Busy | ErrorCode::Unavailable)
}

/// Retry `f` while it is refused transiently.
fn retrying<T>(interval: Duration, mut f: impl FnMut() -> Result<T, ApiError>) -> Result<T> {
    let mut pause = interval;
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if transient(&e) => {
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_secs(5));
            }
            Err(e) => bail!("{e}"),
        }
    }
}

/// Follow `request` until it is answered: output to `out` as it arrives, the decision to `err`, and
/// the exit status the request ended with. The topic is read through `exec.follow` (it is reserved:
/// no queue operation reads it, so there is no consumer group to leave behind). Every ending is
/// confirmed with `exec.status`, and the topic is read to its end before returning, so no output
/// published before the end is missed. A chunk the topic dropped (it was full) shows as a gap in the
/// chunk numbers and is reported, naming the log that holds everything.
///
/// An ending `settle` wrote by age alone is provisional: the waiter keeps following until the
/// runner reports its own. Gives up at the latest a request can still end
/// ([`jkb_core::exec::LATEST_END_MS`] after it was made, and a minute): past that, nothing it waits
/// for can arrive, and a provisional ending is reported as it stands.
///
/// # Errors
/// A refusal that is not transient, or the deadline.
pub fn wait(
    backend: &dyn Backend,
    request: &ExecView,
    out: &mut dyn Write,
    err: &mut dyn Write,
    interval: Duration,
) -> Result<i32> {
    let latest = request
        .created_at
        .saturating_add(jkb_core::exec::LATEST_END_MS)
        .saturating_add(60_000);
    let mut s = Follow {
        after: 0,
        next_n: 0,
        printed: 0,
        gaps: Vec::new(),
    };
    let mut ended: Option<ExecView> = None;
    // The ending `settle` wrote by age alone, while the runner may still report its own.
    let mut provisional: Option<ExecView> = None;
    let mut announced_provisional = false;
    loop {
        if jkb_core::mq::now_ms() > latest {
            if let Some(view) = provisional {
                return finish(&view, &s, out, err);
            }
            bail!(
                "{} has not ended by the latest a request can end — the record is unreachable or \
                 stuck",
                request.id
            );
        }
        let polled = retrying(interval, || {
            backend.call(Request::ExecFollow {
                id: request.id.clone(),
                after: (s.after > 0).then_some(s.after),
                max: jkb_core::mq::MAX_BATCH,
            })
        })?;
        let Response::Messages { messages } = polled else {
            bail!("unexpected response to exec.follow: {polled:?}");
        };
        let got = !messages.is_empty();
        let mut announced_end = false;
        for m in &messages {
            s.after = m.seq;
            match m.kind.as_str() {
                "exec.output" => s.chunk(&m.payload, out, err)?,
                "exec.end" => announced_end = true,
                "exec.decision" => {
                    if m.payload["state"].as_str() == Some("running") {
                        writeln!(err, "jkb exec: the operator is running it")?;
                    } else {
                        announced_end = true;
                    }
                }
                _ => {}
            }
        }
        if let Some(view) = ended.take() {
            if !got {
                // Drained after the end: report it.
                return finish(&view, &s, out, err);
            }
            ended = Some(view);
            continue;
        }
        // Confirm with the row: on an announced ending, and on every idle read — a decision that
        // could not be published (a full topic) is still a fact the row holds.
        if announced_end || !got {
            let view = retrying(interval, || {
                backend.call(Request::ExecStatus {
                    id: request.id.clone(),
                })
            })
            .and_then(expect_view)?;
            if is_provisional(&view) {
                // Not the runner's word: it may only have been asleep. Keep following until it
                // reports, or until nothing more can arrive.
                if !announced_provisional {
                    writeln!(
                        err,
                        "jkb exec: {} went quiet past its timeout and was marked interrupted; \
                         waiting for its runner's own report",
                        view.id
                    )?;
                    announced_provisional = true;
                }
                provisional = Some(view);
            } else if exit_code(&view).is_some() {
                ended = Some(view);
                continue;
            }
        }
        if !got {
            std::thread::sleep(interval);
        }
    }
}

/// Where a [`wait`] is in a request's stream.
struct Follow {
    /// The last seq read.
    after: i64,
    /// The chunk number expected next.
    next_n: i64,
    /// How many chunks were printed.
    printed: i64,
    /// Ranges of chunk numbers that never arrived.
    gaps: Vec<(i64, i64)>,
}

impl Follow {
    fn chunk(
        &mut self,
        payload: &serde_json::Value,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> Result<()> {
        let Some(n) = payload["n"].as_i64() else {
            return Ok(());
        };
        if n < self.next_n {
            return Ok(());
        }
        if n > self.next_n {
            self.gap(self.next_n, n - 1, err)?;
        }
        let b64 = payload["b64"].as_str().unwrap_or_default();
        if let Ok(bytes) = jkb_api::exec::bytes("chunk", b64) {
            out.write_all(&bytes)?;
            out.flush()?;
        }
        self.printed += 1;
        self.next_n = n + 1;
        Ok(())
    }

    fn gap(&mut self, from: i64, to: i64, err: &mut dyn Write) -> Result<()> {
        self.gaps.push((from, to));
        let what = if from == to {
            format!("chunk {from}")
        } else {
            format!("chunks {from}–{to}")
        };
        writeln!(
            err,
            "jkb exec: output {what} did not reach this stream (its topic was full); the run's log \
             holds all of it"
        )?;
        Ok(())
    }
}

/// Report a terminal request and return its exit status.
fn finish(view: &ExecView, s: &Follow, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32> {
    let mut s = Follow {
        gaps: s.gaps.clone(),
        ..*s
    };
    let published = view.output.as_ref().map_or(0, |o| o.chunks);
    if s.next_n < published {
        s.gap(s.next_n, published - 1, err)?;
    }
    // Nothing reached this stream: the row's tail is what there is.
    if s.printed == 0 {
        if let Some(o) = &view.output {
            let tail = jkb_api::exec::bytes("tail", &o.tail_b64).unwrap_or_default();
            out.write_all(&tail)?;
            out.flush()?;
        }
    }
    let code = exit_code(view).unwrap_or(1);
    let log = view.output.as_ref().and_then(|o| o.log_path.as_deref());
    match view.state.as_str() {
        "declined" => writeln!(
            err,
            "jkb exec: {} was declined: {}",
            view.id,
            view.note.as_deref().unwrap_or("(no note)")
        )?,
        "refused" => writeln!(
            err,
            "jkb exec: {} was refused ({}); ask again — a new request is reviewed fresh",
            view.id,
            view.refuse_reason.as_deref().unwrap_or("unknown")
        )?,
        "expired" => writeln!(
            err,
            "jkb exec: {} {}",
            view.id,
            view.note.as_deref().unwrap_or("expired")
        )?,
        _ => {
            if let Some(note) = &view.note {
                writeln!(err, "jkb exec: {} {note}", view.id)?;
            }
            if !s.gaps.is_empty() {
                writeln!(
                    err,
                    "jkb exec: {} ended ({code}) with output missing from this stream; the full \
                     output is in {}",
                    view.id,
                    log.unwrap_or("the runner's log")
                )?;
            } else if let Some(p) = log {
                writeln!(
                    err,
                    "jkb exec: {} ended ({code}); full output in {p}",
                    view.id
                )?;
            }
        }
    }
    Ok(code)
}

#[cfg(test)]
mod tests;
