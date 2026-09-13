//! `jkb serve`: [`jkb_api`] operations over HTTP/1.1.
//!
//! ```text
//! GET  /v1/hello                → {"protocol","schema_version","supported_schema","ops"}
//! POST /v1/op[?wait_ms=N]       → a jkb_api::Response, or a jkb_api::ApiError with a 4xx/5xx status
//! ```
//!
//! Both require `Authorization: Bearer <token>`. `wait_ms` applies to `mq.poll` only: an empty
//! answer is held until a message arrives or the wait (capped) runs out — woken at once by a send
//! this daemon served, and at worst every 250 ms for one another process wrote (the host CLI opens
//! the database directly).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, StatusCode};
use hyper_util::rt::TokioIo;
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

/// A running daemon. Dropping it does not stop it; call [`Handle::shutdown`].
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
    backend: LocalBackend,
    db: Db,
    token: String,
    ops: Arc<Semaphore>,
    polls: Arc<Semaphore>,
    sent: Notify,
    max_body: usize,
    max_wait: Duration,
    poll_floor: Duration,
}

/// Bind, write a fresh token, and serve on a background thread.
///
/// The token is written only after the bind succeeds, so a client that finds a new token finds a
/// daemon behind it.
///
/// # Errors
/// [`ServeError::Unspecified`] for `0.0.0.0`/`::`, an I/O error binding, or a token write failure.
pub fn spawn(db: Db, cfg: &ServeConfig) -> Result<Handle, ServeError> {
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
        backend: LocalBackend::new(db.clone()),
        db,
        token,
        ops: Arc::new(Semaphore::new(cfg.max_ops)),
        polls: Arc::new(Semaphore::new(cfg.max_polls)),
        sent: Notify::new(),
        max_body: cfg.max_body_bytes,
        max_wait: cfg.max_wait,
        poll_floor: cfg.poll_floor,
    });
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("jkb-serve-accept".to_owned())
        .spawn(move || {
            runtime.block_on(async move {
                loop {
                    tokio::select! {
                        _ = &mut stop_rx => break,
                        accepted = listener.accept() => {
                            let Ok((stream, _)) = accepted else { continue };
                            let state = Arc::clone(&state);
                            tokio::spawn(async move {
                                let service = service_fn(move |req| handle(Arc::clone(&state), req));
                                let _ = http1::Builder::new()
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
    let schema = match user_version(state.db.clone()).await {
        Ok(v) => v,
        Err(e) => return Ok(refuse(&e)),
    };
    let supported = jkb_core::supported_schema_version();
    if route.0 == Method::GET {
        return Ok(reply(
            StatusCode::OK,
            &json!({
                "protocol": crate::PROTOCOL_VERSION,
                "schema_version": schema,
                "supported_schema": supported,
                "ops": Request::OPS,
            }),
        ));
    }
    if schema > supported {
        return Ok(refuse(&ApiError::with_code(
            ErrorCode::SchemaNewer,
            format!(
                "the database is at schema {schema} and this jkb serve knows {supported}; restart it \
                 from the newer jkb (setup.sh does)"
            ),
        )));
    }
    let wait = Duration::from_millis(wait_ms(&req)).min(state.max_wait);
    let body = match Limited::new(req.into_body(), state.max_body)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            return Ok(refuse(&ApiError::with_code(
                ErrorCode::TooLarge,
                format!("request body refused: {e}"),
            )))
        }
    };
    let request: Request = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return Ok(refuse(&ApiError::bad_request(e.to_string()))),
    };
    let long_poll = matches!(request, Request::MqPoll { .. }) && !wait.is_zero();
    let budget = if long_poll { &state.polls } else { &state.ops };
    let Ok(_permit) = Arc::clone(budget).try_acquire_owned() else {
        return Ok(refuse(&ApiError::with_code(
            ErrorCode::Busy,
            "the daemon is at its concurrency limit; retry",
        )));
    };
    Ok(match serve_op(&state, request, wait).await {
        Ok(response) => reply(StatusCode::OK, &json!(response)),
        Err(e) => refuse(&e),
    })
}

async fn call(state: &Arc<State>, request: Request) -> Result<Response, ApiError> {
    let backend = state.backend.clone();
    tokio::task::spawn_blocking(move || backend.call(request))
        .await
        .map_err(|e| ApiError::with_code(ErrorCode::Internal, e.to_string()))?
}

async fn serve_op(
    state: &Arc<State>,
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
        let response = call(state, request.clone()).await?;
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
