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
    // A policy resolution is reported apart from an ordinary import: it threw the KB side
    // away, and that must never look like a routine sync.
    assert_eq!(report.count(Outcome::ResolvedFromDisk), 1);
    assert_eq!(report.resolved().len(), 1);
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
    //
    // The failure is REPORTED, not propagated. It used to come back as `Err` from `sync`, which
    // meant one unreadable file — a PNG dropped into a `document` mount — returned an error out
    // of the watcher thread for that mount, after which `watch_all` blocked joining the others
    // forever, launchd never restarted the still-live process, and the mount silently stopped
    // syncing for good.
    fs::write(&file, [0xff, 0xfe, 0x00]).unwrap();
    let report = sync(&db, "docs/repo").expect("one bad file must not end the run");
    assert_eq!(report.count(Outcome::Failed), 1);
    assert!(
        report.failed()[0].1.contains("UTF-8"),
        "the reason must survive to the report: {:?}",
        report.failed()
    );

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

    // First sync creates one item per task line. Prose is NOT an item — it round-trips as
    // namespace metadata (see `prose_is_not_an_item_so_a_section_cannot_outlive_the_file`).
    assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::Created), 1);
    assert_eq!(task_count(&db), 3);

    // Section headers became namespaces under the file's own namespace — which since D39.1
    // is named after the file, so a sibling document's sections cannot land beside these.
    assert!(db
        .read(|conn| ns::get(conn, "docs/plan/tasks.md/backend"))
        .unwrap()
        .is_some());
    assert!(db
        .read(|conn| ns::get(conn, "docs/plan/tasks.md/frontend"))
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

/// Whether a `parent_of` edge runs from `parent_uri` to `child_uri`.
fn has_parent_edge(db: &Db, parent_uri: &str, child_uri: &str) -> bool {
    let parent_uri = parent_uri.to_owned();
    let child_uri = child_uri.to_owned();
    db.read(move |conn| {
        let (Some(parent), Some(child)) = (
            binding::item_for_uri(conn, &parent_uri)?,
            binding::item_for_uri(conn, &child_uri)?,
        ) else {
            return Ok(false);
        };
        Ok(
            jkb_core::edge::edges_from(conn, parent, jkb_types::EdgeType::ParentOf)?
                .contains(&child),
        )
    })
    .unwrap()
}

#[test]
fn disk_reindent_survives_a_three_way_merge() {
    // Regression: a re-parenting (indentation) edit on disk is a `parent_of` change, which
    // the merge signature must capture — otherwise a both-sides-changed merge silently
    // reverts it (the child's `Sig` looks identical and the edge is taken from `base`).
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tasks.md");
    fs::write(&file, TASKS_MD).unwrap();
    let uri = uri_for(&file);

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();

    // Initially `fix` is a top-level sibling of `setup`, not its child.
    assert!(!has_parent_edge(
        &db,
        &format!("{uri}#setup"),
        &format!("{uri}#fix")
    ));

    // Disk re-indents `fix` under `setup` (a pure `parent_of` change). Independently, the
    // KB edits a *different* task (`ship`) so the reconcile takes the both-changed →
    // three-way merge path rather than a plain import.
    kb_set_status(&db, &format!("{uri}#ship"), "in_progress");
    fs::write(
        &file,
        "<!-- notes -->\n## Backend\n- [ ] Set up CI ^setup\n  - [ ] Fix flaky test !p1 needs:^setup ^fix\n## Frontend\n- [x] Ship button ^ship\n",
    )
    .unwrap();

    let report = sync(&db, "docs/plan").unwrap();
    assert_eq!(
        report.count(Outcome::Merged),
        1,
        "expected a three-way merge"
    );

    // The re-parenting landed in the KB…
    assert!(
        has_parent_edge(&db, &format!("{uri}#setup"), &format!("{uri}#fix")),
        "disk re-indent was dropped by the merge"
    );
    // …the disjoint KB edit survived…
    assert_eq!(
        status_of(&db, &format!("{uri}#ship")).as_deref(),
        Some("in_progress")
    );
    // …and the file was not reverted to the flat base — the child stays indented.
    assert!(
        fs::read_to_string(&file)
            .unwrap()
            .contains("  - [ ] Fix flaky test"),
        "file lost its indentation"
    );
}

/// The `memory/sync-export-wins` regression, end to end.
///
/// Prose used to become `text` items whose ids (content hash + occurrence counter) broke on
/// the next edit. An orphaned prose item kept its section namespace alive, `assemble_kb_doc`
/// re-emitted that section's `##` header, and from then on the KB render disagreed with the
/// disk forever — so every disk-only edit was resolved as a both-changed conflict and the
/// stale header was written back over it.
#[test]
fn prose_is_not_an_item_so_a_section_cannot_outlive_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tasks.md");
    fs::write(
        &file,
        "# Plan\n\nPreamble.\n\n## Alpha\n\n- [ ] first\n  a continuation line\n\n## Beta\n\n- [ ] second\n",
    )
    .unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();

    // Prose never materializes as an item, so it can never orphan.
    let text_items: i64 = db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM items WHERE kind = \'text\'",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(text_items, 0, "prose must not become items");

    // A settled file re-syncs UpToDate: the KB render reproduces the disk exactly.
    assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::UpToDate), 1);

    // Remove Beta's task (it is cancelled + detached, and its namespace survives)…
    let without_task: String = fs::read_to_string(&file)
        .unwrap()
        .lines()
        .filter(|l| !l.contains("second"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&file, format!("{without_task}\n")).unwrap();
    assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::Imported), 1);

    // …then remove its now-empty header. It must STAY removed.
    let without_header: String = fs::read_to_string(&file)
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with("## Beta"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&file, format!("{without_header}\n")).unwrap();
    assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::Imported), 1);

    for _ in 0..3 {
        assert_eq!(
            sync(&db, "docs/plan").unwrap().count(Outcome::UpToDate),
            1,
            "a settled file must not keep flipping between disk and KB"
        );
        assert!(
            !fs::read_to_string(&file).unwrap().contains("## Beta"),
            "the retired section header came back: {}",
            fs::read_to_string(&file).unwrap()
        );
    }
    // The prose above it survived the whole exercise.
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.contains("Preamble."), "prose was lost: {text}");
    assert!(
        text.contains("  a continuation line"),
        "prose was lost: {text}"
    );
}

