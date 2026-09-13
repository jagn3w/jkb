//! `jkb mq` — the message queue from the command line (design r3.2 Q6).
//!
//! Every verb goes through [`jkb_api::Backend`], never through `jkb_core::mq` directly: today that is
//! a [`jkb_api::LocalBackend`], and in the dev container it will be the host daemon, so the CLI and
//! the daemon cannot disagree about what an operation does.
//!
//! `jkb mq subscribe` is the language-neutral consumer API: NDJSON events on stdout, one command per
//! line on stdin. The protocol is specified in `docs/message-queue.md`, and pinned end to end by
//! `tests/cli.rs` driving a real `jkb` through pipes.

use std::io::{BufRead as _, Write};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;

use anyhow::{bail, Result};
use clap::Subcommand;
use jkb_api::{ApiError, Backend, ErrorCode, Message, Request, Response, SpecInput, Topic};
use serde_json::{json, Value};

const HOUR_MS: i64 = 60 * 60 * 1000;
const DAY_MS: i64 = 24 * HOUR_MS;

#[derive(Subcommand)]
pub enum MqCmd {
    /// Create or list topics.
    Topic {
        #[command(subcommand)]
        cmd: TopicCmd,
    },
    /// Create or list consumer groups.
    Group {
        #[command(subcommand)]
        cmd: GroupCmd,
    },
    /// Send one message; prints its seq.
    Send {
        /// The topic (it must already exist).
        topic: String,
        /// What the message is about (Kafka's key), e.g. `host/mac/repo/jkb/session/<id>`.
        #[arg(long)]
        key: String,
        /// What consumers dispatch on, e.g. `notify.post`.
        #[arg(long)]
        kind: String,
        /// The JSON body, or `-` to read it from stdin.
        #[arg(long)]
        payload: String,
        /// Expire after this many milliseconds (expired messages are still delivered).
        #[arg(long)]
        ttl_ms: Option<i64>,
        /// Who is sending, for diagnosis (default `jkb-cli:<pid>`).
        #[arg(long)]
        producer: Option<String>,
    },
    /// Consume a topic as NDJSON on stdout, acking with `{"ack":<seq>}` lines on stdin. EOF on
    /// stdin ends the subscription; whatever was not acked is delivered again next time.
    Subscribe {
        /// The topic.
        topic: String,
        /// The consumer group (created if missing).
        #[arg(long)]
        group: String,
        /// A NEW group starts before everything the topic holds, instead of after it.
        #[arg(long)]
        from_start: bool,
        /// Commit each message before emitting it: a crash loses it instead of redelivering it.
        #[arg(long)]
        at_most_once: bool,
        /// Messages fetched per poll.
        #[arg(long, default_value_t = 50)]
        batch: usize,
        /// How long to wait between polls that found nothing, in milliseconds.
        #[arg(long, default_value_t = 250)]
        interval_ms: u64,
    },
    /// The newest messages a topic holds, oldest first. Moves no group.
    Tail {
        /// The topic.
        topic: String,
        /// How many.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Remove idle groups and reap consumed, expired messages (the reap service runs this).
    Compact {
        /// Ignore each topic's compaction interval.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
pub enum TopicCmd {
    /// Create a topic. Re-running with the same limits is a no-op; different limits are refused.
    Create {
        /// The topic, e.g. `claude/notify`.
        topic: String,
        /// Size cap in bytes (default 10 MiB).
        #[arg(long)]
        max_bytes: Option<i64>,
        /// Message cap (default 10,000).
        #[arg(long)]
        max_messages: Option<i64>,
        /// TTL for messages that name none, in milliseconds (default: none).
        #[arg(long)]
        default_ttl_ms: Option<i64>,
        /// Remove a group nobody has polled or acked for this many days (default 7).
        #[arg(long)]
        group_idle_days: Option<i64>,
        /// Compact at most this often, in hours (default 72).
        #[arg(long)]
        compact_every_hours: Option<i64>,
    },
    /// Every topic, with what it holds.
    Ls,
}

#[derive(Subcommand)]
pub enum GroupCmd {
    /// Create a consumer group. An existing group keeps its position.
    Create {
        /// The topic.
        topic: String,
        /// The group.
        group: String,
        /// Start before everything the topic holds, instead of after it.
        #[arg(long)]
        from_start: bool,
    },
    /// A topic's groups, with their positions and backlog.
    Ls {
        /// The topic.
        topic: String,
    },
}

/// A refusal with its wire error, displayed as the message alone. [`run`] prints its error — or
/// `bad_request` for invalid input, `internal` for anything else — under `--json`.
#[derive(Debug)]
struct Refusal(ApiError);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.message)
    }
}

impl std::error::Error for Refusal {}

fn refusal(code: ErrorCode, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Refusal(ApiError::with_code(code, message)))
}

/// Call the backend; a refusal becomes a [`Refusal`] error.
fn call(backend: &dyn Backend, request: Request) -> Result<Response> {
    backend
        .call(request)
        .map_err(|e| anyhow::Error::new(Refusal(e)))
}

