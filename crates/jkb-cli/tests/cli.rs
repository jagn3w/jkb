//! CLI integration tests over a temp database via the built `jkb` binary.
//!
//! All offline: no ollama is required. Ingestion still *captures* (and is
//! keyword-searchable via FTS) when the embedder is down, so `ingest`, the `fts`
//! search route, `query`, `task`, `view`, `ns`, `sync`, `undo`, and `doctor` all run
//! without a model. The vector/hybrid search routes are not exercised here.

use std::path::Path;
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;

/// A `jkb` invocation against database `db`.
fn jkb(db: &Path) -> Command {
    let mut cmd = Command::cargo_bin("jkb").unwrap();
    cmd.arg("--db").arg(db);
    cmd
}

fn db_path(dir: &TempDir) -> std::path::PathBuf {
    dir.path().join("jkb.db")
}

#[test]
fn ingest_then_query_lists_the_document() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let note = dir.path().join("note.md");
    std::fs::write(&note, "hello world knowledge base").unwrap();

    jkb(&db)
        .args(["ingest", note.to_str().unwrap(), "--ns", "docs"])
        .assert()
        .success()
        .stdout(predicate::str::contains("document"));

    jkb(&db)
        .args(["--global", "query", "kind:document"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hello world"));
}

#[test]
fn task_add_and_next() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);

    jkb(&db)
        .args(["task", "add", "write the docs", "!p1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("added task"));

    jkb(&db)
        .args(["--global", "task", "next"])
        .assert()
        .success()
        .stdout(predicate::str::contains("write the docs"));
}

#[test]
fn task_next_json_is_an_array() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    jkb(&db).args(["task", "add", "a task"]).assert().success();

    jkb(&db)
        .args(["--global", "--json", "task", "next"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("["));
}

#[test]
fn task_show_prints_the_full_untruncated_body() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);

    // A body deliberately longer than the 80-char listing snippet.
    let body = "this task body is deliberately much longer than the eighty character \
                snippet that listings show so we can prove show returns the whole thing";
    let out = jkb(&db)
        .args(["--json", "task", "add", body])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let uid = v["uid"].as_str().unwrap().to_string();

    // Human form shows the full body (the tail past the 80-char snippet).
    jkb(&db)
        .args(["task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "prove show returns the whole thing",
        ));

    // JSON form carries the full content, not a snippet.
    let shown = jkb(&db)
        .args(["--json", "task", "show", &uid])
        .output()
        .unwrap();
    assert!(shown.status.success());
    let sj: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(sj["content"].as_str().unwrap(), body);

    // The bare slug (without the `task:` prefix) also resolves.
    let slug = uid.strip_prefix("task:").unwrap();
    jkb(&db).args(["task", "show", slug]).assert().success();

    // An unknown uid errors.
    jkb(&db)
        .args(["task", "show", "task:does-not-exist-0000"])
        .assert()
        .failure();
}

#[test]
fn view_save_list_and_run() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    jkb(&db)
        .args(["task", "add", "review pr"])
        .assert()
        .success();

    jkb(&db)
        .args(["view", "save", "mytasks", "kind:task"])
        .assert()
        .success()
        .stdout(predicate::str::contains("saved view mytasks"));

    jkb(&db)
        .args(["view", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("mytasks"));

    jkb(&db)
        .args(["view", "run", "mytasks"])
        .assert()
        .success()
        .stdout(predicate::str::contains("review pr"));
}

#[test]
fn search_fts_route_finds_captured_content_offline() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let note = dir.path().join("note.md");
    std::fs::write(&note, "a peculiar distinctive vocabulary term").unwrap();
    jkb(&db)
        .args(["ingest", note.to_str().unwrap(), "--ns", "docs"])
        .assert()
        .success();

    jkb(&db)
        .args(["--global", "search", "--route", "fts", "peculiar"])
        .assert()
        .success()
        .stdout(predicate::str::contains("peculiar").and(predicate::str::contains("[fts")));
}

#[test]
fn mount_and_sync_imports_files() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let repo = TempDir::new().unwrap();
    std::fs::write(repo.path().join("README.md"), "sync me please").unwrap();

    jkb(&db)
        .args([
            "mount",
            "create",
            "docs/repo",
            repo.path().to_str().unwrap(),
            "--include",
            "**/*.md",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("mounted docs/repo"));

    jkb(&db)
        .args(["sync", "docs/repo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 created"));

    jkb(&db)
        .args(["--global", "query", "kind:document"])
        .assert()
        .success()
        .stdout(predicate::str::contains("sync me"));
}