/// Deleting a task line and putting it back must RE-ATTACH the same item, not fail on its
/// uid. A removed line is detached rather than deleted, and it keeps its file-derived uid —
/// so a plain insert hit `UNIQUE constraint failed: items.uid` and the whole sync errored.
#[test]
fn re_adding_a_deleted_task_line_reattaches_the_same_item() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tasks.md");
    fs::write(
        &file,
        "## Alpha\n\n- [ ] first ^first\n- [ ] second ^second\n",
    )
    .unwrap();
    let uri = uri_for(&file);

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();
    let before = id_of(&db, &format!("{uri}#second"));

    // Delete the line: the item is cancelled and detached, never destroyed.
    fs::write(&file, "## Alpha\n\n- [ ] first ^first\n").unwrap();
    sync(&db, "docs/plan").unwrap();
    assert_eq!(
        status_of(&db, &format!("{uri}#second")).as_deref(),
        Some("cancelled")
    );

    // Put it back: this must succeed, and resurrect the SAME item.
    fs::write(
        &file,
        "## Alpha\n\n- [ ] first ^first\n- [ ] second ^second\n",
    )
    .unwrap();
    let report = sync(&db, "docs/plan").unwrap();
    assert_eq!(report.count(Outcome::Imported), 1, "sync must not fail");
    assert_eq!(
        id_of(&db, &format!("{uri}#second")),
        before,
        "the same item is re-attached, keeping its history and edges"
    );
    assert_eq!(
        status_of(&db, &format!("{uri}#second")).as_deref(),
        Some("open"),
        "and it is live again"
    );
}

/// The item id behind a binding uri (which is also the item uid), for identity assertions.
fn id_of(db: &Db, uid: &str) -> i64 {
    let uid = uid.to_owned();
    db.read(move |conn| item::id_for_uid(conn, &uid))
        .unwrap()
        .expect("item exists")
        .get()
}

/// A three-way merge must not delete content neither side touched. Prose has no identity, so
/// it is taken wholesale from the disk side — omitting it silently stripped every blank line
/// and paragraph out of a merged file (it destroyed a real openspec document).
#[test]
fn a_three_way_merge_keeps_the_files_prose() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tasks.md");
    fs::write(
        &file,
        "# Plan\n\nA paragraph of prose.\n\n## Alpha\n\n- [ ] first ^first\n  the first body\n- [ ] second ^second\n",
    )
    .unwrap();
    let uri = uri_for(&file);

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();

    // Disjoint edits: the disk retitles `first`, the KB completes `second`.
    fs::write(
        &file,
        "# Plan\n\nA paragraph of prose.\n\n## Alpha\n\n- [ ] first RETITLED ^first\n  the first body\n- [ ] second ^second\n",
    )
    .unwrap();
    let second_uri = format!("{uri}#second");
    db.write_txn("cli", move |conn, meta| {
        let id = binding::item_for_uri(conn, &second_uri)?.expect("bound");
        task::set_status(conn, meta, id, jkb_types::TaskStatus::Done)
    })
    .unwrap();

    let report = sync(&db, "docs/plan").unwrap();
    assert_eq!(
        report.count(Outcome::Merged),
        1,
        "expected a three-way merge"
    );

    let text = fs::read_to_string(&file).unwrap();
    assert!(
        text.contains("A paragraph of prose."),
        "prose was deleted: {text}"
    );
    assert!(
        text.contains("# Plan"),
        "the title line was deleted: {text}"
    );
    assert!(
        text.contains("  the first body"),
        "a task body was deleted: {text}"
    );
    assert!(
        text.contains("first RETITLED"),
        "the disk edit was lost: {text}"
    );
    assert!(
        text.contains("- [x] second"),
        "the KB edit was lost: {text}"
    );
    // Blank lines survive, so the document still reads the way it was written.
    assert!(
        text.matches("\n\n").count() >= 3,
        "blank lines were stripped: {text:?}"
    );
    // And it settles rather than flip-flopping.
    assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::UpToDate), 1);
}

/// Every `##` header wedged into surrounding content — one whose neighbouring line is
/// non-blank, meaning it split an item or its body instead of standing between blocks.
///
/// The shape actually observed was a header sitting between a task line and that task's own
/// indented continuation, so checking only the line *before* (or only the line after) misses
/// it. Both sides must be clear.
fn headers_mid_item(text: &str) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    lines
        .iter()
        .enumerate()
        .filter(|(i, l)| {
            l.starts_with("## ")
                && ((*i > 0 && !lines[i - 1].trim().is_empty())
                    || lines.get(i + 1).is_some_and(|n| !n.trim().is_empty()))
        })
        .map(|(_, l)| *l)
        .collect()
}

/// Document order is the layout, not three drifting integer sequences.
///
/// Section order used to come from `namespaces.metadata.position`, item order from
/// `placements.position`, and prose from its own ordinal — written at different times, and
/// mixed across up to three different parses by a merge. The numbers stopped describing one
/// document and a `##` header rendered into the middle of an item (twice, on a real file).
/// These are the two paths that produced it: an export rendered purely from KB state, and a
/// three-way merge.
#[test]
fn document_order_survives_kb_side_changes_and_merges() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tasks.md");
    let source = "# Plan\n\nIntro prose.\n\n## Alpha\n\n- [ ] first ^first\n  the first body\n  a second body line\n- [ ] second ^second\n\n## Beta\n\nSection prose.\n\n- [ ] third ^third\n  the third body\n";
    fs::write(&file, source).unwrap();
    let uri = uri_for(&file);

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        source,
        "byte-exact import"
    );

    // 1. A KB-side status change forces an EXPORT: the file is rendered from KB state alone,
    //    which is where section/item ordinals used to be read from different writes.
    db.write_txn("cli", {
        let u = format!("{uri}#second");
        move |conn, meta| {
            let id = binding::item_for_uri(conn, &u)?.expect("bound");
            task::set_status(conn, meta, id, jkb_types::TaskStatus::Done)
        }
    })
    .unwrap();
    assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::Exported), 1);
    let exported = fs::read_to_string(&file).unwrap();
    assert_eq!(
        exported,
        source.replace("- [ ] second ^second", "- [x] second ^second"),
        "an export must reproduce the file exactly but for the changed checkbox"
    );
    assert!(headers_mid_item(&exported).is_empty());

    // 2. Re-homing an item KB-side changes its `placements.position` — the ordinal the old
    //    render trusted. Document order must be unaffected.
    db.write_txn("cli", {
        let u = format!("{uri}#first");
        move |conn, meta| {
            let id = binding::item_for_uri(conn, &u)?.expect("bound");
            let beta = ns::ensure(conn, "docs/plan/tasks-md/beta")?;
            jkb_core::placement::set_primary(conn, meta, id, beta, 99)
        }
    })
    .unwrap();
    sync(&db, "docs/plan").unwrap();
    let after_move = fs::read_to_string(&file).unwrap();
    assert!(headers_mid_item(&after_move).is_empty(), "{after_move}");
    assert!(
        after_move.contains("Intro prose."),
        "prose lost: {after_move}"
    );
    assert!(
        after_move.contains("Section prose."),
        "prose lost: {after_move}"
    );
    assert!(
        after_move.contains("  the first body"),
        "a task body was lost: {after_move}"
    );

    // 3. A three-way merge draws items from different parses; the layout must still be one
    //    coherent document.
    let disk: String = after_move.replace("- [ ] first", "- [ ] first RETITLED");
    fs::write(&file, &disk).unwrap();
    db.write_txn("cli", {
        let u = format!("{uri}#third");
        move |conn, meta| {
            let id = binding::item_for_uri(conn, &u)?.expect("bound");
            task::set_status(conn, meta, id, jkb_types::TaskStatus::Done)
        }
    })
    .unwrap();
    let report = sync(&db, "docs/plan").unwrap();
    assert_eq!(
        report.count(Outcome::Merged),
        1,
        "expected a three-way merge, got {:?}",
        report.results.iter().map(|r| r.outcome).collect::<Vec<_>>()
    );

    let merged = fs::read_to_string(&file).unwrap();
    assert!(
        headers_mid_item(&merged).is_empty(),
        "merge misplaced a header: {merged}"
    );
    assert!(
        merged.contains("first RETITLED"),
        "disk edit lost: {merged}"
    );
    assert!(merged.contains("- [x] third"), "KB edit lost: {merged}");
    assert!(merged.contains("Intro prose."), "prose lost: {merged}");
    assert!(merged.contains("Section prose."), "prose lost: {merged}");
    assert!(merged.contains("  the third body"), "body lost: {merged}");
    assert_eq!(
        merged.matches("## ").count(),
        2,
        "sections duplicated or dropped"
    );
    // And it settles.
    assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::UpToDate), 1);
}