fn print_json(v: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn topics(backend: &dyn Backend) -> Result<Vec<Topic>> {
    match call(backend, Request::MqInspect {})? {
        Response::Topics { topics } => Ok(topics),
        other => bail!("unexpected response to mq.inspect: {other:?}"),
    }
}

/// Run a `jkb mq` verb.
///
/// Under `--json`, a failure is ALSO printed to stdout as `{"error":{"code":…,"message":…}}` — once,
/// here, for every verb, so a scripted producer can branch on `queue_full` versus `no_such_topic`
/// rather than on stderr prose and a shared exit status of 1. `subscribe` is the exception: its
/// stdout is the event stream, which reports its own errors as events.
///
/// # Errors
/// A refused operation, unreadable input, or a failure writing output.
pub fn run(backend: &dyn Backend, cmd: MqCmd, json_out: bool) -> Result<()> {
    let prints = prints_json_errors(&cmd);
    let result = dispatch(backend, cmd, json_out);
    if let (true, Err(e)) = (json_out && prints, &result) {
        print_json_error(e);
    }
    result
}

/// The wire code for a database that would not open: `schema_newer` when a newer jkb migrated it,
/// `unavailable` otherwise. One mapping, for the daemon's answer and a local `jkb --json mq` alike.
#[must_use]
pub fn open_failure_code(e: &anyhow::Error) -> ErrorCode {
    match e.downcast_ref::<jkb_core::Error>() {
        Some(jkb_core::Error::SchemaNewer { .. }) => ErrorCode::SchemaNewer,
        _ => ErrorCode::Unavailable,
    }
}

/// A refusal with `code`, for [`print_failure`] to print as that code.
pub fn refused(code: ErrorCode, message: impl Into<String>) -> anyhow::Error {
    refusal(code, message)
}

/// Under `--json`, the error line for `cmd` failing with `e` — wherever that failure arose: in the
/// verb, at a database that would not open, or at a remote-mode refusal. The ONE place the rule
/// lives, `subscribe`'s exclusion with it: its stdout is its event stream, and its failure to start
/// has no event (see the protocol doc).
pub fn print_failure(cmd: &MqCmd, e: &anyhow::Error) {
    if prints_json_errors(cmd) {
        print_json_error(e);
    }
}

/// Every verb but `subscribe`, whose stdout is its event stream.
const fn prints_json_errors(cmd: &MqCmd) -> bool {
    !matches!(cmd, MqCmd::Subscribe { .. })
}

/// `{"error":{"code":…,"message":…}}` on stdout for `e`: a [`Refusal`]'s own error, else `internal`.
fn print_json_error(e: &anyhow::Error) {
    let api = e.downcast_ref::<Refusal>().map_or_else(
        || ApiError::with_code(ErrorCode::Internal, format!("{e:#}")),
        |r| r.0.clone(),
    );
    println!("{}", json!({ "error": api }));
}

#[allow(clippy::too_many_lines)] // a flat command dispatcher; one arm per subcommand, as in main.rs
fn dispatch(backend: &dyn Backend, cmd: MqCmd, json_out: bool) -> Result<()> {
    match cmd {
        MqCmd::Topic { cmd: TopicCmd::Ls } => {
            let topics = topics(backend)?;
            if json_out {
                return print_json(&topics);
            }
            if topics.is_empty() {
                println!("(no topics)");
            }
            for t in topics {
                println!(
                    "{}  {} msgs / {} max  {} / {} bytes  {} group(s)  newest seq {}",
                    t.name,
                    t.messages,
                    t.max_messages,
                    t.bytes,
                    t.max_bytes,
                    t.groups.len(),
                    t.newest_seq
                        .map_or_else(|| "-".to_owned(), |s| s.to_string())
                );
            }
            Ok(())
        }
        MqCmd::Topic {
            cmd:
                TopicCmd::Create {
                    topic,
                    max_bytes,
                    max_messages,
                    default_ttl_ms,
                    group_idle_days,
                    compact_every_hours,
                },
        } => {
            let spec = SpecInput {
                max_bytes,
                max_messages,
                default_ttl_ms,
                group_idle_ms: group_idle_days.map(|d| d.saturating_mul(DAY_MS)),
                compact_every_ms: compact_every_hours.map(|h| h.saturating_mul(HOUR_MS)),
            };
            let r = call(
                backend,
                Request::MqTopicCreate {
                    topic: topic.clone(),
                    spec,
                },
            )?;
            if json_out {
                return print_json(&r);
            }
            match r {
                Response::Created { created: true } => println!("created topic {topic}"),
                _ => println!("topic {topic} already exists with this spec"),
            }
            Ok(())
        }
        MqCmd::Group {
            cmd:
                GroupCmd::Create {
                    topic,
                    group,
                    from_start,
                },
        } => {
            let r = call(
                backend,
                Request::MqGroupCreate {
                    topic: topic.clone(),
                    group: group.clone(),
                    from_start,
                },
            )?;
            if json_out {
                return print_json(&r);
            }
            match r {
                Response::Created { created: true } => {
                    println!("created group {group} on {topic}");
                }
                _ => println!("group {group} already exists on {topic} (position kept)"),
            }
            Ok(())
        }
        MqCmd::Group {
            cmd: GroupCmd::Ls { topic },
        } => {
            let Some(t) = topics(backend)?.into_iter().find(|t| t.name == topic) else {
                // Found missing here rather than refused by an op: the same code as the op's.
                return Err(refusal(
                    ErrorCode::NoSuchTopic,
                    format!("no such topic: {topic}"),
                ));
            };
            if json_out {
                return print_json(&t.groups);
            }
            if t.groups.is_empty() {
                println!("(no groups on {topic})");
            }
            for g in t.groups {
                println!(
                    "{}  position {}  backlog {}  last poll {}",
                    g.name,
                    g.position,
                    g.backlog,
                    g.last_poll_at
                        .map_or_else(|| "never".to_owned(), |t| t.to_string())
                );
            }
            Ok(())
        }
        MqCmd::Send {
            topic,
            key,
            kind,
            payload,
            ttl_ms,
            producer,
        } => {
            let text = if payload == "-" {
                let mut s = String::new();
                std::io::stdin()
                    .read_to_string_checked(&mut s)
                    .map_err(|e| {
                        refusal(
                            ErrorCode::BadRequest,
                            format!("reading the payload from stdin: {e}"),
                        )
                    })?;
                s
            } else {
                payload
            };
            let payload: Value = serde_json::from_str(&text).map_err(|e| {
                refusal(
                    ErrorCode::BadRequest,
                    format!("--payload is not valid JSON: {e}"),
                )
            })?;
            let r = call(
                backend,
                Request::MqSend {
                    topic,
                    key,
                    kind,
                    payload,
                    ttl_ms,
                    producer: producer.unwrap_or_else(|| format!("jkb-cli:{}", std::process::id())),
                },
            )?;
            if json_out {
                return print_json(&r);
            }
            if let Response::Sent { seq } = r {
                println!("{seq}");
            }
            Ok(())
        }
        MqCmd::Tail { topic, limit } => {
            let Response::Messages { messages } = call(backend, Request::MqTail { topic, limit })?
            else {
                bail!("unexpected response to mq.tail");
            };
            if json_out {
                return print_json(&messages);
            }
            for m in messages {
                println!(
                    "{}  {}  {}  {}{}",
                    m.seq,
                    m.kind,
                    m.key,
                    m.payload,
                    if m.expired { "  (expired)" } else { "" }
                );
            }
            Ok(())
        }
        MqCmd::Compact { force } => {
            let r = call(backend, Request::MqCompact { force })?;
            if json_out {
                return print_json(&r);
            }
            if let Response::Compacted {
                topics_compacted,
                topics_skipped,
                messages_reaped,
                groups_removed,
            } = r
            {
                println!(
                    "compacted {topics_compacted} topic(s), skipped {topics_skipped}; reaped \
                     {messages_reaped} message(s), removed {groups_removed} idle group(s)"
                );
            }
            Ok(())
        }
        MqCmd::Subscribe {
            topic,
            group,
            from_start,
            at_most_once,
            batch,
            interval_ms,
        } => {
            let opts = SubscribeOpts {
                topic,
                group,
                from_start,
                at_most_once,
                batch: batch.max(1),
                interval: Duration::from_millis(interval_ms.max(1)),
                // Longer than the remote client's 5 s unreachable cache, so a restart fits.
                eof_ack_wait: Duration::from_secs(10),
            };
            let inputs = spawn_stdin_reader();
            let code = subscribe(backend, &opts, &inputs, &mut std::io::stdout().lock())?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
    }
}

/// `read_to_string` with the `io::Read` import kept local to the one place it is needed.
trait ReadToStringChecked {
    fn read_to_string_checked(&mut self, buf: &mut String) -> std::io::Result<usize>;
}

impl ReadToStringChecked for std::io::Stdin {
    fn read_to_string_checked(&mut self, buf: &mut String) -> std::io::Result<usize> {
        std::io::Read::read_to_string(self, buf)
    }
}

/// What `subscribe` needs to know.
pub struct SubscribeOpts {
    /// The topic.
    pub topic: String,
    /// The group.
    pub group: String,
    /// A new group starts from the beginning.
    pub from_start: bool,
    /// Ack before emitting.
    pub at_most_once: bool,
    /// Messages per poll.
    pub batch: usize,
    /// Wait between empty polls.
    pub interval: Duration,
    /// How long stdin closing waits for a held ack to be applied before giving up on it loudly.
    pub eof_ack_wait: Duration,
}

/// One line of the consumer's stdin.
#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    /// `{"ack": <seq>}`.
    Ack(i64),
    /// A line that is not a command this protocol knows (including one that is not UTF-8).
    Bad(String),
    /// stdin closed.
    Eof,
    /// Reading stdin failed. Not EOF: acks sent after it would silently never apply.
    ReadFailed(String),
}

/// Parse one stdin line.
#[must_use]
pub fn parse_input(line: &str) -> Input {
    match serde_json::from_str::<Value>(line.trim()) {
        Ok(Value::Object(map)) if map.len() == 1 => match map.get("ack").and_then(Value::as_i64) {
            Some(seq) => Input::Ack(seq),
            None => Input::Bad(line.to_owned()),
        },
        _ => Input::Bad(line.to_owned()),
    }
}

fn spawn_stdin_reader() -> Receiver<Input> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let input = match stdin.read_until(b'\n', &mut buf) {
                Ok(0) => Input::Eof,
                // Bytes, not `lines()`: a line that is not UTF-8 is a bad command to report, not a
                // read error that ends the stream and silently drops every ack after it.
                Ok(_) => match std::str::from_utf8(&buf) {
                    Ok(line) if line.trim().is_empty() => continue,
                    Ok(line) => parse_input(line),
                    Err(_) => Input::Bad(String::from_utf8_lossy(&buf).trim_end().to_owned()),
                },
                Err(e) => Input::ReadFailed(e.to_string()),
            };
            let last = matches!(input, Input::Eof | Input::ReadFailed(_));
            if tx.send(input).is_err() || last {
                return;
            }
        }
    });
    rx
}

