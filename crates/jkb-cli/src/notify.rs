//! `jkb notify hook`: the Claude Code hook's client of the notification machine (design r3.2 N1,
//! `openspec/changes/jkb-message-queue/design-r3.md`; the machine itself is
//! [`jkb_core::notify`]).
//!
//! **The hook decides nothing and performs nothing.** It reads the hook payload, sends what it
//! observed to `jkb serve` — one `notify.event`, plus a `session.*` request at a session's start and
//! end — and exits. The daemon runs the lifecycle table
//! against its own record of the session and puts the effects on the `claude/notify` queue, where
//! the notifier on the Mac picks them up. So a hook in the dev container — which cannot reach the
//! Mac's notification centre, and must never open the host's database — works exactly as one on the
//! host does.
//!
//! **It never opens a database** (`tests/cli.rs` `notify_needs_no_database`). It runs after every
//! tool call: opening one costs ~110 ms, and a database a newer migration has locked this binary out
//! of would stop every withdrawal. When the daemon cannot be reached, notifications stop — the
//! stated residual of design R1 — and the hook stays silent.
//!
//! **Silent and bounded, whatever happens.** A hook's stdout lands in the transcript, and a hook that
//! blocks holds up the session, so: at most [`CONNECT`] to connect and [`TOTAL`] per request, nothing
//! on stdout, and every failure appended to `~/.jkb/logs/notify-hook.log` instead of reported.
//!
//! **The one thing only the producer can do is probe a pid** — the daemon cannot see a container's
//! processes. So at `SessionStart` the hook asks the daemon which notifications are on screen
//! (`notify.open_sessions`), decides for each whether its session is provably gone ([`verdict`]), and
//! tells the daemon (`notify.gone`). That sweep is the only route by which a killed session's
//! Alerts-style notification — which waits for ever by design — comes down.
//!
//! **It also feeds the session registry** (tasks S6.4, [`jkb_core::claude_session`];
//! docs/notifications.md, "The session registry"): `SessionStart` sends `session.started`,
//! `SessionEnd` sends `session.ended`, every `notify.event` marks its process running, and the same
//! sweep ends the processes it proves gone (`session.list`, `session.gone`) — the only way a killed
//! `claude` or a restarted container, which send no `SessionEnd` (measured), is ever recorded as ended.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use jkb_api::{ApiError, Backend, HookEvent, Request, Response};
use jkb_fsm::Fact;

use crate::NotifyCmd;

/// How long the hook waits to connect to the daemon.
pub const CONNECT: Duration = Duration::from_millis(200);

/// How long one request may take. `SessionStart` — a start and a sweep — starts no request once this
/// has passed since the hook began, so it is bounded by about twice this (a request started just before
/// the deadline runs its own full `TOTAL`), once per session. `SessionEnd`'s own, shorter limit is
/// [`SESSION_END_SECOND_REQUEST`].
pub const TOTAL: Duration = Duration::from_secs(1);

/// `SessionEnd`'s hooks get 1.5 s by default (Claude Code's hooks documentation), and it sends two
/// requests: the second starts only this soon after the hook began (measured from [`hook`]'s first
/// line, so the shim's and this binary's start-up come on top), because it may itself take a full
/// [`TOTAL`] — 0.3 + 1.0 s leaves 200 ms for start-up inside the budget. That margin is assumed, not
/// measured (docs/notifications.md). A warm round trip on the host is a few milliseconds. A hook killed at the budget logs nothing, so this is what keeps a slow end
/// visible in the log instead.
pub const SESSION_END_SECOND_REQUEST: Duration = Duration::from_millis(300);

/// The log grows to this, then starts again beside its predecessor (`.1`).
const LOG_CAP_BYTES: u64 = 256 * 1024;

/// Run a `jkb notify` verb. `hook` never fails: it logs its failures rather than returning them.
///
/// # Errors
/// `sessions`: the daemon could not be reached or refused.
pub fn run(cmd: &NotifyCmd, json: bool) -> Result<()> {
    match cmd {
        NotifyCmd::Events => {
            for (name, _) in HOOK_EVENTS {
                println!("{name}");
            }
        }
        NotifyCmd::Topic => println!("{}", jkb_core::notify::TOPIC),
        NotifyCmd::Hook => hook(),
        NotifyCmd::Sessions { all } => sessions(*all, json)?,
    }
    Ok(())
}

