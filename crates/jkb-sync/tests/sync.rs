//! End-to-end file-sync tests over a real migrated in-memory DB and a temp dir.
//!
//! Sync only touches items/placements/bindings; FTS follows via the `V002` triggers
//! and no vector index is involved, so these need neither the `sqlite-vec` extension
//! nor an embedder.

use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use jkb_core::{binding, item, ns, task, Db};
use jkb_sync::{sync, sync_paths, Outcome};
use jkb_types::{ConflictPolicy, SyncMode};
use rusqlite::OptionalExtension;

/// Create a `file://` mount at `ns_path` backing `dir`.
#[allow(clippy::too_many_arguments)]
fn mount_dir(
    db: &Db,
    ns_path: &str,
    dir: &Path,
    mode: SyncMode,
    serializer: &str,
    include: Option<&str>,
    exclude: Option<&str>,
    policy: ConflictPolicy,
) {
    let backing = format!("file://{}", dir.to_string_lossy());
    let ns_path = ns_path.to_owned();
    let serializer = serializer.to_owned();
    let include = include.map(str::to_owned);
    let exclude = exclude.map(str::to_owned);
    db.write_txn("t", move |conn, meta| {
        let ns_id = ns::ensure(conn, &ns_path)?;
        jkb_core::mount::create(
            conn,
            meta,
            ns_id,
            &backing,
            mode,
            &serializer,
            include.as_deref(),
            exclude.as_deref(),
            policy,
        )
    })
    .unwrap();
}

fn uri_for(path: &Path) -> String {
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

fn last_hash(db: &Db, uri: &str) -> Option<String> {
    let uri = uri.to_owned();
    db.read(move |conn| match binding::item_for_uri(conn, &uri)? {
        Some(item) => Ok(binding::get(conn, item)?.and_then(|b| b.last_synced_hash)),
        None => Ok(None),
    })
    .unwrap()
}

fn fts_hits(db: &Db, term: &str) -> i64 {
    let term = term.to_owned();
    db.read(move |conn| {
        Ok(conn.query_row(
            "SELECT count(*) FROM fts_items WHERE fts_items MATCH ?1",
            [term],
            |r| r.get(0),
        )?)
    })
    .unwrap()
}

/// Edit an item's content in the KB (as the CLI would), without marking it synced.
fn kb_edit(db: &Db, uri: &str, content: &str) {
    let uri = uri.to_owned();
    let content = content.to_owned();
    db.write_txn("cli", move |conn, meta| {
        let item = binding::item_for_uri(conn, &uri)?.expect("bound item");
        item::set_content(conn, meta, item, &content, None)
    })
    .unwrap();
}

#[test]
fn readme_imports_then_round_trips_both_ways() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("README.md");
    fs::write(&file, "hello").unwrap();
    let uri = uri_for(&file);

    let db = Db::open_in_memory().unwrap();
    mount_dir(
        &db,
        "docs/repo",
        dir.path(),
        SyncMode::Bidirectional,
        "document",
        Some("**/*.md"),
        None,
        ConflictPolicy::Manual,
    );

    // First sync imports the file as a new document item, FTS-searchable, bound.
    let report = sync(&db, "docs/repo").unwrap();
    assert_eq!(report.count(Outcome::Created), 1);
    assert_eq!(content_for(&db, &uri).as_deref(), Some("hello"));
    assert_eq!(fts_hits(&db, "hello"), 1);
    assert!(last_hash(&db, &uri).is_some());

    // Edit on disk → import updates the item.
    fs::write(&file, "hello world").unwrap();
    let report = sync(&db, "docs/repo").unwrap();
    assert_eq!(report.count(Outcome::Imported), 1);
    assert_eq!(content_for(&db, &uri).as_deref(), Some("hello world"));

    // A second sync with nothing changed is a no-op.
    let report = sync(&db, "docs/repo").unwrap();
    assert_eq!(report.count(Outcome::UpToDate), 1);

    // Edit in the KB → export rewrites the file.
    kb_edit(&db, &uri, "edited in kb");
    let report = sync(&db, "docs/repo").unwrap();
    assert_eq!(report.count(Outcome::Exported), 1);
    assert_eq!(fs::read_to_string(&file).unwrap(), "edited in kb");
    // And now it's settled.
    assert_eq!(sync(&db, "docs/repo").unwrap().count(Outcome::UpToDate), 1);
}