/// A change to how a file is *rendered* must not read as a change to what it *contains*.
///
/// Direction used to be decided on raw byte hashes, where the KB side is whatever today's
/// serializer renders. Any change to the renderer therefore moved every file's bytes while
/// no item moved, manufacturing a phantom KB edit across a whole mount at once: with the
/// disk unchanged the phantom won and exported over real work, and where the disk had also
/// moved it produced conflicts that were not conflicts.
///
/// Simulated here the way it actually happens — the stored base is bytes an older serializer
/// wrote (non-canonical for today's), while the KB's content is untouched.
#[test]
fn a_renderer_change_is_not_mistaken_for_a_content_change() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tasks.md");
    // The renderer emits modifiers in a canonical order (priority, due, tags, ...), so the
    // same document written with them in another order is byte-different but identical in
    // substance — the shape a serializer upgrade leaves behind.
    fs::write(
        &file,
        "## Alpha\n\n- [ ] first !p1 #size=small ^first\n- [ ] second ^second\n",
    )
    .unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();

    // Rewrite the journal's base to bytes that are semantically identical but not what
    // today's serializer renders — exactly the state a serializer upgrade leaves behind.
    let uri = uri_for(&file);
    let stale = "## Alpha\n\n- [ ] first #size=small !p1 ^first\n- [ ] second ^second\n";
    db.write_txn("t", {
        let uri = uri.clone();
        move |conn, meta| {
            let hash = jkb_core::blob::hash_bytes(stale.as_bytes());
            jkb_core::blob::store(conn, &hash, stale.as_bytes(), None)?;
            jkb_core::sync_state::upsert(
                conn,
                meta,
                &jkb_core::sync_state::SyncStateWrite {
                    uri: &uri,
                    serializer: "tasks",
                    status: "ok",
                    last_synced_hash: Some(&hash),
                    base_blob_hash: Some(&hash),
                    parse_error: None,
                    quarantine_blob_hash: None,
                    document: None,
                },
            )
        }
    })
    .unwrap();

    // Neither side changed in substance. The skew is settled once, and the document is
    // untouched — only the stale base is brought up to today's canonical rendering.
    let before = fs::read_to_string(&file).unwrap();
    let report = sync(&db, "docs/plan").unwrap();
    assert_eq!(
        report.count(Outcome::Normalized),
        1,
        "a renderer-only difference must normalize, not import/export/conflict, got {:?}",
        report.results.iter().map(|r| r.outcome).collect::<Vec<_>>()
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        before,
        "the file was already canonical, so normalizing must not touch it"
    );
    // Self-healing: having settled once, the byte fast path hits again.
    assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::UpToDate), 1);

    // And with the DISK also edited, the one real change wins outright instead of colliding
    // with the phantom and producing a conflict.
    fs::write(
        &file,
        "## Alpha\n\n- [ ] first RETITLED !p1 #size=small ^first\n- [ ] second ^second\n",
    )
    .unwrap();
    let report = sync(&db, "docs/plan").unwrap();
    assert_eq!(
        report.count(Outcome::Imported),
        1,
        "the disk's real edit must simply win, got {:?}",
        report.results.iter().map(|r| r.outcome).collect::<Vec<_>>()
    );
    assert_eq!(report.conflicts().len(), 0);
    assert!(fs::read_to_string(&file)
        .unwrap()
        .contains("first RETITLED"));
}

/// Normalizing rewrites the FILE when its bytes are not today's canonical rendering, so the
/// skew a serializer upgrade leaves behind is settled once instead of being re-derived on
/// every future sync. The document must be unchanged — only its formatting.
#[test]
fn a_non_canonical_file_is_normalized_once_then_fast_paths() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tasks.md");
    // Canonical modifier order is priority, due, tags. Written the other way round, this is
    // the same document rendered differently.
    let non_canonical = "## Alpha\n\n- [ ] first #size=small !p1 ^first\n";
    fs::write(&file, non_canonical).unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    // The first sight of a file imports it, which already writes the canonical form back.
    sync(&db, "docs/plan").unwrap();
    let canonical = fs::read_to_string(&file).unwrap();
    assert!(
        canonical.contains("!p1 #size=small"),
        "import normalizes: {canonical}"
    );

    // Put the non-canonical bytes back on disk *and* in the journal, which is the state an
    // upgraded serializer leaves: both sides agree, neither matches today's rendering.
    fs::write(&file, non_canonical).unwrap();
    let uri = uri_for(&file);
    db.write_txn("t", {
        let uri = uri.clone();
        move |conn, meta| {
            let hash = jkb_core::blob::hash_bytes(non_canonical.as_bytes());
            jkb_core::blob::store(conn, &hash, non_canonical.as_bytes(), None)?;
            jkb_core::sync_state::upsert(
                conn,
                meta,
                &jkb_core::sync_state::SyncStateWrite {
                    uri: &uri,
                    serializer: "tasks",
                    status: "ok",
                    last_synced_hash: Some(&hash),
                    base_blob_hash: Some(&hash),
                    parse_error: None,
                    quarantine_blob_hash: None,
                    document: None,
                },
            )
        }
    })
    .unwrap();

    // One normalize: the file is rewritten to the canonical form, same document.
    let report = sync(&db, "docs/plan").unwrap();
    assert_eq!(
        report.count(Outcome::Normalized),
        1,
        "got {:?}",
        report.results.iter().map(|r| r.outcome).collect::<Vec<_>>()
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), canonical);
    // The task itself is untouched — normalizing is formatting, never content.
    assert_eq!(
        status_of(&db, &format!("{uri}#first")).as_deref(),
        Some("open")
    );

    // Settled: no further writes, and the byte fast path hits from here on.
    for _ in 0..3 {
        assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::UpToDate), 1);
        assert_eq!(fs::read_to_string(&file).unwrap(), canonical);
    }
}