/// `jkb notify sessions`: the session registry, as the daemon holds it.
fn sessions(all: bool, json: bool) -> Result<()> {
    let url = crate::remote::daemon_url();
    let backend = jkb_daemon::client::RemoteBackend::new(&url, crate::remote::token_file(&url))
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    let mut sessions = Vec::new();
    let mut after = None;
    loop {
        match backend.call(Request::SessionList { all, after }) {
            Ok(Response::ClaudeSessions {
                sessions: page,
                next,
            }) => {
                sessions.extend(page);
                match next {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
            Ok(other) => anyhow::bail!("session.list: unexpected {other:?}"),
            Err(e) => anyhow::bail!("session.list: {}", e.message),
        }
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
        return Ok(());
    }
    for s in &sessions {
        println!("{}", session_line(s));
    }
    Ok(())
}

/// One process's hold on a session, on one line: id, state, process and where it runs.
fn session_line(s: &jkb_api::ClaudeSession) -> String {
    let state = match (&s.end_reason, &s.start_source) {
        (Some(reason), _) => format!("ended ({reason})"),
        (None, Some(source)) => format!("live ({source})"),
        (None, None) => "live (seen)".to_owned(),
    };
    let pid = if s.pid.is_empty() { "?" } else { &s.pid };
    format!(
        "{}  {state}  pid {pid} on {}  {}",
        s.session, s.instance, s.cwd
    )
}

/// The Claude Code hook events this command answers to, and the ONE place they are spelled.
///
/// `SessionStart` maps to no machine event — it drives the sweep over records other sessions
/// left — but it belongs here because the question this table answers is "which registrations
/// must exist", and a name that drifts out of `.claude/settings.json` or the shim is silent in
/// the worst way: renaming the `SessionStart` literal alone disabled the sweep permanently with
/// every check still green. `jkb notify events` prints it so
/// `scripts/tests/notify-hook.test.sh` can diff all three spellings instead of two.
const HOOK_EVENTS: &[(&str, Option<HookEvent>)] = &[
    ("Notification", Some(HookEvent::Needed)),
    ("PostToolUse", Some(HookEvent::ToolFinished)),
    ("UserPromptSubmit", Some(HookEvent::UserActed)),
    ("Stop", Some(HookEvent::TurnEnded)),
    ("SessionEnd", Some(HookEvent::SessionEnded)),
    ("SessionStart", None),
];

/// What a payload asks of the daemon: requests, in order, and then perhaps the sweep. Nothing at all
/// for an event we do not act on.
#[derive(Debug, Default, PartialEq)]
struct Ask {
    requests: Vec<Request>,
    sweep: bool,
    /// The payload's session, sanitized, or empty.
    session: String,
}

/// The payload, read into what it asks for. `owner` and `instance` are handed in, resolved at the
/// edge, so this is a pure function of its arguments.
///
/// A payload that names no session sends nothing addressed to one, but `SessionStart` still sweeps:
/// the sweep is about other sessions.
fn ask(raw: &str, owner: &str, instance: &str) -> Result<Ask> {
    let payload: serde_json::Value =
        serde_json::from_str(raw).context("the hook payload is not JSON")?;
    let field = |k: &str| {
        payload
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned()
    };
    let Some((_, event)) = HOOK_EVENTS
        .iter()
        .find(|(n, _)| *n == field("hook_event_name"))
    else {
        return Ok(Ask::default());
    };
    let session = jkb_core::notify::sanitize(&field("session_id"));
    // Claude Code names these; an absent one is still an event worth recording.
    let word = |k: &str| {
        let w = field(k);
        if w.is_empty() {
            "unknown".to_owned()
        } else {
            w
        }
    };
    let mut out = Ask {
        requests: Vec::new(),
        sweep: event.is_none(),
        session: session.clone(),
    };
    if session.is_empty() {
        return Ok(out);
    }
    match event {
        None => out.requests.push(Request::SessionStarted {
            session: session.clone(),
            source: word("source"),
            pid: owner.to_owned(),
            instance: instance.to_owned(),
            cwd: field("cwd"),
        }),
        Some(event) => {
            out.requests.push(Request::NotifyEvent {
                session: session.clone(),
                event: *event,
                tool: field("tool_name"),
                message: field("message"),
                cwd: field("cwd"),
                owner: owner.to_owned(),
                instance: instance.to_owned(),
            });
            if *event == HookEvent::SessionEnded {
                out.requests.push(Request::SessionEnded {
                    session: session.clone(),
                    reason: word("reason"),
                    pid: owner.to_owned(),
                    instance: instance.to_owned(),
                });
            }
        }
    }
    Ok(out)
}

/// The process whose existence answers "is this session still alive?".
///
/// Refuses anything it cannot tell apart from this invocation's own short-lived ancestry, and
/// answers with an empty string when there is nothing trustworthy — which the daemon records, and
/// a sweep then reads as [`Fact::Unknown`] and leaves alone. Getting this wrong in the other
/// direction takes a live session's notification off the screen, so the default is to do nothing.
///
/// The owner is handed over by the shim (`JKB_HOOK_OWNER`), which measurement shows is the
/// `claude` process itself. **`jkb` cannot ask for it**: its own parent is the shim, a bash script
/// that exits milliseconds later, so recording that made every record read as *provably dead* and
/// the next session's sweep withdrew a live session's pending prompt.
fn owner_id() -> String {
    owner_from(
        std::env::var("JKB_HOOK_OWNER").unwrap_or_default().trim(),
        std::os::unix::process::parent_id(),
        std::process::id(),
    )
}

/// The owner to send from `instance`: none when there is no instance. A pid means nothing without the
/// instance it belongs to, and the daemon refuses the pair, so the hook names no process rather than
/// losing the notification.
fn owner_in(instance: &str, owner: impl FnOnce() -> String) -> String {
    if instance.is_empty() {
        String::new()
    } else {
        owner()
    }
}

/// The decision itself, with the two pids it must reject handed in, so it can be tested.
fn owner_from(raw: &str, parent: u32, me: u32) -> String {
    let Ok(pid) = raw.parse::<u32>() else {
        return String::new();
    };
    // The shim is our parent and dies with this call; so does anything claiming to be us.
    if pid == 0 || pid == parent || pid == me {
        return String::new();
    }
    pid.to_string()
}

/// Where this process's pids mean something, as `host[#boot][/pidns]`:
///
/// * `host` — the machine's name;
/// * `boot` — in the dev container, the container's boot: the pid namespace its entrypoint recorded
///   in the marker (`JKB_NS_MARKER`, `.container/README.md`, "The discriminator is namespace
///   identity");
/// * `pidns` — the pid namespace **this process** is in (`/proc/self/ns/pid`), where there is one.
///
/// The boot is what lets a sweep prove a whole container instance gone: a container keeps its
/// hostname across `docker stop`/`start` but its entrypoint writes a new marker, and one container
/// runs one boot at a time — so a record from the same host with a different boot names processes
/// that no longer exist anywhere. The marker is readable from nested sandboxes too (`bwrap --bind / /`),
/// so every process of a boot agrees on it.
///
/// **The process's own namespace is what makes a pid probe sound.** A nested sandbox shares the
/// hostname and the marker but not the pid namespace — measured in the Bash sandbox,
/// `/proc/self/ns/pid` was `pid:[4026532823]` while the marker said `pid:[4026532556]` — so without
/// it a `claude` started in a sandbox would have read the outer sessions' records as its own
/// instance, probed their pids in a namespace where they do not exist, and withdrawn every live
/// prompt in the container. With it, two instances are equal only when a pid means the same process
/// in both. (Found by the stage-5 review.)
fn instance() -> String {
    let boot = std::env::var_os("JKB_NS_MARKER").and_then(|p| std::fs::read_to_string(p).ok());
    let pidns = std::fs::read_link("/proc/self/ns/pid")
        .ok()
        .map(|l| l.to_string_lossy().into_owned());
    instance_from(&crate::owner::hostname(), boot.as_deref(), pidns.as_deref())
}

/// The instance string, from its parts. Each part is cleaned of control characters and of the two
/// separators, and bounded, so the whole stays inside the daemon's 150-byte limit.
fn instance_from(host: &str, marker: Option<&str>, pidns: Option<&str>) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .filter(|c| !c.is_control() && *c != '#' && *c != '/')
            .take(48)
            .collect()
    };
    let mut out = clean(host);
    if let Some(boot) = marker
        .and_then(|m| m.lines().find_map(|l| l.strip_prefix("pid=")))
        .map(clean)
        .filter(|b| !b.is_empty())
    {
        out.push('#');
        out.push_str(&boot);
    }
    if let Some(ns) = pidns.map(clean).filter(|n| !n.is_empty()) {
        out.push('/');
        out.push_str(&ns);
    }
    out
}

