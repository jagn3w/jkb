//! `jkb exec forward`: push each run request's result into the Claude Code inbox socket of the
//! session that filed it, so an idle session takes a turn without waiting for its next prompt (the
//! "session that files a run request hears its result" design, part 4).
//!
//! **Where it runs.** In the dev container, outside the agent's sandbox, as the pinned
//! `$JKB_HOOK_BIN exec forward` (`/usr/local/lib/jkb-hook/jkb`), started and restarted by the
//! entrypoint's supervise loop. It authenticates with the container credential, which the sandboxed
//! tools cannot read — the same trust as the managed hooks, which run the same binary. `jkb serve`
//! runs on the host and cannot reach a socket in the container, so this is the only place a push
//! can come from.
//!
//! **What it does.** It keeps a consumer group `inbox/<hostname>` on the run-request index
//! (`exec/requests`), through the same in-process consumer `jkb mq subscribe` is
//! ([`crate::mq_cli::subscribe`]), so it waits out transient refusals and daemon restarts the same
//! way. On start, after each burst of hints carrying a terminal state, and every
//! [`SWEEP_EVERY`] (a hint is a hint: a full topic drops it), it sweeps: `exec.inbox.due` for its
//! container boot, then for each id `exec.inbox.claim` (the message and the session's inbox
//! targets, under a lease), a post to the first target that takes it, and `exec.inbox.report`. A
//! claim is not delivery: a forwarder that dies between the two leaves the row to the hook path once
//! the lease lapses. Hints are acknowledged after the sweep that read them.
//!
//! **The post.** One connection per target: the auth line, then the body ([`inbox_body`]), each one
//! newline-terminated line, and the connection closed — the socket writes nothing back (experiment
//! E1, Claude Code 2.1.317), so there is no reply to wait for, and a message the session holds
//! after accepting it cannot be seen from here. The hook path still delivers anything never
//! reported posted.
//!
//! **One per container, and replaced with its binary.** A `flock` beside the entrypoint's namespace
//! marker keeps a second instance from starting; and once a [`REPIN_EVERY`] it compares its own
//! executable with `$JKB_HOOK_BIN`, exiting 0 when a re-pin replaced it, so the supervise loop starts
//! the new one within about a minute.

use std::io::Write;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _, Result};
use jkb_api::exec::InboxTarget;
use jkb_api::{ApiError, Backend, ErrorCode, Request, Response};
use serde_json::Value;

use crate::mq_cli::{self, Input, SubscribeOpts};

/// How long a claim holds a result for its post: thirty seconds, beyond any local socket write.
pub const LEASE_MS: i64 = 30_000;

/// How often the forwarder sweeps with no hint to prompt it.
pub const SWEEP_EVERY: Duration = Duration::from_mins(1);

/// How often it checks whether a re-pin replaced its binary.
pub const REPIN_EVERY: Duration = Duration::from_mins(1);

/// The longest one write to a session's socket may block.
const POST_TIMEOUT: Duration = Duration::from_secs(5);

/// The most sweep rounds one trigger runs: a full page of due ids asks for another round, and a row
/// this round could not claim (another deliverer holds it) is not due again until its lease lapses,
/// so more than a few rounds means something else is wrong.
const MAX_ROUNDS: usize = 8;

/// The variable naming the pinned binary the forwarder must be: the Dockerfile's `ENV`, spelled once.
pub const HOOK_BIN_VAR: &str = "JKB_HOOK_BIN";