/// Two `tasks` files in one directory both sync, each keeping its own document (design D39.4).
///
/// This is the case that used to be **refused**. `namespace_for` derived a file's namespace
/// from its containing directory and dropped the filename, so siblings shared the `layout`
/// that `render` treats as the sole authority on document order — and syncing a directory
/// holding `tasks.md` beside any other markdown file exported the *same* rendered bytes over
/// both. Real damage, not hypothetical: 30 files collapsed onto 10 documents and 62 lost every
/// markdown header.
///
/// Seven guards over eight review passes tried to keep answering "whose layout is this?".
/// Giving each file its own namespace means there is nothing to answer.
#[test]
fn two_tasks_files_in_one_directory_each_keep_their_own_document() {
    let dir = tempfile::tempdir().unwrap();
    let tasks = dir.path().join("tasks.md");
    let design = dir.path().join("design.md");
    fs::write(&tasks, "## Plan\n\n- [ ] ship it !p1\n").unwrap();
    fs::write(&design, "## Notes\n\n- [ ] think about it !p2\n").unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);

    let report = sync(&db, "docs/plan").unwrap();
    assert_eq!(report.count(Outcome::Created), 2, "both files sync");

    // Each document is intact and distinct — the collapse, asserted absent.
    let tasks_text = fs::read_to_string(&tasks).unwrap();
    assert!(
        tasks_text.starts_with("## Plan\n\n- [ ] ship it !p1 ^ship-it-"),
        "tasks.md must keep its own section and item: {tasks_text}"
    );
    assert!(
        fs::read_to_string(&design).unwrap().contains("## Notes"),
        "design.md must keep its own header, not tasks.md's"
    );
    assert!(
        !fs::read_to_string(&design).unwrap().contains("ship it"),
        "design.md must not have been given tasks.md's items"
    );

    // Each file has its own namespace, named after it.
    for name in ["tasks.md", "design.md"] {
        let path = format!("docs/plan/{name}");
        assert!(
            db.read(move |conn| ns::get(conn, &path)).unwrap().is_some(),
            "{name} must own a namespace"
        );
    }

    // And a second run is a no-op, so nothing about this is oscillating.
    let again = sync(&db, "docs/plan").unwrap();
    assert_eq!(again.count(Outcome::UpToDate), 2);
}

/// Deleting a sibling must never overwrite the survivor with the dead file's content.
///
/// This is the pass-3/4/8 regression — the same failure found three times, each time with the
/// guard moved to a different condition. It is now unreachable by construction rather than by
/// a guard, and is asserted directly so a future change to `namespace_for` fails here.
#[test]
fn deleting_a_sibling_never_overwrites_the_survivor() {
    let dir = tempfile::tempdir().unwrap();
    let tasks = dir.path().join("tasks.md");
    let design = dir.path().join("design.md");
    fs::write(&tasks, "## Plan\n\n- [ ] ship it !p1\n").unwrap();
    fs::write(&design, "## Design\n\n- [ ] decide the shape !p2\n").unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();

    // The documented recovery procedure from the old collision refusal: delete one of them.
    fs::remove_file(&design).unwrap();
    sync(&db, "docs/plan").unwrap();

    let survivor = fs::read_to_string(&tasks).unwrap();
    assert!(
        survivor.contains("## Plan") && survivor.contains("ship it"),
        "the survivor must keep its own document: {survivor}"
    );
    assert!(
        !survivor.contains("## Design") && !survivor.contains("decide the shape"),
        "the survivor must NOT be given the deleted sibling's content: {survivor}"
    );
}

/// A legacy journal row — structure still in the namespace tree, `document` NULL — is populated
/// once from the file's own base blob, and the run after that is an ordinary no-op (D45.6).
#[test]
fn a_legacy_journal_row_is_populated_once_from_its_own_base() {
    let dir = tempfile::tempdir().unwrap();
    let tasks = dir.path().join("tasks.md");
    fs::write(&tasks, "## Plan\n\nSome prose.\n\n- [ ] ship it !p1\n").unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();
    let settled = fs::read_to_string(&tasks).unwrap();

    // Rewind to the pre-D45 shape: the journal knows the hashes but not the structure.
    db.write_txn("t", |conn, _m| {
        conn.execute("UPDATE sync_state SET document = NULL", [])?;
        Ok(())
    })
    .unwrap();

    // The file must be untouched, and the structure must come back.
    let report = sync(&db, "docs/plan").unwrap();
    assert_eq!(
        report.count(Outcome::Refused),
        0,
        "a legacy row must repopulate, not refuse"
    );
    assert_eq!(
        fs::read_to_string(&tasks).unwrap(),
        settled,
        "populating structure must not rewrite the file"
    );

    let doc: Option<String> = db
        .read(|conn| Ok(conn.query_row("SELECT document FROM sync_state", [], |r| r.get(0))?))
        .unwrap();
    let doc = doc.expect("document populated");
    assert!(doc.contains("Some prose."), "prose recovered: {doc}");
    assert!(doc.contains("## Plan"), "header recovered: {doc}");

    assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::UpToDate), 1);
}

/// Renaming a file's namespace must not let the next sync strip the file (D45.4).
///
/// `jkb ns mv` — and one click of the VS Code Rename button — used to make the structure
/// unreachable, after which the export arm wrote a structureless render over the file. Structure
/// now lives on the journal row, keyed by the file's uri, so the namespace tree cannot reach it.
#[test]
fn renaming_a_files_namespace_does_not_strip_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let tasks = dir.path().join("tasks.md");
    fs::write(
        &tasks,
        "## Plan\n\nProse that must survive.\n\n- [ ] ship it !p1\n",
    )
    .unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();
    let before = fs::read_to_string(&tasks).unwrap();

    // Move the file's whole namespace out from under it, the way `jkb ns mv` does.
    db.write_txn("t", |conn, meta| {
        ns::move_subtree(conn, meta, "docs/plan/tasks.md", "docs/elsewhere")?;
        Ok(())
    })
    .unwrap();

    sync(&db, "docs/plan").unwrap();
    assert_eq!(
        fs::read_to_string(&tasks).unwrap(),
        before,
        "the file must be byte-identical after its namespace was renamed away"
    );
}

