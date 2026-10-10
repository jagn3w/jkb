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
/// applied twice. A request that connected is resent only when it carries an idempotency key, the
/// daemon says it honours keys, and the request went unanswered past [`ATTEMPT_TIMEOUT`]. Retries and
/// their pauses fit inside the request's own deadline, never beyond it (`send_attempts`).
pub const CONNECT_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_millis(100), Duration::from_millis(300)];

/// How long one attempt of an ordinary op may go unanswered before it is resent, with the same
/// `Idempotency-Key`, under the call's one deadline. The first attempt is not abandoned: it stays
/// open beside the resend, and whichever is answered first is the answer.
///
/// **Only to a daemon that honours keys.** An older `jkb serve` ignores the header, and the container's
/// `jkb` and the host's routinely differ between `setup.sh` runs, so a resend to one would apply a
/// slow write twice. Before its first resend a client asks `GET /v1/hello`, whose `idempotency: true`
/// says the daemon honours keys, and remembers the answer; to a daemon that does not say so the call
/// keeps its one attempt, as before keys existed.
///
/// Measured: Claude Code's sandbox proxy stalls about 1 in 200 requests to `jkb serve` for 10–30 s —
/// the request connects and no answer comes back — while a warm answer takes milliseconds. Without a
/// key a resend could apply a write twice, so the client waited out its whole 30 s op timeout. With
/// one, the daemon runs the op once and answers every attempt with that one answer
/// (`crate::idempotency`), so the resend is safe. A long op is never abandoned: see
/// [`RemoteBackend::budget`].
///
/// **Residual:** the daemon keeps keys in memory only, so an attempt applied just before a daemon
/// restart and resent after it is applied twice.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);

/// The header naming a logical call (the daemon's `server::IDEMPOTENCY_KEY`).
const IDEMPOTENCY_KEY: &str = crate::server::IDEMPOTENCY_KEY;

/// How long the `GET /v1/hello` asking whether the daemon honours keys may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// The pause before resending a keyed call whose resend was refused `busy` while an earlier attempt
/// may still be running.
const BUSY_PAUSE: Duration = Duration::from_millis(200);

/// What one call may spend, from [`RemoteBackend::budget`] — the one place it is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Budget {
    /// How long the daemon may hold an `mq.poll` with nothing to hand over.
    wait: Duration,
    /// The whole call, every attempt and pause included.
    total: Duration,
    /// How long an attempt may go unanswered before the call is resent; `None` for a call never resent
    /// after it connected.
    resend_after: Option<Duration>,
    /// Whether the call carries an `Idempotency-Key`. A long-poll does not: it is never resent, and the
    /// daemon serves it on its own handler, so that a client that hangs up frees its group's slot.
    keyed: bool,
}

/// Whether the daemon honours `Idempotency-Key`, as `GET /v1/hello` said: not asked yet, yes, no.
const HONOURS_UNKNOWN: u8 = 0;
const HONOURS_YES: u8 = 1;
const HONOURS_NO: u8 = 2;

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

/// A send that ran out: the last error, whether it was a failed connect, and whether any attempt
/// connected — only a call none of whose attempts connected marks the daemon down.
struct Failed {
    error: String,
    connect: bool,
    connected: bool,
}

impl From<(reqwest::Error, bool)> for Failed {
    fn from((error, connected): (reqwest::Error, bool)) -> Self {
        Self {
            connect: error.is_connect(),
            error: error.to_string(),
            connected,
        }
    }
}

/// An attempt's outcome: the status and the whole body, or the transport's error.
type Sent = Result<(reqwest::StatusCode, bytes::Bytes), reqwest::Error>;

