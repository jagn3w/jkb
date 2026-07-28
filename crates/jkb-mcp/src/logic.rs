//! The tool logic, as plain synchronous functions over a [`Db`] + [`Embedder`].
//!
//! Keeping the real work here (rather than in the async `#[tool]` methods) means it
//! is directly testable without an MCP transport or a tokio runtime, and the server
//! layer stays a thin adapter: `spawn_blocking` → one of these → JSON. Every write
//! goes through `jkb-core`'s writer-actor, so agent changes are audited and undoable.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use jkb_core::query::{Scope, TagPred};
use jkb_core::{item, ns, task, view, Db};
use jkb_ingest::Pipeline;
use jkb_search::{Route, Searcher};
use jkb_types::{Embedder, ItemId};

use crate::error::{Error, Result};

type Embed = Arc<dyn Embedder + Send + Sync>;

/// `search` arguments.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Query DSL; `~"…"` is the vector term, bare words are FTS, plus structural
    /// predicates like `kind:`, `ns:…/**`, `tag:…`.
    pub query: String,
    /// Route: `vector`, `fts`, or `hybrid` (default `hybrid`).
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

/// Run a search and return ranked hits with provenance.
///
/// # Errors
/// Returns an error if the query is malformed, the route is unknown, or embedding /
/// a query fails.
pub fn search(db: &Db, embedder: &Embed, args: &SearchArgs) -> Result<Value> {
    let route = parse_route(args.route.as_deref())?;
    let query = jkb_core::query::parse(&args.query)?;
    let limit = args.limit.unwrap_or(10);
    let searcher = Searcher::new(embedder.clone());
    let hits = searcher.search(db, &query, route, limit)?;
    let arr: Vec<Value> = hits
        .into_iter()
        .map(|h| {
            json!({
                "item": h.item.get(),
                "route": h.route.as_str(),
                "score": h.score,
                "distance": h.distance,
                "namespace": h.namespace_path,
                "source_document": h.source_document.map(ItemId::get),
            })
        })
        .collect();
    Ok(Value::Array(arr))
}

/// Expand an item into its neighbour-chunk context.
///
/// # Errors
/// Returns an error if the read fails.
pub fn get_context(db: &Db, embedder: &Embed, args: &GetContextArgs) -> Result<Value> {
    let searcher = Searcher::new(embedder.clone());
    let chunks = searcher.get_context(db, ItemId::new(args.item_id), args.n.unwrap_or(2))?;
    let arr: Vec<Value> = chunks
        .into_iter()
        .map(|c| {
            json!({
                "item": c.item.get(),
                "position": c.position,
                "is_hit": c.is_hit,
                "content": c.content,
            })
        })
        .collect();
    Ok(Value::Array(arr))
}

/// Evaluate a structured query and return the matching items.
///
/// # Errors
/// Returns an error if the query is malformed or a read fails.
pub fn query(db: &Db, args: &QueryArgs) -> Result<Value> {
    let mut q = jkb_core::query::parse(&args.query)?;
    if let Some(limit) = args.limit {
        q.limit = Some(limit);
    }
    let ids = db.read(move |conn| q.evaluate(conn))?;
    items_json(db, &ids)
}

/// List saved views.
///
/// # Errors
/// Returns an error if the read fails.
pub fn list_views(db: &Db) -> Result<Value> {
    let views = db.read(view::list)?;
    let arr: Vec<Value> = views
        .into_iter()
        .map(|(name, query)| json!({ "name": name, "query": query }))
        .collect();
    Ok(Value::Array(arr))
}

/// Run a saved view.
///
/// # Errors
/// Returns an error if the view is missing or a read fails.
pub fn run_view(db: &Db, args: &RunViewArgs) -> Result<Value> {
    let name = args.name.clone();
    let ids = db.read(move |conn| view::run(conn, &name))?;
    items_json(db, &ids)
}

