//! End-to-end verification (Section 14) wiring every library crate together over a
//! real migrated DB with `sqlite-vec` registered.
//!
//! A deterministic dim-16 fake embedder stands in for ollama, so the vector/hybrid
//! routes and the ingest→embed pipeline run offline. PDF and URL ingestion are
//! deferred with task 7.3, so this exercises text + Markdown sources.

use std::sync::Arc;

use jkb_core::query::{Query, Scope};
use jkb_core::{binding, item, mount, ns, task, view, Db};
use jkb_ingest::Pipeline;
use jkb_search::{Route, Searcher};
use jkb_sync::Outcome;
use jkb_types::{ConflictPolicy, Embedder, ItemId, Result as TypesResult, SyncMode};

/// Deterministic offline embedder (dim 16), mirroring the search/ingest tests.
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

fn uri_for(path: &std::path::Path) -> String {
    format!("file://{}", path.to_string_lossy())
}

fn content_for(db: &Db, uri: &str) -> Option<String> {
    let uri = uri.to_owned();
    db.read(move |conn| match binding::item_for_uri(conn, &uri)? {
        Some(item) => item::get_content(conn, item),
        None => Ok(None),
    })
    .unwrap()
}

fn db_today(db: &Db) -> String {
    db.read(|conn| Ok(conn.query_row("SELECT date('now', 'localtime')", [], |r| r.get(0))?))
        .unwrap()
}

fn changelog_count(db: &Db) -> i64 {
    db.read(|conn| Ok(conn.query_row("SELECT count(*) FROM changelog", [], |r| r.get(0))?))
        .unwrap()
}

fn count_kind(db: &Db, kind: &str) -> i64 {
    let kind = kind.to_owned();
    db.read(move |conn| {
        Ok(
            conn.query_row("SELECT count(*) FROM items WHERE kind = ?1", [kind], |r| {
                r.get(0)
            })?,
        )
    })
    .unwrap()
}

fn evaluate(db: &Db, dsl: &str) -> Vec<ItemId> {
    let q = jkb_core::query::parse(dsl).unwrap();
    db.read(move |conn| q.evaluate(conn)).unwrap()
}

fn uid_of(db: &Db, id: ItemId) -> String {
    db.read(move |conn| {
        Ok(
            conn.query_row("SELECT uid FROM items WHERE id = ?1", [id.get()], |r| {
                r.get(0)
            })?,
        )
    })
    .unwrap()
}