/// Whether an instance names neither a boot nor a pid namespace — the macOS host, whose instance is its
/// hostname alone. The dev container always records both, and a Linux host its namespace. An empty
/// instance names nothing, so it is not the host either: a pid recorded without one must never be
/// probed here (stage-1 review, round 2).
fn is_bare(instance: &str) -> bool {
    !instance.is_empty() && !instance.contains(['#', '/'])
}

/// Whether a pid recorded in `theirs` means the same process here, in `mine`.
///
/// Equal instances do. So do two bare ones, even with different names: a bare instance is the machine
/// `jkb serve` runs on — its clients are that host and its containers — and that machine's name changes
/// under it (macOS renames the host on a network change), which would otherwise leave a session that
/// ended under the new name recorded live for ever (stage-1 review).
fn same_pid_space(theirs: &str, mine: &str) -> bool {
    theirs == mine || (is_bare(theirs) && is_bare(mine))
}

/// An instance string's host and boot.
fn host_and_boot(instance: &str) -> (&str, Option<&str>) {
    let without_ns = instance.split_once('/').map_or(instance, |(head, _)| head);
    match without_ns.split_once('#') {
        Some((host, boot)) => (host, Some(boot)),
        None => (without_ns, None),
    }
}

/// Whether the session a record names — by its `owner` pid and the `instance` that pid belongs to —
/// is provably gone, as seen from this process. The notification record and the registry row ask the
/// same question of the same two fields.
///
/// * **The same instance** — same host, boot and pid namespace, or both the bare host
///   ([`same_pid_space`]): its owner pid means the same process here, so probe it — by the kernel,
///   with `EPERM` counted as alive ([`crate::owner::pid_alive`]).
/// * **Another boot of this same container** — same host, both with a boot, the boots differ: every
///   process of that boot is gone, whatever namespace either side is in.
/// * **Anything else** — the host from a container, another container, a nested sandbox of this
///   boot, a record with no owner — is [`Fact::Unknown`]: nothing here can establish it, and
///   `Unknown` never withdraws.
fn verdict(owner: &str, instance: &str, mine: &str, probe: impl Fn(u32) -> Fact) -> Fact {
    let Ok(pid) = owner.parse::<u32>() else {
        return Fact::Unknown;
    };
    if same_pid_space(instance, mine) {
        return match probe(pid) {
            Fact::No => Fact::No,
            _ => Fact::Unknown,
        };
    }
    match (host_and_boot(instance), host_and_boot(mine)) {
        ((their_host, Some(theirs)), (my_host, Some(ours)))
            if their_host == my_host && theirs != ours =>
        {
            Fact::No
        }
        _ => Fact::Unknown,
    }
}