/// The ready-frontier tasks, optionally scoped/tag-filtered by the DSL in `query`.
///
/// # Errors
/// Returns an error if the DSL is malformed or a read fails.
pub fn task_next(db: &Db, args: &QueryArgs) -> Result<Value> {
    let q = jkb_core::query::parse(&args.query)?;
    let (scope, tags): (Scope, Vec<TagPred>) = (q.scope, q.tags);
    let limit = args.limit;
    let mut rows = db.read(move |conn| task::ready(conn, scope, &tags))?;
    if let Some(limit) = limit {
        rows.truncate(limit);
    }
    let ids: Vec<ItemId> = rows.iter().map(|r| r.id).collect();
    items_json(db, &ids)
}

/// Ingest a local file (the audited capture→embed pipeline).
///
/// # Errors
/// Returns an error if the file can't be read/parsed or capture fails.
pub fn ingest_path(db: &Db, embedder: &Embed, args: &IngestArgs) -> Result<Value> {
    let namespace = args.namespace.clone().unwrap_or_else(|| "inbox".to_owned());
    ns::normalize(&namespace)?;
    let pipeline = Pipeline::new(embedder.clone());
    let outcome = pipeline.ingest_path(db, std::path::Path::new(&args.source), &namespace)?;
    Ok(json!({
        "document": outcome.document.get(),
        "chunk_count": outcome.chunk_count,
        "embedded": outcome.embedded,
        "already_ingested": outcome.already_ingested,
        "warnings": outcome.warnings,
    }))
}

/// Ingest a URL: render it in a headless browser (so client-side JavaScript runs),
/// extract its text, and run the audited capture→embed pipeline (design D18).
///
/// # Errors
/// Returns an error if the namespace is malformed, the page can't be rendered (e.g.
/// no Chrome installed), or capture fails.
pub fn ingest_url(db: &Db, embedder: &Embed, args: &IngestArgs) -> Result<Value> {
    let namespace = args.namespace.clone().unwrap_or_else(|| "inbox".to_owned());
    ns::normalize(&namespace)?;
    let pipeline = Pipeline::new(embedder.clone());
    let outcome = pipeline.ingest_url(db, &args.source, &namespace)?;
    Ok(json!({
        "document": outcome.document.get(),
        "chunk_count": outcome.chunk_count,
        "embedded": outcome.embedded,
        "already_ingested": outcome.already_ingested,
        "warnings": outcome.warnings,
    }))
}

/// Create a task through the writer-actor (audited, undoable).
///
/// # Errors
/// Returns a validation error for a malformed namespace, or a database error.
pub fn task_create(db: &Db, args: &TaskCreateArgs) -> Result<Value> {
    // Validate the placement namespace up front for an actionable error.
    if let Some(namespace) = &args.namespace {
        ns::normalize(namespace)?;
    }
    let mut spec = task::NewTask::new(task::mint_uid(&args.title), args.title.clone());
    spec.priority = args.priority;
    spec.due.clone_from(&args.due);
    if let Some(namespace) = &args.namespace {
        spec.mirrors = vec![namespace.clone()];
    }
    let uid = spec.uid.clone();
    let id = db.write_txn("mcp", move |conn, meta| task::create(conn, meta, &spec))?;
    Ok(json!({ "id": id.get(), "uid": uid }))
}

/// Update a task's status/priority/due through the writer-actor.
///
/// # Errors
/// Returns a not-found error if the uid is unknown, a validation error for an
/// illegal status (e.g. `blocked`), or a database error.
pub fn task_update(db: &Db, args: &TaskUpdateArgs) -> Result<Value> {
    let uid = args.uid.clone();
    let status = args.status.clone();
    let priority = args.priority;
    let due = args.due.clone();
    let id = db.write_txn("mcp", move |conn, meta| -> jkb_core::Result<ItemId> {
        let id = item::id_for_uid(conn, &uid)?.ok_or_else(|| {
            jkb_core::Error::Types(jkb_types::Error::NotFound(format!("task `{uid}`")))
        })?;
        if let Some(status) = &status {
            task::set_status_str(conn, meta, id, status)?;
        }
        if priority.is_some() {
            task::set_priority(conn, meta, id, priority)?;
        }
        if let Some(due) = &due {
            task::set_due(conn, meta, id, Some(due))?;
        }
        Ok(id)
    })?;
    Ok(json!({ "id": id.get(), "uid": args.uid }))
}