fn emit(out: &mut dyn Write, event: &Value) -> bool {
    writeln!(out, "{event}").and_then(|()| out.flush()).is_ok()
}

fn error_event(code: ErrorCode, reason: &str, fatal: bool) -> Value {
    json!({ "event": "error", "code": code, "reason": reason, "fatal": fatal })
}

/// Refusals that end by themselves, and so are waited out rather than ending the stream: a lock held
/// past the busy timeout, a daemon restarting — and a daemon older than the database, which
/// `setup.sh` restarts, but only when `backend` is such a daemon. In-process, `schema_newer` means
/// THIS process is too old, and waiting would keep it from ever being replaced: it is fatal, so the
/// supervisor starts the newer binary.
fn transient(code: ErrorCode, backend: &dyn Backend) -> bool {
    match code {
        ErrorCode::Busy | ErrorCode::Unavailable => true,
        ErrorCode::SchemaNewer => backend.schema_newer_clears(),
        _ => false,
    }
}

/// The refusals an ack can meet that are about that ack alone, and so end nothing: the stream reports
/// them and goes on. (`no_such_group` is handled too, by recreating the group.)
const ACK_REFUSALS: &[ErrorCode] = &[
    ErrorCode::NoSuchGroup,
    ErrorCode::AckBeyondEnd,
    ErrorCode::Invalid,
    ErrorCode::BadRequest,
];

/// What a regroup came to.
enum Regroup {
    Created,
    /// The backend is waiting: try again after the interval, not at once.
    Waiting,
    Over(i32),
}

/// What one call came to.
enum Called {
    Done(Response),
    /// A transient refusal, reported if it began an outage: try again later.
    Waiting,
    /// One of the refusals the caller said it handles.
    Refused(ApiError),
    /// The stream is over, with this exit code: a refusal the caller does not handle (reported as a
    /// fatal event already), or stdout gone.
    Over(i32),
}

/// One subscription's connection to its backend and its stdout. Every call goes through
/// [`Stream::call`], and the rule is structural rather than remembered per call site: a caller names
/// the refusals it handles, a transient one is waited out, and **anything else is fatal** — so a
/// refusal no one planned for (`schema_newer` in-process, a code added later) ends the stream instead
/// of being taken for a harmless one.
struct Stream<'a> {
    backend: &'a dyn Backend,
    o: &'a SubscribeOpts,
    out: &'a mut dyn Write,
    /// The op whose transient refusal began the outage being waited out, reported already.
    outage: Option<&'static str>,
    /// The code of the latest transient refusal, for the event that gives up on one.
    last_transient: ErrorCode,
    /// An ack not applied yet because the backend was waiting. Acks commit *through* a seq, so one
    /// pending seq stands for every ack before it.
    pending_ack: Option<i64>,
}