/// An export must refuse rather than delete the lines of items that are still bound (D45.5).
///
/// `assemble_kb_doc` skips a bound item with no primary placement, and the export arm then
/// writes that render over the file — so `jkb undo` after a re-home silently deletes task lines.
#[test]
fn an_export_refuses_when_a_bound_items_line_would_vanish() {
    let dir = tempfile::tempdir().unwrap();
    let tasks = dir.path().join("tasks.md");
    fs::write(&tasks, "## Plan\n\n- [ ] keep me !p1\n- [ ] and me !p2\n").unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();
    let before = fs::read_to_string(&tasks).unwrap();

    // Strip one bound item's primary placement — exactly what `jkb undo` after a re-home leaves
    // behind — and make a KB-side change so an export is attempted.
    db.write_txn("t", |conn, _m| {
        let victim: i64 = conn.query_row(
            "SELECT i.id FROM items i JOIN bindings b ON b.item_id = i.id
              WHERE i.content LIKE '%keep me%'",
            [],
            |r| r.get(0),
        )?;
        conn.execute(
            "DELETE FROM placements WHERE item_id = ?1 AND role = 'primary'",
            [victim],
        )?;
        conn.execute(
            "UPDATE items SET status = 'done' WHERE content LIKE '%and me%'",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let report = sync(&db, "docs/plan").unwrap();
    assert_eq!(
        report.count(Outcome::Refused),
        1,
        "the export must refuse, not silently drop a line"
    );
    assert_eq!(
        fs::read_to_string(&tasks).unwrap(),
        before,
        "a refusal writes nothing at all"
    );
    let (_, reason) = report.refused()[0];
    assert!(
        reason.contains("missing from the assembled document"),
        "the refusal must say why: {reason}"
    );
}

/// A legitimate KB-only edit still exports — the guard must not block the ordinary path.
#[test]
fn a_kb_only_status_change_still_exports() {
    let dir = tempfile::tempdir().unwrap();
    let tasks = dir.path().join("tasks.md");
    fs::write(&tasks, "## Plan\n\n- [ ] ship it !p1\n").unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();

    db.write_txn("t", |conn, _m| {
        conn.execute("UPDATE items SET status = 'done' WHERE kind = 'task'", [])?;
        Ok(())
    })
    .unwrap();

    let report = sync(&db, "docs/plan").unwrap();
    assert_eq!(
        report.count(Outcome::Exported),
        1,
        "the ordinary path works"
    );
    let after = fs::read_to_string(&tasks).unwrap();
    assert!(
        after.contains("- [x]"),
        "the status reached the file: {after}"
    );
    assert!(after.contains("## Plan"), "structure survived: {after}");
}

/// `jkb undo` after a sync must rewind structure and hashes **together** (D45.2).
///
/// The document now lives on the journal row beside the hashes, so restoring one without the
/// other would undo a sync into a KB that disagrees with its own base — the state every export
/// bug in this subsystem grew out of.
#[test]
fn undoing_a_sync_rewinds_structure_with_the_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let tasks = dir.path().join("tasks.md");
    fs::write(&tasks, "## One\n\n- [ ] first !p1\n").unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();

    let read_row = || -> (Option<String>, Option<String>) {
        db.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT document, last_synced_hash FROM sync_state",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .unwrap_or((None, None)))
        })
        .unwrap()
    };
    let (doc_v1, hash_v1) = read_row();
    assert!(doc_v1.as_ref().is_some_and(|d| d.contains("## One")));

    // A second sync with different structure.
    fs::write(&tasks, "## Two\n\nNew prose.\n\n- [ ] first !p1\n").unwrap();
    sync(&db, "docs/plan").unwrap();
    let (doc_v2, hash_v2) = read_row();
    assert_ne!(doc_v1, doc_v2, "structure moved on");
    assert_ne!(hash_v1, hash_v2);

    db.write_txn("t", jkb_core::undo::undo_last).unwrap();

    let (doc_after, hash_after) = read_row();
    assert_eq!(
        (doc_after, hash_after),
        (doc_v1, hash_v1),
        "structure and hashes must rewind together, not one without the other"
    );
}

/// `kb_wins` writes the KB side over a disk that changed structurally — a path the export
/// property does NOT cover (D45.4), so it is asserted on bytes rather than assumed.
#[test]
fn kb_wins_over_a_structurally_changed_disk_keeps_the_items() {
    let dir = tempfile::tempdir().unwrap();
    let tasks = dir.path().join("tasks.md");
    fs::write(&tasks, "## Plan\n\n- [ ] shared !p1\n").unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::KbWins);
    sync(&db, "docs/plan").unwrap();

    // Both sides change: the disk gains a section, the KB closes the task.
    fs::write(
        &tasks,
        "## Plan\n\n## Extra\n\n- [ ] shared !p1\n- [ ] disk only !p2\n",
    )
    .unwrap();
    db.write_txn("t", |conn, _m| {
        conn.execute(
            "UPDATE items SET status = 'done' WHERE content LIKE '%shared%'",
            [],
        )?;
        Ok(())
    })
    .unwrap();

    let report = sync(&db, "docs/plan").unwrap();
    assert_eq!(
        report.count(Outcome::Refused),
        0,
        "a legitimate kb_wins resolution must not be refused"
    );
    let after = fs::read_to_string(&tasks).unwrap();
    assert!(
        after.contains("[x] shared"),
        "the KB side won, as kb_wins means: {after}"
    );
}

