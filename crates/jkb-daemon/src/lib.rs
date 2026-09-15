//! `jkb serve` and its client (design r3.2 H2/H3, `openspec/changes/jkb-message-queue/`).
//!
//! **Why this exists.** A process on the dev container's kernel must never open the host's
//! `jkb.db` — measured, one on each side of the bind mount corrupts it. So the host runs one daemon
//! that holds the database and serves [`jkb_api`] operations over HTTP, and the container's `jkb`
//! is a client of it. HTTP rather than a bespoke framing because Claude Code's sandbox only lets a
//! command out through an HTTP proxy.
//!
//! **The boundary is the op set, not the token.** The container's agent can read the token file.
//! The token keeps out other local processes and containers that do not mount `~/.jkb`; what bounds
//! the agent is that [`jkb_api::Request`] contains only pure database work.
//!
//! - [`server`]: `jkb serve` — bind (loopback by default, never an unspecified address), bearer
//!   token rotated per start, `GET /v1/hello`, `POST /v1/op`, long-poll for `mq.poll`, body and
//!   concurrency limits, and a refusal to serve a database migrated past what this build knows.
//! - [`client`]: [`client::RemoteBackend`], the same [`jkb_api::Backend`] the host CLI uses locally.

pub mod client;
pub mod server;
pub mod token;

/// The HTTP protocol version `GET /v1/hello` reports. Bumped only for an incompatible change; ops
/// and fields are otherwise only ever added (see `jkb_api`).
pub const PROTOCOL_VERSION: u32 = 1;

/// The directory under the host's `$HOME` that a client's task writes may have this host's sync write
/// files in (`jkb_api::tasks::FileRoots`): the one host directory the dev container binds, at the
/// same place under its own home. `.container/check-config.sh` reads this line and requires
/// `container.json` to bind `${localEnv:HOME}/<it>` at `/home/vscode/<it>` — renamed on one side
/// alone, every file-backed write from the container would be refused, or admitted for a directory
/// the container does not see.
pub const CLIENT_FILE_ROOT: &str = "repos";

/// The default address `jkb serve` binds and a client assumes.
pub const DEFAULT_ADDR: &str = "127.0.0.1:7117";

/// The largest request body `jkb serve` accepts ([`server::ServeConfig::max_body_bytes`]'s default, which
/// the unit does not change). A client checks a large request against it before sending: the daemon
/// refuses a body past it by closing the connection mid-upload, and a body many times the cap then
/// reached the client as a failed send — "cannot reach jkb serve" — rather than the refusal.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