/// Whether an answer is jkb serve refusing `busy`.
fn is_busy((status, bytes): &(reqwest::StatusCode, bytes::Bytes)) -> bool {
    *status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        && serde_json::from_slice::<ApiError>(bytes).is_ok_and(|e| e.code == ErrorCode::Busy)
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
    /// Whether the daemon honours keys (`HONOURS_*`), asked once.
    honours: std::sync::atomic::AtomicU8,
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
            honours: std::sync::atomic::AtomicU8::new(HONOURS_UNKNOWN),
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

    /// What `request` may spend: its long-poll wait, its whole deadline, whether it carries a key, and
    /// how long an attempt may go unanswered before it is resent. The one place an op's budget is
    /// decided, so no call site can forget it.
    ///
    /// A **long** op is never resent: its one attempt has the whole deadline. Long is an op given more
    /// than the plain [`RemoteBackend::with_deadlines`] total — `mq.poll`, which the daemon may hold for
    /// its wait, and which carries no key at all — and `ingest.text`, whose capture of a body at the cap
    /// holds the writer about half a second and whose resend could only queue behind it. Everything
    /// else is resent after [`ATTEMPT_TIMEOUT`] — when that is inside the deadline, so a hook, whose
    /// `TOTAL` is 1 s, makes one attempt.
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
            resend_after: (!long).then_some(self.attempt_timeout),
            keyed: wait.is_zero(),
        }
    }

    fn send(
        &self,
        request: &Request,
        token: &str,
        key: Option<&str>,
    ) -> Result<(reqwest::StatusCode, bytes::Bytes), ApiError> {
        let budget = self.budget(request);
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
        // Without a key nothing makes a resend safe.
        let resend_after = key.and(budget.resend_after);
        self.send_attempts(budget.total, resend_after, token, |left| {
            let builder = self
                .client
                .post(&url)
                .bearer_auth(token)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone())
                .timeout(left);
            match key {
                Some(key) => builder.header(IDEMPOTENCY_KEY, key),
                None => builder,
            }
        })
        .map_err(|failed| {
            // Only a call none of whose attempts connected means the daemon is out of reach —
            // refused, or the connect timeout, which reqwest reports as a connect error. A request
            // that connected and then ran past its deadline reached a daemon that is busy (a write
            // lock held by another process), and marking that down made every other client — a hook
            // for a permission prompt among them — give up without trying for the next few seconds.
            if failed.connect && !failed.connected {
                self.mark_unreachable(true);
            }
            ApiError::with_code(
                ErrorCode::Unavailable,
                format!("cannot reach jkb serve at {}: {}", self.base, failed.error),
            )
        })
    }

    /// Whether the daemon honours `Idempotency-Key`: asked of `GET /v1/hello` once, within `left`, and
    /// remembered. A probe that fails is not remembered, and counts as no.
    fn honours_keys(&self, token: &str, left: Duration) -> bool {
        use std::sync::atomic::Ordering;
        match self.honours.load(Ordering::SeqCst) {
            HONOURS_YES => return true,
            HONOURS_NO => return false,
            _ => {}
        }
        let said = self
            .client
            .get(format!("{}/v1/hello", self.base))
            .bearer_auth(token)
            .timeout(left.min(PROBE_TIMEOUT))
            .send()
            .ok()
            .filter(|resp| resp.status().is_success())
            .and_then(|resp| resp.bytes().ok())
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
        let Some(hello) = said else {
            return false;
        };
        let yes = hello["idempotency"] == serde_json::Value::Bool(true);
        self.honours
            .store(if yes { HONOURS_YES } else { HONOURS_NO }, Ordering::SeqCst);
        yes
    }

    /// Send the request `build` makes, answer read whole. The one place the retry rule lives:
    ///
    /// - A failed connect, with nothing else in flight, is sent again after each of
    ///   [`CONNECT_RETRY_DELAYS`].
    /// - With `resend_after` (a keyed call), an attempt unanswered that long is resent with the same
    ///   key — if the daemon honours keys ([`RemoteBackend::honours_keys`]) — and the earlier attempt
    ///   stays open: whichever is answered first is the answer. So is an attempt that failed after
    ///   connecting. And once an earlier attempt may be running, a `busy` refusal of a resend means
    ///   "not yet", not "not run": it is resent after [`BUSY_PAUSE`], under the same key, rather than
    ///   handed to a caller who would retry it under a fresh one and apply it twice.
    /// - Anything else ends the call.
    ///
    /// The whole of it, retries and their pauses included, is held to ONE deadline, `budget` from
    /// now: `build` is handed the time left, for the attempt's own timeout, and a connect retry whose
    /// pause and connect timeout would not fit before the deadline is not made. Each attempt given a
    /// fresh budget ran a hook's request to about twice its `TOTAL` — past Claude Code's `SessionEnd`
    /// budget — when a failed connect was followed by a slow answer.
    fn send_attempts(
        &self,
        budget: Duration,
        resend_after: Option<Duration>,
        token: &str,
        build: impl Fn(Duration) -> reqwest::blocking::RequestBuilder,
    ) -> Result<(reqwest::StatusCode, bytes::Bytes), Failed> {
        let deadline = Instant::now() + budget;
        let (tx, rx) = std::sync::mpsc::channel::<Sent>();
        // Each attempt on a thread of its own, so an unanswered one can stay open beside its resend.
        let launch = |left: Duration| {
            let request = build(left);
            let tx = tx.clone();
            std::thread::spawn(move || {
                let sent = request.send().and_then(|resp| {
                    let status = resp.status();
                    resp.bytes().map(|bytes| (status, bytes))
                });
                let _ = tx.send(sent);
            });
        };
        launch(budget);
        let mut in_flight = 1usize;
        let mut resend_at = resend_after.map(|after| Instant::now() + after);
        let mut delays = CONNECT_RETRY_DELAYS.iter();
        let mut connected = false;
        // Whether an attempt other than the latest may have reached the daemon and be running.
        let mut may_run = false;
        let mut last: Option<Result<(reqwest::StatusCode, bytes::Bytes), Failed>> = None;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if in_flight == 0 && (resend_at.is_none() || left.is_zero()) {
                return last.unwrap_or_else(|| {
                    Err(Failed {
                        error: "no attempt was made".to_owned(),
                        connect: false,
                        connected,
                    })
                });
            }
            let due = resend_at.map_or(deadline, |at| at.min(deadline));
            let received = if left.is_zero() {
                // Past the deadline: the attempts' own timeouts end them now.
                rx.recv()
                    .map_err(|_| std::sync::mpsc::RecvTimeoutError::Disconnected)
            } else {
                rx.recv_timeout(due.saturating_duration_since(Instant::now()))
            };
            let Ok(sent) = received else {
                // A resend is due.
                if resend_at.is_some_and(|at| at <= Instant::now()) {
                    resend_at = None;
                    let left = deadline.saturating_duration_since(Instant::now());
                    if !left.is_zero() && self.honours_keys(token, left) {
                        may_run |= in_flight > 0 || connected;
                        launch(deadline.saturating_duration_since(Instant::now()));
                        in_flight += 1;
                        resend_at = resend_after.map(|after| Instant::now() + after);
                    }
                }
                continue;
            };
            in_flight -= 1;
            match sent {
                Ok(answer) => {
                    connected = true;
                    if may_run && resend_after.is_some() && is_busy(&answer) {
                        // Not yet: an earlier attempt may be running. Waited out, or resent.
                        last = Some(Ok(answer));
                        if in_flight == 0 {
                            resend_at = Some(Instant::now() + BUSY_PAUSE);
                        }
                        continue;
                    }
                    return Ok(answer);
                }
                Err(error) if error.is_connect() => {
                    last = Some(Err((error, connected).into()));
                    if in_flight == 0 {
                        match delays.next() {
                            Some(delay) if Instant::now() + *delay + self.connect <= deadline => {
                                std::thread::sleep(*delay);
                                launch(deadline.saturating_duration_since(Instant::now()));
                                in_flight += 1;
                            }
                            // Out of retries, or of room for one: later pauses are only longer.
                            _ => resend_at = None,
                        }
                    }
                }
                Err(error) => {
                    connected = true;
                    may_run = true;
                    last = Some(Err((error, true).into()));
                    // Failed after connecting: resent at once, when resends are allowed at all.
                    if in_flight == 0 && resend_after.is_some() {
                        resend_at = Some(Instant::now());
                    }
                }
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
        let answer = self
            .send_attempts(Duration::from_secs(5), None, &token, |left| {
                self.client.get(&url).bearer_auth(&token).timeout(left)
            })
            .map_err(|f| ApiError::with_code(ErrorCode::Unavailable, f.error))?;
        decode(answer).map_err(|(e, _)| e)
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
    (status, bytes): (reqwest::StatusCode, bytes::Bytes),
) -> Result<T, (ApiError, Answered)> {
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
        // One key for the whole call, every attempt of it: the daemon answers a repeat from the first
        // one's answer (`crate::idempotency`).
        let key = self.budget(&request).keyed.then(idempotency_key).flatten();
        let key = key.as_deref();
        // The token rotates each time the daemon starts: one retry with a freshly read token, and only
        // when jkb serve itself said `unauthorized` (not any 401), so a wrong token is never retried
        // in a loop.
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

    /// What a stub does with an op request it accepted, by its index.
    #[derive(Clone, Copy)]
    enum Then {
        /// Read the request and never answer.
        Swallow,
        /// Answer after this long.
        AnswerAfter(Duration),
        /// Refuse it `busy`, as jkb serve does.
        Busy,
    }

    fn http(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// A stub HTTP server. `GET /v1/hello` is answered at once, saying the daemon honours keys or not;
    /// each op request gets what `plan` says by its index (the last for any past the list). Returns the
    /// `Idempotency-Key` each op request carried.
    fn stub(plan: Vec<Then>, honours: bool) -> (String, Arc<Mutex<Vec<Option<String>>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let keys = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&keys);
        let answer = serde_json::to_string(&Response::Position { position: 1 }).unwrap();
        let busy = serde_json::to_string(&jkb_api::ApiError::with_code(
            jkb_api::ErrorCode::Busy,
            "the daemon is at its concurrency limit; retry",
        ))
        .unwrap();
        let hello = if honours {
            r#"{"protocol":1,"idempotency":true}"#
        } else {
            r#"{"protocol":1}"#
        };
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request_text = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                if request_text.starts_with("get /v1/hello") {
                    let _ = stream.write_all(http("200 OK", hello).as_bytes());
                    continue;
                }
                let i = {
                    let mut seen = seen.lock().unwrap();
                    seen.push(
                        request_text
                            .lines()
                            .find_map(|l| l.strip_prefix("idempotency-key:"))
                            .map(|k| k.trim().to_owned()),
                    );
                    seen.len() - 1
                };
                match plan[i.min(plan.len() - 1)] {
                    Then::Swallow => held.push(stream),
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
        (base, keys)
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

    /// An ordinary op whose first attempt is swallowed is resent once its attempt budget runs out,
    /// with the same key, and answered — well inside its deadline. A long op is not resent: its one
    /// attempt is answered after the ordinary attempt budget has passed. A long-poll carries no key.
    #[test]
    fn an_ordinary_op_is_resent_with_its_key_and_a_long_op_is_not_abandoned() {
        let dir = tempfile::tempdir().unwrap();
        let attempt = Duration::from_millis(200);
        let (base, keys) = stub(vec![Then::Swallow, Then::AnswerAfter(Duration::ZERO)], true);
        let started = Instant::now();
        let c = client(&base, &dir, attempt);
        assert_eq!(
            c.call(Request::MqInspect {}).unwrap(),
            Response::Position { position: 1 }
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        let keys = keys.lock().unwrap().clone();
        assert_eq!(keys.len(), 2, "resent once: {keys:?}");
        let key = keys[0].clone().expect("the first attempt carried a key");
        assert_eq!(key.len(), 32, "128 bits, hex: {key}");
        assert_eq!(
            keys[1].as_deref(),
            Some(key.as_str()),
            "the same key both times"
        );

        for (name, request) in [("ingest", ingest()), ("poll", poll())] {
            let (base, keys) = stub(vec![Then::AnswerAfter(attempt * 3)], true);
            let c = client(&base, &dir, attempt).with_poll_wait(Duration::from_secs(1));
            assert_eq!(
                c.call(request).unwrap(),
                Response::Position { position: 1 },
                "{name}"
            );
            let keys = keys.lock().unwrap().clone();
            assert_eq!(keys.len(), 1, "{name}: one attempt, not resent");
            assert_eq!(keys[0].is_none(), name == "poll", "{name}: {keys:?}");
        }
    }

    /// A daemon that does not say it honours keys — an older `jkb serve`, which ignores the header —
    /// is never sent a second attempt of a request that connected: a slow write would land twice. The
    /// call waits for its one attempt, as before keys existed.
    #[test]
    fn a_daemon_that_ignores_keys_is_never_sent_a_second_connected_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let attempt = Duration::from_millis(200);
        let (base, keys) = stub(vec![Then::AnswerAfter(attempt * 4)], false);
        let c = client(&base, &dir, attempt);
        assert_eq!(
            c.call(Request::MqInspect {}).unwrap(),
            Response::Position { position: 1 }
        );
        std::thread::sleep(attempt * 2);
        assert_eq!(keys.lock().unwrap().len(), 1, "sent once");
    }

    /// A resend refused `busy` while an earlier attempt may still be running is "not yet": resent under
    /// the same key, not handed to the caller, who would retry under a fresh one and apply it twice.
    #[test]
    fn a_resend_refused_busy_is_resent_under_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let (base, keys) = stub(
            vec![Then::Swallow, Then::Busy, Then::AnswerAfter(Duration::ZERO)],
            true,
        );
        let c = client(&base, &dir, Duration::from_millis(200));
        assert_eq!(
            c.call(Request::MqInspect {}).unwrap(),
            Response::Position { position: 1 }
        );
        let keys = keys.lock().unwrap().clone();
        assert_eq!(keys.len(), 3, "{keys:?}");
        assert!(
            keys.iter().all(|k| k.is_some() && *k == keys[0]),
            "{keys:?}"
        );
    }

    /// A call one of whose attempts connected does not mark the daemon down, even when every later
    /// attempt failed to connect: it reached a daemon, which is busy, not gone.
    #[test]
    fn a_call_that_connected_once_does_not_mark_the_daemon_down() {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            // The op: read, and held unanswered.
            let (mut op, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let _ = op.read(&mut buf);
            // The probe: this daemon honours keys.
            let (mut probe, _) = listener.accept().unwrap();
            let _ = probe.read(&mut buf);
            let _ = probe.write_all(http("200 OK", r#"{"idempotency":true}"#).as_bytes());
            // Closed, so every later connect is refused; then the op's connection drops, so it is
            // resent — and that resend, and its retries, fail to connect.
            drop(listener);
            drop(op);
        });
        let marker = dir.path().join("unreachable");
        let c = client(&base, &dir, Duration::from_millis(200))
            .with_deadlines(Duration::from_millis(200), Duration::from_secs(2))
            .unwrap()
            .with_down_marker(marker.clone());
        assert!(c.call(Request::MqInspect {}).is_err());
        server.join().unwrap();
        assert!(!marker.exists(), "an attempt connected: busy, not down");
    }

    /// The hook's deadlines (`jkb-cli`'s `notify::CONNECT` 200 ms and `TOTAL` 1 s) still bound a call
    /// whose attempt is swallowed: the default attempt budget is longer than the hook's whole
    /// deadline, so the one attempt ends at the deadline, not after it.
    #[test]
    fn a_hook_call_ends_by_its_total_with_the_default_attempt_budget() {
        let dir = tempfile::tempdir().unwrap();
        let (base, keys) = stub(vec![Then::Swallow], true);
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
        assert_eq!(keys.lock().unwrap().len(), 1);
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