/// Following the refusal's own advice must not destroy the data the refusal protected.
///
/// The refusal says "edit the file". That edit makes both sides differ, which routes the file
/// into the **merge** arm — which had no item guard at all, so `apply_doc` cancelled the item and
/// `write_file` dropped its line. The tool's instructions were the exploit.
#[test]
fn editing_a_refused_file_does_not_delete_the_protected_line() {
    let dir = tempfile::tempdir().unwrap();
    let tasks = dir.path().join("tasks.md");
    fs::write(&tasks, "## Plan\n\n- [ ] keep me !p1\n- [ ] and me !p2\n").unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();

    // Break one item's primary placement, as `jkb undo` after a re-home does.
    db.write_txn("t", |conn, _m| {
        let victim: i64 = conn.query_row(
            "SELECT i.id FROM items i JOIN bindings b ON b.item_id = i.id
              WHERE i.content LIKE '%keep me%'",
            [],
            |r| r.get(0),
        )?;
        conn.execute(
            "DELETE FROM placements WHERE item_id = ?1 AND role = 'primary'",
            [victim],
        )?;
        conn.execute(
            "UPDATE items SET status = 'done' WHERE content LIKE '%and me%'",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::Refused), 1);

    // Now do exactly what the refusal tells the user to do: edit the file.
    let current = fs::read_to_string(&tasks).unwrap();
    fs::write(&tasks, format!("{current}- [ ] newly added !p3\n")).unwrap();
    sync(&db, "docs/plan").unwrap();

    let after = fs::read_to_string(&tasks).unwrap();
    assert!(
        after.contains("keep me"),
        "editing a refused file must not delete the line the refusal protected: {after}"
    );

    // …and the refusal must actually be CLEARABLE. Asserting only the first edit was the gap:
    // the guard judged expectation from the base, so the item stayed "expected" no matter what
    // the file said, every later reconcile refused again, and the edit above was never imported.
    // Deleting the offending line is the remedy the message prints, so it has to work.
    let text = fs::read_to_string(&tasks).unwrap();
    let mut without = String::new();
    for line in text.lines().filter(|l| !l.contains("keep me")) {
        without.push_str(line);
        without.push('\n');
    }
    fs::write(&tasks, &without).unwrap();

    let report = sync(&db, "docs/plan").unwrap();
    assert_eq!(
        report.count(Outcome::Refused),
        0,
        "deleting the line must clear the refusal — the printed remedy has to take effect"
    );
    // Asserted against the KB, not against the bytes this test just wrote — re-reading the file
    // would pass identically if the engine had done nothing at all.
    let (added, detached): (i64, i64) = db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT
                   (SELECT count(*) FROM items i JOIN bindings b ON b.item_id = i.id
                     WHERE i.content LIKE '%newly added%' AND b.uri LIKE 'file://%'),
                   (SELECT count(*) FROM items i JOIN bindings b ON b.item_id = i.id
                     WHERE i.content LIKE '%keep me%' AND b.uri = 'managed:')",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .unwrap();
    assert_eq!(
        added, 1,
        "the edit made while refused must finally IMPORT — bound as a real item, not just \
         present in bytes the test wrote"
    );
    assert_eq!(
        detached, 1,
        "the item the user deleted must be detached to `managed:` by apply_doc"
    );
}

/// A section the file stops declaring must stop being a section (`retire_undeclared_sections`).
///
/// Asserted on the **marker**, not the render: since D45 nothing renders from namespaces, so a
/// render-based assertion passes even when retirement has become a no-op — which is exactly what
/// re-keying it from `header_line` to `sync_section` risked.
#[test]
fn a_section_the_file_drops_stops_being_a_section() {
    let dir = tempfile::tempdir().unwrap();
    let tasks = dir.path().join("tasks.md");
    fs::write(&tasks, "## Keep\n\n- [ ] a !p1\n\n## Drop\n\n- [ ] b !p2\n").unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();

    let marked = |ns: &str| -> bool {
        let ns = ns.to_owned();
        db.read(move |conn| {
            let Some(id) = ns::get(conn, &ns)? else {
                return Ok(false);
            };
            Ok(ns::get_metadata(conn, id)?
                .and_then(|m| m.get("sync_section").cloned())
                .is_some())
        })
        .unwrap()
    };
    assert!(marked("docs/plan/tasks.md/keep"));
    assert!(marked("docs/plan/tasks.md/drop"));

    // The file stops declaring `## Drop`.
    fs::write(&tasks, "## Keep\n\n- [ ] a !p1\n").unwrap();
    sync(&db, "docs/plan").unwrap();

    assert!(marked("docs/plan/tasks.md/keep"), "the kept section stays");
    assert!(
        !marked("docs/plan/tasks.md/drop"),
        "the dropped section must lose its marker, or retirement has silently become a no-op"
    );
}

/// Whatever a sync overwrites is recoverable from the blob archive (design D25).
///
/// Asserted on the **merge** path, which never carried the rule: it was hand-placed at one of
/// four `write_file` sites, so three could destroy bytes no blob held — and the one that had it
/// archived inside the reconcile's own transaction, where a later failure rolled the archive
/// back while the file stayed overwritten. The archive now happens once per reconcile, in its
/// own committed transaction, before anything can write.
#[test]
fn what_a_sync_overwrites_is_recoverable_from_the_archive() {
    let dir = tempfile::tempdir().unwrap();
    let tasks = dir.path().join("tasks.md");
    fs::write(&tasks, "## Plan\n\n- [ ] first !p1\n").unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::KbWins);
    sync(&db, "docs/plan").unwrap();

    // Both sides change, so the reconcile rewrites the file.
    let settled = fs::read_to_string(&tasks).unwrap();
    let doomed = settled.replace("## Plan\n", "## Plan\n\nProse the user typed.\n");
    fs::write(&tasks, &doomed).unwrap();
    db.write_txn("t", |conn, _m| {
        conn.execute(
            "UPDATE items SET status = 'done' WHERE content LIKE '%first%'",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    sync(&db, "docs/plan").unwrap();

    // Whatever happened to the file, the bytes that were on disk beforehand are in the archive.
    let hash = jkb_core::blob::hash_bytes(doomed.as_bytes());
    let recovered = db
        .read(move |conn| jkb_core::blob::load(conn, &hash))
        .unwrap();
    assert_eq!(
        recovered.as_deref(),
        Some(doomed.as_bytes()),
        "the pre-sync bytes must be recoverable — `jkb blob ls --contains` is the whole story"
    );
}

/// A file whose bytes cannot be archived is left alone, not overwritten (design D25).
///
/// This is the branch the previous commit existed to add and did not pin — the regression it
/// keeps taking is `let _ = archive_current_bytes(...)`, which silently downgrades the
/// "every overwrite is recoverable" guarantee to best-effort exactly when the database is
/// contended. Forced here by making the file unreadable, which is the same class as a failed
/// archive write: bytes we cannot copy must not be destroyed.
#[cfg(unix)]
#[test]
fn a_file_whose_bytes_cannot_be_archived_is_not_overwritten() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let tasks = dir.path().join("tasks.md");
    fs::write(&tasks, "## Plan\n\n- [ ] ship it !p1\n").unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::KbWins);
    sync(&db, "docs/plan").unwrap();
    let before = fs::read_to_string(&tasks).unwrap();

    // A KB-side change, so the next reconcile wants to write the file…
    db.write_txn("t", |conn, _m| {
        conn.execute("UPDATE items SET status = 'done' WHERE kind = 'task'", [])?;
        Ok(())
    })
    .unwrap();
    // …and the bytes become unreadable, so they cannot be archived first.
    fs::set_permissions(&tasks, std::fs::Permissions::from_mode(0o000)).unwrap();

    let report = sync(&db, "docs/plan").unwrap();
    fs::set_permissions(&tasks, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(
        report.count(Outcome::Failed),
        1,
        "an unarchivable file must be reported as failed, not silently written"
    );
    assert_eq!(
        fs::read_to_string(&tasks).unwrap(),
        before,
        "the file must be untouched when its bytes could not be archived"
    );
    let flagged = db.read(jkb_core::sync_state::needs_attention).unwrap();
    assert!(
        flagged.iter().any(|s| s
            .parse_error
            .as_deref()
            .is_some_and(|e| e.contains("archive"))),
        "and the journal must say why: {flagged:?}"
    );
}

/// An export-only mount meeting a file that ALREADY EXISTS on disk, with no journal row yet.
///
/// This is the first sight of a file the mount will never import, so the KB is authoritative and
/// the correct answer is to export over it. The engine routed this through the same helper as the
/// genuinely-absent-file case, which told the write seam to expect no file at all — so the seam
/// refused, reporting "changed on disk while it was being synced" about a file that had not
/// changed at all. An export-only mount could therefore never write its first file, and said
/// something false about why.
#[test]
fn an_export_only_mount_overwrites_a_file_it_has_never_imported() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("note.md");
    fs::write(&file, "stale bytes on disk").unwrap();
    let uri = uri_for(&file);

    let db = Db::open_in_memory().unwrap();
    mount_dir(
        &db,
        "docs/out",
        dir.path(),
        SyncMode::Export,
        "document",
        Some("**/*.md"),
        None,
        ConflictPolicy::Manual,
    );

    // A KB item bound to that path, created without ever importing the file.
    let bound_uri = uri.clone();
    db.write_txn("t", move |conn, meta| {
        let ns_id = ns::ensure(conn, "docs/out")?;
        let id = item::upsert(
            conn,
            meta,
            &jkb_core::item::NewItem {
                uid: "doc:note".to_owned(),
                kind: "document".to_owned(),
                content: Some("authored in the kb".to_owned()),
                content_hash: None,
                mime: None,
            },
        )?;
        jkb_core::placement::place(conn, meta, id, ns_id, jkb_types::PlacementRole::Primary, 0)?;
        binding::set(
            conn,
            meta,
            id,
            &bound_uri,
            Some(SyncMode::Export),
            Some("document"),
        )?;
        Ok(())
    })
    .unwrap();

    let report = sync(&db, "docs/out").unwrap();
    assert!(
        report.failed().is_empty(),
        "an export-only mount refused its first export: {:?}",
        report.failed()
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "authored in the kb",
        "the KB side must overwrite a file this mount never imports"
    );
    // The disk side is never read back into the KB on an export-only mount.
    assert_eq!(
        content_for(&db, &uri).as_deref(),
        Some("authored in the kb")
    );
}

/// Read one task's status by uid.
fn kb_status(db: &Db, uid: &str) -> Option<String> {
    let uid = uid.to_owned();
    db.read(move |conn| {
        let Some(id) = item::id_for_uid(conn, &uid)? else {
            return Ok(None);
        };
        Ok(item::get(conn, id)?.and_then(|m| m.status))
    })
    .unwrap()
}

/// An export-only mount must never take item edits FROM disk — not even inside a three-way
/// merge, where the disk and KB edits touch different tasks and so merge cleanly.
///
/// `finish_import` carried the `ctx.imports()` check and the `Merged` arm did not, so a hand
/// edit deleting a line cancelled and detached the KB task behind it: the merged document
/// simply lacked that item, and applying it to the KB is what cancels a removed task. The
/// mount's whole contract is that the KB is authoritative and the file is an output.
#[test]
fn an_export_only_mount_never_cancels_a_task_a_disk_edit_removed() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tasks.md");
    fs::write(&file, TASKS_MD).unwrap();
    let uri = uri_for(&file);

    // Import once, so the KB holds the tasks and the file is settled.
    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    sync(&db, "docs/plan").unwrap();
    assert_eq!(
        task_count(&db),
        3,
        "setup: the three tasks should have imported"
    );

    // From here the mount is export-only: the KB is authoritative, the file is an output.
    mount_dir(
        &db,
        "docs/plan",
        dir.path(),
        SyncMode::Export,
        "tasks",
        Some("**/*.md"),
        None,
        ConflictPolicy::Manual,
    );

    // Both sides change, on DIFFERENT items — which is exactly what merges cleanly and so
    // reaches the `Merged` arm rather than a conflict.
    let edited = TASKS_MD.replace("- [ ] Fix flaky test !p1 needs:^setup ^fix\n", "");
    assert_ne!(edited, TASKS_MD, "setup: the disk edit must remove a line");
    fs::write(&file, &edited).unwrap();
    kb_set_status(&db, &format!("{uri}#ship"), "in_progress");

    let report = sync(&db, "docs/plan").unwrap();
    assert!(
        report.failed().is_empty(),
        "the export-only reconcile failed: {:?}",
        report.failed()
    );

    // The harm: the task behind the deleted line must still be open.
    assert_eq!(
        kb_status(&db, &format!("{uri}#fix")).as_deref(),
        Some("open"),
        "a hand edit to the file cancelled a KB task on a mount that does not import"
    );
    // And the KB is written back over the file, so the line returns.
    assert!(
        fs::read_to_string(&file).unwrap().contains("^fix"),
        "the export-only mount did not rewrite the file from the KB"
    );
}

