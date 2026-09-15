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
    /// Concurrent read-set requests (`Request::is_agent_read`) — a third budget. The reads run one at a time
    /// on the reader connection, so a burst of them otherwise held every op permit while queued and a
    /// hook's write was refused `busy`.
    pub max_reads: usize,
    /// About how many bytes of JSON one read may answer with ([`jkb_api::kb::Budget`]); past it the
    /// answer is cut short and marked `truncated`.
    pub read_budget_bytes: usize,
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
    /// How long a response write may make no progress before its connection is closed. A response
    /// body keeps the permit its request ran under until it is written ([`Held`]), and hyper has no
    /// write timeout of its own — so without this a client that stopped reading kept a permit until
    /// the daemon restarted, and enough of them refused the notification hook's every request.
    pub write_stall: Duration,
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
            max_reads: 16,
            read_budget_bytes: 16 * 1024 * 1024,
            max_wait: Duration::from_secs(30),
            poll_floor: Duration::from_millis(250),
            max_connections: 256,
            read_timeout: Duration::from_secs(10),
            reopen_every: Duration::from_secs(5),
            write_stall: Duration::from_secs(10),
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
    /// Another process holds the address. Named apart from [`ServeError::Io`] because the cause is
    /// outside jkb and the bare OS message sends nobody to it: measured on the Mac (2026-09-14), a
    /// VS Code window attached to the dev container forwarded the container's port 7117 to the
    /// host's loopback, and `com.jkb.serve` crash-looped on "Address already in use" for as long as
    /// the window stayed open.
    #[error(
        "{0} is already in use by another process, so jkb serve cannot listen there. Find it with \
         `lsof -nP -iTCP:{port} -sTCP:LISTEN`; a VS Code window forwarding the dev container's port \
         {port} to this host is one measured cause (stop forwarding it in the Ports view)",
        port = .0.port()
    )]
    AddrInUse(SocketAddr),
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
    /// One re-open at a time — held by the blocking open itself, so a cancelled request cannot let a
    /// second open start beside it.
    opening: Arc<tokio::sync::Mutex<()>>,
    reopen_every: Duration,
    token: String,
    ops: Arc<Semaphore>,
    polls: Arc<Semaphore>,
    reads: Arc<Semaphore>,
    read_budget_bytes: usize,
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
/// [`ServeError::Unspecified`] for `0.0.0.0`/`::`, [`ServeError::AddrInUse`] when another process
/// holds the address, another I/O error binding, or a token write failure.
pub fn spawn(db: Db, cfg: &ServeConfig) -> Result<Handle, ServeError> {
    start(Source::Db(db), cfg)
}

/// Like [`spawn`], opening the database with `open` — and serving even when that fails. Exiting
/// would put a supervisor into a restart loop and leave clients with `unavailable` and no reason.
/// Instead every request is answered with the open's error (`schema_newer` for a database a newer
/// jkb migrated), and the open is tried again on a request at most every
/// [`ServeConfig::reopen_every`] — so a failure that passes (a lock held past the busy timeout during
/// a long write, say) ends without anyone restarting the daemon. Not every failure is retried: the
/// token must still be writable at start, and `open` decides what counts as the database (the CLI's
/// creates a missing one).
///
/// # Errors
/// As [`spawn`]; a failed open is not one.
pub fn spawn_opening(open: Opener, cfg: &ServeConfig) -> Result<Handle, ServeError> {
    start(Source::Opener(open), cfg)
}

/// Where the database being served comes from.
enum Source {
    /// Already open ([`spawn`]).
    Db(Db),
    /// Opened by the daemon, and re-opened while that fails ([`spawn_opening`]).
    Opener(Opener),
}

/// The backend serving `db`: every read's answer bounded to `read_budget_bytes`, and the read set on a
/// connection of its own
/// ([`LocalBackend::with_reader`]), so a client's long read does not hold up the writes behind it — a
/// notification hook's among them. A reader that will not open costs that separation, not the daemon.
fn backend_for(db: &Db, read_budget_bytes: usize) -> LocalBackend {
    let backend = LocalBackend::new(db.clone()).with_read_budget(read_budget_bytes);
    match db.reader() {
        Ok(reader) => backend.with_reader(reader),
        Err(e) => {
            eprintln!(
                "jkb serve: reads share the writer's connection, which a long read holds up: {e}"
            );
            backend
        }
    }
}

