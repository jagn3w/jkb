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
//! runs that timer for every head it waits on — a body within the same timeout, and a connection
//! that has not presented the token within that timeout is closed whatever it is doing (a client
//! that pipelines requests and never reads the answers stops hyper's header timer). A refusal before
//! authentication also closes its connection.

use std::collections::HashSet;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// How long a client may take to send a request's headers, or its body, or to authenticate a new
    /// connection — and how long an idle keep-alive connection is kept.
    pub read_timeout: Duration,
    /// How often a daemon whose database could not be opened tries again, on a request.
    pub reopen_every: Duration,
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
            reopen_every: Duration::from_secs(5),
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

/// Opens the database, for a daemon that starts, or keeps trying to start, without one.
pub type Opener = Box<dyn Fn() -> Result<Db, ApiError> + Send + Sync>;

enum Serving {
    Ready(LocalBackend, Db),
    /// The last open failed, `at` then; every request gets `why` until a retry succeeds.
    Failed {
        why: ApiError,
        at: std::time::Instant,
    },
}

struct State {
    serving: Mutex<Serving>,
    /// `None` for a daemon handed an open database, which has nothing to retry.
    opener: Option<Opener>,
    /// One re-open at a time.
    opening: tokio::sync::Mutex<()>,
    reopen_every: Duration,
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
    start(Serving::Ready(LocalBackend::new(db.clone()), db), None, cfg)
}

/// Like [`spawn`], opening the database with `open` — and serving even when that fails. Exiting
/// would put a supervisor into a restart loop and leave clients with `unavailable` and no reason.
/// Instead every request is answered with the open's error (`schema_newer` for a database a newer
/// jkb migrated), and the open is tried again on a request at most every
/// [`ServeConfig::reopen_every`] — so a failure that passes (a lock held past the busy timeout, a
/// volume mounted late) ends without anyone restarting the daemon.
///
/// # Errors
/// As [`spawn`]; a failed open is not one.
pub fn spawn_opening(open: Opener, cfg: &ServeConfig) -> Result<Handle, ServeError> {
    let serving = match open() {
        Ok(db) => Serving::Ready(LocalBackend::new(db.clone()), db),
        Err(why) => {
            eprintln!("jkb serve: {}; retrying on requests", why.message);
            Serving::Failed {
                why,
                at: std::time::Instant::now(),
            }
        }
    };
    start(serving, Some(open), cfg)
}

/// Raise this process's soft descriptor limit toward its hard one (launchd's default soft limit is
/// 256), and return how many connections that leaves room for beside the database's own files.
fn connection_budget(wanted: usize) -> usize {
    use rustix::process::{getrlimit, setrlimit, Resource};
    const RESERVE: u64 = 64;
    let mut limit = getrlimit(Resource::Nofile);
    let target = limit.maximum.map_or(4096, |hard| hard.min(4096));
    if limit.current.is_some_and(|soft| soft < target) {
        limit.current = Some(target);
        let _ = setrlimit(Resource::Nofile, limit);
    }
    let soft = getrlimit(Resource::Nofile).current.unwrap_or(u64::MAX);
    let room = usize::try_from(soft.saturating_sub(RESERVE)).unwrap_or(usize::MAX);
    wanted.min(room.max(8))
}

/// Whether an accept failure is the process running out of something, which every accept will hit
/// until something is released — rather than one connection's own failure (a peer that reset before
/// it was accepted), after which the next accept may well succeed.
fn out_of_resources(e: &std::io::Error) -> bool {
    use rustix::io::Errno;
    matches!(
        Errno::from_io_error(e),
        Some(Errno::MFILE | Errno::NFILE | Errno::NOBUFS | Errno::NOMEM)
    )
}

