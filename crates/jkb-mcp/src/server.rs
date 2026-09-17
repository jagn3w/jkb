//! The MCP server: a thin async adapter that maps tool calls to [`crate::logic`].
//!
//! Each `#[tool]` runs its (blocking) logic on a `spawn_blocking` worker — the
//! `jkb-core` writer-actor and the ollama embedder both block, and must not stall the
//! async runtime — then wraps the JSON result in a `CallToolResult`. `jkb-core`
//! errors become MCP `ErrorData` (client-input errors → `invalid_params`).

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ErrorData, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};

use crate::error::{Error, Result as LogicResult};
use crate::logic::{
    self, GetContextArgs, IngestArgs, QueryArgs, RunViewArgs, SearchArgs, TaskCreateArgs,
    TaskUpdateArgs, Tools,
};

/// The jkb MCP server: every tool an operation on the backend the CLI chose ([`Tools`]).
///
/// `#[tool_handler]` calls the generated `Self::tool_router()` per request, so the
/// router is not stored on the struct.
pub struct JkbServer {
    tools: Tools,
}

impl JkbServer {
    /// Build a server over `tools`.
    #[must_use]
    pub const fn new(tools: Tools) -> Self {
        Self { tools }
    }

    /// Run blocking logic `f` on a worker thread and wrap its JSON as a tool result.
    async fn run<F>(&self, f: F) -> Result<CallToolResult, ErrorData>
    where
        F: FnOnce(Tools) -> LogicResult<logic::Answer> + Send + 'static,
    {
        let tools = self.tools.clone();
        let out = tokio::task::spawn_blocking(move || f(tools))
            .await
            .map_err(|e| ErrorData::internal_error(format!("worker task failed: {e}"), None))?;
        match out {
            Ok(answer) => {
                let mut content = vec![ContentBlock::json(answer.value)?];
                if answer.truncated {
                    content.push(ContentBlock::text(logic::TRUNCATED_NOTE));
                }
                Ok(CallToolResult::success(content))
            }
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
        description = "Search the knowledge base (routes: vector, fts, hybrid; hybrid by default where the server embeds, and only fts in the dev container). Returns ranked items with namespace path and source document for citation."
    )]
    async fn search(&self, params: Parameters<SearchArgs>) -> Result<CallToolResult, ErrorData> {
        self.run(move |t| logic::search(&t, &params.0)).await
    }

    /// Expand a hit into its neighbouring chunks.
    #[tool(
        description = "Return the +/- N neighbour chunks around an item (for citing surrounding context). No re-embedding."
    )]
    async fn get_context(
        &self,
        params: Parameters<GetContextArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(move |t| logic::get_context(&t, &params.0)).await
    }

    /// Structured query over the item substrate.
    #[tool(
        description = "Run a structured query (DSL: kind:, status:, ns:.../**, tag:, is:ready, ...) and return matching items."
    )]
    async fn query(&self, params: Parameters<QueryArgs>) -> Result<CallToolResult, ErrorData> {
        self.run(move |t| logic::query(&t, &params.0)).await
    }

    /// List saved views.
    #[tool(description = "List saved views (named queries).")]
    async fn list_views(&self) -> Result<CallToolResult, ErrorData> {
        self.run(move |t| logic::list_views(&t)).await
    }

    /// Run a saved view.
    #[tool(description = "Run a saved view by name and return its items.")]
    async fn run_view(&self, params: Parameters<RunViewArgs>) -> Result<CallToolResult, ErrorData> {
        self.run(move |t| logic::run_view(&t, &params.0)).await
    }

    /// The ready-frontier tasks.
    #[tool(
        description = "List the ready task frontier (unblocked, non-terminal), ordered by priority then due. Optional DSL scope/tags."
    )]
    async fn task_next(&self, params: Parameters<QueryArgs>) -> Result<CallToolResult, ErrorData> {
        self.run(move |t| logic::task_next(&t, &params.0)).await
    }

    /// Ingest a local file (audited).
    #[tool(
        description = "Ingest a local file into the KB (captured, and embedded where the server embeds, via the audited pipeline)."
    )]
    async fn ingest_path(
        &self,
        params: Parameters<IngestArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(move |t| logic::ingest(&t, &params.0)).await
    }

    /// Ingest a URL (rendered via a headless browser).
    #[tool(
        description = "Ingest a URL into the KB. The page is rendered in a headless browser (JavaScript runs) before its text is captured + embedded via the audited pipeline."
    )]
    async fn ingest_url(
        &self,
        params: Parameters<IngestArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(move |t| logic::ingest(&t, &params.0)).await
    }

    /// Create a task (audited, undoable).
    #[tool(
        description = "Create a task (title, optional priority/due/namespace). Written via the audited writer-actor, so `jkb undo` reverts it."
    )]
    async fn task_create(
        &self,
        params: Parameters<TaskCreateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(move |t| logic::task_create(&t, &params.0)).await
    }

    /// Update a task's status/priority/due (audited).
    #[tool(
        description = "Update a task by uid: status (open/in_progress/needs_review/done/cancelled; blocked is rejected), priority, or due."
    )]
    async fn task_update(
        &self,
        params: Parameters<TaskUpdateArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        self.run(move |t| logic::task_update(&t, &params.0)).await
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