/// Everything a hook invocation touches outside its arguments, resolved once at the edge.
struct Edge<'a> {
    /// When the hook began: what every time limit is measured from.
    began: Instant,
    backend: &'a dyn Backend,
    owner: String,
    instance: String,
    probe: &'a dyn Fn(u32) -> Fact,
}

/// One hook invocation. Returns the failures to log — never an error, because nothing a hook can
/// return reaches anyone but the session it would disturb.
fn handle(raw: &str, edge: &Edge<'_>) -> Vec<String> {
    let started = edge.began;
    let ask = match ask(raw, &edge.owner, &edge.instance) {
        Ok(ask) => ask,
        Err(e) => return vec![format!("{e:#}")],
    };
    // The first request always goes; a later one only while it can still finish inside the event's
    // budget.
    let last_start = if ask.sweep {
        TOTAL
    } else {
        SESSION_END_SECOND_REQUEST
    };
    let mut failures = Vec::new();
    for (i, request) in ask.requests.into_iter().enumerate() {
        let op = request.op();
        if i > 0 && started.elapsed() >= last_start {
            failures.push(format!("{op}: out of time; not sent"));
            continue;
        }
        if let Err(e) = edge.backend.call(request) {
            failures.push(failure(op, &e));
        }
    }
    if ask.sweep {
        failures.extend(sweep(edge, &ask.session));
    }
    failures
}

fn failure(op: &str, e: &ApiError) -> String {
    format!("{op}: {:?}: {}", e.code, e.message)
}

const OUT_OF_TIME: &str = "the sweep ran out of time; the next session resumes it";