/// The body version-1 posts ([`jkb_core::exec::INBOX_BODY_VERSION`]): `text` as a user message, one
/// JSON line. Measured as E1 on Claude Code 2.1.317 (2026-11-25): this shape is routed to the
/// session's queue and starts a turn; `{"type":"message",…}` and `{"text":…}` are dropped.
#[must_use]
pub fn inbox_body(text: &str) -> Vec<u8> {
    // Spelled out rather than built as a `json!` map, which serializes its keys sorted: the bytes are
    // the measured line's, key order included.
    let content = Value::String(text.to_owned());
    format!(r#"{{"type":"user","message":{{"role":"user","content":{content}}}}}"#)
        .into_bytes()
        .into_iter()
        .chain([b'\n'])
        .collect()
}

/// The auth line a post opens with: optional on Linux (E1), sent because the protocol names it.
#[must_use]
pub fn auth_line(token: &str) -> Vec<u8> {
    let token = Value::String(token.to_owned());
    format!(r#"{{"type":"auth","token":{token}}}"#)
        .into_bytes()
        .into_iter()
        .chain([b'\n'])
        .collect()
}

/// Post `text` to `target`: connect, write the auth line and the body, close. Never waits for a
/// reply — the socket sends none.
///
/// # Errors
/// Why the post failed, naming the socket and never the token.
pub fn post(target: &InboxTarget, text: &str) -> std::result::Result<(), String> {
    let fail = |what: &str, e: std::io::Error| format!("{}: {what}: {e}", target.socket);
    let mut s =
        std::os::unix::net::UnixStream::connect(&target.socket).map_err(|e| fail("connect", e))?;
    s.set_write_timeout(Some(POST_TIMEOUT))
        .map_err(|e| fail("set timeout", e))?;
    let mut bytes = auth_line(&target.token);
    bytes.extend(inbox_body(text));
    s.write_all(&bytes).map_err(|e| fail("write", e))?;
    s.flush().map_err(|e| fail("flush", e))?;
    // The whole message is out; closing ends the connection the way the protocol's single line
    // does. A failed shutdown after a complete write does not unsend it.
    let _ = s.shutdown(std::net::Shutdown::Write);
    Ok(())
}

/// Post `text` to the first of `targets` that takes it, newest process first.
///
/// # Errors
/// Every target's failure, joined; or that there was none to post to.
pub fn post_any(targets: &[InboxTarget], text: &str) -> std::result::Result<(), String> {
    if targets.is_empty() {
        return Err("the session has no live inbox in this container".to_owned());
    }
    let mut why = Vec::new();
    for t in targets {
        match post(t, text) {
            Ok(()) => return Ok(()),
            Err(e) => why.push(e),
        }
    }
    Err(why.join("; "))
}

/// How a sweep posts a message to a session's targets: [`post_any`], or a test's recorder.
pub type Poster = fn(&[InboxTarget], &str) -> std::result::Result<(), String>;

/// What one sweep did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Swept {
    /// Results posted and reported delivered.
    pub posted: usize,
    /// Results no target took, reported failed.
    pub failed: usize,
    /// Due ids another deliverer claimed first.
    pub lost: usize,
}

/// One sweep for the forwarder of container boot `boot`: claim every due result, post it with `post`,
/// report how it went. Each post and failure is logged to `log` by id and socket — never a token,
/// never text.
///
/// # Errors
/// The first refusal other than a lost claim: the trigger after this one sweeps again.
pub fn sweep(
    backend: &dyn Backend,
    boot: &str,
    post: Poster,
    log: &mut dyn Write,
) -> Result<Swept, ApiError> {
    let mut swept = Swept::default();
    for _ in 0..MAX_ROUNDS {
        let ids = match backend.call(Request::ExecInboxDue {
            instance: boot.to_owned(),
            max: jkb_core::exec_delivery::MAX_DUE,
        })? {
            Response::ExecInboxDue { ids } => ids,
            other => return Err(unexpected("exec.inbox.due", &other)),
        };
        for id in &ids {
            let claimed = backend.call(Request::ExecInboxClaim {
                id: id.clone(),
                instance: boot.to_owned(),
                lease_ms: LEASE_MS,
            });
            let (text, targets) = match claimed {
                Ok(Response::ExecInboxClaimed { text, targets }) => (text, targets),
                Ok(other) => return Err(unexpected("exec.inbox.claim", &other)),
                Err(e) if e.code == ErrorCode::NotFound => {
                    swept.lost += 1;
                    continue;
                }
                Err(e) => return Err(e),
            };
            let outcome = post(&targets, &text);
            let (ok, error) = match &outcome {
                Ok(()) => (true, None),
                Err(why) => (false, Some(why.clone())),
            };
            backend.call(Request::ExecInboxReport {
                id: id.clone(),
                ok,
                error,
            })?;
            match outcome {
                Ok(()) => {
                    swept.posted += 1;
                    let _ = writeln!(log, "{id}: posted to {}", first_socket(&targets));
                }
                Err(why) => {
                    swept.failed += 1;
                    let _ = writeln!(log, "{id}: not posted: {why}");
                }
            }
        }
        if ids.len() < jkb_core::exec_delivery::MAX_DUE {
            break;
        }
    }
    Ok(swept)
}

fn first_socket(targets: &[InboxTarget]) -> &str {
    targets.first().map_or("", |t| t.socket.as_str())
}

fn unexpected(op: &str, r: &Response) -> ApiError {
    // The variant only: a claim's answer carries a token, which no log line may.
    let name = serde_json::to_value(r)
        .ok()
        .and_then(|v| v.get("result").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default();
    ApiError::with_code(
        ErrorCode::Internal,
        format!("unexpected answer to {op}: {name}"),
    )
}

/// The forwarder's single-instance lock: held while the value lives.
#[derive(Debug)]
pub struct Lock(#[allow(dead_code)] std::fs::File);

/// Take the lock at `path`, or `None` when another live forwarder holds it.
///
/// # Errors
/// The lock file cannot be opened, or `flock` fails for a reason other than its being held.
pub fn lock(path: &Path) -> Result<Option<Lock>> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(Lock(file))),
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => Ok(None),
        Err(e) => {
            Err(std::io::Error::from(e)).with_context(|| format!("locking {}", path.display()))
        }
    }
}