/// Undo, then sync, must never strip the file. This is the harm assertion the previous fix
/// lacked: bytes on disk, not a journal field.
///
/// `jkb undo` of a sync removes the items. If the journal keeps describing them —
/// `base_blob_hash` and `document` surviving while only `last_synced_hash` is cleared — the next
/// reconcile finds the disk unchanged against that base and the KB now empty, takes the export
/// arm, and writes an item-less render over the file. Undo is supposed to give work back.
#[test]
fn undoing_a_sync_then_re_syncing_does_not_strip_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tasks.md");
    fs::write(&file, TASKS_MD).unwrap();

    let db = Db::open_in_memory().unwrap();
    mount_tasks(&db, dir.path(), ConflictPolicy::Manual);
    assert_eq!(sync(&db, "docs/plan").unwrap().count(Outcome::Created), 1);
    assert_eq!(task_count(&db), 3, "setup: the tasks should have imported");

    // Undo the sync the way a user reaching for `jkb undo` would: the trailing mirror
    // transaction, then the reconcile itself. Not more — a third would start unwinding the mount
    // and the test would be measuring something else.
    for _ in 0..2 {
        db.write_txn("cli", jkb_core::undo::undo_last)
            .expect("undo should succeed");
    }

    // Whatever the undo did to the KB, the next reconcile must not damage the file.
    let report = sync(&db, "docs/plan").unwrap();
    assert!(
        report.failed().is_empty(),
        "the reconcile after undo failed: {:?}",
        report.failed()
    );
    let after = fs::read_to_string(&file).unwrap();
    assert!(
        after.contains("Set up CI")
            && after.contains("Fix flaky test")
            && after.contains("Ship button"),
        "syncing after an undo stripped task lines from the file:\n{after}"
    );
}

// ---------------------------------------------------------------------------------------------
// The mount-mode matrix.
//
// Two consecutive review passes found the same shape of must-fix — "this arm behaves differently
// on an export-only mount and nothing tested that axis" — at two different arms. Every reconcile
// arm branches on `ctx.imports()` or `ctx.exports()` somewhere below it, and until this table the
// suite exercised `bidirectional` almost exclusively, so the export-only and import-only halves of
// each arm were reached by a handful of hand-written cases and no rule.
//
// The invariant asserted in every cell is the one D45 is about: **a sync must not delete an item
// line the KB knows about.** Not an outcome — outcomes legitimately differ per mode, and pinning
// them would make this a change-detector. What must never differ is that the file keeps the work.
// ---------------------------------------------------------------------------------------------

