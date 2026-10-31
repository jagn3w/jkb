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
}

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

/// `jkb exec pin`: run before any database or daemon is reached.
///
/// # Errors
/// An unreadable directory, or a pin that cannot be computed.
pub fn pin(dir: &Path, against: Option<&Path>, json: bool) -> Result<()> {
    let dir = std::fs::canonicalize(dir).with_context(|| format!("resolving {}", dir.display()))?;
    let tree = crate::gitrepo::pin_tree(&dir)?;
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