/// A file's identity: its device and inode, following symlinks.
fn identity(path: &Path) -> Option<(u64, u64)> {
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// Whether the binary the forwarder must be is still the one running.
#[derive(Debug)]
pub struct Repin {
    watched: PathBuf,
    me: Option<(u64, u64)>,
}

impl Repin {
    /// Watch `watched` (`$JKB_HOOK_BIN`) against the executable at `me` (`/proc/self/exe`).
    #[must_use]
    pub fn new(watched: PathBuf, me: &Path) -> Self {
        Self {
            me: identity(me),
            watched,
        }
    }

    /// The pinned binary is no longer this process's own: replaced by a re-pin, or removed.
    #[must_use]
    pub fn replaced(&self) -> bool {
        self.me.is_none() || identity(&self.watched) != self.me
    }
}

/// How one forwarder runs.
pub struct Config {
    /// Its container boot, `host#boot`.
    pub boot: String,
    /// Its consumer group on the index topic, `inbox/<hostname>`.
    pub group: String,
    /// Exit when this says the binary was replaced.
    pub repin: Option<Repin>,
    /// The sweep with no hint.
    pub sweep_every: Duration,
    /// The re-pin check.
    pub repin_every: Duration,
    /// The consumer's wait between empty polls.
    pub poll_interval: Duration,
    /// How results are posted: [`post_any`].
    pub post: Poster,
}

/// The consumer's NDJSON stdout, parsed back into events for the forwarder's loop.
struct Events {
    line: Vec<u8>,
    tx: Sender<Value>,
}

impl Write for Events {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for &b in buf {
            if b == b'\n' {
                let event = serde_json::from_slice(&self.line).unwrap_or(Value::Null);
                self.line.clear();
                self.tx
                    .send(event)
                    .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
            } else {
                self.line.push(b);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Whether an index hint names a state a result is due in.
fn terminal_hint(m: &Value) -> bool {
    m["payload"]["state"]
        .as_str()
        .and_then(jkb_core::exec::ExecState::parse)
        .is_some_and(jkb_core::exec::ExecState::is_terminal)
}

/// The forwarder's state across events.
struct Loop<'a> {
    backend: &'a dyn Backend,
    cfg: &'a Config,
    log: &'a mut dyn Write,
    /// A hint with a terminal state arrived since the last sweep.
    dirty: bool,
    /// The last hint read, and the last acknowledged.
    read: i64,
    acked: i64,
    /// The last sweep failed, and said so: a failing one is logged once, until one succeeds.
    failing: bool,
}

impl Loop<'_> {
    /// Sweep, logging a failure once per outage. `Some(code)` when the forwarder must stop: a refusal
    /// no retry clears (the credential rotated or revoked), so the supervisor restarts it.
    fn sweep(&mut self) -> Option<i32> {
        self.dirty = false;
        match sweep(self.backend, &self.cfg.boot, self.cfg.post, self.log) {
            Ok(_) => {
                if self.failing {
                    let _ = writeln!(self.log, "sweeping again");
                }
                self.failing = false;
                None
            }
            Err(e) if matches!(e.code, ErrorCode::Unauthorized | ErrorCode::Forbidden) => {
                let _ = writeln!(self.log, "sweep refused, stopping: {}", e.message);
                Some(1)
            }
            Err(e) => {
                if !self.failing {
                    let _ = writeln!(self.log, "sweep failed ({:?}): {}", e.code, e.message);
                }
                self.failing = true;
                None
            }
        }
    }

    /// One consumer event. `Some(code)` when the forwarder must stop.
    fn event(&mut self, event: &Value, acks: &Sender<Input>) -> Option<i32> {
        match event["event"].as_str() {
            Some("message") => {
                if terminal_hint(&event["message"]) {
                    self.dirty = true;
                }
                if let Some(seq) = event["message"]["seq"].as_i64() {
                    self.read = self.read.max(seq);
                }
            }
            // A burst is over: sweep for what it announced, then acknowledge it.
            Some("caught_up") => {
                if self.dirty {
                    if let Some(code) = self.sweep() {
                        return Some(code);
                    }
                }
                self.ack(acks);
            }
            // A hint that cannot be read is a hint all the same: the minute's sweep covers it.
            Some("unreadable") => {
                if let Some(seq) = event["seq"].as_i64() {
                    self.read = self.read.max(seq);
                }
                self.dirty = true;
            }
            Some("error") => {
                let _ = writeln!(
                    self.log,
                    "index consumer: {} ({})",
                    event["reason"].as_str().unwrap_or_default(),
                    event["code"].as_str().unwrap_or_default()
                );
            }
            _ => {}
        }
        None
    }

    fn ack(&mut self, acks: &Sender<Input>) {
        if self.read > self.acked && acks.send(Input::Ack(self.read)).is_ok() {
            self.acked = self.read;
        }
    }
}

/// The forwarder's loop over `backend`, until its binary is replaced (0) or something it cannot
/// wait out stops it (1). Logs to `log`.
#[must_use]
pub fn run(backend: &(dyn Backend + Sync), cfg: &Config, log: &mut dyn Write) -> i32 {
    let opts = SubscribeOpts {
        topic: jkb_core::exec::INDEX_TOPIC.to_owned(),
        group: cfg.group.clone(),
        from_start: false,
        at_most_once: false,
        batch: 64,
        interval: cfg.poll_interval,
        eof_ack_wait: Duration::from_secs(5),
    };
    let mut l = Loop {
        backend,
        cfg,
        log,
        dirty: false,
        read: 0,
        acked: 0,
        failing: false,
    };
    // The first sweep runs before the consumer starts, not beside it: two first requests at once
    // both miss the daemon's grant cache, and a miss arriving during another's refresh is refused
    // `unauthorized` — which the consumer takes for fatal.
    if let Some(code) = l.sweep() {
        return code;
    }
    // A result that ended between that sweep and the consumer's group is announced to nobody: the
    // consumer's first `caught_up` sweeps again.
    l.dirty = true;
    std::thread::scope(|scope| {
        let (acks, inputs) = std::sync::mpsc::channel::<Input>();
        let (events_tx, events) = std::sync::mpsc::channel::<Value>();
        let consumer = scope.spawn(move || {
            let mut out = Events {
                line: Vec::new(),
                tx: events_tx,
            };
            mq_cli::subscribe(backend, &opts, &inputs, &mut out).unwrap_or(1)
        });
        let code = drive(&mut l, &events, &acks);
        // Closing the consumer's input ends it, after it applies a held ack.
        drop(acks);
        let ended = consumer.join().unwrap_or(1);
        code.unwrap_or(ended)
    })
}

/// The event loop: `Some(code)` when the forwarder decided to stop, `None` when the consumer ended.
fn drive(l: &mut Loop<'_>, events: &Receiver<Value>, acks: &Sender<Input>) -> Option<i32> {
    let cfg = l.cfg;
    let now = Instant::now();
    let (mut next_sweep, mut next_repin) = (now + cfg.sweep_every, now + cfg.repin_every);
    loop {
        let wait = next_sweep
            .min(next_repin)
            .saturating_duration_since(Instant::now());
        match events.recv_timeout(wait) {
            Ok(event) => {
                if let Some(code) = l.event(&event, acks) {
                    return Some(code);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let _ = writeln!(l.log, "the index consumer ended");
                return None;
            }
        }
        let now = Instant::now();
        if now >= next_sweep {
            if let Some(code) = l.sweep() {
                return Some(code);
            }
            l.ack(acks);
            next_sweep = now + cfg.sweep_every;
        }
        if now >= next_repin {
            if cfg.repin.as_ref().is_some_and(Repin::replaced) {
                let _ = writeln!(
                    l.log,
                    "the pinned binary was replaced; exiting for the new one"
                );
                return Some(0);
            }
            next_repin = now + cfg.repin_every;
        }
    }
}

/// `host#boot`: an instance's part before its pid namespace — `None` when it names no boot (not the
/// dev container).
#[must_use]
pub fn host_boot(instance: &str) -> Option<&str> {
    let head = instance.split_once('/').map_or(instance, |(h, _)| h);
    head.split_once('#')
        .is_some_and(|(h, b)| !h.is_empty() && !b.is_empty())
        .then_some(head)
}

/// The consumer group a forwarder on `hostname` keeps: `inbox/<hostname>`, the name cut to what a
/// group segment may hold.
#[must_use]
pub fn group(hostname: &str) -> String {
    let name: String = hostname
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    let name = if name.is_empty() {
        "host".to_owned()
    } else {
        name
    };
    format!("{}{name}", jkb_core::exec::INBOX_GROUP_PREFIX)
}

fn stamped(log: &mut dyn Write, line: &str) {
    let _ = writeln!(log, "jkb exec forward: {line}");
}

/// `jkb exec forward [--once]`: run in the dev container, against `jkb serve`, with the container
/// credential.
///
/// # Errors
/// Not in the dev container (no container boot), the lock cannot be taken, or — with `once` — the
/// sweep was refused.
pub fn run_cli(once: bool) -> Result<()> {
    let instance = crate::notify::instance();
    let Some(boot) = host_boot(&instance).map(str::to_owned) else {
        bail!(
            "jkb exec forward runs in the dev container, outside the agent's sandbox: this process's \
             instance {instance:?} names no container boot (JKB_NS_MARKER)"
        );
    };
    let backend = crate::remote::client(
        &crate::remote::daemon_url(),
        crate::remote::Purpose::Forwarder,
    )
    .map_err(|e| anyhow::anyhow!("building the daemon client: {}", e.message))?;
    let mut err = std::io::stderr();
    if once {
        let swept = sweep(&backend, &boot, post_any, &mut err)
            .map_err(|e| anyhow::anyhow!("sweep refused ({:?}): {}", e.code, e.message))?;
        stamped(
            &mut err,
            &format!(
                "{} posted, {} not posted, {} taken by another deliverer",
                swept.posted, swept.failed, swept.lost
            ),
        );
        return Ok(());
    }
    let marker = std::env::var_os("JKB_NS_MARKER")
        .map(PathBuf::from)
        .context("JKB_NS_MARKER is not set, so there is no directory for the forwarder's lock")?;
    let lock_path = marker
        .parent()
        .context("JKB_NS_MARKER names no directory")?
        .join("exec-forward.lock");
    let Some(_lock) = lock(&lock_path)? else {
        stamped(
            &mut err,
            &format!("another forwarder holds {}; exiting", lock_path.display()),
        );
        return Ok(());
    };
    let repin = std::env::var_os(HOOK_BIN_VAR)
        .filter(|v| !v.is_empty())
        .map(|bin| Repin::new(PathBuf::from(bin), Path::new("/proc/self/exe")));
    if repin.is_none() {
        stamped(
            &mut err,
            &format!("{HOOK_BIN_VAR} is not set: a re-pin will not restart this forwarder"),
        );
    }
    let cfg = Config {
        boot,
        group: group(&crate::owner::hostname()),
        repin,
        sweep_every: SWEEP_EVERY,
        repin_every: REPIN_EVERY,
        poll_interval: Duration::from_secs(1),
        post: post_any,
    };
    stamped(
        &mut err,
        &format!(
            "forwarding run-request results to the sessions of {} (group {}, inbox body version {})",
            cfg.boot,
            cfg.group,
            jkb_core::exec::INBOX_BODY_VERSION
        ),
    );
    let code = run(&backend, &cfg, &mut err);
    if code != 0 {
        bail!("jkb exec forward stopped (exit {code})");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
