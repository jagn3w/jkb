//! The tool logic, as plain synchronous functions over a [`Backend`] (tasks S6.4, design-s6-4.md K).
//!
//! **Every tool is an operation.** On the host the backend is a `LocalBackend` over the database (with
//! the embedder, so `search` defaults to hybrid); in the dev container it is `jkb serve`'s, which embeds
//! nothing, so `search` defaults to FTS there. A file or URL to ingest is read where the server runs
//! and only its text is sent, as `jkb ingest` does. Keeping the work here (rather than in the async
//! `#[tool]` methods) keeps it testable without an MCP transport; the server is a thin adapter.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use jkb_api::kb::{ItemRow, QueryOrder, SearchRoute};
use jkb_api::{Backend, Request, Response};

use crate::error::{Error, Result};

/// What the tools are served by.
#[derive(Clone)]
pub struct Tools {
    /// The backend. What it can do — embed, take a source's bytes — it says itself.
    pub backend: Arc<dyn Backend + Send + Sync>,
}

impl Tools {
    fn call(&self, request: Request) -> Result<Response> {
        Ok(self.backend.call(request)?)
    }
}

/// A tool's answer: its JSON, and whether it was cut short — which the server tells the agent, so a
/// partial list is never read as every match.
#[derive(Debug)]
pub struct Answer {
    /// The JSON.
    pub value: Value,
    /// Cut short at the backend's read budget.
    pub truncated: bool,
}

impl From<Value> for Answer {
    fn from(value: Value) -> Self {
        Self {
            value,
            truncated: false,
        }
    }
}

/// What the server says of a cut answer.
pub const TRUNCATED_NOTE: &str =
    "this answer was cut short at the server's read budget: it is not every match — narrow the \
     query or pass a smaller limit";

fn unexpected(op: &str, answer: &Response) -> Error {
    Error::Unexpected(format!("{op} answered {answer:?}"))
}

/// `search` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Query DSL; `~"…"` is the vector term, bare words are FTS, plus structural
    /// predicates like `kind:`, `ns:…/**`, `tag:…`.
    pub query: String,
    /// Route: `vector`, `fts`, or `hybrid`. Defaults to `hybrid` where the server embeds; a server in
    /// the dev container embeds nothing, serves only `fts`, and defaults to it.
    #[serde(default)]
    pub route: Option<String>,
    /// Maximum hits (default 10).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `get_context` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetContextArgs {
    /// The item id to expand (typically a chunk hit from `search`).
    pub item_id: i64,
    /// Number of neighbour chunks on each side (default 2).
    #[serde(default)]
    pub n: Option<usize>,
}

/// `query` / `task_next` arguments (a DSL string; `task_next` uses only its
/// scope/tag parts).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct QueryArgs {
    /// Query DSL (may be empty for `task_next` = the whole ready frontier).
    #[serde(default)]
    pub query: String,
    /// Maximum results.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `run_view` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunViewArgs {
    /// The saved view name.
    pub name: String,
}

/// `ingest_path` / `ingest_url` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct IngestArgs {
    /// The file path (or URL, for `ingest_url`).
    pub source: String,
    /// Namespace to place the document under (default `inbox`).
    #[serde(default)]
    pub namespace: Option<String>,
}

/// `task_create` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskCreateArgs {
    /// The task title.
    pub title: String,
    /// Optional priority (lower is more important).
    #[serde(default)]
    pub priority: Option<i64>,
    /// Optional ISO due date.
    #[serde(default)]
    pub due: Option<String>,
    /// Optional namespace to also place the task under (a `repos/…` mirror).
    #[serde(default)]
    pub namespace: Option<String>,
}

/// `task_update` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskUpdateArgs {
    /// The task's stable uid.
    pub uid: String,
    /// New status (`open`/`in_progress`/`needs_review`/`done`/`cancelled`; `blocked` is
    /// rejected).
    #[serde(default)]
    pub status: Option<String>,
    /// New priority (use `null`/omit to leave unchanged).
    #[serde(default)]
    pub priority: Option<i64>,
    /// New due date.
    #[serde(default)]
    pub due: Option<String>,
}