#[test]
fn task_edit_appends_a_body_to_a_file_backed_task_but_refuses_a_blank_line() {
    // A file-backed task's content after the first line is its indented BODY in the source
    // file, so `--append` round-trips. Only a blank line is refused: it ends the body on
    // re-parse, so anything after it would detach from the task and drift into section prose.
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let repo = TempDir::new().unwrap();
    std::fs::write(
        repo.path().join("tasks.md"),
        "## Inbox\n\n- [ ] fix the parser bug\n",
    )
    .unwrap();

    jkb(&db)
        .args([
            "mount",
            "create",
            "work",
            repo.path().to_str().unwrap(),
            "--serializer",
            "tasks",
            "--include",
            "**/tasks.md",
        ])
        .assert()
        .success();
    jkb(&db).args(["sync", "work"]).assert().success();

    // The file-backed uid is the only `file://…` string in the JSON frontier.
    let out = jkb(&db)
        .args(["--global", "task", "next", "--json", "ns:work/**"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let start = stdout.find("file://").expect("a file-backed task uid");
    let uid = &stdout[start..start + stdout[start..].find('"').unwrap()];
    assert!(uid.contains("tasks.md#"), "unexpected uid: {uid}");

    // A multi-line replacement is fine: the extra lines become the task's body.
    jkb(&db)
        .args(["task", "edit", uid, "line one\nline two"])
        .assert()
        .success()
        .stdout(predicate::str::contains("edited"));

    // A blank line WOULD detach the tail from the task, so it is still refused.
    jkb(&db)
        .args(["task", "edit", uid, "title\n\ndetached tail"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("blank line"));

    // A single-line replacement is of course fine.
    jkb(&db)
        .args(["task", "edit", uid, "renamed", "title"])
        .assert()
        .success()
        .stdout(predicate::str::contains("edited"));

    // `--append` now works on a file-backed task, and the appended body SURVIVES a sync
    // round trip as indented lines under the task — the point of carrying it on the item.
    jkb(&db)
        .args(["task", "edit", uid, "--append", "Design: use trait X"])
        .assert()
        .success()
        .stdout(predicate::str::contains("appended"));
    jkb(&db).args(["sync", "work"]).assert().success();
    let on_disk = std::fs::read_to_string(repo.path().join("tasks.md")).unwrap();
    assert!(
        on_disk.contains("  Design: use trait X"),
        "the appended body must render indented under its task: {on_disk}"
    );
    // And it settles — no flip-flop between disk and KB.
    jkb(&db)
        .args(["sync", "work"])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 up-to-date"));
    // The body is the task's own content, so `task show` displays it.
    jkb(&db)
        .args(["task", "show", uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("Design: use trait X"));

    // The guard is specific to file-backed tasks: a managed task still appends fine.
    jkb(&db)
        .args(["task", "add", "managed task"])
        .assert()
        .success();
    let managed = jkb(&db)
        .args(["--global", "task", "next", "--json", "ns:tasks/**"])
        .output()
        .unwrap();
    let mstdout = String::from_utf8(managed.stdout).unwrap();
    let mstart = mstdout.find("task:").expect("a managed task uid");
    let muid = &mstdout[mstart..mstart + mstdout[mstart..].find('"').unwrap()];
    jkb(&db)
        .args(["task", "edit", muid, "--append", "Design: settled"])
        .assert()
        .success()
        .stdout(predicate::str::contains("appended"));
}

#[test]
fn ns_ls_shows_created_namespaces() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    jkb(&db)
        .args(["task", "add", "x", "+repos/app"])
        .assert()
        .success();

    jkb(&db)
        .args(["ns", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("repos"));
}

#[test]
fn undo_reverts_the_last_change() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    jkb(&db)
        .args(["task", "add", "throwaway"])
        .assert()
        .success();

    jkb(&db)
        .args(["undo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("reverted"));
}

#[test]
fn doctor_reports_integrity_and_version() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let backup = dir.path().join("backup.db");

    jkb(&db)
        .args(["doctor", "--backup", backup.to_str().unwrap()])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("fts integrity: ok")
                .and(predicate::str::contains("schema user_version:"))
                .and(predicate::str::contains("backup written")),
        );
    assert!(backup.exists());
}

/// Add a task via the CLI and return its minted uid (`--json` emits `{uid}`).
fn add_task(db: &Path, text: &str) -> String {
    let out = jkb(db)
        .args(["--json", "task", "add", text])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    v["uid"].as_str().unwrap().to_string()
}

/// Read one task's `status` via `task show --json`.
fn task_status(db: &Path, uid: &str) -> String {
    let out = jkb(db)
        .args(["--json", "task", "show", uid])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    v["status"].as_str().unwrap_or("").to_string()
}

#[test]
fn task_set_status_priority_due_roundtrips_and_is_undoable() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let uid = add_task(&db, "editable task");

    jkb(&db)
        .args([
            "task",
            "set",
            &uid,
            "--status",
            "in_progress",
            "--priority",
            "2",
            "--due",
            "2026-08-01",
        ])
        .assert()
        .success();
    assert_eq!(task_status(&db, &uid), "in_progress");

    // The last mutation (set due) is undoable like any other write.
    jkb(&db).args(["undo"]).assert().success();
}

#[test]
fn task_set_blocked_status_is_rejected() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let uid = add_task(&db, "a task");
    jkb(&db)
        .args(["task", "set", &uid, "--status", "blocked"])
        .assert()
        .failure();
}

#[test]
fn task_depend_excludes_from_frontier_and_refuses_cycles() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let a = add_task(&db, "task a");
    let b = add_task(&db, "task b");

    // a depends_on b → a drops out of the ready frontier, b remains.
    jkb(&db).args(["task", "depend", &a, &b]).assert().success();
    let out = jkb(&db)
        .args(["--global", "--json", "task", "next"])
        .output()
        .unwrap();
    let arr: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let uids: Vec<&str> = arr
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["uid"].as_str().unwrap())
        .collect();
    assert!(uids.contains(&b.as_str()));
    assert!(!uids.contains(&a.as_str()));

    // b depends_on a would close a cycle a→b→a: refused by the cycle guard.
    jkb(&db).args(["task", "depend", &b, &a]).assert().failure();

    // Detaching the dependency returns a to the frontier.
    jkb(&db)
        .args(["task", "undepend", &a, &b])
        .assert()
        .success();
    jkb(&db)
        .args(["--global", "task", "next"])
        .assert()
        .success()
        .stdout(predicate::str::contains("task a"));
}

#[test]
fn task_tag_add_and_remove_roundtrip() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let uid = add_task(&db, "taggable");

    jkb(&db)
        .args(["task", "tag", "add", &uid, "size=small"])
        .assert()
        .success();
    // Filtering the frontier by the tag finds it.
    jkb(&db)
        .args(["--global", "task", "next", "#size=small"])
        .assert()
        .success()
        .stdout(predicate::str::contains("taggable"));

    jkb(&db)
        .args(["task", "tag", "rm", &uid, "size=small"])
        .assert()
        .success();
    // A malformed tag is rejected.
    jkb(&db)
        .args(["task", "tag", "add", &uid, "nofacet"])
        .assert()
        .failure();
}

#[test]
fn task_place_and_bind_are_orthogonal() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let uid = add_task(&db, "placeable");

    // A reference mirror under a repo namespace makes the task visible when scoped there.
    jkb(&db)
        .args(["task", "place", &uid, "repos/app/backend"])
        .assert()
        .success();
    jkb(&db)
        .args(["task", "next", "ns:repos/app/**"])
        .assert()
        .success()
        .stdout(predicate::str::contains("placeable"));

    // Binding is the other axis: switch to a synced file uri, then back to managed.
    jkb(&db)
        .args(["task", "bind", &uid, "--sync", "file:///tmp/placeable.md"])
        .assert()
        .success();
    jkb(&db)
        .args(["task", "bind", &uid, "--managed"])
        .assert()
        .success();
}

#[test]
fn task_claim_flips_status_and_excludes_from_frontier() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let uid = add_task(&db, "claimable");

    // Claim with an explicit owner: the task becomes in_progress and leaves the frontier.
    jkb(&db)
        .args(["task", "claim", &uid, "--owner", "host:1234"])
        .assert()
        .success()
        .stdout(predicate::str::contains("in_progress"));
    assert_eq!(task_status(&db, &uid), "in_progress");
    jkb(&db)
        .args(["--global", "task", "next"])
        .assert()
        .success()
        .stdout(predicate::str::contains("claimable").not());

    // Release returns it to the frontier (status stays in_progress, claim cleared).
    jkb(&db)
        .args(["task", "release", &uid, "--owner", "host:1234"])
        .assert()
        .success();
    jkb(&db)
        .args(["--global", "task", "next"])
        .assert()
        .success()
        .stdout(predicate::str::contains("claimable"));
}