/// The `SessionStart` sweep, over what provably-gone processes left: their notifications are withdrawn,
/// and their registry rows ended. It starts no request once [`TOTAL`] has passed since the hook began,
/// so the invocation ends within about twice that; whatever is left is swept by the next session to
/// start.
///
/// What each verdict was computed from goes back with its request: the daemon acts only if the record
/// still names that owner in that instance, so a session resumed since the listing is spared.
///
/// **The invoking session's own registry rows are never judged.** It is running — this is its start —
/// so an earlier process of it proved dead (it was killed, then resumed) must not leave it recorded as
/// ended if its own `session.started` was just lost (stage-1 review). A later sweep, from another
/// session, ends that row, by which time this process's own row is live.
fn sweep(edge: &Edge<'_>, own: &str) -> Vec<String> {
    let started = edge.began;
    let mut failures = Vec::new();
    let out_of_time = |failures: &mut Vec<String>, op: &str| {
        let late = started.elapsed() >= TOTAL;
        if late {
            failures.push(format!("{op}: {OUT_OF_TIME}"));
        }
        late
    };

    if out_of_time(&mut failures, "notify.open_sessions") {
        return failures;
    }
    match edge.backend.call(Request::NotifyOpenSessions {}) {
        Ok(Response::Sessions { sessions }) => {
            for record in sessions {
                if verdict(&record.owner, &record.instance, &edge.instance, edge.probe) != Fact::No
                {
                    continue;
                }
                if out_of_time(&mut failures, "notify.gone") {
                    return failures;
                }
                if let Err(e) = edge.backend.call(Request::NotifyGone {
                    session: record.session,
                    owner: record.owner,
                    instance: record.instance,
                }) {
                    failures.push(failure("notify.gone", &e));
                }
            }
        }
        Ok(other) => failures.push(format!("notify.open_sessions: unexpected {other:?}")),
        Err(e) => failures.push(failure("notify.open_sessions", &e)),
    }

    // Page by page, so rows this process can never judge — another container's, the host's from here,
    // pid-less ones — cannot fill the one page and hide one it could (stage-1 review, round 2).
    let mut after = None;
    loop {
        if out_of_time(&mut failures, "session.list") {
            return failures;
        }
        let next = match edge
            .backend
            .call(Request::SessionList { all: false, after })
        {
            Ok(Response::ClaudeSessions { sessions, next }) => {
                for row in sessions {
                    if row.session == own
                        || verdict(&row.pid, &row.instance, &edge.instance, edge.probe) != Fact::No
                    {
                        continue;
                    }
                    if out_of_time(&mut failures, "session.gone") {
                        return failures;
                    }
                    if let Err(e) = edge.backend.call(Request::SessionGone {
                        session: row.session,
                        pid: row.pid,
                        instance: row.instance,
                    }) {
                        failures.push(failure("session.gone", &e));
                    }
                }
                next
            }
            Ok(other) => {
                failures.push(format!("session.list: unexpected {other:?}"));
                None
            }
            Err(e) => {
                failures.push(failure("session.list", &e));
                None
            }
        };
        match next {
            Some(next) => after = Some(next),
            None => return failures,
        }
    }
}

/// Decide and send. This is what the hook shim calls.
fn hook() {
    let began = Instant::now();
    let mut raw = String::new();
    let read = std::io::stdin().read_to_string(&mut raw);
    let log = log_path();
    if let Err(e) = read {
        append_log(&log, &[format!("reading the hook payload: {e}")]);
        return;
    }
    let url = crate::remote::daemon_url();
    let backend =
        match jkb_daemon::client::RemoteBackend::new(&url, crate::remote::token_file(&url))
            .and_then(|b| b.with_deadlines(CONNECT, TOTAL))
        {
            Ok(b) => b.with_down_marker(crate::remote::down_marker(&url)),
            Err(e) => {
                append_log(&log, &[failure("client", &e)]);
                return;
            }
        };
    let instance = instance();
    let owner = owner_in(&instance, owner_id);
    let failures = handle(
        &raw,
        &Edge {
            began,
            backend: &backend,
            owner,
            instance,
            probe: &crate::owner::pid_alive,
        },
    );
    append_log(&log, &failures);
}

fn log_path() -> PathBuf {
    std::env::var_os("HOME")
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join(".jkb/logs/notify-hook.log")
}

/// Append failures, one line each, best-effort. Past [`LOG_CAP_BYTES`] the log is moved aside to
/// `.1` first, so a daemon that stays down — a failure per tool call — cannot fill the disk.
fn append_log(path: &Path, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() >= LOG_CAP_BYTES) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let now = jkb_core::mq::now_ms();
    for line in lines {
        let _ = writeln!(f, "{now} {}", line.replace('\n', " "));
    }
}

#[cfg(test)]
mod tests;