#[test]
fn excluded_files_are_not_synced() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("keep.md"), "keep me").unwrap();
    fs::write(dir.path().join("secret.env"), "nope").unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_dir(
        &db,
        "docs/repo",
        dir.path(),
        SyncMode::Bidirectional,
        "document",
        None,
        Some("**/*.env"),
        ConflictPolicy::Manual,
    );

    let report = sync(&db, "docs/repo").unwrap();
    assert_eq!(report.count(Outcome::Created), 1);
    assert!(content_for(&db, &uri_for(&dir.path().join("keep.md"))).is_some());
    assert!(content_for(&db, &uri_for(&dir.path().join("secret.env"))).is_none());
}

#[test]
fn both_changed_conflict_is_reported_under_manual() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("README.md");
    fs::write(&file, "base").unwrap();
    let uri = uri_for(&file);

    let db = Db::open_in_memory().unwrap();
    mount_dir(
        &db,
        "docs/repo",
        dir.path(),
        SyncMode::Bidirectional,
        "document",
        Some("**/*.md"),
        None,
        ConflictPolicy::Manual,
    );
    sync(&db, "docs/repo").unwrap();

    // Both sides diverge from the last synced base.
    fs::write(&file, "disk change").unwrap();
    kb_edit(&db, &uri, "kb change");

    let report = sync(&db, "docs/repo").unwrap();
    assert_eq!(report.conflicts().len(), 1);
    // Neither side was modified.
    assert_eq!(fs::read_to_string(&file).unwrap(), "disk change");
    assert_eq!(content_for(&db, &uri).as_deref(), Some("kb change"));
}

#[test]
fn both_changed_disk_wins_imports_the_disk_copy() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("README.md");
    fs::write(&file, "base").unwrap();
    let uri = uri_for(&file);

    let db = Db::open_in_memory().unwrap();
    mount_dir(
        &db,
        "docs/repo",
        dir.path(),
        SyncMode::Bidirectional,
        "document",
        Some("**/*.md"),
        None,
        ConflictPolicy::DiskWins,
    );
    sync(&db, "docs/repo").unwrap();

    fs::write(&file, "disk wins").unwrap();
    kb_edit(&db, &uri, "kb loses");

    let report = sync(&db, "docs/repo").unwrap();
    assert_eq!(report.count(Outcome::Imported), 1);
    assert_eq!(content_for(&db, &uri).as_deref(), Some("disk wins"));
}

#[test]
fn failed_import_does_not_corrupt_sync_state() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("README.md");
    fs::write(&file, "valid text").unwrap();
    let uri = uri_for(&file);

    let db = Db::open_in_memory().unwrap();
    mount_dir(
        &db,
        "docs/repo",
        dir.path(),
        SyncMode::Bidirectional,
        "document",
        Some("**/*.md"),
        None,
        ConflictPolicy::Manual,
    );
    sync(&db, "docs/repo").unwrap();
    let good_hash = last_hash(&db, &uri);
    assert!(good_hash.is_some());

    // Corrupt the file with invalid UTF-8: the document serializer's parse fails, so
    // the import transaction rolls back.
    fs::write(&file, [0xff, 0xfe, 0x00]).unwrap();
    assert!(sync(&db, "docs/repo").is_err());

    // The item content and last_synced_hash still reflect the previous good sync.
    assert_eq!(content_for(&db, &uri).as_deref(), Some("valid text"));
    assert_eq!(last_hash(&db, &uri), good_hash);
}

#[test]
fn unknown_serializer_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.md"), "x").unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_dir(
        &db,
        "docs/repo",
        dir.path(),
        SyncMode::Bidirectional,
        "spec", // not available in this build (the v2 spec serializer is future work)
        None,
        None,
        ConflictPolicy::Manual,
    );

    let err = sync(&db, "docs/repo").unwrap_err().to_string();
    assert!(err.contains("unknown serializer"));
}

