//! [`RemoteBackend`]: the [`jkb_api::Backend`] a process that must not open `jkb.db` uses to reach
//! `jkb serve`.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use jkb_api::{ApiError, Backend, ErrorCode, Request, Response};

use crate::token;

/// How long an unreachable daemon is remembered, so a burst of short-lived `jkb` processes (a hook
/// per tool call) pays one connect timeout rather than one each.
pub const UNREACHABLE_FOR: Duration = Duration::from_secs(5);

/// The pauses before each retry of a request whose CONNECT failed: two more tries, about 100 ms and
/// then about 300 ms apart, before the daemon is counted unreachable.
///
/// Measured: the dev container reaching the host daemon at `host.docker.internal:7117`, one TCP
/// connection per call, saw 1–3 isolated connect failures in each of two bursts of 400 and 600
/// sequential `jkb design ls` — every one followed at once by successes, the daemon's pid unchanged.
/// About 0.5% of connects fail transiently (most likely Docker Desktop's port forwarding) while the
/// daemon stays up, and marking it down on the first one failed every client — the per-tool-call
/// attest hook among them — untried for [`UNREACHABLE_FOR`].
///
/// A failed connect is retried whatever the request: no byte reached the daemon, so nothing can be
/// applied twice. A request that connected is resent at most once, and only when it carries an
/// idempotency key ([`ATTEMPT_TIMEOUT`]). Retries and their pauses fit inside the request's own
/// deadline, never beyond it (`send_retrying_connect`).
pub const CONNECT_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_millis(100), Duration::from_millis(300)];

/// How long the first attempt of a keyed call may go unanswered after connecting before it is
/// abandoned and sent once more, with the same `Idempotency-Key`, for the rest of the call's deadline.
///
/// Measured: Claude Code's sandbox proxy stalls about 1 in 200 requests to `jkb serve` for 10–30 s —
/// the request connects and no answer comes back — while a warm answer takes milliseconds. Without a
/// key a resend could apply a write twice, so the client waited out its whole 30 s op timeout. A keyed
/// call goes to `POST /v1/op/keyed`, where the daemon runs the op once and answers every attempt of
/// the key with that one answer (`crate::idempotency`), so the resend is safe.
///
/// - **At most two attempts.** Whatever jkb serve answers the second — a success, `busy`,
///   `unauthorized`, anything — is the call's answer.
/// - **Version skew by construction.** A `jkb serve` older than keys answers the keyed route
///   `bad_request` "no such endpoint" without running the op; the call then makes one unkeyed attempt
///   on `/v1/op`, as before keys existed, never resent.
/// - **Not every call is keyed** ([`RemoteBackend::budget`]): a long op, and a call whose whole
///   deadline is shorter than this (a hook's), make one unkeyed attempt.
///
/// **Residual:** the daemon keeps keys in memory only, so an attempt applied just before a daemon
/// restart and resent after it is applied twice.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);

/// The header naming a logical call (the daemon's `server::IDEMPOTENCY_KEY`).
const IDEMPOTENCY_KEY: &str = crate::server::IDEMPOTENCY_KEY;

/// The route a keyed call is sent to (the daemon's `server::KEYED_PATH`).
const KEYED_PATH: &str = crate::server::KEYED_PATH;

/// What one call may spend, from [`RemoteBackend::budget`] — the one place it is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Budget {
    /// How long the daemon may hold an `mq.poll` with nothing to hand over.
    wait: Duration,
    /// The whole call, every attempt and pause included.
    total: Duration,
    /// How long the first attempt may go unanswered before the call is sent once more, keyed; `None`
    /// for a call that is not keyed, and never resent after it connected.
    resend_after: Option<Duration>,
}

/// A fresh 128-bit `Idempotency-Key`, hex. `None` when the system has no randomness to give, and the
/// call is then sent without one — and so never resent after it connected.
fn idempotency_key() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).ok()?;
    Some(bytes.iter().fold(String::with_capacity(32), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    }))
}

/// An answer as read: its status, and its body or the error reading it.
type Read = (reqwest::StatusCode, reqwest::Result<bytes::Bytes>);

fn read(resp: reqwest::blocking::Response) -> Read {
    let status = resp.status();
    (status, resp.bytes())
}