/// Run a search and return ranked hits with provenance. The route defaults to hybrid where the
/// backend embeds and to FTS through `jkb serve`, which does not.
///
/// # Errors
/// Returns an error if the query is malformed, the route is unknown or unsupported here, or a read
/// fails.
pub fn search(tools: &Tools, args: &SearchArgs) -> Result<Answer> {
    let route = match args.route.as_deref() {
        None if !tools.backend.embeds() => SearchRoute::Fts,
        None | Some("hybrid") => SearchRoute::Hybrid,
        Some("vector") => SearchRoute::Vector,
        Some("fts") => SearchRoute::Fts,
        Some(other) => {
            return Err(Error::Types(jkb_types::Error::Validation(format!(
                "unknown route `{other}`; use vector, fts, or hybrid"
            ))))
        }
    };
    let (hits, truncated) = match tools.call(Request::KbSearch {
        dsl: args.query.clone(),
        default_scope: None,
        route,
        limit: args.limit.unwrap_or(10),
        context: None,
    })? {
        Response::SearchHits { hits, truncated } => (hits, truncated),
        other => return Err(unexpected("kb.search", &other)),
    };
    let value = Value::Array(
        hits.into_iter()
            .map(|h| {
                json!({
                    "item": h.item,
                    "route": h.route,
                    "score": h.score,
                    "distance": h.distance,
                    "namespace": h.namespace,
                    "source_document": h.source_document.map(|d| d.id),
                })
            })
            .collect(),
    );
    Ok(Answer { value, truncated })
}

/// Expand an item into its neighbour-chunk context.
///
/// # Errors
/// Returns an error if the read fails.
pub fn get_context(tools: &Tools, args: &GetContextArgs) -> Result<Answer> {
    let (chunks, truncated) = match tools.call(Request::KbContext {
        item: args.item_id,
        n: args.n.unwrap_or(2),
    })? {
        Response::Context { chunks, truncated } => (chunks, truncated),
        other => return Err(unexpected("kb.context", &other)),
    };
    let value = Value::Array(
        chunks
            .into_iter()
            .map(|c| {
                json!({
                    "item": c.item,
                    "position": c.position,
                    "is_hit": c.is_hit,
                    "content": c.content,
                })
            })
            .collect(),
    );
    Ok(Answer { value, truncated })
}

fn items(tools: &Tools, op: &str, request: Request) -> Result<Answer> {
    match tools.call(request)? {
        Response::Items { items, truncated } => Ok(Answer {
            value: items_json(&items),
            truncated,
        }),
        other => Err(unexpected(op, &other)),
    }
}

/// Evaluate a structured query and return the matching items.
///
/// # Errors
/// Returns an error if the query is malformed or a read fails.
pub fn query(tools: &Tools, args: &QueryArgs) -> Result<Answer> {
    items(
        tools,
        "kb.query",
        Request::KbQuery {
            dsl: args.query.clone(),
            default_scope: None,
            limit: args.limit,
            count: false,
            order: QueryOrder::default(),
        },
    )
}

/// List saved views.
///
/// # Errors
/// Returns an error if the read fails.
pub fn list_views(tools: &Tools) -> Result<Answer> {
    match tools.call(Request::ViewList {})? {
        Response::Views { views, truncated } => Ok(Answer {
            value: Value::Array(
                views
                    .into_iter()
                    .map(|v| json!({ "name": v.name, "query": v.query }))
                    .collect(),
            ),
            truncated,
        }),
        other => Err(unexpected("view.list", &other)),
    }
}

/// Run a saved view.
///
/// # Errors
/// Returns an error if the view is missing or a read fails.
pub fn run_view(tools: &Tools, args: &RunViewArgs) -> Result<Answer> {
    items(
        tools,
        "view.run",
        Request::ViewRun {
            name: args.name.clone(),
            limit: None,
        },
    )
}