/// 14.1 — the full flow: mount + bidirectional sync round-trip, ingest, query
/// (open small tasks + due:today), search (vector/fts/hybrid + context), a task DAG
/// whose ready frontier changes when a dependency completes, saved views, and undo.
#[test]
fn end_to_end_full_flow() {
    let db = db();
    let repo = tempfile::tempdir().unwrap();
    let guide = repo.path().join("guide.md");
    std::fs::write(&guide, "# Guide\nfile-backed content").unwrap();

    // --- mount + import ---
    let repo_dir = repo.path().to_string_lossy().into_owned();
    db.write_txn("t", move |conn, meta| {
        let ns_id = ns::ensure(conn, "docs/repo")?;
        mount::create(
            conn,
            meta,
            ns_id,
            &format!("file://{repo_dir}"),
            SyncMode::Bidirectional,
            "document",
            Some("**/*.md"),
            None,
            ConflictPolicy::Manual,
        )
    })
    .unwrap();

    let report = jkb_sync::sync(&db, "docs/repo").unwrap();
    assert_eq!(report.count(Outcome::Created), 1);
    let guide_uri = uri_for(&guide);
    assert_eq!(
        content_for(&db, &guide_uri).as_deref(),
        Some("# Guide\nfile-backed content")
    );

    // --- bidirectional round-trip: disk edit imports, KB edit exports ---
    std::fs::write(&guide, "# Guide\nedited on disk").unwrap();
    assert_eq!(
        jkb_sync::sync(&db, "docs/repo")
            .unwrap()
            .count(Outcome::Imported),
        1
    );
    assert_eq!(
        content_for(&db, &guide_uri).as_deref(),
        Some("# Guide\nedited on disk")
    );

    let item_id = db
        .read({
            let uri = guide_uri.clone();
            move |conn| binding::item_for_uri(conn, &uri)
        })
        .unwrap()
        .unwrap();
    db.write_txn("cli", move |conn, meta| {
        item::set_content(conn, meta, item_id, "# Guide\nedited in kb", None)
    })
    .unwrap();
    assert_eq!(
        jkb_sync::sync(&db, "docs/repo")
            .unwrap()
            .count(Outcome::Exported),
        1
    );
    assert_eq!(
        std::fs::read_to_string(&guide).unwrap(),
        "# Guide\nedited in kb"
    );

    // --- ingest text + markdown (PDF/URL deferred with 7.3) ---
    let src = tempfile::tempdir().unwrap();
    let note = src.path().join("note.txt");
    std::fs::write(
        &note,
        "A distinctive keyword and plenty of surrounding words so the source has real content.",
    )
    .unwrap();
    let md = src.path().join("article.md");
    std::fs::write(&md, "# Article\nAnother distinctive passage lives here.").unwrap();

    let pipeline = Pipeline::new(embedder());
    let out = pipeline.ingest_path(&db, &note, "docs/e2e").unwrap();
    assert!(out.embedded && !out.already_ingested);
    pipeline.ingest_path(&db, &md, "docs/e2e").unwrap();
    assert!(count_kind(&db, "document") >= 3); // guide + note + article

    // --- task DAG with priorities/tags/due ---
    let today = db_today(&db);
    db.write_txn("t", {
        let today = today.clone();
        move |conn, meta| {
            let mut b = task::NewTask::new("task:b", "set up CI");
            b.tags = vec![("size".to_owned(), "small".to_owned())];
            b.priority = Some(3);
            task::create(conn, meta, &b)?;

            let mut c = task::NewTask::new("task:c", "write README");
            c.tags = vec![("size".to_owned(), "small".to_owned())];
            c.priority = Some(2);
            c.due = Some(today.clone());
            task::create(conn, meta, &c)?;

            let mut a = task::NewTask::new("task:a", "ship release");
            a.tags = vec![("size".to_owned(), "small".to_owned())];
            a.priority = Some(1);
            a.due = Some(today.clone());
            a.depends_on = vec!["task:b".to_owned()]; // A blocked by B
            task::create(conn, meta, &a)?;
            Ok(())
        }
    })
    .unwrap();

    // "open small tasks" under tasks/** — all three, before any completion.
    let open_small = evaluate(&db, "kind:task status:open tag:size=small ns:tasks/**");
    assert_eq!(open_small.len(), 3);

    // due:today — A and C.
    let due_today = evaluate(&db, "kind:task due:today");
    let due_uids: Vec<String> = due_today.iter().map(|id| uid_of(&db, *id)).collect();
    assert!(due_uids.contains(&"task:a".to_owned()) && due_uids.contains(&"task:c".to_owned()));

    // --- search: every route returns hits, and context expands a hit ---
    let searcher = Searcher::new(embedder());
    for route in [Route::Fts, Route::Vector, Route::Hybrid] {
        let query = search_query(route);
        let hits = searcher.search(&db, &query, route, 10).unwrap();
        assert!(
            !hits.is_empty(),
            "route {} returned no hits",
            route.as_str()
        );
        let context = searcher.get_context(&db, hits[0].item, 1).unwrap();
        assert!(context.iter().filter(|c| c.is_hit).count() == 1);
    }

    // --- ready frontier changes when the dependency completes ---
    let ready_before = ready_uids(&db);
    assert!(!ready_before.contains(&"task:a".to_owned())); // A is blocked by B
    assert!(ready_before.contains(&"task:b".to_owned()));

    db.write_txn("cli", |conn, meta| {
        let b = item::id_for_uid(conn, "task:b")?.unwrap();
        task::set_status_str(conn, meta, b, "done")
    })
    .unwrap();

    // Now A (p1) is ready and ordered ahead of C (p2); B is done and gone.
    let ready_after = ready_uids(&db);
    assert_eq!(ready_after, vec!["task:a".to_owned(), "task:c".to_owned()]);

    // --- saved view round-trip ---
    db.write_txn("cli", |conn, meta| {
        view::save(conn, meta, "small-tasks", "kind:task tag:size=small")
    })
    .unwrap();
    let view_ids = db.read(|conn| view::run(conn, "small-tasks")).unwrap();
    assert_eq!(view_ids.len(), 3);

    // --- undo reverts the last change ---
    let before = count_kind(&db, "task");
    db.write_txn("cli", |conn, meta| {
        task::create(conn, meta, &task::NewTask::new("task:throwaway", "temp"))
    })
    .unwrap();
    assert_eq!(count_kind(&db, "task"), before + 1);
    let reverted = db.write_txn("cli", jkb_core::undo::undo_last).unwrap();
    assert!(reverted > 0);
    assert_eq!(count_kind(&db, "task"), before);

    // Every mutation was audited.
    assert!(changelog_count(&db) > 0);
}

