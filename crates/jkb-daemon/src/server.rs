//! `jkb serve`: [`jkb_api`] operations over HTTP/1.1.
//!
//! ```text
//! GET  /v1/hello                → {"protocol","schema_version","supported_schema","ops"}
//! POST /v1/op[?wait_ms=N]       → a jkb_api::Response, or a jkb_api::ApiError with a 4xx/5xx status
//! ```
//!
//! Both require `Authorization: Bearer <token>`. `wait_ms` applies to `mq.poll` only (any other op
//! ignores it): an empty answer is held until a message arrives or the wait (capped) runs out — woken
//! at once by a send this daemon served, and at worst every 250 ms for one another process wrote
//! (the host CLI opens the database directly). At most one long-poll per group is held at a time; a
//! second is refused `busy`.
//!
//! **Before authentication nothing is bounded by the token**, so the transport is: at most
//! [`ServeConfig::max_connections`] open connections (one past that is closed on accept), request
//! headers within [`ServeConfig::read_timeout`] — idle keep-alive connections included, since hyper
//! runs that timer for every head it waits on — and a body within the same timeout.

use std::collections::HashSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use jkb_api::{ApiError, Backend as _, ErrorCode, LocalBackend, Request, Response};
use jkb_core::Db;
use serde_json::json;
use tokio::sync::{Notify, Semaphore};

use crate::token;

/// How a daemon is configured.
#[derive(Debug, Clone)]
pub struct ServeConfig {
    /// Where to listen. An unspecified address (`0.0.0.0`, `::`) is refused.
    pub addr: SocketAddr,
    /// Where the bearer token is written.
    pub token_path: PathBuf,
    /// Largest accepted request body.
    pub max_body_bytes: usize,
    /// Concurrent operations, excluding long-polls.
    pub max_ops: usize,
    /// Concurrent long-polls — a separate budget, so subscribers cannot starve a hook's request.
    pub max_polls: usize,
    /// Longest a long-poll is held.
    pub max_wait: Duration,
    /// How often a long-poll re-checks for a message written by another process (the host CLI
    /// opens the database directly, so this daemon hears about only its own sends).
    pub poll_floor: Duration,
    /// Open connections, authenticated or not. One past this is closed as soon as it is accepted.
    pub max_connections: usize,
    /// How long a client may take to send a request's headers, or its body — and how long an idle
    /// keep-alive connection is kept.
    pub read_timeout: Duration,
}

impl ServeConfig {
    /// The defaults for `addr` and `token_path`.
    #[must_use]
    pub const fn new(addr: SocketAddr, token_path: PathBuf) -> Self {
        Self {
            addr,
            token_path,
            max_body_bytes: 1024 * 1024,
            max_ops: 64,
            max_polls: 32,
            max_wait: Duration::from_secs(30),
            poll_floor: Duration::from_millis(250),
            max_connections: 256,
            read_timeout: Duration::from_secs(10),
        }
    }
}

/// Why the daemon could not start.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// `0.0.0.0` / `::` would expose the knowledge base beyond this machine.
    #[error(
        "refusing to listen on {0}: an unspecified address exposes the knowledge base on every \
         interface; name the one address to bind"
    )]
    Unspecified(SocketAddr),
    /// Binding or runtime setup failed.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// The token could not be written.
    #[error(transparent)]
    Token(#[from] token::TokenError),
}

