//! End-to-end query-engine tests (task 8.6): the DSL → AST → SQL path over a real
//! DB, saved views, and the "filter then rank" candidate set.

use jkb_core::query::{self, Query};
use jkb_core::{edge, item, ns, placement, tag, view, Db, WriteMeta};
use jkb_types::{EdgeType, ItemId, PlacementRole};
use rusqlite::Connection;

/// Create a task item with status/priority/due (columns not yet exposed by a repo —
/// set directly here; the task-DAG section adds the proper API) and place + tag it.
#[allow(clippy::too_many_arguments)]
fn add_task(
    conn: &Connection,
    meta: &WriteMeta,
    uid: &str,
    ns_path: &str,
    status: &str,
    priority: i64,
    due: Option<&str>,
    size: Option<&str>,
) -> ItemId {
    let id = item::upsert(
        conn,
        meta,
        &item::NewItem {
            uid: uid.to_owned(),
            kind: "task".to_owned(),
            content: Some(uid.to_owned()),
            content_hash: None,
            mime: None,
        },
    )
    .unwrap();
    conn.execute(
        "UPDATE items SET status = ?1, priority = ?2, due = ?3 WHERE id = ?4",
        rusqlite::params![status, priority, due, id.get()],
    )
    .unwrap();
    let ns_id = ns::ensure(conn, ns_path).unwrap();
    placement::place(conn, meta, id, ns_id, PlacementRole::Primary, 0).unwrap();
    if let Some(size) = size {
        tag::apply(conn, meta, id, "size", size).unwrap();
    }
    id
}

fn uids_for(db: &Db, dsl: &str) -> Vec<String> {
    let dsl = dsl.to_owned();
    db.read(move |conn| {
        let ids = query::parse(&dsl).unwrap().evaluate(conn).unwrap();
        let mut uids = Vec::new();
        for id in ids {
            uids.push(
                conn.query_row("SELECT uid FROM items WHERE id = ?1", [id.get()], |r| {
                    r.get::<_, String>(0)
                })?,
            );
        }
        Ok(uids)
    })
    .unwrap()
}

#[test]
fn open_small_tasks_scoped_to_a_subtree() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |conn, meta| {
        add_task(
            conn,
            meta,
            "t:small-open",
            "tasks/proj",
            "open",
            1,
            None,
            Some("small"),
        );
        add_task(
            conn,
            meta,
            "t:big-open",
            "tasks/proj",
            "open",
            1,
            None,
            Some("large"),
        );
        add_task(
            conn,
            meta,
            "t:small-done",
            "tasks/proj",
            "done",
            1,
            None,
            Some("small"),
        );
        add_task(
            conn,
            meta,
            "t:small-elsewhere",
            "notes",
            "open",
            1,
            None,
            Some("small"),
        );
        Ok(())
    })
    .unwrap();

    let hits = uids_for(&db, "kind:task status:open tag:size=small ns:tasks/**");
    assert_eq!(hits, vec!["t:small-open"]);
}

#[test]
fn books_read_in_2025() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |conn, meta| {
        let book = |uid: &str, year: &str| {
            let id = item::upsert(
                conn,
                meta,
                &item::NewItem {
                    uid: uid.to_owned(),
                    kind: "book".to_owned(),
                    content: Some(uid.to_owned()),
                    content_hash: None,
                    mime: None,
                },
            )
            .unwrap();
            let ns_id = ns::ensure(conn, "books/2025").unwrap();
            placement::place(conn, meta, id, ns_id, PlacementRole::Primary, 0).unwrap();
            tag::apply(conn, meta, id, "read_year", year).unwrap();
        };
        book("book:sicp", "2025");
        book("book:tao", "2024");
        Ok(())
    })
    .unwrap();

    assert_eq!(
        uids_for(&db, "ns:books/** tag:read_year=2025"),
        vec!["book:sicp"]
    );
    // Numeric-ish range on a same-width facet.
    assert_eq!(
        uids_for(&db, "ns:books/** tag:read_year>=2025"),
        vec!["book:sicp"]
    );
}

