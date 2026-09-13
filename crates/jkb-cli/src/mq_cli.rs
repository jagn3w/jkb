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

use anyhow::{anyhow, bail, Context, Result};
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

/// Call the backend; a refusal becomes an error, and under `--json` it is ALSO printed to stdout as
/// `{"error":{"code":…,"message":…}}` — so a scripted producer can branch on `queue_full` versus
/// `no_such_topic` rather than on stderr prose and a shared exit status of 1.
fn call(backend: &dyn Backend, request: Request, json_out: bool) -> Result<Response> {
    backend.call(request).map_err(|e| {
        if json_out {
            println!("{}", json!({ "error": e }));
        }
        anyhow!("{}", e.message)
    })
}

fn print_json(v: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn topics(backend: &dyn Backend, json_out: bool) -> Result<Vec<Topic>> {
    match call(backend, Request::MqInspect {}, json_out)? {
        Response::Topics { topics } => Ok(topics),
        other => bail!("unexpected response to mq.inspect: {other:?}"),
    }
}

/// Run a `jkb mq` verb.
///
/// # Errors
/// A refused operation, unreadable input, or a failure writing output.
#[allow(clippy::too_many_lines)] // a flat command dispatcher; one arm per subcommand, as in main.rs
pub fn run(backend: &dyn Backend, cmd: MqCmd, json_out: bool) -> Result<()> {
    match cmd {
        MqCmd::Topic { cmd: TopicCmd::Ls } => {
            let topics = topics(backend, json_out)?;
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
                json_out,
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
                json_out,
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
            let t = topics(backend, json_out)?
                .into_iter()
                .find(|t| t.name == topic)
                .with_context(|| format!("no such topic: {topic}"))?;
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
                    .context("reading the payload from stdin")?;
                s
            } else {
                payload
            };
            let payload: Value =
                serde_json::from_str(&text).context("--payload is not valid JSON")?;
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
                json_out,
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
            let Response::Messages { messages } =
                call(backend, Request::MqTail { topic, limit }, json_out)?
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
            let r = call(backend, Request::MqCompact { force }, json_out)?;
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
/// backlog to its net effect before acting. A locked database (`busy`) is retried silently, not fatal;
/// an unreachable daemon (`unavailable` — restarting, say) is retried too, with one non-fatal error
/// event per outage so a consumer can show it.
///
/// # Errors
/// Only for failures outside the protocol; a refused operation is reported as an error event.
pub fn subscribe(
    backend: &dyn Backend,
    o: &SubscribeOpts,
    inputs: &Receiver<Input>,
    out: &mut dyn Write,
) -> Result<i32> {
    if let Err(e) = backend.call(group_create(o, o.from_start)) {
        emit(out, &error_event(e.code, &e.message, true));
        return Ok(1);
    }
    let mut emitted: i64 = 0;
    let mut reported_corrupt: Option<i64> = None;
    let mut caught_up_announced = false;
    let mut outage_reported = false;
    loop {
        loop {
            match inputs.try_recv() {
                Ok(input) => {
                    if let Some(code) = handle_input(backend, o, input, out) {
                        return Ok(code);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(0),
            }
        }

        // Fetch after what this run has already handed over, not after the committed position:
        // otherwise a batch of unacked messages comes back on every poll and nothing past it is
        // ever read until the consumer acks.
        let polled = backend.call(Request::MqPoll {
            topic: o.topic.clone(),
            group: o.group.clone(),
            max: o.batch,
            after: (emitted > 0).then_some(emitted),
        });
        let mut handed_over = false;
        if polled.is_ok() {
            outage_reported = false;
        }
        match polled {
            Ok(Response::Messages { messages }) => {
                let drained = messages.len() < o.batch;
                for m in messages {
                    if o.at_most_once {
                        if let Err(e) = backend.call(ack(o, m.seq)) {
                            emit(out, &error_event(e.code, &e.message, true));
                            return Ok(1);
                        }
                    }
                    if !emit(out, &message_event(&m)) {
                        return Ok(0);
                    }
                    emitted = m.seq;
                    handed_over = true;
                }
                if drained && (handed_over || !caught_up_announced) {
                    caught_up_announced = true;
                    if !emit(out, &json!({ "event": "caught_up", "seq": emitted })) {
                        return Ok(0);
                    }
                }
            }
            Ok(other) => bail!("unexpected response to mq.poll: {other:?}"),
            Err(e) if e.code == ErrorCode::NoSuchGroup => {
                if let Some(code) = regroup(backend, o, &e, out) {
                    return Ok(code);
                }
                continue;
            }
            // Another writer held the lock past the busy timeout. Transient: wait and poll again.
            Err(e) if e.code == ErrorCode::Busy => {}
            // The daemon is down or restarting. Its position is in the database, so polling again
            // once it is back resumes exactly where this left off.
            Err(e) if e.code == ErrorCode::Unavailable => {
                if !outage_reported {
                    outage_reported = true;
                    if !emit(out, &error_event(e.code, &e.message, false)) {
                        return Ok(0);
                    }
                }
            }
            Err(e) if e.code == ErrorCode::CorruptPayload => {
                // Reported once; the consumer decides whether to ack past it.
                if reported_corrupt != e.seq {
                    reported_corrupt = e.seq;
                    let event = json!({ "event": "unreadable", "seq": e.seq, "reason": e.message });
                    if !emit(out, &event) {
                        return Ok(0);
                    }
                }
            }
            Err(e) => {
                emit(out, &error_event(e.code, &e.message, true));
                return Ok(1);
            }
        }

        if !handed_over {
            match inputs.recv_timeout(o.interval) {
                Ok(input) => {
                    if let Some(code) = handle_input(backend, o, input, out) {
                        return Ok(code);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(0),
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

/// One stdin command. `Some(exit code)` when the subscription is over.
fn handle_input(
    backend: &dyn Backend,
    o: &SubscribeOpts,
    input: Input,
    out: &mut dyn Write,
) -> Option<i32> {
    match input {
        Input::Eof => Some(0),
        Input::ReadFailed(why) => {
            emit(
                out,
                &error_event(ErrorCode::Internal, &format!("reading stdin: {why}"), true),
            );
            Some(1)
        }
        Input::Bad(line) => {
            let event = error_event(
                ErrorCode::BadRequest,
                &format!("not a command: {line}"),
                false,
            );
            (!emit(out, &event)).then_some(0)
        }
        Input::Ack(seq) => match backend.call(ack(o, seq)) {
            Ok(_) => None,
            Err(e) if e.code == ErrorCode::NoSuchGroup => regroup(backend, o, &e, out),
            Err(e) => (!emit(out, &error_event(e.code, &e.message, false))).then_some(0),
        },
    }
}

/// The group was removed after idling. Recreate it from now and say so — one rule, for a poll and
/// an ack alike. `Some(exit code)` when the subscription cannot go on.
fn regroup(
    backend: &dyn Backend,
    o: &SubscribeOpts,
    e: &ApiError,
    out: &mut dyn Write,
) -> Option<i32> {
    let recreated = backend.call(group_create(o, false)).is_ok();
    let reason = format!(
        "{} — the group was removed after idling; recreated from now, and messages sent in \
         between are not delivered",
        e.message
    );
    if !emit(out, &error_event(e.code, &reason, !recreated)) {
        return Some(0);
    }
    (!recreated).then_some(1)
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

    /// Fails the first `polls` polls with `code`, then serves normally.
    struct Flaky {
        inner: LocalBackend,
        code: jkb_api::ErrorCode,
        polls: std::sync::atomic::AtomicUsize,
    }

    impl Backend for Flaky {
        fn call(&self, request: Request) -> Result<jkb_api::Response, jkb_api::ApiError> {
            use std::sync::atomic::Ordering;
            if matches!(request, Request::MqPoll { .. })
                && self
                    .polls
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok()
            {
                return Err(jkb_api::ApiError::with_code(self.code, "transient"));
            }
            self.inner.call(request)
        }
    }

    #[test]
    fn a_locked_database_or_an_unreachable_daemon_is_waited_out_not_fatal() {
        for (code, reported) in [
            (jkb_api::ErrorCode::Busy, 0),
            // A daemon restart: said once per outage, not once per failed poll.
            (jkb_api::ErrorCode::Unavailable, 1),
        ] {
            let b = Flaky {
                inner: backend_with(1),
                code,
                polls: 3.into(),
            };
            let (tx, rx) = channel();
            let mut out = Vec::new();
            let h = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                tx.send(Input::Eof).unwrap();
            });
            assert_eq!(
                subscribe(&b, &opts(false), &rx, &mut out).unwrap(),
                0,
                "{code:?}"
            );
            h.join().unwrap();
            let ev = events(&out);
            let errors: Vec<&Value> = ev.iter().filter(|e| e["event"] == "error").collect();
            assert_eq!(errors.len(), reported, "{code:?}: {ev:?}");
            assert!(errors.iter().all(|e| e["fatal"] == false), "{code:?}");
            assert_eq!(seqs(&ev), vec![1], "{code:?}: delivered once it recovered");
        }
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
