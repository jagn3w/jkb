//! MCP server exposing the knowledge base to agents such as Claude (design D17,
//! Section 13).
//!
//! An `rmcp` stdio server ([`JkbServer`]) advertising read tools (`search`,
//! `get_context`, `query`, `list_views`, `run_view`, `task_next`) and write tools
//! (`ingest_path`, `ingest_url`, `task_create`, `task_update`). It shares the CLI's
//! [`Db`], writer-actor, and changelog, so every agent write is audited and reverted
//! by `jkb undo`. The tool bodies live in [`logic`] as plain synchronous functions
//! (directly testable); [`server`] is the thin async/`spawn_blocking` adapter.

mod error;
mod server;

pub mod logic;

pub use error::{Error, Result};
pub use server::JkbServer;

use std::sync::Arc;

use rmcp::transport::stdio;
use rmcp::ServiceExt;

use jkb_core::Db;
use jkb_types::Embedder;

/// Run the MCP server over stdio until the client disconnects. Builds a small tokio
/// runtime internally so the (synchronous) CLI can call it directly.
///
/// # Errors
/// Returns an error if the runtime cannot start, the service fails to initialize, or
/// the transport errors.
pub fn run_stdio(db: Db, embedder: Arc<dyn Embedder + Send + Sync>) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let service = JkbServer::new(db, embedder).serve(stdio()).await?;
        service.waiting().await?;
        Ok(())
    })
}
