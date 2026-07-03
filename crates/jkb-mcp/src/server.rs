//! The MCP server: a thin async adapter that maps tool calls to [`crate::logic`].
//!
//! Each `#[tool]` runs its (blocking) logic on a `spawn_blocking` worker — the
//! `jkb-core` writer-actor and the ollama embedder both block, and must not stall the
//! async runtime — then wraps the JSON result in a `CallToolResult`. `jkb-core`
//! errors become MCP `ErrorData` (client-input errors → `invalid_params`).

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ErrorData, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use serde_json::Value;

use jkb_core::Db;
use jkb_types::Embedder;

use crate::error::{Error, Result as LogicResult};
use crate::logic::{
    self, GetContextArgs, IngestArgs, QueryArgs, RunViewArgs, SearchArgs, TaskCreateArgs,
    TaskUpdateArgs,
};

type Embed = Arc<dyn Embedder + Send + Sync>;

/// The jkb MCP server: shares the CLI's [`Db`] and embedder.
///
/// `#[tool_handler]` calls the generated `Self::tool_router()` per request, so the
/// router is not stored on the struct.
pub struct JkbServer {
    db: Db,
    embedder: Embed,
}

impl JkbServer {
    /// Build a server over `db` and `embedder`.
    #[must_use]
    pub fn new(db: Db, embedder: Embed) -> Self {
        Self { db, embedder }
    }

    /// Run blocking logic `f` on a worker thread and wrap its JSON as a tool result.
    async fn run<F>(&self, f: F) -> Result<CallToolResult, ErrorData>
    where
        F: FnOnce(Db, Embed) -> LogicResult<Value> + Send + 'static,
    {
        let db = self.db.clone();
        let embedder = self.embedder.clone();
        let out = tokio::task::spawn_blocking(move || f(db, embedder))
            .await
            .map_err(|e| ErrorData::internal_error(format!("worker task failed: {e}"), None))?;
        match out {
            Ok(value) => Ok(CallToolResult::success(vec![ContentBlock::json(value)?])),
            Err(err) => Err(to_error_data(&err)),
        }
    }
}

/// Map a logic error to MCP error data: client-input errors are `invalid_params`,
/// everything else `internal_error`.
fn to_error_data(err: &Error) -> ErrorData {
    if err.is_user_error() {
        ErrorData::invalid_params(err.to_string(), None)
    } else {
        ErrorData::internal_error(err.to_string(), None)
    }
}

#[tool_router]
impl JkbServer {
    /// Search the knowledge base.
    #[tool(
        description = "Search the knowledge base (routes: vector, fts, hybrid). Returns ranked items with namespace path and source document for citation."
    )]
    async fn search(&self, params: Parameters<SearchArgs>) -> Result<CallToolResult, ErrorData> {
        self.run(move |db, embedder| logic::search(&db, &embedder, &params.0))
            .await
    }

    /// Expand a hit into its neighbouring chunks.
    #[tool(
        description = "Return the +/- N neighbour chunks around an item (for citing surrounding context). No re-embedding."
    )]
    async fn get_context(
        &self,
        params: Parameters<GetContextArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(move |db, embedder| logic::get_context(&db, &embedder, &params.0))
            .await
    }

    /// Structured query over the item substrate.
    #[tool(
        description = "Run a structured query (DSL: kind:, status:, ns:.../**, tag:, is:ready, ...) and return matching items."
    )]
    async fn query(&self, params: Parameters<QueryArgs>) -> Result<CallToolResult, ErrorData> {
        self.run(move |db, _| logic::query(&db, &params.0)).await
    }

    /// List saved views.
    #[tool(description = "List saved views (named queries).")]
    async fn list_views(&self) -> Result<CallToolResult, ErrorData> {
        self.run(move |db, _| logic::list_views(&db)).await
    }

    /// Run a saved view.
    #[tool(description = "Run a saved view by name and return its items.")]
    async fn run_view(&self, params: Parameters<RunViewArgs>) -> Result<CallToolResult, ErrorData> {
        self.run(move |db, _| logic::run_view(&db, &params.0)).await
    }

    /// The ready-frontier tasks.
    #[tool(
        description = "List the ready task frontier (unblocked, non-terminal), ordered by priority then due. Optional DSL scope/tags."
    )]
    async fn task_next(&self, params: Parameters<QueryArgs>) -> Result<CallToolResult, ErrorData> {
        self.run(move |db, _| logic::task_next(&db, &params.0))
            .await
    }

    /// Ingest a local file (audited).
    #[tool(
        description = "Ingest a local file into the KB (captured + embedded via the audited pipeline)."
    )]
    async fn ingest_path(
        &self,
        params: Parameters<IngestArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(move |db, embedder| logic::ingest_path(&db, &embedder, &params.0))
            .await
    }

    /// Ingest a URL (rendered via a headless browser).
    #[tool(
        description = "Ingest a URL into the KB. The page is rendered in a headless browser (JavaScript runs) before its text is captured + embedded via the audited pipeline."
    )]
    async fn ingest_url(
        &self,
        params: Parameters<IngestArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(move |db, embedder| logic::ingest_url(&db, &embedder, &params.0))
            .await
    }

    /// Create a task (audited, undoable).
    #[tool(
        description = "Create a task (title, optional priority/due/namespace). Written via the audited writer-actor, so `jkb undo` reverts it."
    )]
    async fn task_create(
        &self,
        params: Parameters<TaskCreateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(move |db, _| logic::task_create(&db, &params.0))
            .await
    }

    /// Update a task's status/priority/due (audited).
    #[tool(
        description = "Update a task by uid: status (open/in_progress/done/cancelled; blocked is rejected), priority, or due."
    )]
    async fn task_update(
        &self,
        params: Parameters<TaskUpdateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(move |db, _| logic::task_update(&db, &params.0))
            .await
    }
}

#[tool_handler]
impl ServerHandler for JkbServer {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` is `#[non_exhaustive]`, so mutate a default rather than a literal.
        let mut info = ServerInfo::default();
        info.instructions = Some(
            "jkb knowledge base. Read tools: search, get_context, query, list_views, \
             run_view, task_next. Write tools (audited + undoable via `jkb undo`): \
             ingest_path, ingest_url, task_create, task_update."
                .to_owned(),
        );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

#[cfg(test)]
mod tests {
    use super::JkbServer;

    #[test]
    fn tool_router_advertises_all_tools() {
        let names: Vec<String> = JkbServer::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for expected in [
            "search",
            "get_context",
            "query",
            "list_views",
            "run_view",
            "task_next",
            "ingest_path",
            "ingest_url",
            "task_create",
            "task_update",
        ] {
            assert!(
                names.contains(&expected.to_owned()),
                "missing tool {expected}"
            );
        }
    }
}
