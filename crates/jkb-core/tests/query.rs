//! End-to-end query-engine tests (task 8.6): the DSL → AST → SQL path over a real
//! DB, saved views, and the "filter then rank" candidate set.

use jkb_core::query::{self, Query};
use jkb_core::{claim, edge, item, ns, placement, tag, view, Db, WriteMeta};
use jkb_types::{EdgeType, ItemId, PlacementRole, Resolution};
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

/// Create a memory node: a non-task kind with NULL `status` (design Dmem.2), placed under
/// `ns_path`, with an optional resolution and `promise` tag.
fn add_node(
    conn: &Connection,
    meta: &WriteMeta,
    uid: &str,
    kind: &str,
    ns_path: &str,
    resolution: Option<Resolution>,
) -> ItemId {
    let id = item::upsert(
        conn,
        meta,
        &item::NewItem {
            uid: uid.to_owned(),
            kind: kind.to_owned(),
            content: Some(uid.to_owned()),
            content_hash: None,
            mime: None,
        },
    )
    .unwrap();
    let ns_id = ns::ensure(conn, ns_path).unwrap();
    placement::place(conn, meta, id, ns_id, PlacementRole::Primary, 0).unwrap();
    if let Some(r) = resolution {
        item::set_resolution(conn, meta, id, r).unwrap();
    }
    id
}

#[test]
fn is_frontier_excludes_resolved_units_and_units_blocked_by_live_ones() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |conn, meta| {
        let live = add_node(conn, meta, "h:live", "hypothesis", "memory/inv", None);
        add_node(
            conn,
            meta,
            "h:dead",
            "hypothesis",
            "memory/inv",
            Some(Resolution::DeadEnd),
        );
        add_node(
            conn,
            meta,
            "h:won",
            "hypothesis",
            "memory/inv",
            Some(Resolution::Success),
        );
        // Blocked by a still-unresolved unit → off the frontier.
        let blocked = add_node(conn, meta, "h:blocked", "hypothesis", "memory/inv", None);
        edge::link(conn, meta, blocked, live, EdgeType::DependsOn, None)?;
        // Blocked only by a DEAD unit → back on the frontier: a dead end will never
        // complete, so waiting on it forever would strand this unit.
        let after_dead = add_node(conn, meta, "h:after-dead", "hypothesis", "memory/inv", None);
        let dead = item::id_for_uid(conn, "h:dead")?.unwrap();
        edge::link(conn, meta, after_dead, dead, EdgeType::DependsOn, None)?;
        Ok(())
    })
    .unwrap();

    let mut hits = uids_for(&db, "is:frontier ns:memory/**");
    hits.sort();
    assert_eq!(hits, vec!["h:after-dead", "h:live"]);

    // The outcome axis is queryable on its own, and `unresolved` matches NULL columns.
    let mut unresolved = uids_for(&db, "resolution:unresolved ns:memory/**");
    unresolved.sort();
    assert_eq!(
        unresolved,
        vec!["h:after-dead", "h:blocked", "h:live"],
        "every never-resolved node reads as unresolved despite a NULL column"
    );
    assert_eq!(
        uids_for(&db, "resolution:success ns:memory/**"),
        vec!["h:won"]
    );
}

#[test]
fn is_frontier_matches_is_ready_for_tasks_and_respects_claims() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |conn, meta| {
        let dep = add_task(conn, meta, "t:dep", "tasks", "open", 1, None, None);
        let blocked = add_task(conn, meta, "t:blocked", "tasks", "open", 1, None, None);
        add_task(conn, meta, "t:done", "tasks", "done", 1, None, None);
        edge::link(conn, meta, blocked, dep, EdgeType::DependsOn, None)?;
        Ok(())
    })
    .unwrap();

    // A task's `resolution` is always NULL, so the generalized frontier collapses to
    // exactly the task-ready frontier — one concept, two vocabularies.
    let mut ready = uids_for(&db, "kind:task is:ready");
    ready.sort();
    let mut frontier = uids_for(&db, "kind:task is:frontier");
    frontier.sort();
    assert_eq!(ready, frontier);
    assert_eq!(frontier, vec!["t:dep"]);

    // Claiming the one frontier task empties the frontier (claim-aware, like `is:ready`)…
    db.write_txn("t", |conn, meta| {
        let dep = item::id_for_uid(conn, "t:dep")?.unwrap();
        assert!(claim::claim(conn, meta, dep, "host:1")?);
        Ok(())
    })
    .unwrap();
    assert!(uids_for(&db, "kind:task is:frontier").is_empty());
    assert_eq!(uids_for(&db, "kind:task is:claimed"), vec!["t:dep"]);
}

#[test]
fn is_tombstone_is_the_anti_retread_set_by_resolution_or_by_killing_edge() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |conn, meta| {
        add_node(
            conn,
            meta,
            "c:dead",
            "candidate",
            "memory/inv",
            Some(Resolution::DeadEnd),
        );
        add_node(
            conn,
            meta,
            "c:old",
            "candidate",
            "memory/inv",
            Some(Resolution::Superseded),
        );
        // Dropped, not disproved — must stay pickable, so NOT a tombstone.
        add_node(
            conn,
            meta,
            "c:parked",
            "candidate",
            "memory/inv",
            Some(Resolution::Abandoned),
        );
        // Killed by an edge before anyone set the resolution: still anti-retread.
        let regime = add_node(
            conn,
            meta,
            "r:degree2",
            "parameter-regime",
            "memory/inv",
            None,
        );
        let obstruction = add_node(conn, meta, "o:parity", "obstruction", "memory/inv", None);
        edge::link(conn, meta, obstruction, regime, EdgeType::RulesOut, None)?;
        add_node(conn, meta, "c:live", "candidate", "memory/inv", None);
        Ok(())
    })
    .unwrap();

    let mut tombs = uids_for(&db, "is:tombstone ns:memory/**");
    tombs.sort();
    assert_eq!(tombs, vec!["c:dead", "c:old", "r:degree2"]);
    // And the ruled-out regime is off the frontier's radar only via the tombstone read —
    // it has no resolution yet, so it is still formally unresolved.
    assert!(uids_for(&db, "resolution:unresolved ns:memory/**").contains(&"r:degree2".to_owned()));
}

#[test]
fn negated_tag_excludes_and_kind_union_selects_several_kinds() {
    let db = Db::open_in_memory().unwrap();
    db.write_txn("t", |conn, meta| {
        let fresh = add_node(conn, meta, "o:fresh", "observation", "memory/inv", None);
        let stale = add_node(conn, meta, "o:stale", "observation", "memory/inv", None);
        tag::apply(conn, meta, stale, "staleness", "stale")?;
        let _ = fresh;
        add_node(conn, meta, "g:gap", "gap", "memory/inv", None);
        add_node(conn, meta, "x:other", "note", "memory/inv", None);
        Ok(())
    })
    .unwrap();

    assert_eq!(
        uids_for(&db, "kind:observation -tag:staleness=stale ns:memory/**"),
        vec!["o:fresh"],
        "a stale observation is excluded without any raw SQL"
    );
    let mut both = uids_for(&db, "kind:observation,gap ns:memory/**");
    both.sort();
    assert_eq!(both, vec!["g:gap", "o:fresh", "o:stale"]);

    // `-kind:` excludes — how a frontier asks for "work" without enumerating every kind
    // that counts as work.
    let mut kept = uids_for(&db, "-kind:observation,note ns:memory/**");
    kept.sort();
    assert_eq!(kept, vec!["g:gap"]);
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
