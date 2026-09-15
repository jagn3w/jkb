//! [`RemoteBackend`]: the [`jkb_api::Backend`] a process that must not open `jkb.db` uses to reach
//! `jkb serve`.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use jkb_api::{ApiError, Backend, ErrorCode, Request, Response};

use crate::token;

/// How long an unreachable daemon is remembered, so a burst of short-lived `jkb` processes (a hook
/// per tool call) pays one connect timeout rather than one each.
pub const UNREACHABLE_FOR: Duration = Duration::from_secs(5);

/// Serves requests by calling a `jkb serve` daemon over HTTP.
pub struct RemoteBackend {
    base: String,
    token_file: PathBuf,
    token: Mutex<Option<String>>,
    client: reqwest::blocking::Client,
    poll_wait: Duration,
    /// How long a request may take beyond any long-poll wait.
    op_timeout: Duration,
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
            client: http_client(Duration::from_secs(1))?,
            poll_wait: Duration::from_secs(2),
            op_timeout: Duration::from_secs(30),
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

    fn token(&self, fresh: bool) -> Result<String, ApiError> {
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

    fn send(
        &self,
        request: &Request,
        token: &str,
    ) -> Result<reqwest::blocking::Response, ApiError> {
        let wait = match request {
            Request::MqPoll { .. } => self.poll_wait,
            _ => Duration::ZERO,
        };
        let url = if wait.is_zero() {
            format!("{}/v1/op", self.base)
        } else {
            format!("{}/v1/op?wait_ms={}", self.base, wait.as_millis())
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
        self.client
            .post(url)
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .timeout(wait + self.op_timeout)
            .send()
            .map_err(|e| {
                // Only a failed CONNECT means the daemon is out of reach — refused, or the connect
                // timeout, which reqwest reports as a connect error. A request that connected and
                // then ran past its deadline reached a daemon that is busy (a write lock held by
                // another process), and marking that down made every other client — a hook for a
                // permission prompt among them — give up without trying for the next few seconds.
                if e.is_connect() {
                    self.mark_unreachable(true);
                }
                ApiError::with_code(
                    ErrorCode::Unavailable,
                    format!("cannot reach jkb serve at {}: {e}", self.base),
                )
            })
    }

    /// `GET /v1/hello`: the daemon's protocol, schema and op list.
    ///
    /// # Errors
    /// [`ErrorCode::Unavailable`] when unreachable, or the daemon's refusal.
    pub fn hello(&self) -> Result<serde_json::Value, ApiError> {
        let token = self.token(false)?;
        let resp = self
            .client
            .get(format!("{}/v1/hello", self.base))
            .bearer_auth(token)
            .timeout(Duration::from_secs(5))
            .send()
            .map_err(|e| ApiError::with_code(ErrorCode::Unavailable, e.to_string()))?;
        decode(resp).map_err(|(e, _)| e)
    }

    /// One request, decoded, with the down marker kept honest: set when jkb serve did not answer
    /// (no connection, or something in the way answered instead), cleared when it did.
    fn attempt(&self, request: &Request, token: &str) -> Result<Response, ApiError> {
        let result = decode(self.send(request, token)?);
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
    resp: reqwest::blocking::Response,
) -> Result<T, (ApiError, Answered)> {
    let status = resp.status();
    let bytes = resp.bytes().map_err(|e| {
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
        match self.attempt(&request, &self.token(false)?) {
            Err(e) if e.code == ErrorCode::Unauthorized => {
                self.attempt(&request, &self.token(true)?)
            }
            other => other,
        }
    }
}