#[test]
fn sync_paths_reconciles_only_the_named_files() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.md");
    let b = dir.path().join("b.md");
    fs::write(&a, "a1").unwrap();
    fs::write(&b, "b1").unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_dir(
        &db,
        "docs/repo",
        dir.path(),
        SyncMode::Bidirectional,
        "document",
        Some("**/*.md"),
        None,
        ConflictPolicy::Manual,
    );
    // Initial full sync imports both.
    assert_eq!(sync(&db, "docs/repo").unwrap().count(Outcome::Created), 2);

    // Change both on disk, but reconcile only `a` (as a watch event would).
    fs::write(&a, "a2").unwrap();
    fs::write(&b, "b2").unwrap();
    let report = sync_paths(&db, "docs/repo", std::slice::from_ref(&a)).unwrap();
    assert_eq!(report.count(Outcome::Imported), 1);
    assert_eq!(content_for(&db, &uri_for(&a)).as_deref(), Some("a2"));
    // `b` was not in the path list, so it is untouched (still the old content).
    assert_eq!(content_for(&db, &uri_for(&b)).as_deref(), Some("b1"));
}

#[test]
fn sync_paths_drops_paths_outside_scope() {
    let dir = tempfile::tempdir().unwrap();
    let keep = dir.path().join("keep.md");
    fs::write(&keep, "keep").unwrap();
    fs::write(dir.path().join("skip.env"), "skip").unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_dir(
        &db,
        "docs/repo",
        dir.path(),
        SyncMode::Bidirectional,
        "document",
        Some("**/*.md"),
        None,
        ConflictPolicy::Manual,
    );

    // An excluded-by-glob file and a file outside the mount are both ignored; only the
    // in-scope path is reconciled.
    let outside = std::env::temp_dir().join("definitely-not-in-mount.md");
    let report = sync_paths(
        &db,
        "docs/repo",
        &[keep.clone(), dir.path().join("skip.env"), outside],
    )
    .unwrap();
    assert_eq!(report.results.len(), 1);
    assert_eq!(report.count(Outcome::Created), 1);
    assert!(content_for(&db, &uri_for(&keep)).is_some());
}

#[test]
fn watch_runs_an_initial_reconcile_then_stops() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("README.md");
    fs::write(&file, "watch me").unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_dir(
        &db,
        "docs/repo",
        dir.path(),
        SyncMode::Bidirectional,
        "document",
        Some("**/*.md"),
        None,
        ConflictPolicy::Manual,
    );

    // Ask it to stop right away; `watch` still performs its initial reconcile before
    // the (idle) event loop notices the stop flag.
    let stop = Arc::new(AtomicBool::new(true));
    jkb_sync::watch(&db, "docs/repo", Duration::from_millis(20), &stop).unwrap();

    assert_eq!(
        content_for(&db, &uri_for(&file)).as_deref(),
        Some("watch me")
    );
}

#[test]
fn watch_all_reconciles_every_mount_then_stops() {
    let db = Db::open_in_memory().unwrap();
    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();
    fs::write(repo_a.path().join("a.md"), "alpha").unwrap();
    fs::write(repo_b.path().join("b.md"), "beta").unwrap();

    for (path, dir) in [("docs/a", repo_a.path()), ("docs/b", repo_b.path())] {
        mount_dir(
            &db,
            path,
            dir,
            SyncMode::Bidirectional,
            "document",
            Some("**/*.md"),
            None,
            ConflictPolicy::Manual,
        );
    }

    // Pre-set the shared stop flag: every per-mount watcher still runs its initial
    // reconcile before noticing it, so both files import.
    let stop = Arc::new(AtomicBool::new(true));
    jkb_sync::watch_all(&db, Duration::from_millis(20), &stop).unwrap();

    assert_eq!(
        content_for(&db, &uri_for(&repo_a.path().join("a.md"))).as_deref(),
        Some("alpha")
    );
    assert_eq!(
        content_for(&db, &uri_for(&repo_b.path().join("b.md"))).as_deref(),
        Some("beta")
    );
}

// ---------------------------------------------------------------------------
// Multi-item `tasks` serializer (D24) + sync robustness (D25)
// ---------------------------------------------------------------------------

const TASKS_MD: &str = "\
<!-- notes -->
## Backend
- [ ] Set up CI ^setup
- [ ] Fix flaky test !p1 needs:^setup ^fix
## Frontend
- [x] Ship button ^ship
";

/// The count of `kind='task'` items in the KB.
fn task_count(db: &Db) -> i64 {
    db.read(|conn| {
        Ok(
            conn.query_row("SELECT count(*) FROM items WHERE kind = 'task'", [], |r| {
                r.get(0)
            })?,
        )
    })
    .unwrap()
}