/// Whether jkb serve refused the keyed route as an endpoint it does not have — a daemon older than
/// keys, which did not run the op.
fn no_such_endpoint((status, bytes): &Read) -> bool {
    *status == reqwest::StatusCode::BAD_REQUEST
        && bytes
            .as_ref()
            .ok()
            .and_then(|b| serde_json::from_slice::<ApiError>(b).ok())
            .is_some_and(|e| {
                e.code == ErrorCode::BadRequest && e.message.starts_with("no such endpoint")
            })
}

/// Serves requests by calling a `jkb serve` daemon over HTTP.
pub struct RemoteBackend {
    base: String,
    token_file: PathBuf,
    token: Mutex<Option<String>>,
    /// A token given outright — a role grant or a harness ticket (D52.3) — sent in place of the
    /// token file's, and never re-read: it does not rotate with the daemon.
    fixed: Option<String>,
    client: reqwest::blocking::Client,
    /// The client's connect timeout, which a retry must have room for before its deadline.
    connect: Duration,
    poll_wait: Duration,
    /// How long a request may take beyond any long-poll wait.
    op_timeout: Duration,
    /// [`ATTEMPT_TIMEOUT`], shortened by tests.
    attempt_timeout: Duration,
    down_marker: Option<PathBuf>,
}

fn http_client(connect: Duration) -> Result<reqwest::blocking::Client, ApiError> {
    reqwest::blocking::Client::builder()
        .connect_timeout(connect)
        .build()
        .map_err(|e| ApiError::with_code(ErrorCode::Internal, e.to_string()))
}