/// The first open for [`Source::Opener`]: a failure is served, not returned.
fn first_open(open: &Opener, read_budget_bytes: usize) -> Serving {
    match open() {
        Ok(db) => Serving::Ready(backend_for(&db, read_budget_bytes), db),
        Err(why) => {
            eprintln!("jkb serve: {}; retrying on requests", why.message);
            Serving::Failed {
                why,
                at: std::time::Instant::now(),
            }
        }
    }
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

/// Bind, then open, then write the token. The database is opened only once the port is held: a
/// daemon that cannot listen (another process on the port, restarted by its supervisor for as long
/// as that lasts) must not open and migrate the database on every attempt, nor log a "retrying on
/// requests" it will never serve.
fn start(source: Source, cfg: &ServeConfig) -> Result<Handle, ServeError> {
    if cfg.addr.ip().is_unspecified() {
        return Err(ServeError::Unspecified(cfg.addr));
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("jkb-serve")
        .build()?;
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind(cfg.addr))
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::AddrInUse => ServeError::AddrInUse(cfg.addr),
            _ => ServeError::Io(e),
        })?;
    let addr = listener.local_addr()?;
    let (serving, opener) = match source {
        Source::Db(db) => (
            Serving::Ready(backend_for(&db, cfg.read_budget_bytes), db),
            None,
        ),
        Source::Opener(open) => (first_open(&open, cfg.read_budget_bytes), Some(open)),
    };
    let token = token::mint()?;
    token::write(&cfg.token_path, &token)?;
    let state = Arc::new(State {
        serving: Mutex::new(serving),
        opener,
        opening: Arc::new(tokio::sync::Mutex::new(())),
        reopen_every: cfg.reopen_every,
        token,
        ops: Arc::new(Semaphore::new(cfg.max_ops)),
        polls: Arc::new(Semaphore::new(cfg.max_polls)),
        reads: Arc::new(Semaphore::new(cfg.max_reads)),
        read_budget_bytes: cfg.read_budget_bytes,
        polling: Mutex::new(HashSet::new()),
        sent: Notify::new(),
        max_body: cfg.max_body_bytes,
        max_wait: cfg.max_wait,
        poll_floor: cfg.poll_floor,
        read_timeout: cfg.read_timeout,
    });
    let connections = Arc::new(Semaphore::new(connection_budget(cfg.max_connections)));
    let read_timeout = cfg.read_timeout;
    let write_stall = cfg.write_stall;
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
                            tokio::spawn(serve_connection(
                                stream,
                                Arc::clone(&state),
                                slot,
                                read_timeout,
                                write_stall,
                            ));
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
        ErrorCode::NoSuchTopic | ErrorCode::NoSuchGroup | ErrorCode::NotFound => {
            StatusCode::NOT_FOUND
        }
        ErrorCode::Unsupported => StatusCode::NOT_IMPLEMENTED,
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
    let one_at_a_time = Arc::clone(&state.opening).lock_owned().await;
    // Another request may have re-opened, or failed again, while this one waited.
    match current(state) {
        Ok(ready) => return Ok(ready),
        Err((why, false)) => return Err(why),
        Err((_, true)) => {}
    }
    // The open, the state it leaves and the guard all live in the blocking task: if this request is
    // cancelled (its client went away), the open still finishes, records its result, and only then
    // lets the next one start.
    let opener_state = Arc::clone(state);
    tokio::task::spawn_blocking(move || {
        let _one_at_a_time = one_at_a_time;
        let opened = match &opener_state.opener {
            Some(open) => open(),
            None => Err(ApiError::with_code(ErrorCode::Internal, "no opener")),
        };
        let mut serving = opener_state
            .serving
            .lock()
            .map_err(|_| ApiError::with_code(ErrorCode::Internal, "state lock poisoned"))?;
        match opened {
            Ok(db) => {
                eprintln!("jkb serve: the database opened; serving");
                let backend = backend_for(&db, opener_state.read_budget_bytes);
                *serving = Serving::Ready(backend.clone(), db.clone());
                Ok((backend, db))
            }
            Err(why) => {
                *serving = Serving::Failed {
                    why: why.clone(),
                    at: std::time::Instant::now(),
                };
                Err(why)
            }
        }
    })
    .await
    .map_err(|e| ApiError::with_code(ErrorCode::Internal, e.to_string()))?
}