#[test]
fn doctor_reclaims_orphaned_claims_but_keeps_live_ones() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let dead = add_task(&db, "dead owner task");
    let live = add_task(&db, "live owner task");

    // `dead` is claimed by a non-existent pid; `live` by this test process (alive during
    // the doctor run). host:<pid> is the liveness-checkable owner id format.
    let live_owner = format!("host:{}", std::process::id());
    jkb(&db)
        .args(["task", "claim", &dead, "--owner", "host:4294967290"])
        .assert()
        .success();
    jkb(&db)
        .args(["task", "claim", &live, "--owner", &live_owner])
        .assert()
        .success();

    // A bare doctor run reports the orphaned claim (dead owner) and touches nothing.
    jkb(&db)
        .args(["doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("orphaned").and(predicate::str::contains("dead owner")));

    // `--fix` clears the orphaned claim; the live owner's claim is retained.
    jkb(&db)
        .args(["doctor", "--fix"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cleared 1 orphaned claim"));

    // The reclaimed task returns to the frontier; the live-owned one stays out.
    let out = jkb(&db)
        .args(["--global", "--json", "task", "next"])
        .output()
        .unwrap();
    let arr: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let uids: Vec<&str> = arr
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["uid"].as_str().unwrap())
        .collect();
    assert!(uids.contains(&dead.as_str()));
    assert!(!uids.contains(&live.as_str()));
}

#[test]
fn task_reclaim_keeps_named_owner_and_clears_dead_ones() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let dead = add_task(&db, "dead owner task");
    let kept = add_task(&db, "kept owner task");

    // `dead` is owned by a non-existent pid; `kept` by a string owner we will --keep
    // (its pid is unparseable/dead, so only --keep preserves it).
    jkb(&db)
        .args(["task", "claim", &dead, "--owner", "host:4294967290"])
        .assert()
        .success();
    jkb(&db)
        .args(["task", "claim", &kept, "--owner", "swarm-run-xyz"])
        .assert()
        .success();

    jkb(&db)
        .args(["task", "reclaim", "--keep", "swarm-run-xyz"])
        .assert()
        .success()
        .stdout(predicate::str::contains("reclaimed 1 of 2"));

    // The kept owner's task stays out of the frontier; the dead one returns.
    let out = jkb(&db)
        .args(["--global", "--json", "task", "next"])
        .output()
        .unwrap();
    let arr: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let uids: Vec<&str> = arr
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["uid"].as_str().unwrap())
        .collect();
    assert!(uids.contains(&dead.as_str()));
    assert!(!uids.contains(&kept.as_str()));
}

#[test]
fn service_print_emits_a_unit_referencing_the_watcher() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    // `service print` is a dry run — it writes nothing, just emits the unit that runs
    // `sync --watch`. Works on macOS (launchd) and Linux (systemd).
    if cfg!(any(target_os = "macos", target_os = "linux")) {
        jkb(&db)
            .args(["service", "print"])
            .assert()
            .success()
            .stdout(predicate::str::contains("--watch"));
    }
}

#[test]
fn sync_with_no_mounts_is_a_clean_noop() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    jkb(&db)
        .args(["sync"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no mounts configured"));
}

#[test]
fn help_advertises_the_mcp_subcommand() {
    // `mcp` now launches the stdio server (covered by jkb-mcp's own tests); here we
    // just confirm the CLI advertises it rather than blocking a test on stdio I/O.
    Command::cargo_bin("jkb")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("mcp"));
}

#[test]
fn index_on_empty_kb_has_nothing_to_embed() {
    // An empty KB has no content items, so `index` short-circuits before touching the
    // embedder — deterministic regardless of whether ollama is running (this suite is
    // ollama-independent). The live embed path is exercised manually / by jkb-ingest.
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    jkb(&db)
        .arg("index")
        .assert()
        .success()
        .stdout(predicate::str::contains("nothing to embed"));
}

// ---- task homing (design D26) ---------------------------------------------

#[test]
fn task_add_explicit_placement_sets_home() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);

    jkb(&db)
        .args(["task", "add", "explicit home task", "+projects/alpha"])
        .assert()
        .success();

    // The first `+<ns>` is the Primary home; there is no forced `tasks/inbox` placement.
    jkb(&db)
        .args(["--global", "query", "kind:task", "ns:projects/alpha"])
        .assert()
        .success()
        .stdout(predicate::str::contains("explicit home task"));
    jkb(&db)
        .args(["--global", "query", "kind:task", "ns:tasks/inbox"])
        .assert()
        .success()
        .stdout(predicate::str::contains("explicit home task").not());
}

#[test]
fn task_add_backlog_outside_repo_errors() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);

    // No ambient repo and stdin is not a TTY → the global fallback is declined → error.
    jkb(&db)
        .args(["task", "add", "orphan backlog", "--backlog"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--backlog needs an ambient repo"));
}

/// Mount `repo_dir` at namespace `ns` so tasks added from inside it home under `tasks/<ns>`.
fn mount_repo(db: &Path, ns: &str, repo_dir: &Path) {
    jkb(db)
        .args(["mount", "create", ns, repo_dir.to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn task_add_in_repo_homes_per_repo_inbox_and_mirror() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let repo = dir.path().join("myrepo");
    std::fs::create_dir(&repo).unwrap();
    mount_repo(&db, "repos/proj", &repo);

    jkb(&db)
        .current_dir(&repo)
        .args(["task", "add", "in repo task"])
        .assert()
        .success();

    // Primary home at the per-repo inbox, and mirrored into the global inbox (D26.3).
    jkb(&db)
        .args([
            "--global",
            "query",
            "kind:task",
            "ns:tasks/repos/proj/inbox",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("in repo task"));
    jkb(&db)
        .args(["--global", "query", "kind:task", "ns:tasks/inbox"])
        .assert()
        .success()
        .stdout(predicate::str::contains("in repo task"));
}

#[test]
fn task_add_backlog_in_repo_homes_per_repo_backlog_no_mirror() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let repo = dir.path().join("myrepo");
    std::fs::create_dir(&repo).unwrap();
    mount_repo(&db, "repos/proj", &repo);

    jkb(&db)
        .current_dir(&repo)
        .args(["task", "add", "repo backlog item", "--backlog"])
        .assert()
        .success();

    jkb(&db)
        .args([
            "--global",
            "query",
            "kind:task",
            "ns:tasks/repos/proj/.backlog",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("repo backlog item"));
    // A backlog is per-repo: not mirrored into the global inbox.
    jkb(&db)
        .args(["--global", "query", "kind:task", "ns:tasks/inbox"])
        .assert()
        .success()
        .stdout(predicate::str::contains("repo backlog item").not());
}

#[test]
fn task_next_scopes_to_the_repo_task_tree() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let repo = dir.path().join("myrepo");
    std::fs::create_dir(&repo).unwrap();
    mount_repo(&db, "repos/proj", &repo);

    // A purely-global task and a repo-scoped task.
    jkb(&db)
        .args(["task", "add", "global only task"])
        .assert()
        .success();
    jkb(&db)
        .current_dir(&repo)
        .args(["task", "add", "repo scoped task"])
        .assert()
        .success();

    // `task next` from inside the repo defaults to `tasks/repos/proj/**`.
    jkb(&db)
        .current_dir(&repo)
        .args(["task", "next"])
        .assert()
        .success()
        .stdout(predicate::str::contains("repo scoped task"))
        .stdout(predicate::str::contains("global only task").not());
}

#[test]
fn task_unplace_removes_mirror_but_keeps_the_home() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);

    // Add a task (homes at tasks/inbox) and mirror it under proj/mirror.
    let out = jkb(&db)
        .args(["task", "add", "unplace target"])
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    // "added task <uid> (item N)"
    let uid = stdout.split_whitespace().nth(2).unwrap().to_string();

    jkb(&db)
        .args(["task", "place", &uid, "proj/mirror"])
        .assert()
        .success();
    jkb(&db)
        .args(["--global", "query", "kind:task", "ns:proj/mirror"])
        .assert()
        .success()
        .stdout(predicate::str::contains("unplace target"));

    // Unplace the mirror: it goes, the tasks/inbox home stays.
    jkb(&db)
        .args(["task", "unplace", &uid, "proj/mirror"])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 mirror"));
    jkb(&db)
        .args(["--global", "query", "kind:task", "ns:proj/mirror"])
        .assert()
        .success()
        .stdout(predicate::str::contains("unplace target").not());
    jkb(&db)
        .args(["--global", "query", "kind:task", "ns:tasks/inbox"])
        .assert()
        .success()
        .stdout(predicate::str::contains("unplace target"));
}

#[test]
fn task_add_infers_synced_binding_under_a_tasks_mount() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let bk = dir.path().join("bk");
    std::fs::create_dir(&bk).unwrap();
    jkb(&db)
        .args([
            "mount",
            "create",
            "tasks/proj/.backlog",
            bk.to_str().unwrap(),
            "--serializer",
            "tasks",
        ])
        .assert()
        .success();

    // Homed under the tasks mount → a synced file binding is inferred (D26.5).
    jkb(&db)
        .args(["task", "add", "synced task", "+tasks/proj/.backlog"])
        .assert()
        .success()
        .stdout(predicate::str::contains("synced binding"));

    // Sync writes it to tasks.md; a second sync is a no-op (round-trips).
    jkb(&db)
        .args(["sync", "tasks/proj/.backlog"])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 exported"));
    let content = std::fs::read_to_string(bk.join("tasks.md")).unwrap();
    assert!(content.contains("synced task"), "file: {content}");
    assert!(content.contains("- [ ]"), "file: {content}");
    jkb(&db)
        .args(["sync", "tasks/proj/.backlog"])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 up-to-date"));
}