/// The ready-frontier tasks, optionally scoped/tag-filtered by the DSL in `query`.
///
/// # Errors
/// Returns an error if the DSL is malformed or a read fails.
pub fn task_next(tools: &Tools, args: &QueryArgs) -> Result<Answer> {
    items(
        tools,
        "task.ready",
        Request::TaskReady {
            dsl: args.query.clone(),
            default_scope: None,
            limit: args.limit,
        },
    )
}

/// Ingest a local file or a URL: read and parse it here — a URL rendered in a headless browser — and
/// send its text (design D18, tasks S6.3).
///
/// # Errors
/// Returns an error if the namespace is malformed, the source can't be read or rendered, or capture
/// fails.
pub fn ingest(tools: &Tools, args: &IngestArgs) -> Result<Answer> {
    let namespace = args.namespace.clone().unwrap_or_else(|| "inbox".to_owned());
    let ask = jkb_api::ingest::IngestAsk::for_source(&args.source, namespace, &*tools.backend)?;
    let ingested = match tools.call(Request::IngestText(ask))? {
        Response::Ingested { ingested } => ingested,
        other => return Err(unexpected("ingest.text", &other)),
    };
    Ok(json!({
        "document": ingested.document,
        "chunk_count": ingested.chunk_count,
        "embedded": ingested.embedded,
        "already_ingested": ingested.already_ingested,
        "warnings": ingested.warnings,
    })
    .into())
}

/// Create a task (audited, undoable): the title taken word for word, homed in the default inbox and
/// placed under `namespace` too when given.
///
/// # Errors
/// Returns a validation error for a malformed namespace or due date, or a failed write.
pub fn task_create(tools: &Tools, args: &TaskCreateArgs) -> Result<Answer> {
    match tools.call(Request::TaskAdd(jkb_api::tasks::AddAsk {
        text: args.title.clone(),
        literal: true,
        priority: args.priority,
        due: args.due.clone(),
        also: args.namespace.clone(),
        managed: true,
        ..jkb_api::tasks::AddAsk::default()
    }))? {
        Response::Added { added } => Ok(json!({ "id": added.id, "uid": added.uid }).into()),
        other => Err(unexpected("task.add", &other)),
    }
}

/// Update a task's status/priority/due; with none given, only its id is read back.
///
/// # Errors
/// Returns a not-found error if the uid is unknown, a validation error for an illegal status (e.g.
/// `blocked`), or a failed write.
pub fn task_update(tools: &Tools, args: &TaskUpdateArgs) -> Result<Answer> {
    let changes = args.status.is_some() || args.priority.is_some() || args.due.is_some();
    if changes {
        match tools.call(Request::TaskSet {
            uid: args.uid.clone(),
            status: args.status.clone(),
            priority: args.priority,
            due: args.due.clone(),
        })? {
            Response::Applied {} => {}
            other => return Err(unexpected("task.set", &other)),
        }
    }
    // The id, read back. The update has been applied by now, so a failure to read it is not reported
    // as a failed update: the uid alone is answered, with the reason.
    match tools.call(Request::TaskShow {
        uid: args.uid.clone(),
    }) {
        Ok(Response::Task { task, .. }) => {
            Ok(json!({ "id": task.item.id, "uid": task.item.uid }).into())
        }
        Ok(other) => Err(unexpected("task.show", &other)),
        Err(e) if changes => {
            Ok(json!({ "id": null, "uid": args.uid, "note": e.to_string() }).into())
        }
        Err(e) => Err(e),
    }
}

// ---- helpers --------------------------------------------------------------