impl Stream<'_> {
    fn emit(&mut self, event: &Value) -> bool {
        emit(self.out, event)
    }

    /// Call the backend, handling the refusals in `handled` by returning them and waiting out
    /// transient ones — reported once per outage, except `busy`, which is ordinary contention.
    ///
    /// An outage ends when a call succeeds — unless it began with an ack that is still held, which
    /// only that ack succeeding ends: a poll can pass (a read) while the ack (a write) is refused.
    fn call(&mut self, request: Request, handled: &[ErrorCode]) -> Called {
        let op = request.op();
        match self.backend.call(request) {
            Ok(response) => {
                let held_ack = self.outage == Some("mq.ack") && self.pending_ack.is_some();
                if self.outage == Some(op) || !held_ack {
                    self.outage = None;
                }
                Called::Done(response)
            }
            Err(e) if transient(e.code, self.backend) => {
                self.last_transient = e.code;
                if e.code != ErrorCode::Busy && self.outage.is_none() {
                    self.outage = Some(op);
                    if !self.emit(&error_event(e.code, &e.message, false)) {
                        return Called::Over(0);
                    }
                }
                Called::Waiting
            }
            Err(e) if handled.contains(&e.code) => Called::Refused(e),
            Err(e) => {
                self.emit(&error_event(e.code, &e.message, true));
                Called::Over(1)
            }
        }
    }

    /// Apply a held ack, if there is one. `Some(exit code)` when the stream ends.
    fn flush_ack(&mut self) -> Option<i32> {
        let seq = self.pending_ack.take()?;
        self.ack(seq)
    }

    /// Commit through `seq`, holding it when the backend is waiting. `Some(exit code)` when the
    /// stream ends.
    fn ack(&mut self, seq: i64) -> Option<i32> {
        if let Some(code) = self.flush_ack() {
            return Some(code);
        }
        if let Some(held) = self.pending_ack {
            // Still waiting: this ack joins the held one.
            self.pending_ack = Some(held.max(seq));
            return None;
        }
        match self.call(ack(self.o, seq), ACK_REFUSALS) {
            Called::Done(_) => None,
            Called::Waiting => {
                self.pending_ack = Some(seq);
                None
            }
            Called::Over(code) => Some(code),
            Called::Refused(e) if e.code == ErrorCode::NoSuchGroup => match self.regroup(&e) {
                Regroup::Created | Regroup::Waiting => None,
                Regroup::Over(code) => Some(code),
            },
            // About this ack alone: reported, and the stream goes on.
            Called::Refused(e)
                if matches!(
                    e.code,
                    ErrorCode::AckBeyondEnd | ErrorCode::Invalid | ErrorCode::BadRequest
                ) =>
            {
                (!self.emit(&error_event(e.code, &e.message, false))).then_some(0)
            }
            Called::Refused(e) => Some(self.fatal(&e)),
        }
    }

    /// A refusal a call site listed but has no arm for: the list and the arms drifted. Fatal, like
    /// any other unhandled refusal — never a panic.
    fn fatal(&mut self, e: &ApiError) -> i32 {
        self.emit(&error_event(e.code, &e.message, true));
        1
    }

    /// The group was removed after idling. Recreate it from now and say so — one rule, for a poll
    /// and an ack alike; a backend that is waiting is asked again by the next poll, after the usual
    /// interval.
    fn regroup(&mut self, e: &ApiError) -> Regroup {
        let reason = format!(
            "{} — the group was removed after idling; recreated from now, and messages sent in \
             between are not delivered",
            e.message
        );
        match self.call(group_create(self.o, false), &[]) {
            Called::Done(_) if self.emit(&error_event(e.code, &reason, false)) => Regroup::Created,
            Called::Done(_) => Regroup::Over(0),
            Called::Waiting => Regroup::Waiting,
            Called::Over(code) => Regroup::Over(code),
            Called::Refused(again) => Regroup::Over(self.fatal(&again)),
        }
    }

    /// The fatal event for an ack given up on at the end of the run.
    fn give_up(&mut self, seq: i64) -> i32 {
        let reason = format!(
            "stdin closed with the ack through {seq} not applied (the backend did not answer within \
             {}s); those messages will be delivered again",
            self.o.eof_ack_wait.as_secs()
        );
        self.emit(&error_event(self.last_transient, &reason, true));
        1
    }

    /// stdin closed: apply a held ack before ending — waiting out the outage that holds it, up to
    /// `eof_ack_wait`, because EOF is the documented way to stop and the acks before it are promised.
    /// One that still cannot be applied ends the stream with a fatal event naming it, never silently.
    fn finish(&mut self) -> Option<i32> {
        self.finish_by(std::time::Instant::now() + self.o.eof_ack_wait)
    }

    /// [`Stream::finish`] against a deadline already running — `join`'s, when stdin closed before
    /// the group could be created, so the two phases share one `eof_ack_wait`.
    fn finish_by(&mut self, deadline: std::time::Instant) -> Option<i32> {
        loop {
            if let Some(code) = self.flush_ack() {
                return Some(code);
            }
            let Some(seq) = self.pending_ack else {
                return Some(0);
            };
            if std::time::Instant::now() >= deadline {
                return Some(self.give_up(seq));
            }
            std::thread::sleep(self.o.interval);
        }
    }

    /// One stdin command. `Some(exit code)` when the subscription is over.
    fn input(&mut self, input: Input) -> Option<i32> {
        match input {
            Input::Eof => self.finish(),
            Input::ReadFailed(why) => {
                self.emit(&error_event(
                    ErrorCode::Internal,
                    &format!("reading stdin: {why}"),
                    true,
                ));
                Some(1)
            }
            Input::Bad(line) => {
                let event = error_event(
                    ErrorCode::BadRequest,
                    &format!("not a command: {line}"),
                    false,
                );
                (!self.emit(&event)).then_some(0)
            }
            Input::Ack(seq) => self.ack(seq),
        }
    }

    /// Create the group if missing, waiting out a transient refusal. `Some(exit code)` when the
    /// subscription ends before it starts.
    ///
    /// An ack read meanwhile is only held: sent now, before the group is known to exist, it could be
    /// refused `no_such_group` and "recreate" the group from now — dropping `--from-start`. If stdin
    /// closes with one held, the group is still created and the ack applied, within `eof_ack_wait`:
    /// a refused `group_create` says nothing about whether the group already exists.
    fn join(&mut self, inputs: &Receiver<Input>) -> Option<i32> {
        let mut closing: Option<std::time::Instant> = None;
        loop {
            match self.call(group_create(self.o, self.o.from_start), &[]) {
                Called::Done(_) => return closing.and_then(|deadline| self.finish_by(deadline)),
                Called::Over(code) => return Some(code),
                Called::Refused(e) => return Some(self.fatal(&e)),
                Called::Waiting => {}
            }
            if let Some(deadline) = closing {
                if std::time::Instant::now() >= deadline {
                    let seq = self.pending_ack.unwrap_or_default();
                    return Some(self.give_up(seq));
                }
                std::thread::sleep(self.o.interval);
                continue;
            }
            match inputs.recv_timeout(self.o.interval) {
                Ok(Input::Ack(seq)) => {
                    self.pending_ack = Some(self.pending_ack.map_or(seq, |held| held.max(seq)));
                }
                Ok(Input::Eof) | Err(RecvTimeoutError::Disconnected) => {
                    if self.pending_ack.is_none() {
                        return Some(0);
                    }
                    closing = Some(std::time::Instant::now() + self.o.eof_ack_wait);
                }
                Ok(input) => {
                    if let Some(code) = self.input(input) {
                        return Some(code);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// Wait up to the poll interval for a command. `Some(exit code)` when the subscription is over.
    fn wait(&mut self, inputs: &Receiver<Input>) -> Option<i32> {
        match inputs.recv_timeout(self.o.interval) {
            Ok(input) => self.input(input),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => self.finish(),
        }
    }
}

/// The subscription loop, over any backend, input channel and output — the CLI passes stdin and
/// stdout. Returns the process exit code: 0 for a clean end (stdin closed, or stdout gone), 1 after
/// a fatal error event.
///
/// Never emits a message twice in one run: each poll asks for what comes after the last seq this run
/// emitted (`after`, the fetch position), not after the committed position, which is where unacked
/// messages still sit.
///
/// Emits `caught_up` when a poll returns fewer messages than a batch holds — once when the stream
/// first has nothing more to hand over, and again after each burst — so a consumer can fold a
/// backlog to its net effect before acting. Transient refusals ([`transient`]) are waited out, at
/// startup too: `busy` silently, `unavailable` — and `schema_newer` from a daemon that can be
/// restarted under it — with one non-fatal error event per outage. An ack made meanwhile is held and
/// applied once the backend answers. Any refusal not handled where it arises is fatal ([`Stream`]).
///
/// # Errors
/// Only for failures outside the protocol; a refused operation is reported as an error event.
pub fn subscribe(
    backend: &dyn Backend,
    o: &SubscribeOpts,
    inputs: &Receiver<Input>,
    out: &mut dyn Write,
) -> Result<i32> {
    let mut s = Stream {
        backend,
        o,
        out,
        outage: None,
        last_transient: ErrorCode::Unavailable,
        pending_ack: None,
    };
    if let Some(code) = s.join(inputs) {
        return Ok(code);
    }
    let mut emitted: i64 = 0;
    let mut reported_corrupt: Option<i64> = None;
    let mut caught_up_announced = false;
    loop {
        loop {
            match inputs.try_recv() {
                Ok(input) => {
                    if let Some(code) = s.input(input) {
                        return Ok(code);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(s.finish().unwrap_or(0)),
            }
        }
        if let Some(code) = s.flush_ack() {
            return Ok(code);
        }

        // Fetch after what this run has already handed over, not after the committed position:
        // otherwise a batch of unacked messages comes back on every poll and nothing past it is
        // ever read until the consumer acks.
        let polled = s.call(
            Request::MqPoll {
                topic: o.topic.clone(),
                group: o.group.clone(),
                max: o.batch,
                after: (emitted > 0).then_some(emitted),
            },
            &[ErrorCode::NoSuchGroup, ErrorCode::CorruptPayload],
        );
        let mut handed_over = false;
        match polled {
            Called::Done(Response::Messages { messages }) => {
                let mut drained = messages.len() < o.batch;
                for m in messages {
                    if o.at_most_once {
                        match s.call(ack(o, m.seq), &[]) {
                            Called::Done(_) => {}
                            // Not acked, so not emitted: the next poll fetches it again.
                            Called::Waiting => {
                                drained = false;
                                break;
                            }
                            Called::Over(code) => return Ok(code),
                            Called::Refused(e) => return Ok(s.fatal(&e)),
                        }
                    }
                    if !s.emit(&message_event(&m)) {
                        return Ok(0);
                    }
                    emitted = m.seq;
                    handed_over = true;
                }
                if drained && (handed_over || !caught_up_announced) {
                    caught_up_announced = true;
                    if !s.emit(&json!({ "event": "caught_up", "seq": emitted })) {
                        return Ok(0);
                    }
                }
            }
            Called::Done(other) => bail!("unexpected response to mq.poll: {other:?}"),
            Called::Waiting => {}
            Called::Over(code) => return Ok(code),
            Called::Refused(e) if e.code == ErrorCode::NoSuchGroup => match s.regroup(&e) {
                // Poll the recreated group at once.
                Regroup::Created => continue,
                // Fall through to the interval: straight back to a poll would hot-loop two requests
                // against a daemon that is refusing for being busy.
                Regroup::Waiting => {}
                Regroup::Over(code) => return Ok(code),
            },
            Called::Refused(e) if e.code == ErrorCode::CorruptPayload => {
                // Reported once; the consumer decides whether to ack past it.
                if reported_corrupt != e.seq {
                    reported_corrupt = e.seq;
                    let event = json!({ "event": "unreadable", "seq": e.seq, "reason": e.message });
                    if !s.emit(&event) {
                        return Ok(0);
                    }
                }
            }
            Called::Refused(e) => return Ok(s.fatal(&e)),
        }

        if !handed_over {
            if let Some(code) = s.wait(inputs) {
                return Ok(code);
            }
        }
    }
}

fn group_create(o: &SubscribeOpts, from_start: bool) -> Request {
    Request::MqGroupCreate {
        topic: o.topic.clone(),
        group: o.group.clone(),
        from_start,
    }
}

fn ack(o: &SubscribeOpts, seq: i64) -> Request {
    Request::MqAck {
        topic: o.topic.clone(),
        group: o.group.clone(),
        seq,
    }
}

fn message_event(m: &Message) -> Value {
    json!({ "event": "message", "message": m })
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;
    use std::time::Duration;

    use jkb_api::{Backend, LocalBackend, Request};
    use jkb_core::Db;
    use serde_json::{json, Value};

    use super::{parse_input, subscribe, Input, SubscribeOpts};

    fn backend_with(n: usize) -> LocalBackend {
        let b = LocalBackend::new(Db::open_in_memory().unwrap());
        b.call(Request::MqTopicCreate {
            topic: "t".to_owned(),
            spec: jkb_api::SpecInput::default(),
        })
        .unwrap();
        b.call(Request::MqGroupCreate {
            topic: "t".to_owned(),
            group: "g".to_owned(),
            from_start: true,
        })
        .unwrap();
        for i in 0..n {
            b.call(Request::MqSend {
                topic: "t".to_owned(),
                key: "k".to_owned(),
                kind: "k.m".to_owned(),
                payload: json!(i),
                ttl_ms: None,
                producer: "test".to_owned(),
            })
            .unwrap();
        }
        b
    }

    fn opts(at_most_once: bool) -> SubscribeOpts {
        SubscribeOpts {
            topic: "t".to_owned(),
            group: "g".to_owned(),
            from_start: true,
            at_most_once,
            batch: 2,
            interval: Duration::from_millis(5),
            eof_ack_wait: Duration::from_millis(100),
        }
    }

    fn events(out: &[u8]) -> Vec<Value> {
        String::from_utf8_lossy(out)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn seqs(events: &[Value]) -> Vec<i64> {
        events
            .iter()
            .filter(|e| e["event"] == "message")
            .map(|e| e["message"]["seq"].as_i64().unwrap())
            .collect()
    }

    #[test]
    fn stdin_commands_parse_strictly() {
        assert_eq!(parse_input(r#"{"ack": 7}"#), Input::Ack(7));
        assert!(matches!(parse_input(r#"{"ack": "7"}"#), Input::Bad(_)));
        assert!(matches!(
            parse_input(r#"{"ack": 7, "x": 1}"#),
            Input::Bad(_)
        ));
        assert!(matches!(parse_input("ack 7"), Input::Bad(_)));
    }

    #[test]
    fn each_message_is_emitted_once_per_run_and_unacked_ones_come_back_next_run() {
        let b = backend_with(5);
        // Run 1: emit everything, ack only through the second message, then EOF.
        let (tx, rx) = channel();
        let mut out = Vec::new();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            tx.send(Input::Ack(2)).unwrap();
            std::thread::sleep(Duration::from_millis(30));
            tx.send(Input::Eof).unwrap();
        });
        assert_eq!(subscribe(&b, &opts(false), &rx, &mut out).unwrap(), 0);
        handle.join().unwrap();
        let first = seqs(&events(&out));
        assert_eq!(
            first,
            vec![1, 2, 3, 4, 5],
            "emitted once each though polled repeatedly"
        );

        // Run 2 resumes after the ack: 3, 4, 5 are delivered again — at-least-once.
        let (tx, rx) = channel();
        tx.send(Input::Eof).unwrap();
        let mut out = Vec::new();
        // EOF is read before the first poll, so this run ends at once...
        assert_eq!(subscribe(&b, &opts(false), &rx, &mut out).unwrap(), 0);
        assert!(seqs(&events(&out)).is_empty());
        // ...and a run that stays open sees the unacked messages.
        let (tx, rx) = channel();
        let mut out = Vec::new();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            tx.send(Input::Eof).unwrap();
        });
        subscribe(&b, &opts(false), &rx, &mut out).unwrap();
        h.join().unwrap();
        assert_eq!(seqs(&events(&out)), vec![3, 4, 5]);
    }

    #[test]
    fn at_most_once_commits_before_emitting() {
        let b = backend_with(3);
        let (tx, rx) = channel();
        let mut out = Vec::new();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            tx.send(Input::Eof).unwrap();
        });
        subscribe(&b, &opts(true), &rx, &mut out).unwrap();
        h.join().unwrap();
        assert_eq!(seqs(&events(&out)), vec![1, 2, 3]);
        let (tx, rx) = channel();
        let mut out = Vec::new();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            tx.send(Input::Eof).unwrap();
        });
        subscribe(&b, &opts(true), &rx, &mut out).unwrap();
        h.join().unwrap();
        assert!(seqs(&events(&out)).is_empty(), "nothing is redelivered");
    }

    use jkb_api::ErrorCode::{self, Busy, NoSuchGroup, SchemaNewer, Unavailable};

    /// Serves through `inner`, except that the Nth call of an op answers the Nth entry of its script:
    /// `Some(code)` refuses with it, `None` serves. Past the script, every call serves.
    struct Scripted {
        inner: LocalBackend,
        /// Answers [`Backend::schema_newer_clears`] as a remote daemon would.
        remote: bool,
        /// How many times each op was called.
        calls: std::sync::Mutex<std::collections::HashMap<&'static str, usize>>,
        script: std::sync::Mutex<
            std::collections::HashMap<&'static str, std::collections::VecDeque<Option<ErrorCode>>>,
        >,
    }

    impl Scripted {
        fn new(inner: LocalBackend, script: &[(&'static str, &[Option<ErrorCode>])]) -> Self {
            Self {
                inner,
                remote: false,
                calls: std::sync::Mutex::default(),
                script: std::sync::Mutex::new(
                    script
                        .iter()
                        .map(|(op, s)| (*op, s.iter().copied().collect()))
                        .collect(),
                ),
            }
        }
    }

    impl Scripted {
        fn remote(mut self) -> Self {
            self.remote = true;
            self
        }
    }

    impl Backend for Scripted {
        fn schema_newer_clears(&self) -> bool {
            self.remote
        }

        fn call(&self, request: Request) -> Result<jkb_api::Response, jkb_api::ApiError> {
            *self.calls.lock().unwrap().entry(request.op()).or_default() += 1;
            let next = self
                .script
                .lock()
                .unwrap()
                .get_mut(request.op())
                .and_then(std::collections::VecDeque::pop_front)
                .flatten();
            match next {
                Some(code) => Err(jkb_api::ApiError::with_code(code, "scripted")),
                None => self.inner.call(request),
            }
        }
    }

    /// Run a subscription over `b` until stdin closes after `ms`, sending `acks` first; its events.
    fn run_scripted(
        b: &dyn Backend,
        o: &SubscribeOpts,
        acks: &[(u64, i64)],
        ms: u64,
    ) -> (i32, Vec<Value>) {
        let (tx, rx) = channel();
        // An ack at 0 ms is queued before the subscription starts, so which step reads it is not a race.
        let (now, later): (Vec<_>, Vec<_>) = acks.iter().partition(|(at, _)| *at == 0);
        for (_, seq) in now {
            tx.send(Input::Ack(seq)).unwrap();
        }
        let h = std::thread::spawn(move || {
            for (at, seq) in later {
                std::thread::sleep(Duration::from_millis(at));
                tx.send(Input::Ack(seq)).unwrap();
            }
            std::thread::sleep(Duration::from_millis(ms));
            tx.send(Input::Eof).unwrap();
        });
        let mut out = Vec::new();
        let code = subscribe(b, o, &rx, &mut out).unwrap();
        h.join().unwrap();
        (code, events(&out))
    }

    fn errors(ev: &[Value]) -> Vec<&Value> {
        ev.iter().filter(|e| e["event"] == "error").collect()
    }

    fn position(b: &LocalBackend) -> i64 {
        match b.call(Request::MqInspect {}).unwrap() {
            jkb_api::Response::Topics { topics } => topics[0].groups[0].position,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_transient_refusal_is_waited_out_and_reported_once_per_outage() {
        for (code, reported) in [
            // Ordinary contention: never reported.
            (Busy, 0),
            // A daemon restart, and a daemon older than the database waiting for its restart: said
            // once per outage — two outages here, split by a poll that succeeds — never per failure.
            (Unavailable, 2),
            (SchemaNewer, 2),
        ] {
            let script = [Some(code), Some(code), None, Some(code), Some(code)];
            // Remote: only a daemon's `schema_newer` can clear while this process keeps running.
            let b = Scripted::new(backend_with(1), &[("mq.poll", &script)]).remote();
            let (exit, ev) = run_scripted(&b, &opts(false), &[], 150);
            assert_eq!(exit, 0, "{code:?}: {ev:?}");
            let errs = errors(&ev);
            assert_eq!(errs.len(), reported, "{code:?}: {ev:?}");
            assert!(errs.iter().all(|e| e["fatal"] == false), "{code:?}");
            assert_eq!(
                seqs(&ev),
                vec![1],
                "{code:?}: delivered once, after recovering"
            );
        }
    }

    #[test]
    fn schema_newer_in_process_is_fatal_so_the_supervisor_starts_the_newer_binary() {
        let b = Scripted::new(backend_with(1), &[("mq.poll", &[Some(SchemaNewer)])]);
        let (exit, ev) = run_scripted(&b, &opts(false), &[], 150);
        assert_eq!(exit, 1, "{ev:?}");
        let last = ev.last().unwrap();
        assert_eq!(
            (last["code"].clone(), last["fatal"].clone()),
            (json!("schema_newer"), json!(true))
        );
    }

    #[test]
    fn an_outage_begun_by_a_held_ack_ends_only_when_the_ack_succeeds() {
        // Acks refused for a while, polls answered throughout (a read can pass while writes cannot):
        // one outage, one event — not one per poll that happened to succeed in between.
        let inner = backend_with(1);
        let refused = [Some(Unavailable); 20];
        let b = Scripted::new(inner.clone(), &[("mq.ack", &refused)]);
        let (exit, ev) = run_scripted(&b, &opts(false), &[(40, 1)], 250);
        assert_eq!(exit, 0, "{ev:?}");
        assert_eq!(errors(&ev).len(), 1, "{ev:?}");
        assert_eq!(position(&inner), 1, "applied once the outage ended");
    }

    #[test]
    fn stdin_closing_waits_for_a_held_ack_and_says_so_if_it_cannot_be_applied() {
        // Refused twice, then applied: EOF right behind the ack still commits it.
        let inner = backend_with(1);
        let b = Scripted::new(
            inner.clone(),
            &[("mq.ack", &[Some(Unavailable), Some(Unavailable)])],
        );
        let (exit, ev) = run_scripted(&b, &opts(false), &[(40, 1)], 0);
        assert_eq!(exit, 0, "{ev:?}");
        assert_eq!(
            position(&inner),
            1,
            "the ack before EOF was applied: {ev:?}"
        );

        // Refused past the wait: a fatal event naming the seq, never a silent exit 0.
        let inner = backend_with(1);
        let refused = [Some(Unavailable); 1000];
        let b = Scripted::new(inner.clone(), &[("mq.ack", &refused)]);
        let (exit, ev) = run_scripted(&b, &opts(false), &[(40, 1)], 0);
        assert_eq!(exit, 1, "{ev:?}");
        let last = ev.last().unwrap();
        assert_eq!(last["fatal"], true, "{ev:?}");
        assert!(
            last["reason"].as_str().unwrap().contains("through 1"),
            "{last}"
        );
        assert_eq!(position(&inner), 0);
    }

    #[test]
    fn an_ack_read_before_the_group_exists_is_held_and_keeps_from_start() {
        // The group cannot be created yet; the consumer checkpoints seq 1 meanwhile. Sent at once, the
        // ack would meet `no_such_group` and "recreate" the group at the END — seq 2 never delivered.
        let inner = backend_with(2);
        inner
            .call(Request::MqGroupCreate {
                topic: "t".into(),
                group: "g".into(),
                from_start: true,
            })
            .unwrap();
        let fresh = SubscribeOpts {
            group: "fresh".to_owned(),
            ..opts(false)
        };
        let b = Scripted::new(inner.clone(), &[("mq.group_create", &[Some(Unavailable)])]);
        // At 0 ms: queued before the subscription starts, so it is read while `join` waits.
        let (exit, ev) = run_scripted(&b, &fresh, &[(0, 1)], 150);
        assert_eq!(exit, 0, "{ev:?}");
        assert!(
            errors(&ev).iter().all(|e| e["code"] != "no_such_group"),
            "no false idle-removal: {ev:?}"
        );
        assert!(
            seqs(&ev).contains(&2),
            "from the start, after the held ack: {ev:?}"
        );
    }

    #[test]
    fn a_refusal_no_call_site_handles_is_fatal_wherever_it_arises() {
        // In-process `schema_newer` on an ack (a write), while polls (reads) keep passing: fatal, not
        // taken for a refusal about that one ack. Likewise a code nobody planned for.
        for code in [SchemaNewer, ErrorCode::Internal] {
            let b = Scripted::new(backend_with(1), &[("mq.ack", &[Some(code)])]);
            let (exit, ev) = run_scripted(&b, &opts(false), &[(40, 1)], 150);
            assert_eq!(exit, 1, "{code:?}: {ev:?}");
            assert_eq!(ev.last().unwrap()["fatal"], true, "{code:?}: {ev:?}");
        }
    }

    #[test]
    fn stdin_closing_during_a_startup_outage_still_applies_the_held_ack() {
        let inner = backend_with(1);
        let b = Scripted::new(
            inner.clone(),
            &[(
                "mq.group_create",
                &[Some(Unavailable), Some(Unavailable), Some(Unavailable)],
            )],
        );
        let (exit, ev) = run_scripted(&b, &opts(false), &[(0, 1)], 0);
        assert_eq!(exit, 0, "{ev:?}");
        assert_eq!(position(&inner), 1, "created, then the ack applied: {ev:?}");
    }

    #[test]
    fn giving_up_on_a_held_ack_names_the_refusal_that_held_it() {
        let refused = [Some(Busy); 1000];
        let b = Scripted::new(backend_with(1), &[("mq.ack", &refused)]);
        let (exit, ev) = run_scripted(&b, &opts(false), &[(40, 1)], 0);
        assert_eq!(exit, 1, "{ev:?}");
        assert_eq!(ev.last().unwrap()["code"], "busy", "{ev:?}");
    }

    #[test]
    fn an_outage_whose_op_is_abandoned_does_not_silence_the_next_one() {
        // A regroup's `group_create` meets an outage and is never retried (the next poll finds the
        // group); a later outage must still be reported.
        let b = Scripted::new(
            backend_with(1),
            &[
                ("mq.poll", &[Some(NoSuchGroup), None, Some(Unavailable)]),
                ("mq.group_create", &[None, Some(Unavailable)]),
            ],
        );
        let (exit, ev) = run_scripted(&b, &opts(false), &[], 150);
        assert_eq!(exit, 0, "{ev:?}");
        let errs = errors(&ev);
        assert_eq!(errs.len(), 2, "one per outage: {ev:?}");
        assert!(errs.iter().all(|e| e["fatal"] == false));
    }

    #[test]
    fn a_regroup_the_backend_is_too_busy_for_waits_the_interval_before_polling_again() {
        let b = Scripted::new(
            backend_with(1),
            &[
                ("mq.poll", &[Some(NoSuchGroup); 5000]),
                (
                    "mq.group_create",
                    &[None]
                        .into_iter()
                        .chain([Some(Busy); 5000])
                        .collect::<Vec<_>>(),
                ),
            ],
        );
        let (exit, ev) = run_scripted(&b, &opts(false), &[], 100);
        assert_eq!(exit, 0, "{ev:?}");
        let polls = b.calls.lock().unwrap()["mq.poll"];
        // At a 5 ms interval, 100 ms is ~20 polls; straight back to the poll it was thousands.
        assert!(polls < 100, "{polls} polls in 100 ms");
    }

    #[test]
    fn stdin_closing_during_a_startup_outage_spends_one_wait_not_two() {
        let refused_create = [Some(Unavailable); 60];
        let refused_ack = vec![Some(Unavailable); 100_000];
        let b = Scripted::new(
            backend_with(1),
            &[
                ("mq.group_create", &refused_create),
                ("mq.ack", &refused_ack),
            ],
        );
        let o = SubscribeOpts {
            eof_ack_wait: Duration::from_millis(400),
            ..opts(false)
        };
        let started = std::time::Instant::now();
        let (exit, ev) = run_scripted(&b, &o, &[(0, 1)], 0);
        let took = started.elapsed();
        assert_eq!(exit, 1, "{ev:?}");
        // The group comes up late in the wait and the ack never applies: given up at ~400 ms, not
        // after a second full wait on top.
        assert!(took < Duration::from_millis(650), "{took:?}");
    }

    #[test]
    fn a_daemon_that_is_down_at_startup_is_waited_for() {
        let b = Scripted::new(
            backend_with(1),
            &[("mq.group_create", &[Some(Unavailable), Some(Unavailable)])],
        );
        let (exit, ev) = run_scripted(&b, &opts(false), &[], 150);
        assert_eq!(exit, 0, "{ev:?}");
        assert_eq!(errors(&ev).len(), 1, "{ev:?}");
        assert_eq!(seqs(&ev), vec![1]);
    }

    #[test]
    fn an_ack_made_during_an_outage_is_held_and_applied_after_it() {
        let inner = backend_with(1);
        let b = Scripted::new(inner.clone(), &[("mq.ack", &[Some(Unavailable)])]);
        let (exit, ev) = run_scripted(&b, &opts(false), &[(40, 1)], 120);
        assert_eq!(exit, 0, "{ev:?}");
        assert!(errors(&ev).iter().all(|e| e["fatal"] == false), "{ev:?}");
        assert_eq!(position(&inner), 1, "the held ack was applied: {ev:?}");
    }

    #[test]
    fn at_most_once_does_not_emit_a_message_it_could_not_ack_yet() {
        let inner = backend_with(1);
        let b = Scripted::new(inner.clone(), &[("mq.ack", &[Some(Unavailable)])]);
        let (exit, ev) = run_scripted(&b, &opts(true), &[], 120);
        assert_eq!(exit, 0, "{ev:?}");
        assert_eq!(
            seqs(&ev),
            vec![1],
            "emitted once, after its ack went through"
        );
        assert_eq!(position(&inner), 1);
    }

    #[test]
    fn a_regroup_that_meets_an_outage_is_tried_again_not_fatal() {
        let b = Scripted::new(
            backend_with(1),
            &[
                ("mq.poll", &[Some(NoSuchGroup)]),
                ("mq.group_create", &[None, Some(Unavailable)]),
            ],
        );
        let (exit, ev) = run_scripted(&b, &opts(false), &[], 150);
        assert_eq!(exit, 0, "{ev:?}");
        assert!(errors(&ev).iter().all(|e| e["fatal"] == false), "{ev:?}");
        assert_eq!(seqs(&ev), vec![1]);
    }

    #[test]
    fn a_bad_command_and_a_bad_ack_are_reported_without_ending_the_stream() {
        let b = backend_with(1);
        let (tx, rx) = channel();
        tx.send(Input::Bad("hello".to_owned())).unwrap();
        tx.send(Input::Ack(999)).unwrap();
        let mut out = Vec::new();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            tx.send(Input::Eof).unwrap();
        });
        assert_eq!(subscribe(&b, &opts(false), &rx, &mut out).unwrap(), 0);
        h.join().unwrap();
        let ev = events(&out);
        let errors: Vec<&Value> = ev.iter().filter(|e| e["event"] == "error").collect();
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0]["code"], "bad_request");
        assert_eq!(errors[1]["code"], "ack_beyond_end");
        assert!(errors.iter().all(|e| e["fatal"] == false));
        assert_eq!(seqs(&ev), vec![1]);
    }

    fn run_for(b: &LocalBackend, o: &SubscribeOpts, ms: u64) -> Vec<Value> {
        let (tx, rx) = channel();
        let mut out = Vec::new();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(ms));
            let _ = tx.send(Input::Eof);
        });
        subscribe(b, o, &rx, &mut out).unwrap();
        h.join().unwrap();
        events(&out)
    }

    #[test]
    fn caught_up_marks_the_end_of_a_backlog_and_of_each_burst() {
        let b = backend_with(5);
        let ev = run_for(&b, &opts(false), 60);
        let kinds: Vec<&str> = ev.iter().map(|e| e["event"].as_str().unwrap()).collect();
        // batch 2: [1,2] [3,4] [5]+caught_up, then idle polls announce nothing more.
        assert_eq!(
            kinds,
            vec![
                "message",
                "message",
                "message",
                "message",
                "message",
                "caught_up"
            ]
        );
        assert_eq!(ev[5]["seq"], 5);
        // An empty topic announces caught_up once, straight away.
        let b = backend_with(0);
        let ev = run_for(&b, &opts(false), 40);
        assert_eq!(ev.len(), 1);
        assert_eq!(
            (ev[0]["event"].clone(), ev[0]["seq"].clone()),
            (json!("caught_up"), json!(0))
        );
    }

    #[test]
    fn an_unreadable_message_is_reported_once_with_its_seq_and_an_ack_moves_past_it() {
        let db = Db::open_in_memory().unwrap();
        let b = LocalBackend::new(db.clone());
        b.call(Request::MqTopicCreate {
            topic: "t".to_owned(),
            spec: jkb_api::SpecInput::default(),
        })
        .unwrap();
        b.call(Request::MqGroupCreate {
            topic: "t".to_owned(),
            group: "g".to_owned(),
            from_start: true,
        })
        .unwrap();
        for i in 0..2 {
            b.call(Request::MqSend {
                topic: "t".to_owned(),
                key: "k".to_owned(),
                kind: "k.m".to_owned(),
                payload: json!(i),
                ttl_ms: None,
                producer: "test".to_owned(),
            })
            .unwrap();
        }
        db.write_txn("t", |c, _| {
            c.execute("UPDATE mq_messages SET payload = 'x' WHERE seq = 1", [])?;
            Ok(())
        })
        .unwrap();
        let (tx, rx) = channel();
        let mut out = Vec::new();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            tx.send(Input::Ack(1)).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            tx.send(Input::Eof).unwrap();
        });
        subscribe(&b, &opts(false), &rx, &mut out).unwrap();
        h.join().unwrap();
        let ev = events(&out);
        let unreadable: Vec<&Value> = ev.iter().filter(|e| e["event"] == "unreadable").collect();
        assert_eq!(unreadable.len(), 1, "reported once, not every poll: {ev:?}");
        assert_eq!(unreadable[0]["seq"], 1);
        assert_eq!(seqs(&ev), vec![2], "after the ack, the stream moves on");
    }

    #[test]
    fn a_group_removed_mid_stream_is_recreated_with_a_non_fatal_event() {
        let db = Db::open_in_memory().unwrap();
        let b = LocalBackend::new(db.clone());
        b.call(Request::MqTopicCreate {
            topic: "t".to_owned(),
            spec: jkb_api::SpecInput::default(),
        })
        .unwrap();
        let (tx, rx) = channel();
        let mut out = Vec::new();
        let db2 = db.clone();
        let b2 = b.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            db2.write_txn("t", |c, _| {
                c.execute("DELETE FROM mq_groups", [])?;
                Ok(())
            })
            .unwrap();
            std::thread::sleep(Duration::from_millis(40));
            b2.call(Request::MqSend {
                topic: "t".to_owned(),
                key: "k".to_owned(),
                kind: "k.m".to_owned(),
                payload: json!("after"),
                ttl_ms: None,
                producer: "test".to_owned(),
            })
            .unwrap();
            std::thread::sleep(Duration::from_millis(40));
            tx.send(Input::Eof).unwrap();
        });
        assert_eq!(subscribe(&b, &opts(false), &rx, &mut out).unwrap(), 0);
        h.join().unwrap();
        let ev = events(&out);
        let err = ev
            .iter()
            .find(|e| e["event"] == "error")
            .expect("an error event");
        assert_eq!(
            (err["code"].clone(), err["fatal"].clone()),
            (json!("no_such_group"), json!(false))
        );
        assert_eq!(
            ev.iter().filter(|e| e["event"] == "message").count(),
            1,
            "and it keeps delivering"
        );
    }

    struct Closed;
    impl std::io::Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_stdout_ends_the_subscription_cleanly() {
        let b = backend_with(3);
        let (_tx, rx) = channel();
        assert_eq!(subscribe(&b, &opts(false), &rx, &mut Closed).unwrap(), 0);
    }

    #[test]
    fn a_stdin_read_failure_is_fatal_not_eof() {
        let b = backend_with(0);
        let (tx, rx) = channel();
        tx.send(Input::ReadFailed("boom".to_owned())).unwrap();
        let mut out = Vec::new();
        assert_eq!(subscribe(&b, &opts(false), &rx, &mut out).unwrap(), 1);
        let ev = events(&out);
        assert_eq!(ev.last().unwrap()["fatal"], true);
    }

    #[test]
    fn a_missing_topic_is_a_fatal_error_event_and_exit_code_one() {
        let b = LocalBackend::new(Db::open_in_memory().unwrap());
        let (_tx, rx) = channel();
        let mut out = Vec::new();
        assert_eq!(subscribe(&b, &opts(false), &rx, &mut out).unwrap(), 1);
        let ev = events(&out);
        assert_eq!(
            (ev[0]["code"].clone(), ev[0]["fatal"].clone()),
            (json!("no_such_topic"), json!(true))
        );
    }
}