#[test]
fn task_add_stays_managed_without_a_tasks_mount_and_sync_errors() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);

    // No tasks mount over the home → inference keeps it managed (no synced hint).
    jkb(&db)
        .args(["task", "add", "plain managed", "+proj/x"])
        .assert()
        .success()
        .stdout(predicate::str::contains("synced binding").not());

    // --sync with no tasks mount over the home is an error.
    jkb(&db)
        .args(["task", "add", "needs sync", "+proj/x", "--sync"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no `tasks`-serializer file mount"));
}

// ---- ls + item show (UI read surface, design D31) --------------------------

#[test]
fn ls_lists_children_and_hides_terminal_tasks() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    jkb(&db)
        .args(["task", "add", "alpha task", "+proj/x"])
        .assert()
        .success();
    jkb(&db)
        .args(["task", "add", "beta task", "+proj/x"])
        .assert()
        .success();

    // Root: the `proj` namespace appears, flagged as having children.
    jkb(&db)
        .args(["ls", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"ref\": \"proj\""))
        .stdout(predicate::str::contains("\"has_children\": true"));

    // Its child namespace `proj/x` holds the two tasks as leaves.
    jkb(&db)
        .args(["ls", "proj/x", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha task"))
        .stdout(predicate::str::contains("beta task"))
        .stdout(predicate::str::contains("\"kind\": \"task\""));

    // A completed task is hidden by default, revealed with --all.
    let out = jkb(&db)
        .args(["task", "add", "gamma task", "+proj/y"])
        .output()
        .unwrap();
    let uid = String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .nth(2)
        .unwrap()
        .to_string();
    jkb(&db)
        .args(["task", "set", &uid, "--status", "done"])
        .assert()
        .success();
    jkb(&db)
        .args(["ls", "proj/y", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("gamma task").not());
    jkb(&db)
        .args(["ls", "proj/y", "--all", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("gamma task"));
}

#[test]
fn item_show_bounds_the_preview() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let long = "x".repeat(500);
    let out = jkb(&db).args(["task", "add", &long]).output().unwrap();
    let uid = String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .nth(2)
        .unwrap()
        .to_string();

    jkb(&db)
        .args(["item", "show", &uid, "--preview", "10", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"content_chars\": 500"))
        .stdout(predicate::str::contains("\"preview_truncated\": true"));
}

#[test]
fn item_edit_replaces_content() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let out = jkb(&db)
        .args(["task", "add", "before edit"])
        .output()
        .unwrap();
    let uid = String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .nth(2)
        .unwrap()
        .to_string();

    jkb(&db)
        .args(["item", "edit", &uid, "after edit"])
        .assert()
        .success();
    jkb(&db)
        .args(["item", "show", &uid, "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("after edit"))
        .stdout(predicate::str::contains("before edit").not());
}

#[test]
fn ls_sorts_namespaces_first_then_tasks_by_priority() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    jkb(&db)
        .args(["task", "add", "low pri", "+proj", "!p3"])
        .assert()
        .success();
    jkb(&db)
        .args(["task", "add", "high pri", "+proj", "!p1"])
        .assert()
        .success();
    jkb(&db)
        .args(["task", "add", "child ns task", "+proj/sub"])
        .assert()
        .success();

    // Order: namespace `sub` first, then the p1 task, then the p3 task.
    let out = jkb(&db).args(["ls", "proj", "--json"]).output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let sub = stdout.find("\"sub\"").unwrap();
    let high = stdout.find("high pri").unwrap();
    let low = stdout.find("low pri").unwrap();
    assert!(sub < high, "namespace should sort before tasks");
    assert!(high < low, "higher-priority task should sort first");
}

#[test]
fn query_count_reports_the_number_of_matches() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    jkb(&db)
        .args(["task", "add", "one", "+proj"])
        .assert()
        .success();
    jkb(&db)
        .args(["task", "add", "two", "+proj"])
        .assert()
        .success();

    jkb(&db)
        .args(["--global", "query", "kind:task", "--count"])
        .assert()
        .success()
        .stdout(predicate::str::contains("2"));
    jkb(&db)
        .args(["--global", "query", "kind:task", "--count", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"count\":2"));
}

#[test]
fn mount_ls_lists_mounts() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let repo = TempDir::new().unwrap();
    jkb(&db)
        .args([
            "mount",
            "create",
            "repos/x/docs",
            repo.path().to_str().unwrap(),
        ])
        .assert()
        .success();
    jkb(&db)
        .args(["mount", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("repos/x/docs"));
    jkb(&db)
        .args(["mount", "ls", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"namespace\": \"repos/x/docs\""));
}

#[test]
fn ns_rm_refuses_nonempty_and_removes_empty() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let out = jkb(&db)
        .args(["task", "add", "tmp task", "+proj/tmp"])
        .output()
        .unwrap();
    let uid = String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .nth(2)
        .unwrap()
        .to_string();

    // Non-empty namespace is refused.
    jkb(&db)
        .args(["ns", "rm", "proj/tmp"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("placement"));

    // Re-home the task elsewhere, emptying proj/tmp, then rm succeeds.
    jkb(&db)
        .args(["task", "place", &uid, "proj/other", "--home"])
        .assert()
        .success();
    jkb(&db).args(["ns", "rm", "proj/tmp"]).assert().success();
    jkb(&db)
        .args(["ns", "ls", "proj"])
        .assert()
        .success()
        .stdout(predicate::str::contains("tmp").not());
}

#[test]
fn task_mirror_symlinks_repo_tasks_under_tasks() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    // A task homed outside tasks/ (repo content) is auto-mirrored under tasks/ on create.
    jkb(&db)
        .args(["task", "add", "repo task", "+repos/jkb/openspec/x"])
        .assert()
        .success();
    jkb(&db)
        .args(["--global", "query", "kind:task", "ns:tasks/jkb/openspec/x"])
        .assert()
        .success()
        .stdout(predicate::str::contains("repo task"));
    // Its real home stays under repos/.
    jkb(&db)
        .args(["--global", "query", "kind:task", "ns:repos/jkb/openspec/x"])
        .assert()
        .success()
        .stdout(predicate::str::contains("repo task"));
    // A tasks/-homed task isn't mirrored; `task mirror` finds nothing new.
    jkb(&db)
        .args(["task", "add", "inbox task", "+tasks/jkb/.backlog"])
        .assert()
        .success();
    jkb(&db)
        .args(["task", "mirror"])
        .assert()
        .success()
        .stdout(predicate::str::contains("0 tasks/ mirror"));
}

#[test]
fn ns_mk_scaffolds_roots_and_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    // Create several roots at once (the setup-script scaffold).
    jkb(&db)
        .args([
            "ns",
            "mk",
            "repos",
            "tasks",
            "media",
            "references",
            "memory",
        ])
        .assert()
        .success();
    jkb(&db)
        .args(["ns", "ls"])
        .assert()
        .success()
        .stdout(predicate::str::contains("repos").and(predicate::str::contains("media")));
    // Re-running is a no-op success (idempotent), including a nested path.
    jkb(&db)
        .args(["ns", "mk", "repos", "media/transcripts"])
        .assert()
        .success();
    jkb(&db)
        .args(["ns", "ls", "media"])
        .assert()
        .success()
        .stdout(predicate::str::contains("media/transcripts"));
}