impl RemoteBackend {
    /// A backend for the daemon at `base` (e.g. `http://127.0.0.1:7117`), authenticating with the
    /// token in `token_file`.
    ///
    /// # Errors
    /// An [`ErrorCode::Internal`] error if the HTTP client cannot be built.
    pub fn new(base: &str, token_file: PathBuf) -> Result<Self, ApiError> {
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            token_file,
            token: Mutex::new(None),
            fixed: None,
            client: http_client(Duration::from_secs(1))?,
            connect: Duration::from_secs(1),
            poll_wait: Duration::from_secs(2),
            op_timeout: Duration::from_secs(30),
            attempt_timeout: ATTEMPT_TIMEOUT,
            down_marker: None,
        })
    }

    /// Tighter deadlines, for a caller that must not be held up — a Claude Code hook, which runs on
    /// every tool call and blocks the session while it runs: at most `connect` to connect and `total`
    /// for a whole request (beyond any long-poll wait).
    ///
    /// # Errors
    /// An [`ErrorCode::Internal`] error if the HTTP client cannot be rebuilt.
    pub fn with_deadlines(mut self, connect: Duration, total: Duration) -> Result<Self, ApiError> {
        self.client = http_client(connect)?;
        self.connect = connect;
        self.op_timeout = total;
        Ok(self)
    }

    /// How long an `mq.poll` may be held by the daemon when it has nothing to hand over.
    #[must_use]
    pub const fn with_poll_wait(mut self, wait: Duration) -> Self {
        self.poll_wait = wait;
        self
    }

    /// A file whose recent modification means "the daemon was unreachable just now": touched on a
    /// failed connect, removed on success, and consulted before trying, across processes.
    #[must_use]
    pub fn with_down_marker(mut self, marker: PathBuf) -> Self {
        self.down_marker = Some(marker);
        self
    }

    /// Authenticate with `token` instead of the token file's: a role grant, the dev container's
    /// credential, or a harness ticket. The daemon serves the caller it names, held to its role.
    #[must_use]
    pub fn with_token(mut self, token: String) -> Self {
        self.fixed = Some(token);
        self
    }

    fn token(&self, fresh: bool) -> Result<String, ApiError> {
        if let Some(t) = &self.fixed {
            return Ok(t.clone());
        }
        let mut cached = self
            .token
            .lock()
            .map_err(|_| ApiError::with_code(ErrorCode::Internal, "token lock poisoned"))?;
        if fresh || cached.is_none() {
            *cached = Some(token::read(&self.token_file).map_err(|e| {
                ApiError::with_code(
                    ErrorCode::Unavailable,
                    format!("no daemon token ({e}); is jkb serve running on the host?"),
                )
            })?);
        }
        cached
            .clone()
            .ok_or_else(|| ApiError::with_code(ErrorCode::Internal, "no token"))
    }

    fn recently_unreachable(&self) -> bool {
        self.down_marker
            .as_ref()
            .and_then(|m| std::fs::metadata(m).ok())
            .and_then(|meta| meta.modified().ok())
            .and_then(|at| SystemTime::now().duration_since(at).ok())
            .is_some_and(|age| age < UNREACHABLE_FOR)
    }

    fn mark_unreachable(&self, down: bool) {
        let Some(marker) = &self.down_marker else {
            return;
        };
        if down {
            if let Some(dir) = marker.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(marker, b"");
        } else {
            let _ = std::fs::remove_file(marker);
        }
    }

    /// What `request` may spend: its long-poll wait, its whole deadline, and whether it is keyed —
    /// how long its first attempt may go unanswered before it is sent once more. The one place an op's
    /// budget is decided, so no call site can forget it.
    ///
    /// Unkeyed, one attempt with the whole deadline: a **long** op — one given more than the plain
    /// [`RemoteBackend::with_deadlines`] total, `mq.poll`, which the daemon may hold for its wait, and
    /// `ingest.text`, whose capture of a body at the cap holds the writer about half a second and whose
    /// resend could only queue behind it — and a call whose whole deadline is no longer than the
    /// attempt budget (a hook's, whose `TOTAL` is 1 s), which would have no time to resend.
    fn budget(&self, request: &Request) -> Budget {
        let wait = match request {
            Request::MqPoll { .. } => self.poll_wait,
            _ => Duration::ZERO,
        };
        let total = wait + self.op_timeout;
        let long = total > self.op_timeout || matches!(request, Request::IngestText(_));
        Budget {
            wait,
            total,
            resend_after: (!long && self.attempt_timeout < total).then_some(self.attempt_timeout),
        }
    }

    /// Send `request`, keyed with `key` when it is one to resend ([`ATTEMPT_TIMEOUT`]): to the keyed
    /// route for at most the attempt budget, and once more, with the same key, for the rest of the
    /// deadline if that attempt connected and went unanswered. A daemon without the keyed route gets
    /// one unkeyed attempt instead.
    fn send(&self, request: &Request, token: &str, key: Option<&str>) -> Result<Read, ApiError> {
        let budget = self.budget(request);
        let deadline = Instant::now() + budget.total;
        let left = || deadline.saturating_duration_since(Instant::now());
        let url = if budget.wait.is_zero() {
            format!("{}/v1/op", self.base)
        } else {
            format!("{}/v1/op?wait_ms={}", self.base, budget.wait.as_millis())
        };
        // Refused here, before any of it is sent: the daemon refuses a body past its cap by closing the
        // connection mid-upload, and a body many times the cap then read as a daemon out of reach.
        let body = serde_json::to_vec(request)
            .map_err(|e| ApiError::with_code(ErrorCode::Internal, e.to_string()))?;
        if body.len() > crate::MAX_BODY_BYTES {
            return Err(ApiError::with_code(
                ErrorCode::TooLarge,
                format!(
                    "a {}-byte request, more than the {} bytes jkb serve accepts in one",
                    body.len(),
                    crate::MAX_BODY_BYTES
                ),
            ));
        }
        // Shared, so each attempt's copy is a reference count rather than the body again.
        let body = bytes::Bytes::from(body);
        let unkeyed = |left: Duration| {
            self.client
                .post(&url)
                .bearer_auth(token)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone())
                .timeout(left)
        };
        let keyed_url = format!("{}{KEYED_PATH}", self.base);
        let keyed = |key: &str, timeout: Duration| {
            self.client
                .post(&keyed_url)
                .bearer_auth(token)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(IDEMPOTENCY_KEY, key)
                .body(body.clone())
                .timeout(timeout)
        };
        // Whether an attempt reached the daemon: only a call none of whose attempts did marks it down.
        let mut connected = false;
        let sent = match (key, budget.resend_after) {
            (Some(key), Some(after)) => {
                match self.send_retrying_connect(left(), |left| keyed(key, left.min(after))) {
                    // Connected and unanswered past the attempt budget: abandoned, and sent once more
                    // under the same key. Whatever jkb serve answers that is final.
                    Err(e) if e.is_timeout() && !e.is_connect() && !left().is_zero() => {
                        connected = true;
                        self.send_retrying_connect(left(), |left| keyed(key, left))
                            .map(read)
                    }
                    Ok(resp) => match read(resp) {
                        // A daemon older than keys, which did not run the op: one unkeyed attempt.
                        answer if no_such_endpoint(&answer) => {
                            connected = true;
                            self.send_retrying_connect(left(), unkeyed).map(read)
                        }
                        answer => Ok(answer),
                    },
                    Err(e) => Err(e),
                }
            }
            _ => self.send_retrying_connect(left(), unkeyed).map(read),
        };
        sent.map_err(|e| {
            // Only a failed CONNECT means the daemon is out of reach — refused, or the connect
            // timeout, which reqwest reports as a connect error. A request that connected and
            // then ran past its deadline reached a daemon that is busy (a write lock held by
            // another process), and marking that down made every other client — a hook for a
            // permission prompt among them — give up without trying for the next few seconds.
            // A connect error here is the last attempt's: its retries ran out, or the deadline left
            // no room for another — and it counts only if no earlier attempt connected.
            if e.is_connect() && !connected {
                self.mark_unreachable(true);
            }
            ApiError::with_code(
                ErrorCode::Unavailable,
                format!("cannot reach jkb serve at {}: {e}", self.base),
            )
        })
    }

    /// Send the request `build` makes, building and sending it again after each of
    /// [`CONNECT_RETRY_DELAYS`] while the failure is a failed connect — and only then: any other
    /// error, a timeout after connecting among them, is returned at once. The one place the retry
    /// rule lives.
    ///
    /// The whole of it, retries and their pauses included, is held to ONE deadline, `budget` from
    /// now: `build` is handed the time left, for the attempt's own timeout, and a retry whose pause
    /// and connect timeout would not fit before the deadline is not made. Each attempt given a fresh
    /// budget ran a hook's request to about twice its `TOTAL` — past Claude Code's `SessionEnd`
    /// budget — when a failed connect was followed by a slow answer.
    fn send_retrying_connect(
        &self,
        budget: Duration,
        build: impl Fn(Duration) -> reqwest::blocking::RequestBuilder,
    ) -> reqwest::Result<reqwest::blocking::Response> {
        let deadline = Instant::now() + budget;
        let mut delays = CONNECT_RETRY_DELAYS.iter();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match build(left).send() {
                Err(e) if e.is_connect() => match delays.next() {
                    Some(delay) if Instant::now() + *delay + self.connect <= deadline => {
                        std::thread::sleep(*delay);
                    }
                    // Out of retries, or of room for one: later pauses are only longer.
                    _ => return Err(e),
                },
                other => return other,
            }
        }
    }

    /// `GET /v1/hello`: the daemon's protocol, schema and op list.
    ///
    /// # Errors
    /// [`ErrorCode::Unavailable`] when unreachable, or the daemon's refusal.
    pub fn hello(&self) -> Result<serde_json::Value, ApiError> {
        let token = self.token(false)?;
        let url = format!("{}/v1/hello", self.base);
        let resp = self
            .send_retrying_connect(Duration::from_secs(5), |left| {
                self.client.get(&url).bearer_auth(&token).timeout(left)
            })
            .map_err(|e| ApiError::with_code(ErrorCode::Unavailable, e.to_string()))?;
        decode(read(resp)).map_err(|(e, _)| e)
    }

    /// One request, decoded, with the down marker kept honest: set when jkb serve did not answer
    /// (no connection, or something in the way answered instead), cleared when it did.
    fn attempt(
        &self,
        request: &Request,
        token: &str,
        key: Option<&str>,
    ) -> Result<Response, ApiError> {
        let result = decode(self.send(request, token, key)?);
        self.mark_unreachable(matches!(result, Err((_, Answered::NotJkb))));
        result.map_err(|(e, _)| e)
    }
}