/// What has happened to the file and the KB by the time the sync under test runs.
#[derive(Clone, Copy, Debug)]
enum Stage {
    /// No journal row: this mount has never seen the file.
    FirstSight,
    /// Imported once and untouched since.
    Settled,
    /// Only the file moved.
    DiskChanged,
    /// Only the KB moved.
    KbChanged,
    /// Both moved, on different items — the arm that merges rather than conflicts.
    BothChanged,
    /// `jkb undo` of the import: the items and their bindings are gone, the file is not.
    PostUndo,
}

/// The three ids `TASKS_MD` declares — the work the KB is holding on the file's behalf.
const MATRIX_IDS: [&str; 3] = ["^setup", "^fix", "^ship"];

fn mount_mode(db: &Db, dir: &Path, mode: SyncMode) {
    mount_dir(
        db,
        "docs/plan",
        dir,
        mode,
        "tasks",
        Some("**/*.md"),
        None,
        ConflictPolicy::Manual,
    );
}

/// Drive one cell and return what the file looked like before and after the sync under test.
fn matrix_case(mode: SyncMode, stage: Stage) -> (jkb_sync::SyncReport, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("tasks.md");
    fs::write(&file, TASKS_MD).unwrap();
    let uri = uri_for(&file);

    let db = Db::open_in_memory().unwrap();
    // Every stage but `FirstSight` needs the file already imported, which an export-only mount
    // cannot do — so the setup import always runs bidirectional and the mode under test is
    // applied afterwards. That is also how a real export-only mount comes to hold anything.
    mount_mode(&db, dir.path(), SyncMode::Bidirectional);
    if !matches!(stage, Stage::FirstSight) {
        assert_eq!(
            sync(&db, "docs/plan").unwrap().count(Outcome::Created),
            1,
            "setup: the import that seeds every non-first-sight stage"
        );
        assert_eq!(
            task_count(&db),
            3,
            "setup: three tasks should have imported"
        );
    }
    // Before the mode is applied, because `mount_dir` is itself a write transaction: undoing
    // after it unwinds the mount re-creation instead of the sync, and the cell then quietly
    // measures nothing. Not hypothetical — this test's first version did exactly that and
    // reported `UpToDate` for a state that must be refused.
    if matches!(stage, Stage::PostUndo) {
        // The trailing mirror transaction, then the reconcile itself — exactly what a user
        // reaching for `jkb undo` after a sync unwinds.
        for _ in 0..2 {
            db.write_txn("cli", jkb_core::undo::undo_last)
                .expect("undo should succeed");
        }
        assert_eq!(
            task_count(&db),
            0,
            "setup: the undo should have removed the imported tasks"
        );
    }
    mount_mode(&db, dir.path(), mode);

    let disk_edit = |file: &Path| {
        let edited = format!(
            "{}- [ ] Added by hand ^added\n",
            fs::read_to_string(file).unwrap()
        );
        fs::write(file, edited).unwrap();
    };
    match stage {
        // `PostUndo` is applied above, before the mount mode changed.
        Stage::FirstSight | Stage::Settled | Stage::PostUndo => {}
        Stage::DiskChanged => disk_edit(&file),
        Stage::KbChanged => kb_set_status(&db, &format!("{uri}#ship"), "in_progress"),
        Stage::BothChanged => {
            disk_edit(&file);
            kb_set_status(&db, &format!("{uri}#ship"), "in_progress");
        }
    }

    let before = fs::read_to_string(&file).unwrap();
    let report = sync(&db, "docs/plan").unwrap();
    let after = fs::read_to_string(&file).unwrap();
    (report, before, after)
}

/// Every (mode, stage) cell: the sync completes, and the file still declares every task the KB
/// was holding for it.
#[test]
fn no_mount_mode_and_stage_loses_a_task_line() {
    for mode in [SyncMode::Import, SyncMode::Export, SyncMode::Bidirectional] {
        for stage in [
            Stage::FirstSight,
            Stage::Settled,
            Stage::DiskChanged,
            Stage::KbChanged,
            Stage::BothChanged,
            Stage::PostUndo,
        ] {
            let (report, before, after) = matrix_case(mode, stage);
            assert!(
                report.failed().is_empty(),
                "{mode:?}/{stage:?}: the reconcile errored: {:?}",
                report.failed()
            );
            for id in MATRIX_IDS {
                assert!(
                    before.contains(id),
                    "{mode:?}/{stage:?}: setup lost {id} before the sync ran:\n{before}"
                );
                assert!(
                    after.contains(id),
                    "{mode:?}/{stage:?}: the sync deleted {id} from the file.\n\
                     before:\n{before}\nafter:\n{after}\nrefused: {:?}",
                    report.refused()
                );
            }
        }
    }
}

/// The post-undo cell, stated as its own harm rather than as one row of the table.
///
/// `jkb undo` of a sync deletes the items **and their bindings** together, so every guard that
/// walks bindings to decide whether an export is safe reports "nothing would be dropped" — there
/// is nothing left to walk. On an export-only mount the reconcile then takes the export arm with
/// an item-less render and strips the file. This is the shape D45 names: an unverified KB render
/// reaching `write_file`. It must be refused, not written.
#[test]
fn an_export_only_mount_refuses_to_export_an_emptied_kb_over_a_populated_file() {
    let (report, before, after) = matrix_case(SyncMode::Export, Stage::PostUndo);
    assert_eq!(
        after, before,
        "the export wrote over a file whose items the KB had lost"
    );
    let refused = report.refused();
    assert_eq!(
        refused.len(),
        1,
        "the export was not refused, so nothing told the user why the file stopped syncing: \
         {report:?}"
    );
    assert!(
        refused[0].1.contains("the KB side of it has none"),
        "the refusal did not name the wholesale loss: {}",
        refused[0].1
    );
}

/// The seam guard is not a `tasks`-serializer rule. A `document` mount is one item per file, so
/// losing that item is the same wholesale loss — and it arrives by the same route, an export of a
/// KB that no longer holds what the file declares.
///
/// Worth its own case because every other test of this guard uses `tasks`, and "the check only
/// ever ran on one serializer" is the shape of gap this whole pass exists to close.
#[test]
fn a_document_mount_refuses_to_export_an_emptied_kb_over_a_populated_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("README.md");
    fs::write(&file, "the good copy\n").unwrap();
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
    assert_eq!(sync(&db, "docs/repo").unwrap().count(Outcome::Created), 1);

    // Delete the item the file is bound to — what an undo of the import leaves behind.
    let target = uri.clone();
    db.write_txn("cli", move |conn, meta| {
        let id = binding::item_for_uri(conn, &target)?.expect("bound item");
        item::remove(conn, meta, id, true).map(|_| ())
    })
    .unwrap();

    let report = sync(&db, "docs/repo").unwrap();
    assert!(report.failed().is_empty(), "{:?}", report.failed());
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "the good copy\n",
        "the export blanked a document whose KB item had been deleted"
    );
    assert_eq!(
        report.refused().len(),
        1,
        "nothing told the user why the file stopped syncing: {report:?}"
    );
}