#[test]
fn grep_is_literal_scoped_and_exit_coded() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    jkb(&db)
        .args(["task", "add", "buy a 6-inch pipe", "+proj/hw"])
        .assert()
        .success();
    jkb(&db)
        .args(["task", "add", "write the DESIGN doc", "+proj/docs"])
        .assert()
        .success();

    // Literal match, scoped, exit 0, prints uid:line:text.
    jkb(&db)
        .args(["grep", "pipe", "proj"])
        .assert()
        .success()
        .stdout(predicate::str::contains(":1:buy a 6-inch pipe"));
    // Case-sensitive by default: lowercase "design" misses the uppercase title.
    jkb(&db).args(["grep", "design", "proj"]).assert().code(1);
    // -i makes it match; -l prints only the uid (no `:line:` body).
    jkb(&db)
        .args(["grep", "-i", "design", "proj", "-l"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("task:write-the-design-doc")
                .and(predicate::str::contains(":1:").not()),
        );
    // -c counts matching items.
    jkb(&db)
        .args(["grep", "-c", "pipe", "proj"])
        .assert()
        .success()
        .stdout(predicate::str::contains("1"));
    // No match anywhere → exit 1.
    jkb(&db)
        .args(["--global", "grep", "zzznope"])
        .assert()
        .code(1);
    // A scope with no match → exit 1 (pipe is under proj/hw, not proj/docs).
    jkb(&db)
        .args(["grep", "pipe", "proj/docs"])
        .assert()
        .code(1);
}

