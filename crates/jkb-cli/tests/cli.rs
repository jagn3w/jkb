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
fn task_edit_refuses_newline_edits_on_file_backed_tasks() {
    // A file-backed task is a single source line; `--append` (or multi-line content)
    // would split it on sync and detach its `^id`. The CLI must refuse such edits.
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

    // `--append` is refused with a message pointing at the source-file flow.
    jkb(&db)
        .args(["task", "edit", uid, "--append", "Design: use trait X"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("file-backed"))
        .stderr(predicate::str::contains("run `jkb sync`"));

    // A multi-line replacement is refused too (content carrying a newline).
    jkb(&db)
        .args(["task", "edit", uid, "line one\nline two"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("file-backed"));

    // A single-line replacement round-trips and is allowed.
    jkb(&db)
        .args(["task", "edit", uid, "renamed", "title"])
        .assert()
        .success()
        .stdout(predicate::str::contains("edited"));

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
        .args(["mount", ns, repo_dir.to_str().unwrap()])
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
