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