#[test]
fn ls_long_and_recursive() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    jkb(&db)
        .args(["task", "add", "alpha task", "+proj/a"])
        .assert()
        .success();
    jkb(&db)
        .args(["task", "add", "beta task", "+proj/b"])
        .assert()
        .success();

    // -l shows kind + location + label.
    jkb(&db)
        .args(["--global", "ls", "proj/a", "-l"])
        .assert()
        .success()
        .stdout(predicate::str::contains("task").and(predicate::str::contains("alpha task")));
    // -R flattens the subtree so both leaf tasks appear from the root.
    jkb(&db)
        .args(["--global", "ls", "proj", "-R"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha task").and(predicate::str::contains("beta task")));
}

#[test]
fn cat_prints_raw_body() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    jkb(&db)
        .args(["task", "add", "cat me", "+proj/x"])
        .assert()
        .success();
    let out = jkb(&db)
        .args(["grep", "cat me", "proj", "-l"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let uid = String::from_utf8(out).unwrap();
    jkb(&db)
        .args(["cat", uid.trim()])
        .assert()
        .success()
        .stdout(predicate::str::contains("cat me"));
    jkb(&db).args(["cat", "task:nope"]).assert().failure();
}

#[test]
fn tree_find_recent_stat_guide() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    jkb(&db)
        .args(["task", "add", "alpha task", "+proj/a"])
        .assert()
        .success();
    jkb(&db)
        .args(["task", "add", "beta task", "+proj/b"])
        .assert()
        .success();

    // tree: both leaf namespaces appear under the root.
    jkb(&db)
        .args(["--global", "tree", "proj"])
        .assert()
        .success()
        .stdout(predicate::str::contains("├─").and(predicate::str::contains("alpha task")));

    // find: structured filter by kind (and it's the typed complement to grep).
    jkb(&db)
        .args(["--global", "find", "proj", "--kind", "task"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha task").and(predicate::str::contains("beta task")));
    // find with a non-matching status yields nothing but still succeeds.
    jkb(&db)
        .args(["--global", "find", "proj", "--status", "done"])
        .assert()
        .success();

    // recent: newest first — beta (added later) precedes alpha in the output.
    let out = jkb(&db)
        .args(["--global", "recent", "proj"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.find("beta task").unwrap() < text.find("alpha task").unwrap());

    // stat: compact metadata, no body dump.
    let uid = String::from_utf8(
        jkb(&db)
            .args(["grep", "beta", "proj", "-l"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    jkb(&db)
        .args(["stat", uid.trim()])
        .assert()
        .success()
        .stdout(predicate::str::contains("kind:").and(predicate::str::contains("proj/b")));

    // guide: prints the cheat-sheet.
    jkb(&db)
        .args(["guide"])
        .assert()
        .success()
        .stdout(predicate::str::contains("agent quickstart"));
}

#[test]
fn find_guards_unscoped_and_tree_bounds_depth() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    // A deep chain a/b/c/d/e/f so the default tree depth elides the bottom.
    jkb(&db)
        .args(["task", "add", "deep", "+a/b/c/d/e/f"])
        .assert()
        .success();

    // Unfiltered, unscoped, global `find` refuses rather than dumping the whole KB.
    jkb(&db)
        .args(["--global", "find"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("would list the entire KB"));
    // Any filter makes it fine.
    jkb(&db)
        .args(["--global", "find", "--kind", "task"])
        .assert()
        .success();
    // A path makes it fine.
    jkb(&db).args(["--global", "find", "a"]).assert().success();

    // Default-depth tree elides the deepest folder with `…`; --depth reveals it.
    jkb(&db)
        .args(["--global", "tree", "a"])
        .assert()
        .success()
        .stdout(predicate::str::contains("…").and(predicate::str::contains("deep").not()));
    jkb(&db)
        .args(["--global", "tree", "a", "--depth", "9"])
        .assert()
        .success()
        .stdout(predicate::str::contains("deep"));
}

/// Pull the first `uid` out of a `--json` command's output.
fn uid_from(assert: &assert_cmd::assert::Assert) -> String {
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("expected JSON, got {out:?}: {e}"));
    let uid = match &v {
        serde_json::Value::Array(items) => items
            .first()
            .and_then(|i| i.get("uid"))
            .and_then(serde_json::Value::as_str),
        other => other.get("uid").and_then(serde_json::Value::as_str),
    };
    uid.unwrap_or_else(|| panic!("no uid in {out}")).to_owned()
}

/// The investigation surface, driven exactly as an agent would: create, record a dead end,
/// read the three buckets, check anti-retread, and walk the graph.
#[test]
#[allow(clippy::too_many_lines)] // one investigation driven end to end over the CLI
fn an_investigation_runs_end_to_end_over_the_cli() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);

    // No investigations yet — the empty listing names the available strategies rather than
    // leaving an agent guessing.
    jkb(&db).args(["inv", "ls"]).assert().success().stdout(
        predicate::str::contains("debugging").and(predicate::str::contains("conjecture-attack")),
    );

    // An unknown strategy is rejected with the list, not silently created untyped.
    jkb(&db)
        .args(["--global", "inv", "new", "evolutionary-search", "hunt"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown investigation type"));

    // Create one. Outside a repo it homes at `memory/<name>`.
    jkb(&db)
        .args([
            "--global",
            "inv",
            "new",
            "debugging",
            "flaky",
            "--goal",
            "sync flakes on a clean tree",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("memory/flaky"));

    let goal = uid_from(
        &jkb(&db)
            .args(["--json", "inv", "frontier", "memory/flaky"])
            .assert()
            .success(),
    );

    // The verbs come from the descriptor, so `jkb inv verbs` is self-documenting.
    jkb(&db)
        .args(["inv", "verbs", "memory/flaky"])
        .assert()
        .success()
        .stdout(predicate::str::contains("hypothesize").and(predicate::str::contains("rule-out")));

    // A verb from the other strategy is refused, listing the ones that exist.
    jkb(&db)
        .args(["inv", "do", "memory/flaky", "family", "flow formulations"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "is not a verb of the `debugging` strategy",
        ));

    // Two hypotheses; kill one.
    let dead = uid_from(
        &jkb(&db)
            .args([
                "--json",
                "inv",
                "do",
                "memory/flaky",
                "hypothesize",
                "mtime granularity",
                "--on",
                &goal,
            ])
            .assert()
            .success(),
    );
    let live = uid_from(
        &jkb(&db)
            .args([
                "--json",
                "inv",
                "do",
                "memory/flaky",
                "hypothesize",
                "hash read before flush",
                "--on",
                &goal,
            ])
            .assert()
            .success(),
    );
    jkb(&db)
        .args([
            "inv",
            "do",
            "memory/flaky",
            "refute",
            "mtimes differ by 4ms",
            "--on",
            &dead,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("target resolution -> dead_end"));

    // The tombstones bucket shows the dead end AND why it died.
    jkb(&db)
        .args(["inv", "tombstones", "memory/flaky"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains(dead.clone())
                .and(predicate::str::contains("refutes by"))
                .and(predicate::str::contains("4ms")),
        );

    // …and it is gone from the frontier, while the live hypothesis remains.
    jkb(&db)
        .args(["inv", "frontier", "memory/flaky"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains(live.clone())
                .and(predicate::str::contains(dead.clone()).not()),
        );

    // Weighted evidence, read back itemized.
    jkb(&db)
        .args([
            "inv",
            "do",
            "memory/flaky",
            "support",
            "fsync makes 200 runs pass",
            "--on",
            &live,
            "--weight",
            "3",
        ])
        .assert()
        .success();
    jkb(&db)
        .args(["inv", "evidence", &live])
        .assert()
        .success()
        .stdout(predicate::str::contains("balance +3.00"));

    // Anti-retread before working the live hypothesis: its refuted sibling surfaces.
    jkb(&db)
        .args(["inv", "retread", &live, "--depth", "2"])
        .assert()
        .success()
        .stdout(predicate::str::contains(dead.clone()));

    // `jkb related` walks the graph — the goal's inbound edges reach both hypotheses.
    jkb(&db)
        .args(["related", &goal, "--direction", "in"])
        .assert()
        .success()
        .stdout(predicate::str::contains(live.clone()).and(predicate::str::contains(dead.clone())));
    // An unknown edge type is rejected with the vocabulary.
    jkb(&db)
        .args(["related", &goal, "--edge", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown edge type"));

    // The digest renders all three buckets and is written as one reflection unit.
    jkb(&db)
        .args(["inv", "digest", "memory/flaky"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("## Frontier")
                .and(predicate::str::contains("## Tombstones"))
                .and(predicate::str::contains("Acceptance: not met")),
        );
    jkb(&db)
        .args(["--global", "--json", "query", "kind:reflection"])
        .assert()
        .success()
        .stdout(predicate::str::contains("digest"));

    // A dead end is retained, not deleted: `stat` still finds it, resolution and all.
    jkb(&db)
        .args(["stat", &dead])
        .assert()
        .success()
        .stdout(predicate::str::contains("resolution: dead_end"));

    // The `is:frontier` DSL term works over the ordinary query path too.
    jkb(&db)
        .args(["--global", "query", "is:frontier kind:hypothesis"])
        .assert()
        .success()
        .stdout(predicate::str::contains(live).and(predicate::str::contains(dead).not()));

    // Every write went through the audited writer-actor, so `undo` applies here as well.
    jkb(&db).args(["undo"]).assert().success();
}

/// `jkb item rm` deletes an item and its cascade, is reversible by `jkb undo`, and refuses
/// the two cases where deleting is destructive or a lie.
#[test]
fn item_rm_is_guarded_and_undoable() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);

    // A plain note, placed and tagged, is removable.
    jkb(&db)
        .args(["task", "add", "scratch note", "+notes/tmp"])
        .assert()
        .success();
    let out = jkb(&db)
        .args(["--global", "--json", "find", "notes/tmp"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let uid = v[0]["uid"].as_str().unwrap().to_owned();

    jkb(&db)
        .args(["item", "rm", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("removed").and(predicate::str::contains("jkb undo")));
    jkb(&db).args(["stat", &uid]).assert().failure();

    // `jkb undo` brings it back — the delete-only transaction is what undo targets.
    jkb(&db).args(["undo"]).assert().success();
    jkb(&db).args(["stat", &uid]).assert().success();

    // An investigation tombstone is refused: it is the anti-retread record.
    jkb(&db)
        .args([
            "--global",
            "inv",
            "new",
            "debugging",
            "guard",
            "--goal",
            "a symptom",
        ])
        .assert()
        .success();
    let goal = uid_from(
        &jkb(&db)
            .args(["--json", "inv", "frontier", "memory/guard"])
            .assert()
            .success(),
    );
    let dead = uid_from(
        &jkb(&db)
            .args([
                "--json",
                "inv",
                "do",
                "memory/guard",
                "hypothesize",
                "a wrong idea",
                "--on",
                &goal,
            ])
            .assert()
            .success(),
    );
    jkb(&db)
        .args([
            "inv",
            "do",
            "memory/guard",
            "refute",
            "disproved by measurement",
            "--on",
            &dead,
        ])
        .assert()
        .success();
    jkb(&db)
        .args(["item", "rm", &dead])
        .assert()
        .failure()
        .stderr(predicate::str::contains("tombstone").and(predicate::str::contains("--force")));
    // Still there, and still in the tombstones bucket.
    jkb(&db)
        .args(["inv", "tombstones", "memory/guard"])
        .assert()
        .success()
        .stdout(predicate::str::contains(dead.clone()));
    // `--force` gets through, and is still undoable.
    jkb(&db)
        .args(["item", "rm", &dead, "--force"])
        .assert()
        .success();
    jkb(&db).args(["undo"]).assert().success();
    jkb(&db).args(["stat", &dead]).assert().success();
}

/// Code-review regressions at the CLI edge (review 20260730-003611-jkb-memory-1): the digest
/// is not offered as work, `inv new` is idempotent and refuses a re-type, `--accept` is
/// strategy-scoped, and `inv resolve` refuses a task.
#[test]
fn the_inv_surface_refuses_cross_strategy_and_task_misuse() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);

    jkb(&db)
        .args([
            "--global",
            "inv",
            "new",
            "debugging",
            "probe",
            "--goal",
            "a symptom",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("created investigation"));

    // `--accept` belongs to conjecture-attack; applying it to debugging would stamp the
    // mathematical proof bar onto a symptom body that `goal_predicate` never reads.
    jkb(&db)
        .args([
            "--global",
            "inv",
            "new",
            "debugging",
            "probe2",
            "--accept",
            "prove",
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("has no acceptance presets")
                .and(predicate::str::contains("debugging")),
        );

    // The digest must not show up as frontier work, nor inside its own rendering.
    jkb(&db)
        .args(["inv", "digest", "memory/probe"])
        .assert()
        .success();
    jkb(&db)
        .args(["inv", "frontier", "memory/probe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("reflection").not());

    // Re-running `inv new` is idempotent and says so rather than claiming a fresh create.
    jkb(&db)
        .args([
            "--global",
            "inv",
            "new",
            "debugging",
            "probe",
            "--goal",
            "ignored",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("already exists"));
    // Exactly one goal unit, still holding the original body.
    jkb(&db)
        .args(["--global", "--json", "query", "kind:symptom"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("a symptom").and(predicate::str::contains("ignored").not()),
        );

    // Re-typing is refused.
    jkb(&db)
        .args([
            "--global",
            "inv",
            "new",
            "conjecture-attack",
            "probe",
            "--goal",
            "a conjecture",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "already a `debugging` investigation",
        ));

    // `inv resolve` refuses a task and points at the right command.
    let out = jkb(&db)
        .args(["--json", "task", "add", "an ordinary task"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let task_uid = v["uid"].as_str().unwrap().to_owned();
    jkb(&db)
        .args(["inv", "resolve", &task_uid, "dead_end"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("is a task").and(predicate::str::contains("jkb task set")),
        );
    // …so the task is untouched and still on the ready frontier.
    jkb(&db)
        .args(["--global", "task", "next"])
        .assert()
        .success()
        .stdout(predicate::str::contains("an ordinary task"));
}

/// The `conjecture-attack` acceptance preset is the only prove-vs-disprove difference, the
/// seeded goal body carries the enumerated bar, and a blocked route only reopens on a
/// materially new mechanism.
#[test]
#[allow(clippy::too_many_lines)] // one investigation driven end to end over the CLI
fn a_conjecture_investigation_seeds_its_predicate_and_gates_reopening() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);

    jkb(&db)
        .args([
            "--global",
            "inv",
            "new",
            "conjecture-attack",
            "jacobian",
            "--goal",
            "Resolve the Jacobian Conjecture.",
            "--accept",
            "disprove",
        ])
        .assert()
        .success();

    let goal = uid_from(
        &jkb(&db)
            .args(["--json", "inv", "frontier", "memory/jacobian"])
            .assert()
            .success(),
    );
    jkb(&db).args(["cat", &goal]).assert().success().stdout(
        predicate::str::contains("Acceptance predicate (disprove)")
            .and(predicate::str::contains("INSUFFICIENT")),
    );

    // An unknown preset is rejected with the valid set.
    jkb(&db)
        .args([
            "--global",
            "inv",
            "new",
            "conjecture-attack",
            "other",
            "--accept",
            "maybe",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown acceptance preset"));

    // Blocking a route on a gap takes it off the frontier — with the reason attached.
    let route = uid_from(
        &jkb(&db)
            .args([
                "--json",
                "inv",
                "do",
                "memory/jacobian",
                "approach",
                "degree growth",
            ])
            .assert()
            .success(),
    );
    jkb(&db)
        .args([
            "inv",
            "do",
            "memory/jacobian",
            "gap",
            "needs a uniform degree bound",
            "--on",
            &route,
        ])
        .assert()
        .success();
    jkb(&db)
        .args(["inv", "frontier", "memory/jacobian"])
        .assert()
        .success()
        .stdout(predicate::str::contains(route.clone()).not());

    // A partial result is progress worth recording, but not grounds to reopen.
    let partial = uid_from(
        &jkb(&db)
            .args([
                "--json",
                "inv",
                "do",
                "memory/jacobian",
                "partial",
                "holds for degree <= 4",
            ])
            .assert()
            .success(),
    );
    jkb(&db)
        .args(["inv", "reopen", &route, "--mechanism", &partial])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "cannot reopen with a `partial-result`",
        ));

    let mechanism = uid_from(
        &jkb(&db)
            .args([
                "--json",
                "inv",
                "do",
                "memory/jacobian",
                "mechanism",
                "a valuation filtration",
            ])
            .assert()
            .success(),
    );
    jkb(&db)
        .args(["inv", "reopen", &route, "--mechanism", &mechanism])
        .assert()
        .success()
        .stdout(predicate::str::contains("superseded gap"));
    // With its gap superseded, the route is back on the frontier.
    jkb(&db)
        .args(["inv", "frontier", "memory/jacobian"])
        .assert()
        .success()
        .stdout(predicate::str::contains(route));
}

/// The blob archive is the recovery path when a sync has already written a wrong version
/// over a file: every settled version's bytes are stored and never deleted, so you can find
/// the one carrying a line you remember and read it back.
#[test]
fn blob_archive_recovers_a_previous_version_of_a_synced_file() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let repo = TempDir::new().unwrap();
    let file = repo.path().join("tasks.md");
    std::fs::write(
        &file,
        "## Alpha\n\nA line I will regret losing.\n\n- [ ] first ^first\n",
    )
    .unwrap();

    jkb(&db)
        .args([
            "mount",
            "create",
            "work",
            repo.path().to_str().unwrap(),
            "--serializer",
            "tasks",
            "--include",
            "**/tasks.md",
        ])
        .assert()
        .success();
    jkb(&db).args(["sync", "work"]).assert().success();

    // Someone clobbers the file and it settles, so the disk no longer has the line.
    std::fs::write(&file, "## Alpha\n\n- [ ] first ^first\n").unwrap();
    jkb(&db).args(["sync", "work"]).assert().success();
    assert!(!std::fs::read_to_string(&file).unwrap().contains("regret"));

    // The archive still has the version that does.
    let out = jkb(&db)
        .args(["--json", "blob", "ls", "--contains", "regret"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let hash = v[0]["hash"]
        .as_str()
        .expect("a blob carrying the lost line");

    // …and `blob cat` reads it back byte-for-byte.
    let recovered = jkb(&db).args(["blob", "cat", hash]).output().unwrap();
    let text = String::from_utf8(recovered.stdout).unwrap();
    assert!(text.contains("A line I will regret losing."), "got: {text}");
    assert!(text.contains("- [ ] first ^first"));

    // A hash prefix works; an ambiguous or unknown one is refused rather than guessing.
    jkb(&db)
        .args(["blob", "cat", &hash[..12]])
        .assert()
        .success();
    jkb(&db)
        .args(["blob", "cat", "ffffffffffffffff"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no blob with hash prefix"));

    // `jkb history` lists that file's settled versions, newest first.
    jkb(&db)
        .args(["history", file.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("jkb blob cat"));
}

/// `jkb ns type` shows, sets and lists namespace types, and the reserved roots carry theirs
/// without anyone applying it (design D33.4).
#[test]
fn ns_type_shows_sets_and_lists_namespace_types() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);

    // `--list` groups by role, so a contract is never mistaken for something `inv` drives.
    jkb(&db)
        .args(["ns", "type", "--list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("investigation strategies"))
        .stdout(predicate::str::contains("debugging"))
        .stdout(predicate::str::contains("contracts"))
        .stdout(predicate::str::contains("journal"));

    // A reserved system namespace is typed by the migration, with no user action.
    jkb(&db)
        .args(["ns", "type", "_sys/sync"])
        .assert()
        .success()
        .stdout(predicate::str::contains("journal"));

    // Creating the `tasks` root types it, and the subtree inherits — reported as inherited
    // so "why is this enforced here?" is answerable.
    jkb(&db)
        .args(["ns", "mk", "tasks/scratch"])
        .assert()
        .success();
    jkb(&db)
        .args(["ns", "type", "tasks"])
        .assert()
        .success()
        .stdout(predicate::str::contains("tasks"));
    jkb(&db)
        .args(["--json", "ns", "type", "tasks/scratch"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"effective_type\": \"tasks\""))
        .stdout(predicate::str::contains("\"inherited_from\": \"tasks\""))
        .stdout(predicate::str::contains("\"type\": null"));

    // An ordinary namespace is untyped, and setting an unknown type is refused by name.
    jkb(&db)
        .args(["ns", "mk", "references/web"])
        .assert()
        .success();
    jkb(&db)
        .args(["ns", "type", "references/web"])
        .assert()
        .success()
        .stdout(predicate::str::contains("untyped"));
    jkb(&db)
        .args(["ns", "type", "references/web", "no-such-type"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown namespace type"));
}

/// The writer boundary: a typed namespace's contract is enforced on any write, and `jkb inv`
/// refuses a contract-typed namespace rather than reporting an empty verb list (design D33).
#[test]
fn a_namespace_contract_is_enforced_on_write_and_contracts_are_not_investigations() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);
    let note = dir.path().join("note.md");
    std::fs::write(&note, "a note that is not a task").unwrap();

    // Ingesting a document into the `tasks` tree is refused: `tasks` holds tasks only.
    jkb(&db)
        .args(["ingest", note.to_str().unwrap(), "--ns", "tasks/jkb"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("typed `tasks`"))
        .stderr(predicate::str::contains("accepts: task"));

    // …and the same document lands fine in an untyped namespace.
    jkb(&db)
        .args(["ingest", note.to_str().unwrap(), "--ns", "references/web"])
        .assert()
        .success();

    // A task in the tasks tree is exactly what the contract allows.
    jkb(&db)
        .args(["task", "add", "a real task", "+tasks/jkb"])
        .assert()
        .success();

    // `inv` on a contract namespace is a user error naming the type, not an empty listing.
    jkb(&db)
        .args(["inv", "verbs", "tasks"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is typed `tasks`"));
    jkb(&db)
        .args(["--global", "inv", "new", "tasks", "hunt"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is a contract type"));
}

/// A type is **not** a location marker (design D33.5). The reserved roots — `tasks/`,
/// `repos/`, `media/`, `_sys/` — are special cases of the fixed D32 layout, so several
/// namespaces may carry the same contract and none of them relocates anything.
/// `--clear` is the plain inverse of setting a type.
#[test]
fn a_contract_may_type_several_namespaces_and_relocates_nothing() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);

    // Two namespaces may carry the `tasks` contract; neither becomes "the tasks root".
    jkb(&db)
        .args(["ns", "type", "work/todo", "tasks"])
        .assert()
        .success();
    jkb(&db)
        .args(["ns", "type", "alpha/queue", "tasks"])
        .assert()
        .success();

    // Task homing is unmoved: `tasks/` is the root because the layout reserves it.
    jkb(&db)
        .args(["task", "add", "still in the reserved root"])
        .assert()
        .success()
        .stdout(predicate::str::contains("at tasks/inbox"));

    // Both still enforce their contract where they are.
    let note = dir.path().join("n.md");
    std::fs::write(&note, "not a task").unwrap();
    jkb(&db)
        .args(["ingest", note.to_str().unwrap(), "--ns", "work/todo"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("accepts: task"));

    // `--clear` reverts a namespace typed by mistake.
    jkb(&db)
        .args(["ns", "type", "work/todo", "--clear"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cleared type"));
    jkb(&db)
        .args(["ns", "type", "work/todo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("untyped"));
    jkb(&db)
        .args(["ingest", note.to_str().unwrap(), "--ns", "work/todo"])
        .assert()
        .success();
}

/// Showing the type of a namespace that does not exist must not read as "untyped" — that is
/// what a typo gets, and it looks exactly like a valid answer.
#[test]
fn ns_type_show_distinguishes_a_missing_namespace_from_an_untyped_one() {
    let dir = TempDir::new().unwrap();
    let db = db_path(&dir);

    jkb(&db)
        .args(["ns", "mk", "references/web"])
        .assert()
        .success();
    jkb(&db)
        .args(["ns", "type", "references/web"])
        .assert()
        .success()
        .stdout(predicate::str::contains("untyped"));

    jkb(&db)
        .args(["ns", "type", "totally/made/up"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not exist"));
}