/// The `status` column of the item with the given uid.
fn status_of(db: &Db, uid: &str) -> Option<String> {
    let uid = uid.to_owned();
    db.read(move |conn| {
        Ok(conn
            .query_row("SELECT status FROM items WHERE uid = ?1", [uid], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()?
            .flatten())
    })
    .unwrap()
}

/// Set a bound task's status via the uid (as the CLI/MCP would), without marking synced.
fn kb_set_status(db: &Db, uid: &str, status: &str) {
    let uid = uid.to_owned();
    let status = status.to_owned();
    db.write_txn("cli", move |conn, meta| {
        let id = item::id_for_uid(conn, &uid)?.expect("task exists");
        task::set_status_str(conn, meta, id, &status)
    })
    .unwrap();
}

/// The `(status, base_blob_hash)` of a file's `_sys/sync` journal row.
fn journal(db: &Db, uri: &str) -> Option<(String, Option<String>)> {
    let uri = uri.to_owned();
    db.read(move |conn| {
        Ok(conn
            .query_row(
                "SELECT status, base_blob_hash FROM sync_state WHERE uri = ?1",
                [uri],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()?)
    })
    .unwrap()
}

fn mount_tasks(db: &Db, dir: &Path, policy: ConflictPolicy) {
    mount_dir(
        db,
        "docs/plan",
        dir,
        SyncMode::Bidirectional,
        "tasks",
        Some("**/*.md"),
        None,
        policy,
    );
}

#[test]
fn tasks_import_creates_items_sections_and_is_byte_stable() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tasks.md");
    fs::write(&file, TASKS_MD).unwrap();
    let uri = uri_for(&file);

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);

    // First sync creates one item per task line + the prose text item.
    assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::Created), 1);
    assert_eq!(task_count(&db), 3);

    // Section headers became namespaces under the file's mirror namespace.
    assert!(db
        .read(|conn| ns::get(conn, "docs/plan/backend"))
        .unwrap()
        .is_some());
    assert!(db
        .read(|conn| ns::get(conn, "docs/plan/frontend"))
        .unwrap()
        .is_some());

    // Status came from the checkboxes; the dependency edge was linked.
    assert_eq!(
        status_of(&db, &format!("{uri}#fix")).as_deref(),
        Some("open")
    );
    assert_eq!(
        status_of(&db, &format!("{uri}#ship")).as_deref(),
        Some("done")
    );
    assert!(is_blocked(&db, &format!("{uri}#fix")));

    // Journal recorded an `ok` row with a base blob, and the file round-trips byte-stable.
    let (status, base) = journal(&db, &uri).unwrap();
    assert_eq!(status, "ok");
    assert!(base.is_some());
    assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::UpToDate), 1);
    assert_eq!(fs::read_to_string(&file).unwrap(), TASKS_MD);
}

/// Whether a bound task is blocked (has a `depends_on` to a non-terminal task).
fn is_blocked(db: &Db, uid: &str) -> bool {
    let uid = uid.to_owned();
    db.read(move |conn| {
        let id = item::id_for_uid(conn, &uid)?.expect("task");
        task::is_blocked(conn, id)
    })
    .unwrap()
}

#[test]
fn kb_edit_exports_and_preserves_structure() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tasks.md");
    fs::write(&file, TASKS_MD).unwrap();
    let uri = uri_for(&file);

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();

    // Complete a task in the KB, then export.
    kb_set_status(&db, &format!("{uri}#fix"), "done");
    assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::Exported), 1);

    let text = fs::read_to_string(&file).unwrap();
    assert!(text.contains("- [x] Fix flaky test !p1 needs:^setup ^fix"));
    // Headers and prose survived the KB-only edit + export.
    assert!(text.contains("## Backend"));
    assert!(text.contains("## Frontend"));
    assert!(text.contains("<!-- notes -->"));
    assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::UpToDate), 1);
}

#[test]
fn caret_less_task_is_stamped_back_to_disk() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tasks.md");
    fs::write(&file, "- [ ] a fresh task\n").unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();

    // The import wrote a stable `^id` back to the file so future edits reconcile.
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.contains("^a-fresh-task-"), "got: {text}");
    // And it settles (no re-mint loop).
    assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::UpToDate), 1);
}

