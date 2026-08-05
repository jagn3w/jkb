//! Parallel task sessions end-to-end (design D36): two sessions worked at once, landed
//! through the serial merge queue, and every way that is supposed to stop rather than
//! corrupt — a conflict, a red gate, a dirty session, a task someone else holds.
//!
//! These drive the real binary against a real git repo, because everything interesting here
//! is git behaviour (a branch cannot be checked out twice; a rebase conflicts) rather than
//! anything a mock would reproduce.
//!
//! Every `land` here passes `--no-review`: these tests are about landing *mechanics*, and the
//! review gate (design D38.5) has its own tests in `staging.rs`. Without the flag each of
//! these would be re-testing the gate and nothing else.

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;

/// A scratch repo plus a database kept *outside* it — a db file inside the repo would show
/// up as an untracked change and make every land refuse a dirty tree.
struct Fixture {
    /// Kept alive so the scratch directory outlives the test; also where the fake remote goes.
    home: TempDir,
    repo: PathBuf,
    db: PathBuf,
}

/// Run `git` in `dir` with the developer's global config neutralized. This machine sets
/// `core.hooksPath` and commit signing globally; either would fail the fixture for reasons
/// that have nothing to do with sessions.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

impl Fixture {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        let repo = home.path().join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("README.md"), "base\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "base"]);
        Self {
            db: home.path().join("jkb.db"),
            repo,
            home,
        }
    }

    /// A `jkb` invocation rooted in the repo, with git's global config neutralized for the
    /// `git` subprocesses jkb itself spawns.
    fn jkb(&self) -> Command {
        let mut cmd = Command::cargo_bin("jkb").unwrap();
        cmd.arg("--db")
            .arg(&self.db)
            .current_dir(&self.repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t");
        cmd
    }

    /// Add a task and return its uid.
    fn add_task(&self, text: &str) -> String {
        let out = self
            .jkb()
            .args(["--global", "task", "add", text, "--json"])
            .output()
            .unwrap();
        assert!(out.status.success(), "task add: {out:?}");
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        v["uid"].as_str().unwrap().to_owned()
    }

    /// Open a session and return its JSON description.
    fn work(&self, uid: &str) -> serde_json::Value {
        self.work_args(&["task", "work", uid, "--json"])
    }

    /// Open a session aimed at an explicit land target.
    fn work_onto(&self, uid: &str, onto: &str) -> serde_json::Value {
        self.work_args(&["task", "work", uid, "--onto", onto, "--json"])
    }

    fn work_args(&self, args: &[&str]) -> serde_json::Value {
        let out = self.jkb().args(args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn status_of(&self, uid: &str) -> String {
        let out = self
            .jkb()
            .args(["--global", "task", "show", uid, "--json"])
            .output()
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        v["status"].as_str().unwrap_or_default().to_owned()
    }
}

/// Commit `content` to `file` inside a session's worktree — the work an agent would do.
fn commit_in(worktree: &Path, file: &str, content: &str, message: &str) {
    std::fs::write(worktree.join(file), content).unwrap();
    git(worktree, &["add", "-A"]);
    git(worktree, &["commit", "-qm", message]);
}

/// The whole point: two tasks worked at the same time, in separate checkouts, landing one
/// after the other onto a shared branch cut from trunk.
#[test]
fn two_sessions_are_worked_in_parallel_and_land_in_sequence() {
    let f = Fixture::new();
    let a = f.add_task("first task");
    let b = f.add_task("second task");

    let sa = f.work(&a);
    let sb = f.work(&b);

    // Started from trunk, so a batch branch was cut and named after the first task; the
    // second session joined it rather than opening a batch of its own (design D36.3).
    let onto = sa["onto"].as_str().unwrap();
    assert_ne!(onto, "main", "landing on trunk would close tasks instantly");
    assert_eq!(sb["onto"].as_str().unwrap(), onto, "same batch");
    assert_ne!(
        sa["worktree"], sb["worktree"],
        "each session gets its own checkout — that is the isolation"
    );

    // Claiming is what stops a swarm run or a second click from taking the same task.
    assert_eq!(f.status_of(&a), "in_progress");

    let wa = PathBuf::from(sa["worktree"].as_str().unwrap());
    let wb = PathBuf::from(sb["worktree"].as_str().unwrap());
    commit_in(&wa, "a.txt", "from a\n", "add a");
    commit_in(&wb, "b.txt", "from b\n", "add b");

    // A gate that passes, remembered for the repo on the first land.
    f.jkb()
        .args(["task", "land", &a, "--gate", "true", "--no-review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("landed"));

    // The second land needs no --gate: the repo remembers (design D36.5).
    f.jkb()
        .args(["task", "land", &b, "--no-review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("landed"))
        .stdout(predicate::str::contains("remembered for this repo"));

    assert_eq!(f.status_of(&a), "done");
    assert_eq!(f.status_of(&b), "done");

    // Both changes are on the batch branch, in a linear history with no merge commits.
    let files = git(&f.repo, &["ls-tree", "-r", "--name-only", onto]);
    assert!(
        files.contains("a.txt") && files.contains("b.txt"),
        "{files}"
    );
    let merges = git(&f.repo, &["log", "--merges", "--oneline", onto]);
    assert!(merges.is_empty(), "history must stay linear: {merges}");

    // Landing cleans the sessions up; nothing is left claimed or checked out.
    f.jkb()
        .args(["task", "sessions"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no sessions"));
    assert!(!wa.exists() && !wb.exists());
}

/// A local trunk ahead of its remote is the ordinary case. Cutting the batch from
/// `origin/main` there would start the work behind commits you already have — and then land
/// it as though it were on top of them.
#[test]
fn the_batch_is_cut_from_the_local_trunk_not_the_remote() {
    let f = Fixture::new();
    // Give the repo an origin, then move local `main` ahead of it.
    let remote = f.home.path().join("origin.git");
    git(&f.repo, &["init", "--bare", "-q", remote.to_str().unwrap()]);
    git(
        &f.repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&f.repo, &["push", "-q", "-u", "origin", "main"]);
    commit_in(&f.repo, "local-only.txt", "ahead\n", "local commit");

    let uid = f.add_task("task on an advanced trunk");
    let s = f.work(&uid);
    let worktree = PathBuf::from(s["worktree"].as_str().unwrap());
    assert!(
        worktree.join("local-only.txt").exists(),
        "the session must start from the local trunk, not origin/main"
    );
}

/// A second click must return you to the work in progress, not fork it onto a new branch.
#[test]
fn opening_a_session_twice_returns_the_same_one() {
    let f = Fixture::new();
    let uid = f.add_task("some task");
    let first = f.work(&uid);
    let again = f.work(&uid);
    assert_eq!(first["worktree"], again["worktree"]);
    assert_eq!(first["branch"], again["branch"]);
    assert_eq!(again["resumed"], serde_json::Value::Bool(true));
    // Re-taking the claim under a new pid is what makes resuming possible at all (D36.6).
    assert_eq!(f.status_of(&uid), "in_progress");
}

/// The collision this change exists to prevent: someone else is already on it.
#[test]
fn a_task_held_by_another_live_owner_refuses_a_session() {
    let f = Fixture::new();
    let uid = f.add_task("contended task");
    // pid 1 always exists, so this claim is unambiguously live.
    f.jkb()
        .args(["task", "claim", &uid, "--owner", "host:1"])
        .assert()
        .success();

    f.jkb()
        .args(["task", "work", &uid])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already being worked"));
}

/// A red gate must leave the target exactly as it was — that is the difference between a
/// merge queue and just merging.
#[test]
fn a_red_gate_rolls_the_target_back_and_keeps_the_session() {
    let f = Fixture::new();
    let uid = f.add_task("failing task");
    let s = f.work(&uid);
    let (onto, wt) = (
        s["onto"].as_str().unwrap().to_owned(),
        PathBuf::from(s["worktree"].as_str().unwrap()),
    );
    commit_in(&wt, "c.txt", "x\n", "add c");
    let before = git(&f.repo, &["rev-parse", &onto]);

    f.jkb()
        .args(["task", "land", &uid, "--gate", "false", "--no-review"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("gate failed"));

    assert_eq!(
        git(&f.repo, &["rev-parse", &onto]),
        before,
        "the target must be back where it started"
    );
    assert_eq!(f.status_of(&uid), "in_progress", "still yours to fix");
    assert!(wt.exists(), "the session survives a red gate");
}

/// Two sessions touching the same lines: the second must eject, not be resolved blind.
#[test]
fn a_conflicting_second_land_ejects_without_touching_the_target() {
    let f = Fixture::new();
    let a = f.add_task("edit readme one");
    let b = f.add_task("edit readme two");
    let sa = f.work(&a);
    let sb = f.work(&b);
    let onto = sa["onto"].as_str().unwrap().to_owned();
    commit_in(
        Path::new(sa["worktree"].as_str().unwrap()),
        "README.md",
        "one\n",
        "readme one",
    );
    commit_in(
        Path::new(sb["worktree"].as_str().unwrap()),
        "README.md",
        "two\n",
        "readme two",
    );

    f.jkb()
        .args(["task", "land", &a, "--no-gate", "--no-review"])
        .assert()
        .success();
    let after_a = git(&f.repo, &["rev-parse", &onto]);

    f.jkb()
        .args(["task", "land", &b, "--no-gate", "--no-review"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not rebase cleanly"))
        // The message must point at the checkout that has the context to fix it.
        .stderr(predicate::str::contains("git rebase"));

    assert_eq!(git(&f.repo, &["rev-parse", &onto]), after_a);
    assert_eq!(f.status_of(&b), "in_progress");
}

/// Uncommitted work would silently not land. Refuse instead.
#[test]
fn a_session_with_uncommitted_work_refuses_to_land() {
    let f = Fixture::new();
    let uid = f.add_task("half done task");
    let s = f.work(&uid);
    let wt = PathBuf::from(s["worktree"].as_str().unwrap());
    commit_in(&wt, "d.txt", "committed\n", "add d");
    std::fs::write(wt.join("d.txt"), "edited but not committed\n").unwrap();

    f.jkb()
        .args(["task", "land", &uid, "--no-gate", "--no-review"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("uncommitted changes"));
}

/// Nothing to land is a mistake worth naming, not a no-op success.
#[test]
fn landing_a_session_with_no_commits_says_so() {
    let f = Fixture::new();
    let uid = f.add_task("untouched task");
    f.work(&uid);
    f.jkb()
        .args(["task", "land", &uid, "--no-gate", "--no-review"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("nothing to land"));
}

/// Abandoning returns the task to the frontier and takes the checkout with it — but keeps
/// the branch, because the commits on it are real work.
#[test]
fn abandoning_reopens_the_task_and_removes_the_worktree() {
    let f = Fixture::new();
    let uid = f.add_task("abandoned task");
    let s = f.work(&uid);
    let wt = PathBuf::from(s["worktree"].as_str().unwrap());
    let branch = s["branch"].as_str().unwrap().to_owned();
    commit_in(&wt, "e.txt", "work\n", "add e");

    f.jkb().args(["task", "abandon", &uid]).assert().success();

    assert!(!wt.exists());
    assert_eq!(f.status_of(&uid), "open", "back on the frontier");
    assert!(
        !git(&f.repo, &["rev-parse", "--verify", &branch]).is_empty(),
        "the branch keeps the commits"
    );
    // And with the claim gone, the task can be worked again.
    f.jkb().args(["task", "work", &uid]).assert().success();
}

/// A session outlives the process that opened it: the worktree is the liveness signal, and
/// the claim must survive the owner-existence reclaim.
///
/// It must also be *reported as a plain session*. Nothing can observe whether anyone is
/// sitting in one — the owner's pid belongs to the one-second `jkb task work` process — so a
/// report that labels sessions "unattended" labels every one of them, including the one you
/// are working in, and advises abandoning it.
#[test]
fn a_session_survives_the_process_that_opened_it_and_is_reported_plainly() {
    let f = Fixture::new();
    let uid = f.add_task("walked away task");
    f.work(&uid); // the `jkb` process that claimed it has already exited

    f.jkb()
        .args(["task", "reclaim"])
        .assert()
        .success()
        .stdout(predicate::str::contains("reclaimed 0"));
    assert_eq!(f.status_of(&uid), "in_progress");

    f.jkb()
        .args(["task", "sessions"])
        .assert()
        .success()
        .stdout(predicate::str::contains("walked-away-task"))
        .stdout(predicate::str::contains("unattended").not());
    f.jkb()
        .args(["doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("task sessions: 1 in flight"))
        .stdout(predicate::str::contains("unattended").not());
}

/// `.jkb/base` is a reusable checkout. Landing a second batch onto a different branch used to
/// die on `git worktree add` into the existing directory, and stay dead until it was deleted
/// by hand.
#[test]
fn landing_onto_a_second_target_reuses_the_base_checkout() {
    let f = Fixture::new();
    // Batch one: cut from trunk, landed, so `.jkb/base` is left holding that branch.
    let a = f.add_task("first batch task");
    let sa = f.work(&a);
    commit_in(
        Path::new(sa["worktree"].as_str().unwrap()),
        "a.txt",
        "a\n",
        "a",
    );
    f.jkb()
        .args(["task", "land", &a, "--no-gate", "--no-review"])
        .assert()
        .success();
    assert!(
        f.repo.join(".jkb/base").exists(),
        "the batch checkout survives"
    );

    // Batch two: a different target entirely.
    let b = f.add_task("second batch task");
    let sb = f.work_onto(&b, "other-batch");
    assert_eq!(sb["onto"].as_str().unwrap(), "other-batch");
    commit_in(
        Path::new(sb["worktree"].as_str().unwrap()),
        "b.txt",
        "b\n",
        "b",
    );
    f.jkb()
        .args(["task", "land", &b, "--no-gate", "--no-review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("landed"));
    assert!(git(&f.repo, &["ls-tree", "-r", "--name-only", "other-batch"]).contains("b.txt"));
}

/// A merged batch must not keep attracting new sessions, and must not stay undeletable
/// because jkb is holding a checkout of it.
#[test]
fn a_merged_batch_is_released_rather_than_rejoined() {
    let f = Fixture::new();
    let a = f.add_task("batch opener");
    let sa = f.work(&a);
    let batch = sa["onto"].as_str().unwrap().to_owned();
    commit_in(
        Path::new(sa["worktree"].as_str().unwrap()),
        "a.txt",
        "a\n",
        "a",
    );
    f.jkb()
        .args(["task", "land", &a, "--no-gate", "--no-review"])
        .assert()
        .success();

    // Merge the batch into trunk, the way finishing a PR would.
    git(&f.repo, &["merge", "-q", "--ff-only", &batch]);

    // A new task must start a NEW batch, not rejoin the merged one...
    let b = f.add_task("task after the merge");
    let sb = f.work(&b);
    assert_ne!(
        sb["onto"].as_str().unwrap(),
        batch,
        "a merged batch must not attract new work"
    );
    // ...and the merged branch must be deletable, which it is not while a worktree holds it.
    git(&f.repo, &["branch", "-d", &batch]);
}

/// A task can carry two `branch=` values — `jkb task start` writes one too. Picking the wrong
/// one opens a second session for a task that already has one, and then `land` cannot find it.
#[test]
fn a_second_branch_tag_does_not_fork_the_session() {
    let f = Fixture::new();
    let uid = f.add_task("task tagged twice");
    let first = f.work(&uid);
    // A tag that sorts AFTER the session's own branch, which is what a naive map keeps.
    f.jkb()
        .args(["task", "tag", "add", &uid, "branch=zzz-stale-branch"])
        .assert()
        .success();

    let again = f.work(&uid);
    assert_eq!(
        again["worktree"], first["worktree"],
        "the live session must win over a stale branch tag"
    );
    let wt = PathBuf::from(again["worktree"].as_str().unwrap());
    commit_in(&wt, "x.txt", "x\n", "x");
    f.jkb()
        .args(["task", "land", &uid, "--no-gate", "--no-review"])
        .assert()
        .success()
        .stdout(predicate::str::contains("landed"));
}

/// `task work` sets the location facets rather than adding to them, so re-targeting a session
/// cannot leave the task claiming two land targets at once.
#[test]
fn retargeting_a_session_replaces_the_facets_it_records() {
    let f = Fixture::new();
    let uid = f.add_task("retargeted task");
    f.work_onto(&uid, "batch-one");
    f.work_onto(&uid, "batch-two");

    // `item show` is the read that carries tags; `task show` does not.
    let out = f
        .jkb()
        .args(["item", "show", &uid, "--json"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let onto: Vec<&str> = v["tags"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["facet"] == "onto")
        .map(|t| t["value"].as_str().unwrap())
        .collect();
    assert_eq!(onto, vec!["batch-two"], "one target, not two");
}

/// `--keep-worktree` must leave the session on what actually landed. `graft` rebases a
/// detached HEAD, so the branch ref stays on its pre-rebase commits unless it is moved.
///
/// The target has to have *moved* since the session branch was cut, or the rebase is a no-op
/// fast-forward and the branch is already on the landed commit for the wrong reason — so an
/// earlier session lands first.
#[test]
fn keeping_the_worktree_moves_its_branch_to_what_landed() {
    let f = Fixture::new();
    let first = f.add_task("earlier task");
    let uid = f.add_task("kept session task");
    let s1 = f.work(&first);
    let s = f.work(&uid);
    let wt = PathBuf::from(s["worktree"].as_str().unwrap());
    let branch = s["branch"].as_str().unwrap().to_owned();
    let onto = s["onto"].as_str().unwrap().to_owned();
    commit_in(
        Path::new(s1["worktree"].as_str().unwrap()),
        "f.txt",
        "f\n",
        "f",
    );
    commit_in(&wt, "k.txt", "k\n", "k");

    // Move the target out from under the kept session, so its land is a real rebase.
    f.jkb()
        .args(["task", "land", &first, "--no-gate", "--no-review"])
        .assert()
        .success();
    let before = git(&f.repo, &["rev-parse", &branch]);

    f.jkb()
        .args([
            "task",
            "land",
            &uid,
            "--no-gate",
            "--keep-worktree",
            "--no-review",
        ])
        .assert()
        .success();

    assert!(wt.exists(), "the worktree was kept");
    assert_ne!(
        git(&f.repo, &["rev-parse", &branch]),
        before,
        "the rebase produced a new commit, so the branch must have moved to it"
    );
    assert_eq!(
        git(
            &f.repo,
            &["rev-list", "--count", &format!("{onto}..{branch}")]
        ),
        "0",
        "the kept session must sit on what landed, not on its pre-rebase commits"
    );
}

/// jkb appends its exclusion; it must never rewrite a file whose contents it did not author.
#[test]
fn excluding_jkb_preserves_existing_ignore_rules() {
    let f = Fixture::new();
    let exclude = f.repo.join(".git/info/exclude");
    // Contents jkb cannot read as UTF-8 — the case that used to be treated as "empty".
    let original = b"# mine\n/secret-scratch/\n\xff\xfe not utf-8\n".to_vec();
    std::fs::write(&exclude, &original).unwrap();

    let uid = f.add_task("task in a repo with local ignores");
    f.work(&uid);

    let after = std::fs::read(&exclude).unwrap();
    assert!(
        after.starts_with(original.as_slice()),
        "the user's ignore rules must survive verbatim"
    );
    assert!(String::from_utf8_lossy(&after).contains("/.jkb/"));
}

/// The gate is per repo and configurable — the setting a landing depends on must be
/// inspectable without landing something.
#[test]
fn the_gate_is_remembered_and_editable() {
    let f = Fixture::new();
    f.jkb()
        .args(["task", "gate", "echo verified"])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo verified"));
    f.jkb()
        .args(["task", "gate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo verified"));
    f.jkb()
        .args(["task", "gate", "--clear"])
        .assert()
        .success()
        .stdout(predicate::str::contains("UNVERIFIED"));
}

// ---------------------------------------------------------------------------
// Staging branches and the review gate (design D38)
// ---------------------------------------------------------------------------

impl Fixture {
    /// `jkb staging ls --json`.
    fn staging(&self, extra: &[&str]) -> serde_json::Value {
        let mut args = vec!["staging", "ls", "--json"];
        args.extend_from_slice(extra);
        let out = self.jkb().args(&args).output().unwrap();
        assert!(out.status.success(), "staging ls: {out:?}");
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// File a must-fix finding under `ns` and return its uid.
    fn add_finding(&self, ns: &str, text: &str) -> String {
        let out = self
            .jkb()
            .args([
                "--global",
                "task",
                "add",
                text,
                "!p1",
                &format!("+{ns}"),
                "--json",
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "add finding: {out:?}");
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        v["uid"].as_str().unwrap().to_owned()
    }
}

/// `staging ls` groups live sessions under the branch they land on, and derives each task's
/// state rather than storing it (design D38.1/D38.2).
#[test]
fn staging_ls_groups_tasks_under_the_branch_they_land_on() {
    let f = Fixture::new();
    let a = f.add_task("first staged task");
    let b = f.add_task("second staged task");
    let sa = f.work(&a);
    let onto = sa["onto"].as_str().unwrap().to_owned();
    // The second session joins the batch the first one opened — the swarm's integration
    // branch model, driven by hand (D36.3).
    let sb = f.work(&b);
    assert_eq!(sb["onto"].as_str().unwrap(), onto);

    commit_in(
        Path::new(sa["worktree"].as_str().unwrap()),
        "a.txt",
        "a",
        "a",
    );

    let rows = f.staging(&[]);
    assert_eq!(rows.as_array().unwrap().len(), 1, "one staging branch");
    let row = &rows[0];
    assert_eq!(row["branch"].as_str().unwrap(), onto);
    assert_eq!(row["merged"], false);
    let tasks = row["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 2);
    for t in tasks {
        assert_eq!(t["state"].as_str().unwrap(), "implementing");
        assert_eq!(t["open_must_fix"], 0);
        assert!(t["reviewed"].is_null());
    }
    // The one with a commit reports it; the other does not.
    let committed = tasks.iter().find(|t| t["uid"] == a.as_str()).unwrap();
    assert_eq!(committed["commits"], 1);
    assert_eq!(committed["title"].as_str().unwrap(), "first staged task");

    // A task whose `onto=` branch no longer exists is not a phantom staging branch.
    f.jkb()
        .args(["task", "abandon", &a, "--force", "--delete-branch"])
        .assert()
        .success();
    f.jkb()
        .args(["task", "abandon", &b, "--force", "--delete-branch"])
        .assert()
        .success();
    git(&f.repo, &["branch", "-D", &onto]);
    assert!(f.staging(&[]).as_array().unwrap().is_empty());
}

/// Recording a review tags the task and moves it into `needs_review` — the only author of
/// that transition (design D38.4/D38.6).
#[test]
fn recording_a_review_tags_the_task_and_moves_it_to_needs_review() {
    let f = Fixture::new();
    let uid = f.add_task("reviewed task");
    let s = f.work(&uid);
    let branch = s["branch"].as_str().unwrap().to_owned();
    commit_in(
        Path::new(s["worktree"].as_str().unwrap()),
        "a.txt",
        "a",
        "a",
    );
    assert_eq!(f.status_of(&uid), "in_progress");

    f.jkb()
        .args(["task", "review", "record", "--branch", &branch])
        .args(["--findings", "reviews/run-1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("now needs_review"));

    assert_eq!(f.status_of(&uid), "needs_review");
    let t = &f.staging(&[])[0]["tasks"][0];
    assert_eq!(t["state"].as_str().unwrap(), "review");
    assert_eq!(t["review_ns"].as_str().unwrap(), "reviews/run-1");
    let first_sha = t["reviewed"].as_str().unwrap().to_owned();

    // Recording again replaces the SHA rather than accumulating a second one: a task with two
    // `reviewed=` values is a contradiction, and a reader collapsing the multi-map picks one.
    commit_in(
        Path::new(s["worktree"].as_str().unwrap()),
        "b.txt",
        "b",
        "b",
    );
    f.jkb()
        .args(["task", "review", "record", "--branch", &branch])
        .args(["--findings", "reviews/run-2"])
        .assert()
        .success();
    let t = &f.staging(&[])[0]["tasks"][0];
    assert_ne!(t["reviewed"].as_str().unwrap(), first_sha);
    assert_eq!(t["review_ns"].as_str().unwrap(), "reviews/run-2");

    // A branch no task claims is a note, not an error — reviewing an arbitrary range is a
    // legitimate thing to do.
    f.jkb()
        .args(["task", "review", "record", "--branch", "main"])
        .args(["--findings", "reviews/run-3"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no task records branch=main"));
}

/// The land gate: unreviewed refuses, an open must-fix refuses, cancelling it lets the
/// landing through, and `--no-review` records a visible waiver (design D38.5).
#[test]
fn landing_requires_a_review_with_no_open_must_fix_findings() {
    let f = Fixture::new();
    let uid = f.add_task("gated task");
    let s = f.work(&uid);
    let branch = s["branch"].as_str().unwrap().to_owned();
    let onto = s["onto"].as_str().unwrap().to_owned();
    let worktree = s["worktree"].as_str().unwrap().to_owned();
    commit_in(Path::new(&worktree), "a.txt", "a", "a");

    // 1. No review recorded at all.
    let before = git(&f.repo, &["rev-parse", &onto]);
    f.jkb()
        .args(["task", "land", &uid, "--no-gate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no recorded review"));
    assert_eq!(
        git(&f.repo, &["rev-parse", &onto]),
        before,
        "a refusal must not have moved the target"
    );
    assert_eq!(f.status_of(&uid), "in_progress");

    // 2. Reviewed, but the review left a must-fix finding open.
    let finding = f.add_finding("reviews/gate", "a real must-fix problem");
    f.jkb()
        .args(["task", "review", "record", "--branch", &branch])
        .args(["--findings", "reviews/gate"])
        .assert()
        .success();
    f.jkb()
        .args(["task", "land", &uid, "--no-gate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("open must-fix finding"))
        .stderr(predicate::str::contains("a real must-fix problem"));
    assert_eq!(git(&f.repo, &["rev-parse", &onto]), before);

    // The row must carry the same count the gate enforces. A row saying only "reviewed"
    // about a task the gate is about to refuse is worse than no row: it reads as landable.
    let t = &f.staging(&[])[0]["tasks"][0];
    assert_eq!(t["open_must_fix"], 1);
    assert_eq!(t["state"].as_str().unwrap(), "review");

    // 3. Dismissing the finding lets it land. Concerns and nits never blocked.
    f.jkb()
        .args(["--global", "task", "set", &finding, "--status", "cancelled"])
        .assert()
        .success();
    f.jkb()
        .args(["task", "land", &uid, "--no-gate"])
        .assert()
        .success()
        .stdout(predicate::str::contains("is done"));
    assert_eq!(f.status_of(&uid), "done");
    assert_ne!(git(&f.repo, &["rev-parse", &onto]), before);
}

/// `--no-review` lands without a review but leaves a mark, so a bypass is visible.
#[test]
fn no_review_lands_but_records_a_waiver() {
    let f = Fixture::new();
    let uid = f.add_task("waived task");
    let s = f.work(&uid);
    commit_in(
        Path::new(s["worktree"].as_str().unwrap()),
        "a.txt",
        "a",
        "a",
    );

    f.jkb()
        .args([
            "task",
            "land",
            &uid,
            "--no-gate",
            "--no-review",
            "--keep-worktree",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("WAIVED"));

    let t = &f.staging(&["--all"])[0]["tasks"][0];
    assert!(
        t["review_waived"].as_str().is_some(),
        "the waiver is recorded on the task, not just printed"
    );
}

/// `tag add` appends and `tag set` replaces. The distinction is load-bearing: `/task-swarm`
/// re-tags a group on every pass, and an appending `onto=` would leave the task claiming two
/// land targets at once — while a command named `add` must not silently delete a value.
#[test]
fn tag_add_appends_but_tag_set_replaces() {
    let f = Fixture::new();
    let uid = f.add_task("tagged task");

    // An open-ended facet legitimately holds several values.
    for v in ["ui", "cli"] {
        f.jkb()
            .args(["--global", "task", "tag", "add", &uid, &format!("area={v}")])
            .assert()
            .success();
    }
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("area=ui"))
        .stdout(predicate::str::contains("area=cli"));

    // `set` collapses a facet to one value — what the location facets need.
    f.jkb()
        .args(["--global", "task", "tag", "set", &uid, "onto=batch-one"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "tag", "set", &uid, "onto=batch-two"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("onto=batch-two"))
        .stdout(predicate::str::contains("onto=batch-one").not());
}

/// A cancelled task is `dropped`, never `landed`. Those are opposite outcomes, and an earlier
/// derivation inferred "landed" from "no session and not `open`/`in_progress`" — which quietly
/// reported every cancelled task as shipped.
#[test]
fn a_cancelled_task_reads_as_dropped_not_landed() {
    let f = Fixture::new();
    let uid = f.add_task("doomed task");
    f.work(&uid);
    f.jkb()
        .args(["--global", "task", "set", &uid, "--status", "cancelled"])
        .assert()
        .success();

    let rows = f.staging(&["--all"]);
    let t = &rows[0]["tasks"][0];
    assert_eq!(t["state"].as_str().unwrap(), "dropped");
}