/// Who produced an error.
enum Answered {
    /// jkb serve itself: the body is its `ApiError`.
    Jkb,
    /// Something else — a proxy between here and the host saying the daemon is down, say.
    NotJkb,
}

fn decode<T: serde::de::DeserializeOwned>(
    (status, bytes): Read,
) -> Result<T, (ApiError, Answered)> {
    let bytes = bytes.map_err(|e| {
        (
            ApiError::with_code(ErrorCode::Unavailable, e.to_string()),
            Answered::NotJkb,
        )
    })?;
    if status.is_success() {
        return serde_json::from_slice(&bytes).map_err(|e| {
            (
                ApiError::with_code(ErrorCode::Internal, format!("unreadable response: {e}")),
                Answered::Jkb,
            )
        });
    }
    match serde_json::from_slice::<ApiError>(&bytes) {
        Ok(e) => Err((e, Answered::Jkb)),
        // Not jkb serve's answer, so not a refusal of the request: the daemon is out of reach.
        Err(_) => Err((
            ApiError::with_code(
                ErrorCode::Unavailable,
                format!(
                    "HTTP {status} from something other than jkb serve (a proxy?): {}",
                    String::from_utf8_lossy(&bytes)
                        .chars()
                        .take(200)
                        .collect::<String>()
                ),
            ),
            Answered::NotJkb,
        )),
    }
}