fn start(
    serving: Serving,
    opener: Option<Opener>,
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
        serving: Mutex::new(serving),
        opener,
        opening: tokio::sync::Mutex::new(()),
        reopen_every: cfg.reopen_every,
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
    let connections = Arc::new(Semaphore::new(connection_budget(cfg.max_connections)));
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
                            let stream = match accepted {
                                Ok((stream, _)) => stream,
                                // Out of descriptors fails every accept at once until one closes:
                                // back off rather than spin a core. A connection's own failure is
                                // not a reason to slow everyone else's accept.
                                Err(e) => {
                                    if out_of_resources(&e) {
                                        tokio::time::sleep(Duration::from_millis(100)).await;
                                    }
                                    continue;
                                }
                            };
                            // Over the cap: dropping the stream closes it.
                            let Ok(slot) = Arc::clone(&connections).try_acquire_owned() else {
                                continue;
                            };
                            let state = Arc::clone(&state);
                            tokio::spawn(async move {
                                let _slot = slot;
                                let authed = Arc::new(AtomicBool::new(false));
                                let service = {
                                    let authed = Arc::clone(&authed);
                                    service_fn(move |req| {
                                        handle(Arc::clone(&state), Arc::clone(&authed), req)
                                    })
                                };
                                let conn = http1::Builder::new()
                                    .timer(TokioTimer::new())
                                    .header_read_timeout(read_timeout)
                                    .serve_connection(TokioIo::new(stream), service);
                                tokio::pin!(conn);
                                // A connection has `read_timeout` to present the token, whatever it
                                // does meanwhile; one that has not is dropped, which closes it.
                                tokio::select! {
                                    _ = conn.as_mut() => {}
                                    () = tokio::time::sleep(read_timeout) => {
                                        if authed.load(Ordering::SeqCst) {
                                            let _ = conn.await;
                                        }
                                    }
                                }
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

/// A refusal before authentication: the answer, and the connection closed after it, so an
/// unauthenticated client cannot hold a slot by sending refused requests down a kept-alive connection.
fn refuse_and_close(e: &ApiError) -> hyper::Response<Full<Bytes>> {
    let mut response = refuse(e);
    response.headers_mut().insert(
        hyper::header::CONNECTION,
        hyper::header::HeaderValue::from_static("close"),
    );
    response
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

async fn blocking_read<T: Send + 'static>(
    db: &Db,
    f: impl FnOnce(&rusqlite::Connection) -> jkb_core::Result<T> + Send + 'static,
) -> Result<T, ApiError> {
    let db = db.clone();
    tokio::task::spawn_blocking(move || db.read(f).map_err(ApiError::from))
        .await
        .map_err(|e| ApiError::with_code(ErrorCode::Internal, e.to_string()))?
}

/// Refuse a database a newer jkb has migrated. Every write already refuses it inside its own
/// transaction (`jkb_core::Db::write_txn`); this answers reads the same way, and is asked before
/// every re-poll of a long-poll too, which can outlive the migration that makes it stale.
async fn current_schema(db: &Db) -> Result<(), ApiError> {
    blocking_read(db, jkb_core::refuse_newer_schema).await
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

/// The database to serve with, re-opening it if the last open failed long enough ago.
async fn ready(state: &Arc<State>) -> Result<(LocalBackend, Db), ApiError> {
    let current = |state: &State| -> Result<(LocalBackend, Db), (ApiError, bool)> {
        let serving = state.serving.lock().map_err(|_| {
            (
                ApiError::with_code(ErrorCode::Internal, "state lock poisoned"),
                false,
            )
        })?;
        match &*serving {
            Serving::Ready(backend, db) => Ok((backend.clone(), db.clone())),
            Serving::Failed { why, at } => Err((
                why.clone(),
                state.opener.is_some() && at.elapsed() >= state.reopen_every,
            )),
        }
    };
    match current(state) {
        Ok(ready) => return Ok(ready),
        Err((why, false)) => return Err(why),
        Err((_, true)) => {}
    }
    let _one_at_a_time = state.opening.lock().await;
    // Another request may have re-opened, or failed again, while this one waited.
    match current(state) {
        Ok(ready) => return Ok(ready),
        Err((why, false)) => return Err(why),
        Err((_, true)) => {}
    }
    let opener_state = Arc::clone(state);
    let opened = tokio::task::spawn_blocking(move || match &opener_state.opener {
        Some(open) => open(),
        None => Err(ApiError::with_code(ErrorCode::Internal, "no opener")),
    })
    .await
    .map_err(|e| ApiError::with_code(ErrorCode::Internal, e.to_string()))?;
    let mut serving = state
        .serving
        .lock()
        .map_err(|_| ApiError::with_code(ErrorCode::Internal, "state lock poisoned"))?;
    match opened {
        Ok(db) => {
            eprintln!("jkb serve: the database opened; serving");
            *serving = Serving::Ready(LocalBackend::new(db.clone()), db.clone());
            Ok((LocalBackend::new(db.clone()), db))
        }
        Err(why) => {
            *serving = Serving::Failed {
                why: why.clone(),
                at: std::time::Instant::now(),
            };
            Err(why)
        }
    }
}

async fn handle(
    state: Arc<State>,
    authed: Arc<AtomicBool>,
    req: hyper::Request<Incoming>,
) -> Result<hyper::Response<Full<Bytes>>, Infallible> {
    let route = (req.method().clone(), req.uri().path().to_owned());
    if !matches!(
        (&route.0, route.1.as_str()),
        (&Method::GET, "/v1/hello") | (&Method::POST, "/v1/op")
    ) {
        return Ok(refuse_and_close(&ApiError::bad_request(format!(
            "no such endpoint: {} {}",
            route.0, route.1
        ))));
    }
    if !authorized(&state, &req) {
        return Ok(refuse_and_close(&ApiError::with_code(
            ErrorCode::Unauthorized,
            "missing or wrong bearer token (it is rotated each time jkb serve starts)",
        )));
    }
    authed.store(true, Ordering::SeqCst);
    let (backend, db) = match ready(&state).await {
        Ok(serving) => serving,
        Err(refusal) => return Ok(refuse(&refusal)),
    };
    let (backend, db) = (&backend, &db);
    // Every request past authentication holds an op permit from here — through the schema read, the
    // body and the parse — so authenticated clients cannot pile up unbounded work before the budget
    // is asked. A long-poll trades it for a poll permit once it is known to be one.
    let Ok(op_permit) = Arc::clone(&state.ops).try_acquire_owned() else {
        return Ok(busy("the daemon is at its concurrency limit; retry"));
    };
    if route.0 == Method::GET {
        return Ok(
            match blocking_read(db, jkb_core::applied_schema_version).await {
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
            },
        );
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

#[cfg(test)]
mod tests {
    use rustix::io::Errno;

    use super::{connection_budget, out_of_resources};

    #[test]
    fn only_running_out_of_something_slows_the_accept_loop() {
        let err = |e: Errno| std::io::Error::from_raw_os_error(e.raw_os_error());
        for e in [Errno::MFILE, Errno::NFILE, Errno::NOBUFS, Errno::NOMEM] {
            assert!(out_of_resources(&err(e)), "{e:?}");
        }
        for e in [Errno::CONNABORTED, Errno::CONNRESET, Errno::INTR] {
            assert!(
                !out_of_resources(&err(e)),
                "{e:?}: one connection's failure"
            );
        }
    }

    #[test]
    fn the_connection_cap_leaves_room_for_the_database_under_the_descriptor_limit() {
        use rustix::process::{getrlimit, Resource};
        let budget = connection_budget(usize::MAX);
        let limit = getrlimit(Resource::Nofile);
        if let Some(soft) = limit.current {
            let target = limit.maximum.map_or(4096, |hard| hard.min(4096));
            assert!(
                soft >= target,
                "the soft limit was raised: {soft} < {target}"
            );
            assert!(u64::try_from(budget).unwrap() <= soft.saturating_sub(64).max(8));
        }
        assert_eq!(connection_budget(3), 3, "a smaller cap is kept");
    }
}