/// A stream whose writes fail with `TimedOut` once one has made no progress for its deadline — the
/// write timeout hyper does not have ([`ServeConfig::write_stall`]). The failed write ends the
/// connection, which drops the response body and the permit it holds.
struct WriteDeadline<S> {
    inner: S,
    stall: Duration,
    /// Armed by a write that could not proceed; cleared by one that did.
    stalled: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

impl<S> WriteDeadline<S> {
    const fn new(inner: S, stall: Duration) -> Self {
        Self {
            inner,
            stall,
            stalled: None,
        }
    }

    /// `result` as a write's outcome: progress disarms the deadline; no progress arms it, and past it
    /// is an error.
    fn judge<T>(
        &mut self,
        result: std::task::Poll<std::io::Result<T>>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<T>> {
        use std::future::Future as _;
        if result.is_ready() {
            self.stalled = None;
            return result;
        }
        let stall = self.stall;
        let deadline = self
            .stalled
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(stall)));
        if deadline.as_mut().poll(cx).is_ready() {
            return std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the client stopped reading its response",
            )));
        }
        std::task::Poll::Pending
    }
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for WriteDeadline<S> {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for WriteDeadline<S> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let result = std::pin::Pin::new(&mut this.inner).poll_write(cx, buf);
        this.judge(result, cx)
    }

    fn poll_write_vectored(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let result = std::pin::Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        this.judge(result, cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let result = std::pin::Pin::new(&mut this.inner).poll_flush(cx);
        this.judge(result, cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// A permit shared by everything that must finish before it is released.
type Permit = Arc<tokio::sync::OwnedSemaphorePermit>;

/// A response body that keeps its request's permit until hyper has written it, or dropped it for a
/// client that went away. An answer is in memory until then — up to a read's whole budget — so it
/// counts against the budget that admitted it: released when the handler returned, 256 connections
/// that asked for large reads and never read the socket held every answer with every permit free.
///
/// **Handed to hyper in [`Held::CHUNK`] frames.** hyper pulls a body's next frame only once its write
/// buffer has room, but it takes a whole frame when it does: given the answer as one frame it copied
/// all of it into its buffer at once and dropped the body — and the permit — before a byte was sent,
/// which a loopback test measured as the permit coming straight back for a client that never read.
struct Held {
    body: Full<Bytes>,
    /// What of the body's data has not been handed over yet.
    rest: Bytes,
    _permit: Option<Permit>,
}

impl Held {
    const CHUNK: usize = 64 * 1024;

    const fn new(body: Full<Bytes>, permit: Option<Permit>) -> Self {
        Self {
            body,
            rest: Bytes::new(),
            _permit: permit,
        }
    }
}

impl hyper::body::Body for Held {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, Infallible>>> {
        let this = self.get_mut();
        loop {
            if !this.rest.is_empty() {
                let n = this.rest.len().min(Self::CHUNK);
                return std::task::Poll::Ready(Some(Ok(hyper::body::Frame::data(
                    this.rest.split_to(n),
                ))));
            }
            match std::pin::Pin::new(&mut this.body).poll_frame(cx) {
                std::task::Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(data) => this.rest = data,
                    Err(frame) => return std::task::Poll::Ready(Some(Ok(frame))),
                },
                other => return other,
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.rest.is_empty() && self.body.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        let inner = self.body.size_hint();
        let rest = self.rest.len() as u64;
        let mut hint = hyper::body::SizeHint::new();
        hint.set_lower(inner.lower() + rest);
        if let Some(upper) = inner.upper() {
            hint.set_upper(upper + rest);
        }
        hint
    }
}

/// One accepted connection, served until it closes. It has `read_timeout` to present the token,
/// whatever it does meanwhile; one that has not is dropped, which closes it. `slot` is its place under
/// the connection cap, released when it ends.
async fn serve_connection(
    stream: tokio::net::TcpStream,
    state: Arc<State>,
    slot: tokio::sync::OwnedSemaphorePermit,
    read_timeout: Duration,
    write_stall: Duration,
) {
    let _slot = slot;
    let authed = Arc::new(AtomicBool::new(false));
    let service = {
        let authed = Arc::clone(&authed);
        service_fn(move |req| respond(Arc::clone(&state), Arc::clone(&authed), req))
    };
    let conn = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(read_timeout)
        .serve_connection(
            TokioIo::new(WriteDeadline::new(stream, write_stall)),
            service,
        );
    tokio::pin!(conn);
    tokio::select! {
        _ = conn.as_mut() => {}
        () = tokio::time::sleep(read_timeout) => {
            if authed.load(Ordering::SeqCst) {
                let _ = conn.await;
            }
        }
    }
}

/// One request's response, its body holding the permit the request ran under ([`Held`]).
async fn respond(
    state: Arc<State>,
    authed: Arc<AtomicBool>,
    req: hyper::Request<Incoming>,
) -> Result<hyper::Response<Held>, Infallible> {
    let mut permit = None;
    let response = handle(state, authed, req, &mut permit).await?;
    Ok(response.map(|body| Held::new(body, permit)))
}

/// `held` is left holding the permit the request ran under, for the response body to keep.
async fn handle(
    state: Arc<State>,
    authed: Arc<AtomicBool>,
    req: hyper::Request<Incoming>,
    held: &mut Option<Permit>,
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
    let (permit, _slot, wait) = match permit_for(&state, &request, asked_wait, op_permit) {
        Ok(granted) => granted,
        Err(refusal) => return Ok(refuse(&refusal)),
    };
    let permit: Permit = Arc::new(permit);
    *held = Some(Arc::clone(&permit));
    Ok(
        match serve_op(&state, backend, db, request, wait, &permit).await {
            Ok(response) => reply(StatusCode::OK, &json!(response)),
            Err(e) => refuse(&e),
        },
    )
}

/// The permit a parsed request runs under, traded for its `op_permit` once its class is known, with
/// its long-poll slot and wait: a long-poll waits under the poll budget, one per group; a read
/// (`Request::is_agent_read`) queues on the one reader connection, so it waits under the read budget rather
/// than holding an op permit a hook's write needs; anything else keeps the op permit.
fn permit_for<'s>(
    state: &'s State,
    request: &Request,
    asked_wait: Duration,
    op_permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<
    (
        tokio::sync::OwnedSemaphorePermit,
        Option<PollSlot<'s>>,
        Duration,
    ),
    ApiError,
> {
    let busy = |why: String| ApiError::with_code(ErrorCode::Busy, why);
    match request {
        Request::MqPoll { topic, group, .. } if !asked_wait.is_zero() => {
            let Ok(poll_permit) = Arc::clone(&state.polls).try_acquire_owned() else {
                return Err(busy(
                    "the daemon is at its long-poll limit; retry".to_owned(),
                ));
            };
            let Some(slot) = PollSlot::take(&state.polling, topic, group) else {
                return Err(busy(format!(
                    "group {group} on {topic} already has a long-poll in progress; one at a time \
                     per group"
                )));
            };
            drop(op_permit);
            Ok((poll_permit, Some(slot), asked_wait))
        }
        _ if request.is_agent_read() => {
            let Ok(read_permit) = Arc::clone(&state.reads).try_acquire_owned() else {
                return Err(busy("the daemon is at its read limit; retry".to_owned()));
            };
            drop(op_permit);
            Ok((read_permit, None, Duration::ZERO))
        }
        _ => Ok((op_permit, None, Duration::ZERO)),
    }
}

/// Serve `request` on a blocking thread, which holds `permit` until the call returns. Not the awaiting
/// future: hyper drops that when its client goes away, and a permit released then let a client that
/// connects, asks for a long read and hangs up grow the reader's queue without bound while the
/// budget read empty.
async fn call(
    backend: &LocalBackend,
    request: Request,
    permit: Permit,
) -> Result<Response, ApiError> {
    let backend = backend.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        backend.call(request)
    })
    .await
    .map_err(|e| ApiError::with_code(ErrorCode::Internal, e.to_string()))?
}

async fn serve_op(
    state: &Arc<State>,
    backend: &LocalBackend,
    db: &Db,
    request: Request,
    wait: Duration,
    permit: &Permit,
) -> Result<Response, ApiError> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        // Registered BEFORE the poll, so a send that lands between the poll and the wait still wakes
        // it. `notified()` alone registers nothing until first polled — `enable` is what makes it a
        // waiter now; without it such a send was missed and only the floor delivered it.
        let woken = state.sent.notified();
        tokio::pin!(woken);
        woken.as_mut().enable();
        current_schema(db).await?;
        let response = call(backend, request.clone(), Arc::clone(permit)).await?;
        if response.announces_a_send() {
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

    use super::{backend_for, call, connection_budget, out_of_resources, Held, WriteDeadline};

    /// A writer the test opens and shuts.
    struct Gate(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl tokio::io::AsyncWrite for Gate {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if self.0.load(std::sync::atomic::Ordering::SeqCst) {
                std::task::Poll::Ready(Ok(buf.len()))
            } else {
                std::task::Poll::Pending
            }
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// One `poll_write` of a byte, as its `Poll`.
    async fn write(w: &mut WriteDeadline<Gate>) -> std::task::Poll<std::io::Result<usize>> {
        std::future::poll_fn(|cx| {
            std::task::Poll::Ready(tokio::io::AsyncWrite::poll_write(
                std::pin::Pin::new(&mut *w),
                cx,
                b"x",
            ))
        })
        .await
    }

    /// The write deadline measures one stall, not a connection's life: progress disarms it. Without
    /// that, a slow reader making progress, or a keep-alive connection whose earlier answer once had
    /// to wait, was closed the next time a write waited at all.
    #[test]
    fn a_write_deadline_is_reset_by_progress_and_fires_on_a_stall() {
        use std::sync::atomic::Ordering;
        use std::task::Poll;
        // Paused: the sleeps below advance a test clock exactly, so the margins are not at the mercy of a
        // loaded machine.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            let open = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut w = WriteDeadline::new(
                Gate(std::sync::Arc::clone(&open)),
                std::time::Duration::from_millis(300),
            );
            assert!(
                write(&mut w).await.is_pending(),
                "a blocked write arms the deadline"
            );
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            open.store(true, Ordering::SeqCst);
            assert!(
                matches!(write(&mut w).await, Poll::Ready(Ok(1))),
                "progress"
            );
            open.store(false, Ordering::SeqCst);
            assert!(write(&mut w).await.is_pending());
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            // 400 ms since the first stall began, 200 ms into this one.
            assert!(
                write(&mut w).await.is_pending(),
                "the earlier stall's deadline was not carried over"
            );
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            match write(&mut w).await {
                Poll::Ready(Err(e)) => assert_eq!(e.kind(), std::io::ErrorKind::TimedOut),
                other => panic!("a stall past the deadline: {other:?}"),
            }
        });
    }

    /// A permit is released only when everything its request started has finished: the call on its
    /// blocking thread, though the client and its handler are gone, and the body hyper writes.
    #[test]
    fn a_permit_outlives_a_cancelled_request_until_its_call_returns() {
        let dir = tempfile::tempdir().unwrap();
        let db = jkb_core::Db::open(dir.path().join("jkb.db")).unwrap();
        let backend = backend_for(&db, 1024 * 1024);
        let reads = backend.reads().clone();
        let (held, holding) = std::sync::mpsc::channel();
        let blocker = std::thread::spawn(move || {
            reads
                .read(move |_| {
                    held.send(()).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(600));
                    Ok(())
                })
                .unwrap();
        });
        holding.recv().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        runtime.block_on(async {
            let permit = std::sync::Arc::new(budget.clone().try_acquire_owned().unwrap());
            let queued = tokio::spawn({
                let backend = backend.clone();
                async move {
                    call(
                        &backend,
                        jkb_api::Request::KbLs {
                            path: None,
                            all: false,
                            recursive: false,
                        },
                        permit,
                    )
                    .await
                }
            });
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            queued.abort();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            assert_eq!(
                budget.available_permits(),
                0,
                "the client went away, but its read is still queued on the reader"
            );
        });
        blocker.join().unwrap();
        runtime.block_on(async {
            for _ in 0..50 {
                if budget.available_permits() == 1 {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            panic!("the permit was never released after the read ran");
        });

        let permit = std::sync::Arc::new(budget.clone().try_acquire_owned().unwrap());
        let body = Held::new(
            http_body_util::Full::new(bytes::Bytes::from_static(b"answer")),
            Some(permit),
        );
        assert_eq!(
            budget.available_permits(),
            0,
            "held while the body is unwritten"
        );
        drop(body);
        assert_eq!(budget.available_permits(), 1);
    }

    /// The daemon serves the read set on a connection of its own — the `query_only` one, so a write
    /// through it is refused — rather than on the writer every notification waits on.
    #[test]
    fn the_daemon_s_reads_are_served_apart_from_its_writes() {
        let dir = tempfile::tempdir().unwrap();
        let db = jkb_core::Db::open(dir.path().join("jkb.db")).unwrap();
        let backend = backend_for(&db, 1024);
        let refused = backend
            .reads()
            .write_txn("t", |c, _| {
                c.execute("DELETE FROM notify_sessions", [])?;
                Ok(())
            })
            .unwrap_err();
        assert!(refused.to_string().contains("readonly"), "{refused}");
    }

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