/// 14.3 — re-running ingest and sync are no-ops, and the changelog grows only when
/// something actually changes.
#[test]
fn ingest_and_sync_are_idempotent_and_audited() {
    let db = db();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("readme.md"), "stable content").unwrap();

    let repo_dir = repo.path().to_string_lossy().into_owned();
    db.write_txn("t", move |conn, meta| {
        let ns_id = ns::ensure(conn, "docs/repo")?;
        mount::create(
            conn,
            meta,
            ns_id,
            &format!("file://{repo_dir}"),
            SyncMode::Bidirectional,
            "document",
            Some("**/*.md"),
            None,
            ConflictPolicy::Manual,
        )
    })
    .unwrap();

    // First sync imports; a second sync with nothing changed is a pure no-op.
    assert_eq!(
        jkb_sync::sync(&db, "docs/repo")
            .unwrap()
            .count(Outcome::Created),
        1
    );
    let after_first = changelog_count(&db);
    let second = jkb_sync::sync(&db, "docs/repo").unwrap();
    assert_eq!(second.count(Outcome::UpToDate), 1);
    assert_eq!(second.count(Outcome::Created), 0);
    assert_eq!(
        changelog_count(&db),
        after_first,
        "no-op sync wrote to the changelog"
    );

    // Re-ingesting the same source is reported as already-ingested with no new items.
    let note = repo.path().join("note.txt");
    std::fs::write(&note, "some ingestable text content here").unwrap();
    let pipeline = Pipeline::new(embedder());
    assert!(
        !pipeline
            .ingest_path(&db, &note, "docs/e2e")
            .unwrap()
            .already_ingested
    );
    let docs = count_kind(&db, "document");
    let again = pipeline.ingest_path(&db, &note, "docs/e2e").unwrap();
    assert!(again.already_ingested);
    assert_eq!(count_kind(&db, "document"), docs);
}

/// 14.2 — MCP smoke: drive the server logic (`search` → `get_context` →
/// `task_create`), confirm the created task appears in `task_next` and is undoable.
#[test]
fn mcp_smoke_flow() {
    use jkb_mcp::logic;

    let db = db();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("note.md");
    std::fs::write(
        &file,
        "# Note\nA searchable distinctive term for the agent.",
    )
    .unwrap();
    logic::ingest_path(
        &db,
        &embedder(),
        &logic::IngestArgs {
            source: file.to_string_lossy().into_owned(),
            namespace: Some("docs".to_owned()),
        },
    )
    .unwrap();

    // search → get_context.
    let hits = logic::search(
        &db,
        &embedder(),
        &logic::SearchArgs {
            query: "distinctive".to_owned(),
            route: Some("fts".to_owned()),
            limit: Some(5),
        },
    )
    .unwrap();
    let hits = hits.as_array().unwrap();
    assert!(!hits.is_empty());
    let item_id = hits[0]["item"].as_i64().unwrap();
    let context = logic::get_context(
        &db,
        &embedder(),
        &logic::GetContextArgs {
            item_id,
            n: Some(1),
        },
    )
    .unwrap();
    assert!(!context.as_array().unwrap().is_empty());

    // task_create → appears in task_next → is undoable.
    let created = logic::task_create(
        &db,
        &logic::TaskCreateArgs {
            title: "follow up on the note".to_owned(),
            priority: Some(1),
            due: None,
            namespace: Some("repos/app".to_owned()),
        },
    )
    .unwrap();
    let new_uid = created["uid"].as_str().unwrap().to_owned();

    let next = logic::task_next(
        &db,
        &logic::QueryArgs {
            query: String::new(),
            limit: None,
        },
    )
    .unwrap();
    let next_uids: Vec<&str> = next
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["uid"].as_str())
        .collect();
    assert!(next_uids.contains(&new_uid.as_str()));

    // Agent-created change is undoable like any CLI change.
    let reverted = db.write_txn("cli", jkb_core::undo::undo_last).unwrap();
    assert!(reverted > 0);
    assert_eq!(count_kind(&db, "task"), 0);
}

// ---- helpers --------------------------------------------------------------

/// A scoped search query for `route`: FTS uses a keyword, vector uses a `~"…"` term.
fn search_query(route: Route) -> Query {
    let mut q = Query {
        scope: Scope::Subtree("docs".to_owned()),
        ..Query::default()
    };
    match route {
        Route::Vector => q.vector = Some("distinctive keyword".to_owned()),
        Route::Fts => q.fts = Some("distinctive".to_owned()),
        Route::Hybrid => {
            q.fts = Some("distinctive".to_owned());
            q.vector = Some("distinctive keyword".to_owned());
        }
    }
    q
}

fn ready_uids(db: &Db) -> Vec<String> {
    let rows = db.read(|conn| task::ready(conn, Scope::All, &[])).unwrap();
    rows.into_iter().map(|r| r.uid).collect()
}