/// A running daemon. [`Handle::shutdown`] stops it and waits; dropping the handle stops it too,
/// without waiting for the server thread.
pub struct Handle {
    /// The address actually bound (useful with port 0).
    pub addr: SocketAddr,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Handle {
    /// Stop accepting, and wait for the server thread to end.
    pub fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// Block until the server thread ends (it ends only on [`Handle::shutdown`] from elsewhere, or a
    /// fatal runtime error).
    pub fn join(mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct State {
    /// The database, or — when it could not be served at all — the refusal every request gets.
    serving: Result<(LocalBackend, Db), ApiError>,
    token: String,
    ops: Arc<Semaphore>,
    polls: Arc<Semaphore>,
    /// The `(topic, group)` pairs with a long-poll held right now.
    polling: Mutex<HashSet<(String, String)>>,
    sent: Notify,
    max_body: usize,
    max_wait: Duration,
    poll_floor: Duration,
    read_timeout: Duration,
}

/// Bind, write a fresh token, and serve on a background thread.
///
/// The token is written only after the bind succeeds, so a client that finds a new token finds a
/// daemon behind it.
///
/// # Errors
/// [`ServeError::Unspecified`] for `0.0.0.0`/`::`, an I/O error binding, or a token write failure.
pub fn spawn(db: Db, cfg: &ServeConfig) -> Result<Handle, ServeError> {
    start(Ok((LocalBackend::new(db.clone()), db)), cfg)
}

/// Bind and answer every authenticated request with `refusal` — for a database this build cannot
/// serve (one migrated by a newer jkb). Exiting instead would put a supervisor into a restart loop
/// and leave clients with `unavailable`, which says nothing about the fix.
///
/// # Errors
/// As [`spawn`].
pub fn spawn_refusing(refusal: ApiError, cfg: &ServeConfig) -> Result<Handle, ServeError> {
    start(Err(refusal), cfg)
}

fn start(
    serving: Result<(LocalBackend, Db), ApiError>,
    cfg: &ServeConfig,
) -> Result<Handle, ServeError> {
    if cfg.addr.ip().is_unspecified() {
        return Err(ServeError::Unspecified(cfg.addr));
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("jkb-serve")
        .build()?;
    let listener = runtime.block_on(tokio::net::TcpListener::bind(cfg.addr))?;
    let addr = listener.local_addr()?;
    let token = token::mint()?;
    token::write(&cfg.token_path, &token)?;
    let state = Arc::new(State {
        serving,
        token,
        ops: Arc::new(Semaphore::new(cfg.max_ops)),
        polls: Arc::new(Semaphore::new(cfg.max_polls)),
        polling: Mutex::new(HashSet::new()),
        sent: Notify::new(),
        max_body: cfg.max_body_bytes,
        max_wait: cfg.max_wait,
        poll_floor: cfg.poll_floor,
        read_timeout: cfg.read_timeout,
    });
    let connections = Arc::new(Semaphore::new(cfg.max_connections));
    let read_timeout = cfg.read_timeout;
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("jkb-serve-accept".to_owned())
        .spawn(move || {
            runtime.block_on(async move {
                loop {
                    tokio::select! {
                        _ = &mut stop_rx => break,
                        accepted = listener.accept() => {
                            // Out of descriptors (EMFILE) fails every accept at once until one closes: back
                            // off rather than spin a core on it.
                            let Ok((stream, _)) = accepted else {
                                tokio::time::sleep(Duration::from_millis(100)).await;
                                continue;
                            };
                            // Over the cap: dropping the stream closes it.
                            let Ok(slot) = Arc::clone(&connections).try_acquire_owned() else {
                                continue;
                            };
                            let state = Arc::clone(&state);
                            tokio::spawn(async move {
                                let _slot = slot;
                                let service = service_fn(move |req| handle(Arc::clone(&state), req));
                                let _ = http1::Builder::new()
                                    .timer(TokioTimer::new())
                                    .header_read_timeout(read_timeout)
                                    .serve_connection(TokioIo::new(stream), service)
                                    .await;
                            });
                        }
                    }
                }
            });
        })?;
    Ok(Handle {
        addr,
        stop: Some(stop_tx),
        thread: Some(thread),
    })
}

fn reply(status: StatusCode, body: &serde_json::Value) -> hyper::Response<Full<Bytes>> {
    hyper::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap_or_else(|_| hyper::Response::new(Full::new(Bytes::new())))
}

fn refuse(e: &ApiError) -> hyper::Response<Full<Bytes>> {
    reply(status_for(e.code), &json!(e))
}

/// The HTTP status for an error code. The body always carries the code, so a client decides on the
/// code; the status is for people and proxies.
#[must_use]
pub const fn status_for(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::BadRequest => StatusCode::BAD_REQUEST,
        ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
        ErrorCode::NoSuchTopic | ErrorCode::NoSuchGroup => StatusCode::NOT_FOUND,
        ErrorCode::TopicConflict => StatusCode::CONFLICT,
        ErrorCode::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        ErrorCode::Invalid | ErrorCode::AckBeyondEnd | ErrorCode::CorruptPayload => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        ErrorCode::QueueFull => StatusCode::TOO_MANY_REQUESTS,
        ErrorCode::Busy | ErrorCode::SchemaNewer | ErrorCode::Unavailable => {
            StatusCode::SERVICE_UNAVAILABLE
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn authorized(state: &State, req: &hyper::Request<Incoming>) -> bool {
    req.headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|given| token::matches(&state.token, given.trim()))
}

fn wait_ms(req: &hyper::Request<Incoming>) -> u64 {
    req.uri()
        .query()
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|kv| kv.strip_prefix("wait_ms="))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

async fn user_version(db: Db) -> Result<i64, ApiError> {
    tokio::task::spawn_blocking(move || {
        db.read(|c| Ok(c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?))
            .map_err(ApiError::from)
    })
    .await
    .map_err(|e| ApiError::with_code(ErrorCode::Internal, e.to_string()))?
}

/// Refuse a database a newer jkb has migrated. Asked before every operation — and before every
/// re-poll of a long-poll, which can outlive the migration that makes it stale.
async fn current_schema(db: &Db) -> Result<(), ApiError> {
    let schema = user_version(db.clone()).await?;
    let supported = jkb_core::supported_schema_version();
    if schema > supported {
        return Err(ApiError::with_code(
            ErrorCode::SchemaNewer,
            format!(
                "the database is at schema {schema} and this jkb serve knows {supported}; restart it \
                 from the newer jkb (setup.sh does)"
            ),
        ));
    }
    Ok(())
}

/// Holds a group's one long-poll slot; released on drop, including when the client goes away.
struct PollSlot<'a> {
    polling: &'a Mutex<HashSet<(String, String)>>,
    key: (String, String),
}

impl<'a> PollSlot<'a> {
    fn take(
        polling: &'a Mutex<HashSet<(String, String)>>,
        topic: &str,
        group: &str,
    ) -> Option<Self> {
        let key = (topic.to_owned(), group.to_owned());
        let mut held = polling.lock().ok()?;
        held.insert(key.clone()).then(|| Self { polling, key })
    }
}

impl Drop for PollSlot<'_> {
    fn drop(&mut self) {
        if let Ok(mut held) = self.polling.lock() {
            held.remove(&self.key);
        }
    }
}

fn busy(why: &str) -> hyper::Response<Full<Bytes>> {
    refuse(&ApiError::with_code(ErrorCode::Busy, why))
}

async fn handle(
    state: Arc<State>,
    req: hyper::Request<Incoming>,
) -> Result<hyper::Response<Full<Bytes>>, Infallible> {
    let route = (req.method().clone(), req.uri().path().to_owned());
    if !matches!(
        (&route.0, route.1.as_str()),
        (&Method::GET, "/v1/hello") | (&Method::POST, "/v1/op")
    ) {
        return Ok(refuse(&ApiError::bad_request(format!(
            "no such endpoint: {} {}",
            route.0, route.1
        ))));
    }
    if !authorized(&state, &req) {
        return Ok(refuse(&ApiError::with_code(
            ErrorCode::Unauthorized,
            "missing or wrong bearer token (it is rotated each time jkb serve starts)",
        )));
    }
    let (backend, db) = match &state.serving {
        Ok(serving) => serving,
        Err(refusal) => return Ok(refuse(refusal)),
    };
    // Every request past authentication holds an op permit from here — through the schema read, the
    // body and the parse — so authenticated clients cannot pile up unbounded work before the budget
    // is asked. A long-poll trades it for a poll permit once it is known to be one.
    let Ok(op_permit) = Arc::clone(&state.ops).try_acquire_owned() else {
        return Ok(busy("the daemon is at its concurrency limit; retry"));
    };
    if route.0 == Method::GET {
        return Ok(match user_version(db.clone()).await {
            Ok(schema) => reply(
                StatusCode::OK,
                &json!({
                    "protocol": crate::PROTOCOL_VERSION,
                    "schema_version": schema,
                    "supported_schema": jkb_core::supported_schema_version(),
                    "ops": Request::OPS,
                }),
            ),
            Err(e) => refuse(&e),
        });
    }
    let asked_wait = Duration::from_millis(wait_ms(&req)).min(state.max_wait);
    let body = match tokio::time::timeout(
        state.read_timeout,
        Limited::new(req.into_body(), state.max_body).collect(),
    )
    .await
    {
        Ok(Ok(collected)) => collected.to_bytes(),
        Ok(Err(e)) => {
            return Ok(refuse(&ApiError::with_code(
                ErrorCode::TooLarge,
                format!("request body refused: {e}"),
            )))
        }
        Err(_) => {
            return Ok(refuse(&ApiError::bad_request(format!(
                "request body not received within {}s",
                state.read_timeout.as_secs()
            ))))
        }
    };
    let request: Request = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return Ok(refuse(&ApiError::bad_request(e.to_string()))),
    };
    let (long_poll, wait) = match &request {
        Request::MqPoll { topic, group, .. } if !asked_wait.is_zero() => {
            (Some((topic.clone(), group.clone())), asked_wait)
        }
        _ => (None, Duration::ZERO),
    };
    let mut _permit = op_permit;
    let _slot = match &long_poll {
        None => None,
        Some((topic, group)) => {
            let Ok(poll_permit) = Arc::clone(&state.polls).try_acquire_owned() else {
                return Ok(busy("the daemon is at its long-poll limit; retry"));
            };
            _permit = poll_permit;
            let Some(slot) = PollSlot::take(&state.polling, topic, group) else {
                return Ok(busy(&format!(
                    "group {group} on {topic} already has a long-poll in progress; one at a time \
                     per group"
                )));
            };
            Some(slot)
        }
    };
    Ok(match serve_op(&state, backend, db, request, wait).await {
        Ok(response) => reply(StatusCode::OK, &json!(response)),
        Err(e) => refuse(&e),
    })
}

async fn call(backend: &LocalBackend, request: Request) -> Result<Response, ApiError> {
    let backend = backend.clone();
    tokio::task::spawn_blocking(move || backend.call(request))
        .await
        .map_err(|e| ApiError::with_code(ErrorCode::Internal, e.to_string()))?
}

async fn serve_op(
    state: &Arc<State>,
    backend: &LocalBackend,
    db: &Db,
    request: Request,
    wait: Duration,
) -> Result<Response, ApiError> {
    let is_send = matches!(request, Request::MqSend { .. });
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        // Registered BEFORE the poll, so a send that lands between the poll and the wait still wakes
        // it. `notified()` alone registers nothing until first polled — `enable` is what makes it a
        // waiter now; without it such a send was missed and only the floor delivered it.
        let woken = state.sent.notified();
        tokio::pin!(woken);
        woken.as_mut().enable();
        current_schema(db).await?;
        let response = call(backend, request.clone()).await?;
        if is_send {
            state.sent.notify_waiters();
        }
        let empty = matches!(&response, Response::Messages { messages } if messages.is_empty());
        if !empty || tokio::time::Instant::now() >= deadline || wait.is_zero() {
            return Ok(response);
        }
        tokio::select! {
            () = woken => {}
            () = tokio::time::sleep(state.poll_floor) => {}
            () = tokio::time::sleep_until(deadline) => {}
        }
    }
}