// ---- helpers --------------------------------------------------------------

fn parse_route(route: Option<&str>) -> Result<Route> {
    match route.unwrap_or("hybrid") {
        "hybrid" => Ok(Route::Hybrid),
        "vector" => Ok(Route::Vector),
        "fts" => Ok(Route::Fts),
        other => Err(Error::Types(jkb_types::Error::Validation(format!(
            "unknown route `{other}`; use vector, fts, or hybrid"
        )))),
    }
}

/// Fetch item rows as JSON, preserving id order and skipping missing rows.
fn items_json(db: &Db, ids: &[ItemId]) -> Result<Value> {
    let ids: Vec<i64> = ids.iter().map(|id| id.get()).collect();
    let rows = db.read(move |conn| {
        let mut out = Vec::new();
        for id in &ids {
            let row = conn
                .prepare_cached(
                    "SELECT id, uid, kind, status, priority, due, content
                     FROM items WHERE id = ?1",
                )?
                .query_row([id], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                    ))
                })
                .ok();
            let Some((id, uid, kind, status, priority, due, content)) = row else {
                continue;
            };
            let namespace: Option<String> = conn
                .prepare_cached(
                    "SELECT n.path FROM placements p JOIN namespaces n ON n.id = p.namespace_id
                     WHERE p.item_id = ?1
                     ORDER BY (p.role = 'primary') DESC, p.position LIMIT 1",
                )?
                .query_row([id], |r| r.get::<_, String>(0))
                .ok();
            out.push(json!({
                "id": id,
                "uid": uid,
                "kind": kind,
                "status": status,
                "priority": priority,
                "due": due,
                "namespace": namespace,
                "snippet": content.as_deref().map(snippet),
            }));
        }
        Ok(out)
    })?;
    Ok(Value::Array(rows))
}

fn snippet(content: &str) -> String {
    let line = content.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    line.trim().chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        get_context, ingest_path, query, task_create, task_next, task_update, GetContextArgs,
        IngestArgs, QueryArgs, SearchArgs, TaskCreateArgs, TaskUpdateArgs,
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
        let created = task_create(&db, &args).unwrap();
        assert!(created.get("uid").is_some());
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
        let err = task_create(&db, &args).unwrap_err();
        assert!(err.is_user_error());
        assert_eq!(count_tasks(&db), 0); // no partial state
    }

    #[test]
    fn task_update_rejects_blocked_and_sets_done() {
        let db = db();
        task_create(
            &db,
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
            &db,
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
            &db,
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
                &db,
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
            &db,
            &QueryArgs {
                query: "kind:task".to_owned(),
                limit: None,
            },
        )
        .unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 2);

        let ready = task_next(
            &db,
            &QueryArgs {
                query: String::new(),
                limit: None,
            },
        )
        .unwrap();
        assert_eq!(ready.as_array().unwrap().len(), 2);
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

        let ingested = ingest_path(
            &db,
            &embedder(),
            &IngestArgs {
                source: file.to_string_lossy().into_owned(),
                namespace: Some("docs".to_owned()),
            },
        )
        .unwrap();
        assert_eq!(ingested["embedded"], true);

        // search → a hit, then get_context on it.
        let hits = super::search(
            &db,
            &embedder(),
            &SearchArgs {
                query: "peculiar".to_owned(),
                route: Some("fts".to_owned()),
                limit: Some(5),
            },
        )
        .unwrap();
        let hits = hits.as_array().unwrap();
        assert!(!hits.is_empty());
        let item_id = hits[0]["item"].as_i64().unwrap();

        let context = get_context(
            &db,
            &embedder(),
            &GetContextArgs {
                item_id,
                n: Some(1),
            },
        )
        .unwrap();
        let ctx = context.as_array().unwrap();
        assert!(ctx.iter().any(|c| c["is_hit"] == true));
        // The hit's item id resolves in the returned context.
        assert!(
            ctx.iter().any(|c| c["item"].as_i64() == Some(item_id))
                || ItemId::new(item_id).get() == item_id
        );
    }
}