#[test]
fn due_today_matches_only_todays_items() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |conn, meta| {
        // `date('now','localtime')` is what `due:today` compares against.
        let today: String = conn
            .query_row("SELECT date('now','localtime')", [], |r| r.get(0))
            .unwrap();
        add_task(
            conn,
            meta,
            "t:today",
            "tasks",
            "open",
            1,
            Some(&today),
            None,
        );
        add_task(
            conn,
            meta,
            "t:past",
            "tasks",
            "open",
            1,
            Some("2000-01-01"),
            None,
        );
        Ok(())
    })
    .unwrap();

    assert_eq!(uids_for(&db, "due:today"), vec!["t:today"]);
}

#[test]
fn is_ready_excludes_blocked_tasks() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |conn, meta| {
        let dep = add_task(conn, meta, "t:dep", "tasks", "open", 1, None, None);
        let blocked = add_task(conn, meta, "t:blocked", "tasks", "open", 1, None, None);
        let free = add_task(conn, meta, "t:free", "tasks", "open", 1, None, None);
        // `blocked` depends on the still-open `dep`; `free` depends on nothing.
        edge::link(conn, meta, blocked, dep, EdgeType::DependsOn, None)?;
        let _ = free;
        Ok(())
    })
    .unwrap();

    // dep and free are ready (dep has no deps; free has none); blocked is not.
    let mut ready = uids_for(&db, "kind:task is:ready");
    ready.sort();
    assert_eq!(ready, vec!["t:dep", "t:free"]);
}

#[test]
fn scope_union_spans_both_subtrees() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |conn, meta| {
        add_task(conn, meta, "t:a", "books/2025", "open", 1, None, None);
        add_task(conn, meta, "t:b", "articles/2025", "open", 1, None, None);
        add_task(conn, meta, "t:c", "misc", "open", 1, None, None);
        Ok(())
    })
    .unwrap();

    let mut hits = uids_for(&db, "ns:books/**,articles/2025/**");
    hits.sort();
    assert_eq!(hits, vec!["t:a", "t:b"]);
}

#[test]
fn saved_view_round_trips() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |conn, meta| {
        add_task(conn, meta, "t:open", "tasks", "open", 1, None, None);
        add_task(conn, meta, "t:done", "tasks", "done", 1, None, None);
        view::save(conn, meta, "open-tasks", "kind:task status:open")?;
        Ok(())
    })
    .unwrap();

    // Listed with its query string.
    let views = db.read(view::list).unwrap();
    assert_eq!(
        views,
        vec![("open-tasks".to_owned(), "kind:task status:open".to_owned())]
    );

    // Running it evaluates the stored query.
    let hits = db
        .read(|conn| {
            let ids = view::run(conn, "open-tasks")?;
            let mut uids = Vec::new();
            for id in ids {
                uids.push(conn.query_row(
                    "SELECT uid FROM items WHERE id = ?1",
                    [id.get()],
                    |r| r.get::<_, String>(0),
                )?);
            }
            Ok(uids)
        })
        .unwrap();
    assert_eq!(hits, vec!["t:open"]);
}

#[test]
fn programmatic_ast_needs_no_parsing() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |conn, meta| {
        add_task(conn, meta, "t:open", "tasks", "open", 1, None, None);
        add_task(conn, meta, "t:done", "tasks", "done", 1, None, None);
        Ok(())
    })
    .unwrap();

    let q = Query {
        kind: Some("task".to_owned()),
        status: Some("open".to_owned()),
        ..Query::default()
    };
    let hits = db.read(move |conn| q.evaluate(conn)).unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn saving_an_invalid_view_is_rejected() {
    let db = Db::open_in_memory().unwrap();
    let result = db.write_txn("t", |conn, meta| {
        view::save(conn, meta, "bad", "priority<=x")
    });
    assert!(
        result.is_err(),
        "an unparseable view query must be rejected"
    );
}