fn items_json(rows: &[ItemRow]) -> Value {
    Value::Array(
        rows.iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "uid": r.uid,
                    "kind": r.kind,
                    "status": r.status,
                    "priority": r.priority,
                    "due": r.due,
                    "namespace": r.namespace,
                    "snippet": r.snippet,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        get_context, ingest, query, task_create, task_next, task_update, GetContextArgs,
        IngestArgs, QueryArgs, SearchArgs, TaskCreateArgs, TaskUpdateArgs, Tools,
    };
    use std::sync::Arc;

    use jkb_core::Db;
    use jkb_types::{Embedder, ItemId, Result as TypesResult};

    /// Deterministic offline embedder (dim 16), mirroring the ingest/search tests.
    struct FakeEmbedder;
    impl Embedder for FakeEmbedder {
        #[allow(clippy::unnecessary_literal_bound)]
        fn model(&self) -> &str {
            "fake"
        }
        fn dim(&self) -> usize {
            16
        }
        fn embed(&self, text: &str) -> TypesResult<Vec<f32>> {
            let mut v = vec![0.0f32; 16];
            for (i, b) in text.bytes().enumerate() {
                v[i % 16] += f32::from(b);
            }
            Ok(v)
        }
        fn health_check(&self) -> TypesResult<()> {
            Ok(())
        }
    }

    fn embedder() -> Arc<dyn Embedder + Send + Sync> {
        Arc::new(FakeEmbedder)
    }

    fn db() -> Db {
        Db::open_in_memory_with(&[jkb_index::register]).unwrap()
    }

    /// The tools over `db` in this process, with the embedder — as `jkb mcp` runs on the host.
    fn tools(db: &Db) -> Tools {
        Tools {
            backend: Arc::new(jkb_api::LocalBackend::new(db.clone()).with_embedder(embedder())),
        }
    }

    fn count_tasks(db: &Db) -> i64 {
        db.read(|conn| {
            Ok(
                conn.query_row("SELECT count(*) FROM items WHERE kind = 'task'", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .unwrap()
    }

    #[test]
    fn task_create_is_audited_and_undoable() {
        let db = db();
        let args = TaskCreateArgs {
            title: "write the design doc".to_owned(),
            priority: Some(1),
            due: None,
            namespace: Some("repos/app".to_owned()),
        };
        let created = task_create(&tools(&db), &args).unwrap();
        assert!(created.value.get("uid").is_some());
        assert_eq!(count_tasks(&db), 1);

        // The write is in the changelog, so undo reverts it.
        let reverted = db.write_txn("cli", jkb_core::undo::undo_last).unwrap();
        assert!(reverted > 0);
        assert_eq!(count_tasks(&db), 0);
    }

    #[test]
    fn task_create_rejects_a_bad_namespace() {
        let db = db();
        let args = TaskCreateArgs {
            title: "bad".to_owned(),
            priority: None,
            due: None,
            namespace: Some("repos/../secret".to_owned()),
        };
        let err = task_create(&tools(&db), &args).unwrap_err();
        assert!(err.is_user_error());
        assert_eq!(count_tasks(&db), 0); // no partial state
    }

    #[test]
    fn task_update_rejects_blocked_and_sets_done() {
        let db = db();
        task_create(
            &tools(&db),
            &TaskCreateArgs {
                title: "ship it".to_owned(),
                priority: None,
                due: None,
                namespace: None,
            },
        )
        .unwrap();
        let uid = db
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT uid FROM items WHERE kind = 'task' LIMIT 1",
                    [],
                    |r| r.get::<_, String>(0),
                )?)
            })
            .unwrap();

        // `blocked` is derived, not settable.
        let blocked = task_update(
            &tools(&db),
            &TaskUpdateArgs {
                uid: uid.clone(),
                status: Some("blocked".to_owned()),
                priority: None,
                due: None,
            },
        );
        assert!(blocked.is_err());

        // A real status is accepted.
        task_update(
            &tools(&db),
            &TaskUpdateArgs {
                uid: uid.clone(),
                status: Some("done".to_owned()),
                priority: Some(2),
                due: None,
            },
        )
        .unwrap();
        let status: String = db
            .read(move |conn| {
                Ok(
                    conn.query_row("SELECT status FROM items WHERE uid = ?1", [uid], |r| {
                        r.get(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(status, "done");
    }

    #[test]
    fn query_and_task_next_list_tasks() {
        let db = db();
        for title in ["alpha task", "beta task"] {
            task_create(
                &tools(&db),
                &TaskCreateArgs {
                    title: title.to_owned(),
                    priority: None,
                    due: None,
                    namespace: None,
                },
            )
            .unwrap();
        }
        let listed = query(
            &tools(&db),
            &QueryArgs {
                query: "kind:task".to_owned(),
                limit: None,
            },
        )
        .unwrap();
        assert_eq!(listed.value.as_array().unwrap().len(), 2);

        let ready = task_next(
            &tools(&db),
            &QueryArgs {
                query: String::new(),
                limit: None,
            },
        )
        .unwrap();
        assert_eq!(ready.value.as_array().unwrap().len(), 2);
    }

    #[test]
    fn agent_flow_ingest_search_then_context() {
        let db = db();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.md");
        std::fs::write(
            &file,
            "Alpha section. A peculiar distinctive keyword lives here. Omega section.",
        )
        .unwrap();

        let ingested = ingest(
            &tools(&db),
            &IngestArgs {
                source: file.to_string_lossy().into_owned(),
                namespace: Some("docs".to_owned()),
            },
        )
        .unwrap();
        assert_eq!(ingested.value["embedded"], true);

        // search → a hit, then get_context on it.
        let hits = super::search(
            &tools(&db),
            &SearchArgs {
                query: "peculiar".to_owned(),
                route: Some("fts".to_owned()),
                limit: Some(5),
            },
        )
        .unwrap();
        let hits = hits.value.as_array().unwrap();
        assert!(!hits.is_empty());
        let item_id = hits[0]["item"].as_i64().unwrap();

        let context = get_context(
            &tools(&db),
            &GetContextArgs {
                item_id,
                n: Some(1),
            },
        )
        .unwrap();
        let ctx = context.value.as_array().unwrap();
        assert!(ctx.iter().any(|c| c["is_hit"] == true));
        // The hit's item id resolves in the returned context.
        assert!(
            ctx.iter().any(|c| c["item"].as_i64() == Some(item_id))
                || ItemId::new(item_id).get() == item_id
        );
    }

    /// Saved views are listed and run through the ops, and an answer cut at the read budget says so.
    #[test]
    fn views_are_served_and_a_cut_answer_is_marked() {
        let db = db();
        db.write_txn("t", |c, m| {
            jkb_core::view::save(c, m, "all-tasks", "kind:task")
        })
        .unwrap();
        task_create(
            &tools(&db),
            &TaskCreateArgs {
                title: "one".to_owned(),
                priority: None,
                due: None,
                namespace: None,
            },
        )
        .unwrap();
        let views = super::list_views(&tools(&db)).unwrap();
        assert_eq!(views.value.as_array().unwrap().len(), 1);
        let run = super::run_view(
            &tools(&db),
            &super::RunViewArgs {
                name: "all-tasks".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(run.value.as_array().unwrap().len(), 1);
        assert!(!run.truncated);
        let missing = super::run_view(
            &tools(&db),
            &super::RunViewArgs {
                name: "nope".to_owned(),
            },
        )
        .unwrap_err();
        assert!(missing.is_user_error(), "{missing}");

        let tight = Tools {
            backend: Arc::new(jkb_api::LocalBackend::new(db).with_read_budget(1)),
        };
        let cut = query(
            &tight,
            &QueryArgs {
                query: "kind:task".to_owned(),
                limit: None,
            },
        )
        .unwrap();
        assert!(cut.truncated);
        // And no embedder means the default route is the one that needs none.
        let hits = super::search(
            &tight,
            &SearchArgs {
                query: "one".to_owned(),
                route: None,
                limit: Some(1),
            },
        );
        assert!(hits.is_ok(), "{hits:?}");
    }
}