impl Backend for RemoteBackend {
    fn schema_newer_clears(&self) -> bool {
        true
    }

    fn call(&self, request: Request) -> Result<Response, ApiError> {
        if self.recently_unreachable() {
            return Err(ApiError::with_code(
                ErrorCode::Unavailable,
                format!(
                    "jkb serve at {} was unreachable less than {}s ago",
                    self.base,
                    UNREACHABLE_FOR.as_secs()
                ),
            ));
        }
        // The token rotates each time the daemon starts: one retry with a freshly read token, and only
        // when jkb serve itself said `unauthorized` (not any 401), so a wrong token is never retried
        // in a loop.
        // One key for the whole call, every attempt of it: the daemon answers a repeat from the first
        // one's answer (`crate::idempotency`). Only a call that may be resent is keyed.
        let key = self
            .budget(&request)
            .resend_after
            .and_then(|_| idempotency_key());
        let key = key.as_deref();
        match self.attempt(&request, &self.token(false)?, key) {
            // A fixed token does not rotate, so there is nothing fresher to retry with.
            Err(e) if e.code == ErrorCode::Unauthorized && self.fixed.is_none() => {
                self.attempt(&request, &self.token(true)?, key)
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use jkb_api::{Backend as _, Request, Response};

    use super::RemoteBackend;

    /// What a stub does with a request it accepted, by its index.
    #[derive(Clone, Copy)]
    enum Then {
        /// Read the request and never answer.
        Swallow,
        /// Answer after this long.
        AnswerAfter(Duration),
        /// Refuse it `busy`, as jkb serve does.
        Busy,
        /// A proxy's 502, not jkb serve's.
        Gateway,
    }

    fn http(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// One request a stub saw: its path, and its `Idempotency-Key`.
    type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

    /// A stub HTTP server. Each request gets what `plan` says by its index (the last for any past the
    /// list) — except that an `old` stub, a `jkb serve` from before keys, answers the keyed route
    /// `no such endpoint` as that daemon does.
    fn stub(plan: Vec<Then>, old: bool) -> (String, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Seen::default();
        let log = Arc::clone(&seen);
        let answer = serde_json::to_string(&Response::Position { position: 1 }).unwrap();
        let refusal = |code, why: &str| {
            serde_json::to_string(&jkb_api::ApiError::with_code(code, why)).unwrap()
        };
        let busy = refusal(jkb_api::ErrorCode::Busy, "at its concurrency limit");
        let no_such = refusal(
            jkb_api::ErrorCode::BadRequest,
            "no such endpoint: POST /v1/op/keyed",
        );
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                let path = text
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                let key = text
                    .lines()
                    .find_map(|l| l.strip_prefix("idempotency-key:"))
                    .map(|k| k.trim().to_owned());
                let i = {
                    let mut log = log.lock().unwrap();
                    log.push((path.clone(), key));
                    log.len() - 1
                };
                if old && path == "/v1/op/keyed" {
                    let _ = stream.write_all(http("400 Bad Request", &no_such).as_bytes());
                    continue;
                }
                match plan[i.min(plan.len() - 1)] {
                    Then::Swallow => held.push(stream),
                    Then::Gateway => {
                        let _ = stream.write_all(http("502 Bad Gateway", "Bad Gateway").as_bytes());
                    }
                    Then::Busy => {
                        let _ = stream.write_all(http("503 Service Unavailable", &busy).as_bytes());
                    }
                    Then::AnswerAfter(after) => {
                        let reply = http("200 OK", &answer);
                        std::thread::spawn(move || {
                            std::thread::sleep(after);
                            let _ = stream.write_all(reply.as_bytes());
                        });
                    }
                }
            }
        });
        (base, seen)
    }

    fn client(base: &str, dir: &tempfile::TempDir, attempt: Duration) -> RemoteBackend {
        let token = dir.path().join("token");
        crate::token::write(&token, "t").unwrap();
        let mut c = RemoteBackend::new(base, token).unwrap();
        c.attempt_timeout = attempt;
        c
    }

    fn ingest() -> Request {
        Request::IngestText(jkb_api::ingest::IngestAsk {
            text: "t".into(),
            mime: "text/plain".into(),
            namespace: "inbox".into(),
            raw: None,
        })
    }

    fn poll() -> Request {
        Request::MqPoll {
            topic: "t".into(),
            group: "g".into(),
            max: 1,
            after: None,
        }
    }

    /// A stalled first attempt gets exactly one resend, on the keyed route under the same key, sent
    /// once the attempt budget runs out.
    #[test]
    fn a_stall_gets_one_resend_with_the_same_key_on_the_keyed_route() {
        let dir = tempfile::tempdir().unwrap();
        let attempt = Duration::from_millis(300);
        let (base, seen) = stub(
            vec![Then::Swallow, Then::AnswerAfter(Duration::ZERO)],
            false,
        );
        let c = client(&base, &dir, attempt);
        let started = Instant::now();
        assert_eq!(
            c.call(Request::MqInspect {}).unwrap(),
            Response::Position { position: 1 }
        );
        let took = started.elapsed();
        assert!(
            took >= attempt && took < attempt + Duration::from_secs(1),
            "answered at about the attempt budget: {took:?}"
        );
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "{seen:?}");
        let key = seen[0].1.clone().expect("keyed");
        assert_eq!(key.len(), 32, "128 bits, hex: {key}");
        for (path, k) in &seen {
            assert_eq!(
                (path.as_str(), k.as_deref()),
                ("/v1/op/keyed", Some(key.as_str()))
            );
        }
    }