#[test]
fn removing_a_task_line_cancels_the_item() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tasks.md");
    fs::write(&file, TASKS_MD).unwrap();
    let uri = uri_for(&file);

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();

    // Drop the Frontend section's task from the file.
    fs::write(
        &file,
        "<!-- notes -->\n## Backend\n- [ ] Set up CI ^setup\n- [ ] Fix flaky test !p1 needs:^setup ^fix\n## Frontend\n",
    )
    .unwrap();
    assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::Imported), 1);

    // The item is not destroyed — it is marked cancelled (design D25).
    assert_eq!(
        status_of(&db, &format!("{uri}#ship")).as_deref(),
        Some("cancelled")
    );
    assert_eq!(task_count(&db), 3); // still three items, one now cancelled
}

#[test]
fn disjoint_disk_and_kb_edits_merge_three_way() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tasks.md");
    fs::write(&file, TASKS_MD).unwrap();
    let uri = uri_for(&file);

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();

    // KB checks off `ship`; disk edits a *different* task's title (`setup`).
    kb_set_status(&db, &format!("{uri}#ship"), "done"); // already done; make it in_progress instead
    kb_set_status(&db, &format!("{uri}#fix"), "in_progress");
    fs::write(
        &file,
        "<!-- notes -->\n## Backend\n- [ ] Provision CI ^setup\n- [ ] Fix flaky test !p1 needs:^setup ^fix\n## Frontend\n- [x] Ship button ^ship\n",
    )
    .unwrap();

    assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::Merged), 1);
    // Disk's title edit landed…
    assert_eq!(
        content_for(&db, &format!("{uri}#setup")).as_deref(),
        Some("Provision CI")
    );
    // …and the KB's status edit survived.
    assert_eq!(
        status_of(&db, &format!("{uri}#fix")).as_deref(),
        Some("in_progress")
    );
}

#[test]
fn same_task_edited_both_sides_conflicts_under_manual() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tasks.md");
    fs::write(&file, TASKS_MD).unwrap();
    let uri = uri_for(&file);

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();

    // Both sides change the *same* task (`fix`): disk title vs KB status.
    kb_set_status(&db, &format!("{uri}#fix"), "in_progress");
    fs::write(
        &file,
        "<!-- notes -->\n## Backend\n- [ ] Set up CI ^setup\n- [ ] Repair flaky test !p1 needs:^setup ^fix\n## Frontend\n- [x] Ship button ^ship\n",
    )
    .unwrap();

    let report = sync(&db, "docs/plan").unwrap();
    assert_eq!(report.conflicts().len(), 1);
    // Neither side was modified.
    assert_eq!(
        status_of(&db, &format!("{uri}#fix")).as_deref(),
        Some("in_progress")
    );
    assert!(fs::read_to_string(&file)
        .unwrap()
        .contains("Repair flaky test"));
    assert_eq!(journal(&db, &uri).unwrap().0, "conflict");
}

#[test]
fn malformed_file_is_quarantined_then_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tasks.md");
    fs::write(&file, TASKS_MD).unwrap();
    let uri = uri_for(&file);

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();

    // Corrupt the file with genuine structural breakage (two tasks claiming the same
    // `^id`). It is quarantined, not overwritten.
    fs::write(&file, "## Backend\n- [ ] one ^dup\n- [ ] two ^dup\n").unwrap();
    let report = sync(&db, "docs/plan").unwrap();
    assert_eq!(report.count(Outcome::Quarantined), 1);
    assert_eq!(journal(&db, &uri).unwrap().0, "needs_attention");
    // Last-good items are intact, and the failing bytes were left on disk untouched.
    assert_eq!(task_count(&db), 3);
    assert_eq!(
        status_of(&db, &format!("{uri}#setup")).as_deref(),
        Some("open")
    );
    assert!(fs::read_to_string(&file).unwrap().contains("^dup"));

    // Fixing the file clears the quarantine.
    fs::write(&file, "## Backend\n- [ ] fixed now ^x\n").unwrap();
    let report = sync(&db, "docs/plan").unwrap();
    assert!(report.count(Outcome::Imported) + report.count(Outcome::Merged) >= 1);
    assert_eq!(journal(&db, &uri).unwrap().0, "ok");
}
