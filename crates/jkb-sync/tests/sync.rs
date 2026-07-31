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

    // First sync creates one item per task line. Prose is NOT an item — it round-trips as
    // namespace metadata (see `prose_is_not_an_item_so_a_section_cannot_outlive_the_file`).
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
                },
            )
        }
    })
    .unwrap();

    // Neither side changed in substance, so nothing must be written in either direction.
    let before = fs::read_to_string(&file).unwrap();
    let report = sync(&db, "docs/plan").unwrap();
    assert_eq!(
        report.count(Outcome::UpToDate),
        1,
        "a renderer-only difference must read as UpToDate, got {:?}",
        report.results.iter().map(|r| r.outcome).collect::<Vec<_>>()
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        before,
        "the file was rewritten"
    );

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