    /// A daemon from before keys refuses the keyed route `no such endpoint` without running the op;
    /// the call then makes one unkeyed attempt on `/v1/op`, never resent however slow.
    #[test]
    fn a_daemon_without_the_keyed_route_gets_one_unkeyed_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let attempt = Duration::from_millis(200);
        let (base, seen) = stub(vec![Then::AnswerAfter(attempt * 3)], true);
        let c = client(&base, &dir, attempt);
        assert_eq!(
            c.call(Request::MqInspect {}).unwrap(),
            Response::Position { position: 1 }
        );
        std::thread::sleep(attempt * 2);
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen.len(),
            2,
            "the refused keyed attempt and one fallback: {seen:?}"
        );
        assert_eq!(seen[0].0, "/v1/op/keyed");
        assert_eq!((seen[1].0.as_str(), seen[1].1.as_deref()), ("/v1/op", None));
    }

    /// Whatever jkb serve — or a proxy — answers the resend is final: `busy`, a 502. The call ends
    /// at once, after exactly two requests.
    #[test]
    fn any_answer_to_the_resend_is_final() {
        let dir = tempfile::tempdir().unwrap();
        let attempt = Duration::from_millis(200);
        for (name, then, code) in [
            ("busy", Then::Busy, jkb_api::ErrorCode::Busy),
            ("502", Then::Gateway, jkb_api::ErrorCode::Unavailable),
        ] {
            let (base, seen) = stub(vec![Then::Swallow, then], false);
            let c = client(&base, &dir, attempt);
            let started = Instant::now();
            let err = c.call(Request::MqInspect {}).unwrap_err();
            assert_eq!(err.code, code, "{name}: {err:?}");
            assert!(
                started.elapsed() < attempt + Duration::from_secs(1),
                "{name}: at once, {:?}",
                started.elapsed()
            );
            std::thread::sleep(attempt * 2);
            assert_eq!(seen.lock().unwrap().len(), 2, "{name}");
        }
    }

    /// A long-poll, an ingest and a hook's call are unkeyed and make one attempt: the long ops are
    /// answered after the attempt budget, and the hook's deadline is shorter than it.
    #[test]
    fn long_ops_and_hook_calls_are_unkeyed_with_one_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let attempt = Duration::from_millis(200);
        for (name, request) in [("ingest", ingest()), ("poll", poll())] {
            let (base, seen) = stub(vec![Then::AnswerAfter(attempt * 3)], false);
            let c = client(&base, &dir, attempt).with_poll_wait(Duration::from_secs(1));
            assert_eq!(
                c.call(request).unwrap(),
                Response::Position { position: 1 },
                "{name}"
            );
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 1, "{name}: one attempt");
            assert!(
                seen[0].0.starts_with("/v1/op") && seen[0].0 != "/v1/op/keyed",
                "{name}"
            );
            assert_eq!(seen[0].1, None, "{name}: unkeyed");
        }

        // The hook's deadlines (`jkb-cli`'s `notify::CONNECT` 200 ms and `TOTAL` 1 s) with the default
        // attempt budget: unkeyed, one attempt, ended by the hook's TOTAL.
        let (base, seen) = stub(vec![Then::Swallow], false);
        let total = Duration::from_secs(1);
        let c = client(&base, &dir, super::ATTEMPT_TIMEOUT)
            .with_deadlines(Duration::from_millis(200), total)
            .unwrap();
        let started = Instant::now();
        assert!(c.call(Request::NotifyOpenSessions {}).is_err());
        let took = started.elapsed();
        assert!(
            took < total + Duration::from_millis(250),
            "took {took:?}, past the hook's {total:?}"
        );
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![("/v1/op".to_owned(), None)],
            "the hook: one unkeyed attempt"
        );
    }

    /// A call one of whose attempts connected does not mark the daemon down, even when its resend
    /// failed to connect: it reached a daemon, which is busy, not gone.
    #[test]
    fn a_call_that_connected_once_does_not_mark_the_daemon_down() {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            // The first attempt: read and held unanswered; then the port closes, so the resend and
            // its retries fail to connect.
            let (mut op, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let _ = op.read(&mut buf);
            drop(listener);
            let _ = done_rx.recv();
        });
        let marker = dir.path().join("unreachable");
        let c = client(&base, &dir, Duration::from_millis(200)).with_down_marker(marker.clone());
        assert!(c.call(Request::MqInspect {}).is_err());
        done_tx.send(()).unwrap();
        server.join().unwrap();
        assert!(!marker.exists(), "an attempt connected: busy, not down");
    }

    /// End to end through a proxy that forwards the first attempt to a real `jkb serve` and loses its
    /// answer — the stall measured in the sandbox proxy, after the daemon applied the write. The
    /// resend gets the first attempt's answer, and the task exists once.
    #[test]
    fn a_write_whose_answer_was_lost_is_resent_and_applied_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = jkb_core::Db::open(dir.path().join("jkb.db")).unwrap();
        let token = dir.path().join("daemon/token");
        let cfg = crate::server::ServeConfig::new("127.0.0.1:0".parse().unwrap(), token.clone());
        let daemon = crate::server::spawn(db.clone(), &cfg).unwrap();
        let upstream = daemon.addr;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let connections = Arc::new(Mutex::new(0usize));
        let counted = Arc::clone(&connections);
        std::thread::spawn(move || {
            for (i, client) in listener.incoming().enumerate() {
                let Ok(client) = client else { return };
                *counted.lock().unwrap() += 1;
                let up = TcpStream::connect(upstream).unwrap();
                let (mut client_r, mut up_w) =
                    (client.try_clone().unwrap(), up.try_clone().unwrap());
                std::thread::spawn(move || std::io::copy(&mut client_r, &mut up_w));
                let (mut up_r, mut client_w) = (up, client);
                std::thread::spawn(move || {
                    if i == 0 {
                        // The answer is lost; the client's connection is kept open, unanswered.
                        let _ = std::io::copy(&mut up_r, &mut std::io::sink());
                        drop(client_w);
                    } else {
                        let _ = std::io::copy(&mut up_r, &mut client_w);
                    }
                });
            }
        });

        let mut c = RemoteBackend::new(&proxy, token).unwrap();
        c.attempt_timeout = Duration::from_millis(300);
        let request: Request =
            serde_json::from_value(serde_json::json!({ "op": "task.add", "text": "once" }))
                .unwrap();
        let answer = c.call(request).unwrap();
        assert!(matches!(answer, Response::Added { .. }), "{answer:?}");
        assert!(
            *connections.lock().unwrap() >= 2,
            "the first attempt was resent"
        );
        let tasks: i64 = db
            .read(|c| {
                Ok(
                    c.query_row("SELECT count(*) FROM items WHERE kind = 'task'", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(tasks, 1, "applied once");
        daemon.shutdown().unwrap();
    }
}
