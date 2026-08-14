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
        // The refusal comes from `staging::land_blocker` — the same string the In Flight row
        // renders, which is the point: the row cannot say "Landable" about a task refused
        // here, because there is only one sentence and both surfaces read it.
        .stderr(predicate::str::contains(
            "It has no commits that the staging branch does not",
        ));
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

    // A review must have produced findings to be recordable: an empty namespace means the
    // findings never reached the KB, which the gate must not read as clean.
    f.add_finding("reviews/run-1", "something to fix");
    f.jkb()
        .args(["task", "review", "record", "--branch", &branch])
        .args(["--findings", "reviews/run-1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("now needs_review"));

    assert_eq!(f.status_of(&uid), "needs_review");
    let t = &f.staging(&[])[0]["tasks"][0];
    assert_eq!(t["state"].as_str().unwrap(), "review");
    assert_eq!(t["review_nss"][0].as_str().unwrap(), "reviews/run-1");
    let first_sha = t["reviewed"].as_str().unwrap().to_owned();

    // Recording again replaces the SHA rather than accumulating a second one: a task with two
    // `reviewed=` values is a contradiction, and a reader collapsing the multi-map picks one.
    commit_in(
        Path::new(s["worktree"].as_str().unwrap()),
        "b.txt",
        "b",
        "b",
    );
    f.add_finding("reviews/run-2", "a second-run finding");
    f.jkb()
        .args(["task", "review", "record", "--branch", &branch])
        .args(["--findings", "reviews/run-2"])
        .assert()
        .success();
    let t = &f.staging(&[])[0]["tasks"][0];
    assert_ne!(t["reviewed"].as_str().unwrap(), first_sha);
    // Both runs are reported: the gate unions them, so a surface that showed only one could
    // open a clean namespace while the count came from the other.
    assert_eq!(t["review_nss"][0].as_str().unwrap(), "reviews/run-1");
    assert_eq!(t["review_nss"][1].as_str().unwrap(), "reviews/run-2");
    // Both runs still gate: re-recording must not retire the first run's open must-fix
    // findings, or fixing one finding and re-reviewing would silently un-block the rest.
    assert_eq!(t["open_must_fix"], 2);

    // A branch no task claims is a note, not an error — reviewing an arbitrary range is a
    // legitimate thing to do.
    f.add_finding("reviews/run-3", "a third finding");
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

/// The gate must fail CLOSED. A review recorded against a namespace that holds no findings is
/// a review whose findings never reached the KB — a quarantined `tasks.md`, a typo, a renamed
/// namespace — and reading that as "clean" is the one direction a safety check must not fail.
#[test]
fn a_review_with_no_findings_is_refused_not_read_as_clean() {
    let f = Fixture::new();
    let uid = f.add_task("gate-fails-closed task");
    let s = f.work(&uid);
    let branch = s["branch"].as_str().unwrap().to_owned();
    commit_in(
        Path::new(s["worktree"].as_str().unwrap()),
        "a.txt",
        "a",
        "a",
    );

    // Recording is refused at the point it can still be fixed.
    f.jkb()
        .args(["task", "review", "record", "--branch", &branch])
        .args(["--findings", "reviews/never-synced"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no findings found"));

    // And a task that somehow carries such a facet pair still cannot land: the gate checks
    // that findings exist, not merely that a namespace was named.
    f.jkb()
        .args(["--global", "task", "tag", "set", &uid, "reviewed=deadbeef"])
        .assert()
        .success();
    f.jkb()
        .args([
            "--global",
            "task",
            "tag",
            "set",
            &uid,
            "review=reviews/never-synced",
        ])
        .assert()
        .success();
    f.jkb()
        .args(["task", "land", &uid, "--no-gate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("holds no findings at all"));
    assert_eq!(f.status_of(&uid), "in_progress");
}

/// A failed land must leave no waiver behind. `--no-review` records one only once the landing
/// has actually happened — otherwise a task carries a permanent "deliberately unreviewed" mark
/// for something that never occurred, and the UI reads it as landable while the CLI refuses.
#[test]
fn a_failed_land_records_no_waiver() {
    let f = Fixture::new();
    let uid = f.add_task("failing waived task");
    let s = f.work(&uid);
    commit_in(
        Path::new(s["worktree"].as_str().unwrap()),
        "a.txt",
        "a",
        "a",
    );

    // A red gate fails the land after the graft.
    f.jkb()
        .args(["task", "land", &uid, "--gate", "false", "--no-review"])
        .assert()
        .failure();

    let t = &f.staging(&[])[0]["tasks"][0];
    assert!(
        t["review_waived"].is_null(),
        "a land that failed must not leave a waiver: {t}"
    );
    assert!(t["reviewed"].is_null());
}

/// Abandoning must not free a claim this session does not hold **and that is still alive**.
/// The swarm's tasks now appear in the same views, so another implementer's live work is one
/// right-click away — but a claim left behind by one that crashed must not block the cleanup.
#[test]
fn abandon_refuses_a_claim_held_by_someone_else() {
    let f = Fixture::new();
    let uid = f.add_task("someone elses task");
    // A live owner that is not a session here — what a running swarm implementer looks like
    // from this process's point of view. This test's own pid is, definitionally, alive.
    let live = format!("swarm:{}", std::process::id());
    f.jkb()
        .args(["--global", "task", "claim", &uid, "--owner", &live])
        .assert()
        .success();
    f.jkb()
        .args([
            "--global",
            "task",
            "tag",
            "set",
            &uid,
            "branch=swarm-task/thing",
        ])
        .assert()
        .success();

    f.jkb()
        .args(["task", "abandon", &uid])
        .assert()
        .failure()
        .stderr(predicate::str::contains(format!("claimed by {live}")));
    assert_eq!(f.status_of(&uid), "in_progress");

    // --force is the deliberate override.
    f.jkb()
        .args(["task", "abandon", &uid, "--force"])
        .assert()
        .success();
    assert_eq!(f.status_of(&uid), "open");
}

/// ...but a claim whose owner is **gone** must not block the one command that cleans a
/// session up. Judging by owner-string identity refused every claim this process did not
/// hold, so the wreckage of a crashed implementer blocked the verb that exists to remove it,
/// and pointed the user at `jkb task release` for an owner that provably no longer exists.
#[test]
fn abandon_frees_a_dead_owners_claim() {
    let f = Fixture::new();
    let uid = f.add_task("a crashed implementers task");
    f.jkb()
        .args([
            "--global",
            "task",
            "claim",
            &uid,
            "--owner",
            "swarm:4294967290",
        ])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "tag", "set", &uid, "branch=swarm/gone"])
        .assert()
        .success();

    f.jkb().args(["task", "abandon", &uid]).assert().success();
    assert_eq!(f.status_of(&uid), "open");
}

/// A finished task must not be **reopened** by either verb that could.
///
/// `land` removes the worktree and marks the task done, but the row stays in the In Flight
/// view — where one click on "Abandon this session" ran `task abandon`, which found no
/// session, skipped every removal step, and set the status straight back to `open`. Work
/// already on the staging branch was then on the ready frontier again, still tagged with its
/// branch and re-dispatchable to the swarm, with nothing saying so. Landing a *cancelled*
/// task is the mirror image: the graft succeeds and `settle_landing` marks
/// deliberately-dropped work `done`.
///
/// Note what is NOT refused: abandoning a terminal task still disposes of its session.
/// Refusing that outright was the first version of this guard, and it stranded the worktree
/// of every cancelled task — nothing else removes one — while the escape it suggested
/// (reopen, then abandon) performed exactly the reopening it existed to prevent.
#[test]
fn a_terminal_task_is_not_reopened_by_abandoning_or_landed_again() {
    let f = Fixture::new();
    let landed = f.add_task("already landed task");
    let s = f.work(&landed);
    commit_in(
        Path::new(s["worktree"].as_str().unwrap()),
        "a.txt",
        "a",
        "a",
    );
    f.jkb()
        .args(["task", "land", &landed, "--no-gate", "--no-review"])
        .assert()
        .success();
    assert_eq!(f.status_of(&landed), "done");

    f.jkb()
        .args(["task", "abandon", &landed])
        .assert()
        .success()
        .stdout(predicate::str::contains("it stays done"));
    assert_eq!(f.status_of(&landed), "done", "still done");

    // The In Flight row says the same thing, from the CLI's own verdict.
    let branches = f.staging(&["--all"]);
    let row = branches[0]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["uid"] == serde_json::json!(landed))
        .unwrap()
        .clone();
    assert_eq!(row["state"], serde_json::json!("landed"));
    assert!(
        row["land_blocked"]
            .as_str()
            .unwrap()
            .contains("nothing left to land"),
        "the row must carry the CLI's refusal: {row}"
    );

    // And the cancelled half: a dropped task is not landable either.
    let dropped = f.add_task("cancelled task");
    let s = f.work(&dropped);
    commit_in(
        Path::new(s["worktree"].as_str().unwrap()),
        "b.txt",
        "b",
        "b",
    );
    f.jkb()
        .args(["--global", "task", "set", &dropped, "--status", "cancelled"])
        .assert()
        .success();
    f.jkb()
        .args(["task", "land", &dropped, "--no-gate", "--no-review"])
        .assert()
        .failure()
        // From `land_blocker`'s terminal arm — the same sentence the In Flight row shows,
        // rather than a second one written beside it in `land_preflight`.
        .stderr(predicate::str::contains(
            "It was cancelled, so it will not be landing",
        ));
    assert_eq!(f.status_of(&dropped), "cancelled");
}

/// Abandoning takes the task off the staging branch it was going to land on.
///
/// Leaving `onto=` behind kept the abandoned task rendering as live `implementing` work —
/// indistinguishable from the session just destroyed — and, worse, kept `has_live_work` true
/// for its branch, so a spent batch stayed listed and stayed on offer as a land target long
/// after everything on it had merged (the failure D36.3 exists to prevent).
#[test]
fn abandoning_takes_the_task_off_its_staging_branch() {
    let f = Fixture::new();
    let uid = f.add_task("abandoned staged task");
    let s = f.work(&uid);
    let onto = s["onto"].as_str().unwrap().to_owned();
    commit_in(
        Path::new(s["worktree"].as_str().unwrap()),
        "c.txt",
        "c",
        "c",
    );
    assert_eq!(f.staging(&[])[0]["branch"], serde_json::json!(onto));

    f.jkb().args(["task", "abandon", &uid]).assert().success();

    let rows = f.staging(&["--all"]);
    assert!(
        rows.as_array().unwrap().is_empty(),
        "an abandoned task is not on a staging branch any more: {rows}"
    );
}

/// Abandoning a **cancelled** task must dispose of its session — nothing else will.
///
/// Cancelling removes neither worktree, branch nor claim, and `land` refuses a terminal task,
/// so refusing here too left the checkout in place with no verb able to remove it. The task's
/// status is the one thing abandon must not touch.
#[test]
fn abandoning_a_cancelled_task_removes_its_session_and_leaves_it_cancelled() {
    let f = Fixture::new();
    let uid = f.add_task("cancelled but checked out");
    let s = f.work(&uid);
    let wt = PathBuf::from(s["worktree"].as_str().unwrap());
    commit_in(&wt, "a.txt", "a", "a");
    f.jkb()
        .args(["--global", "task", "set", &uid, "--status", "cancelled"])
        .assert()
        .success();
    assert!(wt.exists(), "cancelling leaves the checkout behind");

    f.jkb()
        .args(["task", "abandon", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("it stays cancelled"));

    assert!(!wt.exists(), "the session is gone");
    assert_eq!(
        f.status_of(&uid),
        "cancelled",
        "and the task was NOT put back on the frontier"
    );
}

/// A landing that cannot dispose of its session must not have marked the task done first.
///
/// The status write and `claim::clear` used to run before `worktree_remove`, which git
/// refuses on a dirty tree — leaving the task `done` with its claim freed and its worktree
/// still there, a state `land` ("is done — there is nothing to land") and `abandon` both
/// then declined to touch. Writing during the gate is the ordinary case here: the session
/// holds a live agent.
#[test]
fn a_session_dirtied_during_the_gate_keeps_its_work_and_its_task() {
    let f = Fixture::new();
    let uid = f.add_task("dirtied during the gate");
    let s = f.work(&uid);
    let wt = PathBuf::from(s["worktree"].as_str().unwrap());
    commit_in(&wt, "a.txt", "a", "a");

    // The gate writes into the session while it runs — exactly what an agent left working in
    // it would do. It still passes, so the landing itself succeeds.
    let gate = format!(
        "sh -c 'printf mid-gate > {}/scratch.txt'",
        wt.to_str().unwrap()
    );
    f.jkb()
        .args(["task", "land", &uid, "--gate", &gate, "--no-review"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "uncommitted changes written since",
        ));

    assert!(
        wt.join("scratch.txt").exists(),
        "the work written during the gate survives"
    );
    assert_ne!(
        f.status_of(&uid),
        "done",
        "and the task is not left done with a worktree nothing can remove"
    );
}

/// A review of a session that has not committed yet must still record (design D38.4).
///
/// `jkb task work` creates the branch at the sha it records as `base=`, so the branch's tip and
/// its base are the same commit. Probing that branch for containment *against itself* answers
/// `NothingToMerge`, which is not "covered" — so the task was skipped, no `reviewed=` was
/// written, and `jkb task land` refused the branch `/review-log` had just called landable,
/// leaving `--no-review` as the remedy at hand. Reviewing a dirty working tree is the documented
/// common case, and every other test here commits first, so nothing covered it.
#[test]
fn a_review_of_an_uncommitted_session_still_records() {
    let f = Fixture::new();
    let uid = f.add_task("reviewed before committing");
    let s = f.work(&uid);
    let branch = s["branch"].as_str().unwrap().to_owned();

    // Deliberately NO commit: the session is exactly as `task work` left it.
    f.add_finding("reviews/early", "found while the tree was dirty");
    f.jkb()
        .args(["task", "review", "record", "--branch", &branch])
        .args(["--findings", "reviews/early"])
        .assert()
        .success()
        .stdout(predicate::str::contains("now needs_review"));

    assert_eq!(
        f.status_of(&uid),
        "needs_review",
        "the task's own branch is covered by definition — it must not be skipped"
    );
    let t = &f.staging(&[])[0]["tasks"][0];
    assert!(
        t["reviewed"].as_str().is_some(),
        "reviewed= must be recorded, or `task land` refuses and --no-review becomes the habit"
    );
}

/// The cut point a branch was created with must survive the branch being re-worked.
///
/// `task work` used to decide whether to record one by asking `resumed`, which is worktree
/// existence. `abandon` removes the worktree and keeps the branch, so re-working gives
/// `resumed == false` while `worktree_add` merely re-attaches the branch that is already there —
/// and the "cut point" then recorded was the land target's *current* tip, long past where the
/// branch actually began. `is_merged` compares the branch tip against that value to tell "freshly
/// cut, nothing on it yet" from "landed"; with the wrong value the guard is skipped and
/// `close-merged` closes a task whose work is still in flight.
///
/// The question that answers this correctly is "is a cut point already recorded for this branch",
/// and it now has one implementation for every writer.
#[test]
fn re_working_an_abandoned_branch_keeps_its_original_cut_point() {
    let f = Fixture::new();
    let empty = f.add_task("re-worked task");
    let other = f.add_task("other task");

    // Open the session under test first, so the batch branch is cut here and its recorded cut
    // point is this branch's own tip.
    let s = f.work(&empty);
    let branch = s["branch"].as_str().unwrap().to_owned();
    let onto = s["onto"].as_str().unwrap().to_owned();
    let cut_point = git(&f.repo, &["rev-parse", &branch]);

    // A sibling lands onto the same batch, so the land target moves on. This is the ordinary
    // case, not a contrivance: a batch exists to collect several tasks.
    let so = f.work(&other);
    assert_eq!(so["onto"].as_str().unwrap(), onto, "setup: one batch");
    commit_in(
        Path::new(so["worktree"].as_str().unwrap()),
        "o.txt",
        "from other\n",
        "add o",
    );
    f.jkb()
        .args(["task", "land", &other, "--gate", "true", "--no-review"])
        .assert()
        .success();
    assert_ne!(
        git(&f.repo, &["rev-parse", &onto]),
        cut_point,
        "setup: the land target must have moved for this to test anything"
    );

    // Abandon without committing, then re-work. The worktree is gone but the branch is not, so
    // `worktree_add` re-attaches it and `resumed` — which this used to be gated on — is false.
    f.jkb().args(["task", "abandon", &empty]).assert().success();
    f.jkb().args(["task", "work", &empty]).assert().success();
    assert_eq!(
        git(&f.repo, &["rev-parse", &branch]),
        cut_point,
        "setup: the re-worked branch still has no commits of its own"
    );

    // Asserted as a **value**, before the consequence. This test passed under a mutation that
    // recorded no cut point at all: with none, nothing ever closes, so the status assertion below
    // is satisfied by the very regression the test is named for. A test whose name promises
    // preservation has to look at what was preserved.
    f.jkb()
        .args(["--global", "task", "show", &empty])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "base={branch}:{cut_point}"
        )));

    // The harm itself. A branch with nothing on it re-merges to trunk's own tree, so on content
    // alone it reads as merged; the recorded cut point is the only thing that separates "not
    // started" from "landed".
    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&empty),
        "in_progress",
        "an empty re-worked session was closed as merged: its cut point had been overwritten \
         with the land target's current tip"
    );
}

/// The cut point cannot be written through the generic tag command, which would delete the
/// records belonging to the task's *other* branches.
///
/// This is not a style preference. The refusal replaced an error message that named
/// `jkb task tag set base=<branch>:<sha>` as its own remedy, and `/task-swarm` was changed to run
/// exactly that — so following the tool's advice destroyed the records the tool then refused to
/// act without.
#[test]
fn the_cut_point_cannot_be_written_through_the_generic_tag_command() {
    let f = Fixture::new();
    let uid = f.add_task("hand-tagged task");

    for mode in ["add", "set"] {
        f.jkb()
            .args(["--global", "task", "tag", mode, &uid, "base=task/x:abc"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("jkb task base"));
    }

    // The verb it points at records per branch, leaving siblings alone. Real revisions: the verb
    // refuses one this repo cannot resolve, because an unresolvable cut point is treated as none.
    let first = git(&f.repo, &["rev-parse", "HEAD"]);
    commit_in(&f.repo, "second.txt", "second\n", "second");
    let second = git(&f.repo, &["rev-parse", "HEAD"]);
    for (branch, sha) in [("task/x", &first), ("task/y", &second)] {
        f.jkb()
            .args(["--global", "task", "base", &uid, branch, sha])
            .assert()
            .success();
    }
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("base=task/x:{first}")))
        .stdout(predicate::str::contains(format!("base=task/y:{second}")));
}

/// A cut point git cannot resolve must not act as one, however it got into the store.
///
/// `is_merged` separates "freshly cut, nothing on it yet" from "landed" by comparing the branch
/// tip against `rev-parse <base>`. When the base does not resolve, that comparison is false rather
/// than unknown, so the guard is *skipped*: an empty branch falls through to `merge-tree`, gets
/// trunk's own tree back, and reads as merged. A missing cut point refuses to act; a garbage one
/// closed the task with the work never written.
///
/// The value is injected the way a user actually can — `#base=` in quick-add text, which reaches
/// `tag::apply` without passing the `jkb task tag` refusal — and `task work` then adopts the bare
/// value as this branch's cut point. So this covers the reservation's gap as well as the policy.
///
/// **Run at both lengths, and the 40-character one is the case that mattered.** `git rev-parse`
/// is a parser, not a lookup: a full-length hex string is already a well-formed object name, so
/// it exits 0 and echoes it back for an object the clone does not have. The first version of this
/// test used a 16-character value, which `rev-parse` *does* reject, so it passed against a check
/// that let every fabricated 40-character sha straight through — a constant chosen without
/// thinking about it hid the entire defect.
#[test]
fn a_cut_point_git_cannot_resolve_is_treated_as_none() {
    // Three shapes, three different reasons the task must not close, and picking only one is how
    // the first two versions of this test passed against real defects:
    //
    // - a short hex string: rejected by `rev-parse`, and never adopted, since it is not an
    //   object id;
    // - a full-length hex string: **accepted** by `rev-parse`, which parses rather than looks up,
    //   so only a verifying resolution rejects it;
    // - a symbolic revision: resolves in *every* repository, to something different in each, so
    //   the write must refuse the form outright and the reader must not honour it.
    for fake in [
        "deadbeefdeadbeef",
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        "HEAD",
    ] {
        a_cut_point_git_cannot_resolve_case(fake);
    }
}

fn a_cut_point_git_cannot_resolve_case(fake: &str) {
    let f = Fixture::new();
    let uid = f.add_task(&format!("bogus base task #base={fake}"));
    let s = f.work(&uid);
    let branch = s["branch"].as_str().unwrap().to_owned();

    // Two rules, and which applies depends on the shape — asserting only one is what tied the
    // earlier versions of this test to a single path.
    //
    // Object-id *form* is what the write refuses, so a value without it must never appear as a
    // qualified cut point at all. One that has the form is legitimately adopted, and is stopped
    // instead by the reader, which cannot resolve it. Either way the task must not close.
    let object_id_form =
        matches!(fake.len(), 40 | 64) && fake.chars().all(|c| c.is_ascii_hexdigit());
    let recorded = predicate::str::contains(format!("base={branch}:{fake}"));
    let show = f
        .jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success();
    if object_id_form {
        show.stdout(recorded);
    } else {
        show.stdout(recorded.not());
    }
    assert_eq!(
        git(&f.repo, &["rev-parse", &branch]),
        git(&f.repo, &["rev-parse", "main"]),
        "setup: the session branch must have no commits of its own"
    );

    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "an empty branch was closed as merged: the cut point `{fake}` did not resolve, so the \
         freshly-cut guard was skipped instead of applied"
    );
}

/// And the verb refuses to record one in the first place, so a typo is loud rather than a task
/// that quietly stops auto-closing.
#[test]
fn task_base_refuses_a_revision_this_repo_cannot_resolve() {
    let f = Fixture::new();
    let uid = f.add_task("typo base task");

    // Both lengths: a 40-character hex string parses as a well-formed object name, so only a
    // *verifying* resolution rejects one this repo does not have.
    for fake in ["aaaaaaa", "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"] {
        f.jkb()
            .args(["--global", "task", "base", &uid, "task/x", fake])
            .assert()
            .failure()
            .stderr(predicate::str::contains(
                "not a revision this repo can resolve",
            ));
    }
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("base=").not());

    // A real revision still records, resolved to its full id.
    let head = git(&f.repo, &["rev-parse", "HEAD"]);
    f.jkb()
        .args(["--global", "task", "base", &uid, "task/x", "HEAD"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("base=task/x:{head}")));
}

/// `close-merged` must say "no usable cut point" rather than "still in flight". They are
/// different answers: one resolves itself when the work lands, the other never resolves and has a
/// remedy the user cannot guess.
///
/// Pending tasks are counted, not listed, so folding this case in there meant the one place a
/// user meets a missing cut point printed nothing about it at all.
#[test]
fn close_merged_names_a_task_it_cannot_decide() {
    let f = Fixture::new();
    let uid = f.add_task("undecidable task");
    git(&f.repo, &["branch", "feature-x"]);
    f.jkb()
        .args(["task", "start", &uid, "--branch", "feature-x"])
        .assert()
        .success();

    // With its cut point recorded the task is merely in flight, and nothing is printed for it.
    f.jkb()
        .args(["task", "close-merged"])
        .assert()
        .success()
        .stdout(predicate::str::contains("still in flight"))
        .stdout(predicate::str::contains(&uid).not());

    // Remove it, as `task tag rm` or an unattributable legacy value would.
    let base = git(&f.repo, &["rev-parse", "HEAD"]);
    f.jkb()
        .args([
            "--global",
            "task",
            "tag",
            "rm",
            &uid,
            &format!("base=feature-x:{base}"),
        ])
        .assert()
        .success();

    f.jkb()
        .args(["task", "close-merged"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&uid))
        .stdout(predicate::str::contains("no usable cut point"))
        .stdout(predicate::str::contains("jkb task base"));
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "an undecidable task must be reported, never closed"
    );
}

/// A branch that lives only on the remote is not gone.
///
/// `close-merged` asks with `Prefer::Remote` precisely because after a merged PR the local copy is
/// usually deleted, so `origin/<branch>` is the honest answer to "did this ship". A bare
/// `refs/heads/` probe therefore called a live branch gone and printed "remove the stale tag" —
/// advice that deletes the only record of work still in flight.
#[test]
fn a_branch_that_exists_only_on_the_remote_is_not_reported_gone() {
    let f = Fixture::new();
    let origin = f.home.path().join("origin.git");
    git(&f.repo, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(
        &f.repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&f.repo, &["push", "-q", "origin", "main"]);

    // Work on a branch, publish it, then drop the local copy — the post-PR state.
    git(&f.repo, &["checkout", "-q", "-b", "shipped"]);
    commit_in(&f.repo, "s.txt", "shipped\n", "ship it");
    git(&f.repo, &["push", "-q", "origin", "shipped"]);
    git(&f.repo, &["checkout", "-q", "main"]);
    git(&f.repo, &["branch", "-D", "shipped"]);

    let uid = f.add_task("remote-only branch task");
    f.jkb()
        .args(["task", "start", &uid, "--branch", "shipped"])
        .assert()
        .success();

    f.jkb()
        .args(["task", "close-merged"])
        .assert()
        .success()
        .stdout(predicate::str::contains("gone").not());
}

/// `jkb task base` must not resolve a revision against a repo that is not the task's.
///
/// The database is global across repos, so this command runs from anywhere. Resolving in whatever
/// checkout happens to be current means a sha that exists *there* is recorded as this task's cut
/// point and printed as though verified — a wrong commit id presented as a checked one, which is
/// worse than the rejected-good-sha nit the check was added to fix.
#[test]
fn task_base_does_not_resolve_a_sha_against_a_foreign_repo() {
    let f = Fixture::new();
    let other = f.home.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q", "-b", "main", "."]);
    std::fs::write(other.join("o.txt"), "other\n").unwrap();
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-qm", "other base"]);
    let foreign = git(&other, &["rev-parse", "HEAD"]);

    let uid = f.add_task("cross repo task");
    f.jkb()
        .args(["--global", "task", "tag", "add", &uid, "repo=proj"])
        .assert()
        .success();

    // Standing in the *other* repo, where `foreign` resolves and the task's repo is elsewhere.
    let mut cmd = f.jkb();
    cmd.current_dir(&other)
        .args(["--global", "task", "base", &uid, "task/x", &foreign])
        .assert()
        .success()
        .stderr(predicate::str::contains("unverified"));

    // Recorded verbatim and flagged, never presented as a resolved commit in this repo.
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("base=task/x:{foreign}")));
}

/// A batch branch that exists only on the remote must be checked out, never re-cut from trunk.
///
/// `git branch <name> <trunk>` succeeds when no *local* ref of that name exists, so a bare
/// `refs/heads/` probe let a session point at an empty same-named branch carrying none of the
/// batch's work — `staging ls` then reported it as the batch and the eventual push was rejected as
/// non-fast-forward. The question "should I create this branch" has to count the remote.
#[test]
fn a_remote_only_batch_branch_is_checked_out_not_re_cut() {
    let f = Fixture::new();
    let origin = f.home.path().join("origin.git");
    git(&f.repo, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(
        &f.repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&f.repo, &["push", "-q", "origin", "main"]);

    // A batch with work on it, published, then pruned locally.
    git(&f.repo, &["checkout", "-q", "-b", "batch/x"]);
    commit_in(&f.repo, "batch.txt", "batch work\n", "batch work");
    let batch_tip = git(&f.repo, &["rev-parse", "HEAD"]);
    git(&f.repo, &["push", "-q", "origin", "batch/x"]);
    git(&f.repo, &["checkout", "-q", "main"]);
    git(&f.repo, &["branch", "-D", "batch/x"]);

    let uid = f.add_task("joins the remote batch");
    let s = f.work_onto(&uid, "batch/x");
    assert_eq!(s["onto"].as_str().unwrap(), "batch/x");

    // The local branch now carries the batch's commit, not a fresh cut from trunk.
    assert_eq!(
        git(&f.repo, &["rev-parse", "batch/x"]),
        batch_tip,
        "the remote-only batch was re-cut from trunk, so the session would land on a branch \
         carrying none of the batch's work"
    );
    assert!(
        git(&f.repo, &["ls-tree", "-r", "--name-only", "batch/x"]).contains("batch.txt"),
        "the checked-out batch is missing its own work"
    );
}

/// Cutting a *new* batch must not adopt a same-named branch left on the remote.
///
/// The mirror of `a_remote_only_batch_branch_is_checked_out_not_re_cut`, and the reason the two
/// behaviours are separate functions rather than one remote-aware primitive. An explicit
/// `--onto <batch>` names a branch that already exists, so its remote copy is the thing meant.
/// The unnamed path is *creating* the first batch of a run at a known start point, and a stale
/// namesake on the remote — an earlier batch of a similarly-titled task, quite possibly already
/// merged — is not that branch. Adopting it would land the session onto merged work.
#[test]
fn a_fresh_batch_is_cut_from_trunk_not_adopted_from_a_stale_remote_namesake() {
    let f = Fixture::new();
    let origin = f.home.path().join("origin.git");
    git(&f.repo, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(
        &f.repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&f.repo, &["push", "-q", "origin", "main"]);

    // A stale batch of the same name the next task will mint, published and gone locally.
    git(&f.repo, &["checkout", "-q", "-b", "stale-task"]);
    commit_in(&f.repo, "stale.txt", "old batch\n", "old batch work");
    git(&f.repo, &["push", "-q", "origin", "stale-task"]);
    git(&f.repo, &["checkout", "-q", "main"]);
    git(&f.repo, &["branch", "-D", "stale-task"]);
    let trunk = git(&f.repo, &["rev-parse", "main"]);

    let uid = f.add_task("stale task");
    let s = f.work(&uid);
    assert_eq!(
        s["onto"].as_str().unwrap(),
        "stale-task",
        "setup: the batch should be named after the task"
    );
    assert_eq!(
        git(&f.repo, &["rev-parse", "stale-task"]),
        trunk,
        "a fresh batch adopted a stale remote namesake, so the session lands onto old work"
    );
    assert!(
        !git(&f.repo, &["ls-tree", "-r", "--name-only", "stale-task"]).contains("stale.txt"),
        "the fresh batch carries the stale remote batch's commits"
    );
}

/// A branch name is user input and reaches `git` as a positional operand, so one beginning with
/// `-` becomes an option.
///
/// `jkb task work <uid> --onto=-D` ran `git branch -D <trunk>` and **deleted the repository's
/// trunk branch**. `clap` blocks the separated form `--onto -D` but passes `--onto=-D` through,
/// which is the ordinary way to give an option a hyphenated value — so the CLI parser is not the
/// guard, and cannot be.
///
/// Every flag that accepts a ref is probed, because the point is that the check lives below all of
/// them rather than at each one.
#[test]
fn a_branch_name_cannot_smuggle_a_git_option() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "victim"]);
    git(&f.repo, &["checkout", "-q", "-b", "feature"]);
    let uid = f.add_task("injection probe");

    let hostile = [
        vec!["task", "work", uid.as_str(), "--onto=-D"],
        vec!["task", "start", uid.as_str(), "--branch=-D"],
        vec!["--global", "task", "base", uid.as_str(), "-D", "HEAD"],
        vec!["task", "close-merged", "--trunk=-D"],
    ];
    for args in hostile {
        f.jkb().args(&args).assert().failure();
    }

    // Both branches survive every attempt.
    let after = git(&f.repo, &["branch", "--format=%(refname:short)"]);
    assert!(after.contains("main"), "trunk was deleted: {after}");
    assert!(after.contains("victim"), "a branch was deleted: {after}");
}

/// The cut point must be measured on the branch, not guessed from the land target.
///
/// They agree for a freshly cut session — it has no commits, so its tip *is* the target's tip —
/// and come apart whenever `task work` adopts a branch it did not just create: one left behind by
/// a run that failed after creating it, one made by hand, or one adopted from the remote. The
/// target has moved on since, so the guess records a commit the branch never sat on; the tip then
/// differs from the base, `is_merged` skips its freshly-cut guard, and an empty branch closes the
/// task as merged.
#[test]
fn the_cut_point_is_measured_on_the_branch_not_the_land_target() {
    let f = Fixture::new();
    let uid = f.add_task("stranded task");

    // A batch, and a branch sitting on it — the state a failed `task work` leaves behind.
    git(&f.repo, &["branch", "batch", "main"]);
    git(&f.repo, &["branch", "task/stranded-task", "batch"]);
    let branch_tip = git(&f.repo, &["rev-parse", "task/stranded-task"]);

    // The batch moves on, as it does whenever a sibling task lands onto it.
    git(&f.repo, &["checkout", "-q", "batch"]);
    commit_in(&f.repo, "sibling.txt", "landed\n", "sibling landed");
    git(&f.repo, &["checkout", "-q", "main"]);
    assert_ne!(git(&f.repo, &["rev-parse", "batch"]), branch_tip, "setup");

    f.jkb()
        .args(["task", "work", &uid, "--onto", "batch"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "base=task/stranded-task:{branch_tip}"
        )));

    // The branch carries nothing of its own, so the task must not close.
    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "an empty adopted branch closed as merged: its cut point was taken from the land target, \
         which had moved, so the freshly-cut guard was skipped"
    );
}

/// `task start` records the branch's **own tip at tracking time** — the same rule `task work`
/// uses, and the only one that answers the question the readers actually ask: *has anything
/// happened on this branch since we started tracking it?*
///
/// Four cases, deliberately **four tests**. They were one test with four blocks, and a `#[test]`
/// stops at its first failing assertion — so a mutation that should have killed case 4 killed
/// case 1 first and the other three were never reached. A case that cannot be shown to fail on
/// its own is not covering anything, which is the same defect as an assertion that cannot fail.
mod cut_point {
    use super::*;

    /// Cut from trunk, trunk has since moved. Recording trunk's *tip* closes this as merged.
    #[test]
    fn a_branch_cut_from_trunk_that_moved_is_held() {
        let f = Fixture::new();
        git(&f.repo, &["branch", "feature"]);
        commit_in(&f.repo, "moved.txt", "trunk moves\n", "trunk moves on");
        let tip = git(&f.repo, &["rev-parse", "feature"]);
        let uid = f.add_task("cut from trunk");
        f.jkb()
            .args(["task", "start", &uid, "--branch", "feature"])
            .assert()
            .success();
        f.jkb()
            .args(["--global", "task", "show", &uid])
            .assert()
            .success()
            .stdout(predicate::str::contains(format!("base=feature:{tip}")));
        f.jkb().args(["task", "close-merged"]).assert().success();
        assert_eq!(f.status_of(&uid), "in_progress");
    }

    /// Cut from a **staging** branch — the D38 flow this project is built around — then staging
    /// lands. Measuring against *trunk* puts the base behind the tip before any work happens.
    #[test]
    fn a_branch_cut_from_staging_is_held_when_staging_lands() {
        let f = Fixture::new();
        git(&f.repo, &["checkout", "-q", "-b", "stg"]);
        commit_in(&f.repo, "s.txt", "staging\n", "staging work");
        git(&f.repo, &["checkout", "-q", "-b", "mytask"]);
        git(&f.repo, &["checkout", "-q", "main"]);
        let tip = git(&f.repo, &["rev-parse", "mytask"]);

        let uid = f.add_task("cut from staging");
        f.jkb()
            .args(["task", "start", &uid, "--branch", "mytask"])
            .assert()
            .success();
        f.jkb()
            .args(["--global", "task", "show", &uid])
            .assert()
            .success()
            .stdout(predicate::str::contains(format!("base=mytask:{tip}")));

        git(&f.repo, &["merge", "-q", "--ff-only", "stg"]);
        f.jkb().args(["task", "close-merged"]).assert().success();
        assert_eq!(
            f.status_of(&uid),
            "in_progress",
            "a branch cut from staging closed as merged with zero commits of its own"
        );
    }

    /// Pre-existing commits and none after: held. The accepted cost of one rule — a missed
    /// auto-close, which costs a command, rather than a false one, which buries work (D34.4).
    #[test]
    fn a_branch_worked_before_tracking_began_is_held() {
        let f = Fixture::new();
        git(&f.repo, &["checkout", "-q", "-b", "wip"]);
        commit_in(&f.repo, "w.txt", "work\n", "work");
        git(&f.repo, &["checkout", "-q", "main"]);
        let uid = f.add_task("started after the work");
        f.jkb()
            .args(["task", "start", &uid, "--branch", "wip"])
            .assert()
            .success();
        git(&f.repo, &["merge", "-q", "--ff-only", "wip"]);
        f.jkb().args(["task", "close-merged"]).assert().success();
        assert_eq!(f.status_of(&uid), "in_progress");
    }

    /// A commit after tracking began: closes. Without this the feature could be dead — "always
    /// held" satisfies every other case here.
    #[test]
    fn a_branch_worked_after_tracking_began_still_auto_closes() {
        let f = Fixture::new();
        git(&f.repo, &["branch", "later"]);
        let uid = f.add_task("started then worked");
        f.jkb()
            .args(["task", "start", &uid, "--branch", "later"])
            .assert()
            .success();
        git(&f.repo, &["checkout", "-q", "later"]);
        commit_in(&f.repo, "l.txt", "later\n", "later work");
        git(&f.repo, &["checkout", "-q", "main"]);
        git(&f.repo, &["merge", "-q", "--ff-only", "later"]);
        f.jkb().args(["task", "close-merged"]).assert().success();
        assert_eq!(
            f.status_of(&uid),
            "done",
            "auto-close no longer fires at all, so the feature is dead"
        );
    }
}

/// A land target that exists only on the remote must be materialised, not merely counted as live.
///
/// A bare branch name under `refs/remotes` is not a valid start point, so deciding the batch is
/// live and handing the name on gave `git branch <session> <batch>` something it could not
/// resolve — aborting `task work` after the claim had been taken. The mirror is `land`, which
/// refused a target it would itself have checked out a moment later.
#[test]
fn a_remote_only_land_target_is_materialised_for_both_work_and_land() {
    let f = Fixture::new();
    let origin = f.home.path().join("origin.git");
    git(&f.repo, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(
        &f.repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&f.repo, &["push", "-q", "origin", "main"]);

    // Session A opens a batch, publishes it; the local ref is then pruned while A stays open.
    let a = f.add_task("task a");
    let sa = f.work_onto(&a, "batch");
    commit_in(
        Path::new(sa["worktree"].as_str().unwrap()),
        "a.txt",
        "a\n",
        "a",
    );
    git(&f.repo, &["push", "-q", "origin", "batch"]);
    git(&f.repo, &["branch", "-D", "batch"]);
    assert!(
        git(&f.repo, &["branch", "--format=%(refname:short)"])
            .lines()
            .all(|b| b != "batch"),
        "setup: the local batch ref must be gone"
    );

    // A second task joins the batch through the implicit path — no --onto.
    let c = f.add_task("task c");
    f.jkb().args(["task", "work", &c]).assert().success();
    assert_eq!(
        git(&f.repo, &["rev-parse", "batch"]),
        git(&f.repo, &["rev-parse", "origin/batch"]),
        "the batch was not materialised from its remote copy"
    );

    // And the mirror: A can still land onto it. The local ref is deleted **again** first, or the
    // land half asserts nothing — `task work c` above materialised it, so the preflight's probe
    // was already true either way and reverting that probe left this test green.
    git(&f.repo, &["branch", "-D", "batch"]);
    assert!(
        git(&f.repo, &["branch", "--format=%(refname:short)"])
            .lines()
            .all(|b| b != "batch"),
        "setup: the land half must start with no local ref"
    );
    f.jkb()
        .args(["task", "land", &a, "--gate", "true", "--no-review"])
        .assert()
        .success();
}

/// `staging ls` must count a remote-only batch, because `task work` and `task land` both do.
///
/// It is the one read behind the branch picker and In Flight (D38.2), so a local-only existence
/// check made a live batch vanish from both while the two write commands went on acting on it —
/// the surfaces disagreeing, which is the single thing that read exists to prevent.
///
/// Admitting the row was only half of it, and the half this test used to assert. The counts were
/// still taken with the bare branch name, which a remote-only branch does not resolve to, so
/// `rev-list` exited non-zero and the failure read as **zero commits**: the row said "0 commit(s)
/// vs trunk" and told the task it had nothing to land, while the command landed it. A row that is
/// present but wrong is worse than one that is missing, so the contents are asserted too.
#[test]
fn staging_ls_counts_a_batch_that_exists_only_on_the_remote() {
    let f = Fixture::new();
    let origin = f.home.path().join("origin.git");
    git(&f.repo, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(
        &f.repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&f.repo, &["push", "-q", "origin", "main"]);

    // A batch with a task's worth of work already landed on it, so its own commit count is not
    // trivially zero and a failure to measure it is visible.
    git(&f.repo, &["branch", "batch", "main"]);
    git(&f.repo, &["checkout", "-q", "batch"]);
    commit_in(&f.repo, "landed.txt", "landed\n", "an earlier task landed");
    git(&f.repo, &["checkout", "-q", "main"]);

    let uid = f.add_task("on a pruned batch");
    let s = f.work_onto(&uid, "batch");
    commit_in(
        Path::new(s["worktree"].as_str().unwrap()),
        "a.txt",
        "a\n",
        "a",
    );
    git(&f.repo, &["push", "-q", "origin", "batch"]);
    git(&f.repo, &["branch", "-D", "batch"]);

    let rows = f.staging(&[]);
    assert!(
        !rows.as_array().unwrap().is_empty(),
        "a batch whose local ref was pruned vanished from the one read the picker and In Flight \
         both use, while work and land still act on it: {rows}"
    );
    assert_eq!(rows[0]["branch"], serde_json::json!("batch"));
    assert_eq!(
        rows[0]["ahead"], 1,
        "the batch's commits were counted with a name git cannot resolve, so the failure read \
         as zero: {rows}"
    );
    let task = &rows[0]["tasks"][0];
    assert_eq!(
        task["commits"], 1,
        "the task's own commits were counted the same wrong way: {rows}"
    );
    // And therefore the row does not refuse a landing the command performs.
    assert!(
        !task["land_blocked"]
            .as_str()
            .unwrap_or_default()
            .contains("no commits"),
        "the row said the task has nothing to land while its branch is a commit ahead: {rows}"
    );
    f.jkb()
        .args(["task", "land", &uid, "--gate", "true", "--no-review"])
        .assert()
        .success();
}

/// A repo whose trunk cannot be discovered must still open a session when `--onto` names a branch
/// that already exists — computing a start point that is not needed made the escape hatch the
/// error message recommends fail too.
#[test]
fn an_explicit_onto_works_in_a_repo_with_no_discoverable_trunk() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "-m", "main", "dev"]); // not a DEFAULT_TRUNK, and no remote
    git(&f.repo, &["branch", "feature-x"]);

    let uid = f.add_task("trunkless task");
    let s = f.work_onto(&uid, "feature-x");
    assert_eq!(s["onto"].as_str().unwrap(), "feature-x");
}

/// `--repo` names which tasks to consider; the git questions are still asked here. When that
/// cannot even be determined the command must refuse, not proceed and call every live branch gone.
#[test]
fn close_merged_refuses_an_explicit_repo_it_cannot_verify() {
    let f = Fixture::new();
    let uid = f.add_task("elsewhere task");
    git(&f.repo, &["branch", "feature"]);
    f.jkb()
        .args(["task", "start", &uid, "--branch", "feature"])
        .assert()
        .success();

    let outside = f.home.path().join("not-a-repo");
    std::fs::create_dir_all(&outside).unwrap();
    let mut cmd = f.jkb();
    cmd.current_dir(&outside)
        .args(["task", "close-merged", "--repo", "proj", "--trunk", "main"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a git repository"));
}

/// When no cut point can be measured, `task start` says so.
///
/// `--branch` exists to name a branch other than the one you are on, including one you are about
/// to create — and a branch that does not exist here cannot be measured. Recording nothing is the
/// right answer (a commit the branch never sat on is the alternative, and that is what closes
/// tasks wrongly), but it must not be silent: the task simply never auto-closes, and without a
/// word at the point of the decision that is discovered much later, by a `close-merged` that
/// quietly declines to act.
#[test]
fn task_start_says_so_when_it_can_measure_no_cut_point() {
    let f = Fixture::new();
    let uid = f.add_task("branch not cut yet");

    f.jkb()
        .args(["task", "start", &uid, "--branch", "not-yet"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no cut point was recorded"))
        .stdout(predicate::str::contains("jkb task base"));

    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("base=").not());

    // The JSON path carries the same fact, or a consumer cannot see it at all.
    let out = f
        .jkb()
        .args(["task", "start", &uid, "--branch", "not-yet", "--json"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        v.get("base").is_some() && v["base"].is_null(),
        "the JSON path must say that no cut point was recorded: {v}"
    );

    // And it stays quiet when there is one.
    git(&f.repo, &["branch", "real"]);
    let other = f.add_task("branch exists");
    f.jkb()
        .args(["task", "start", &other, "--branch", "real"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no cut point").not());
    let out = f
        .jkb()
        .args(["task", "start", &other, "--branch", "real", "--json"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["base"].as_str(),
        Some(git(&f.repo, &["rev-parse", "real"]).as_str()),
        "the JSON path must carry the recorded cut point"
    );
}

/// A branch that exists nowhere **is** reported gone, with the remedy that clears it.
///
/// The sibling test asserts the opposite — that a branch living only on the remote is *not*
/// called gone — and a mutation making `gone_branches` return "nothing is ever gone" satisfied it
/// trivially while killing no other test. A pair of assertions where only the negative one exists
/// covers nothing: the reporting could be deleted outright and the suite would stay green.
#[test]
fn a_branch_that_exists_nowhere_is_reported_gone() {
    let f = Fixture::new();
    let uid = f.add_task("vanished branch task");
    f.jkb()
        .args(["--global", "task", "tag", "add", &uid, "repo=proj"])
        .assert()
        .success();
    f.jkb()
        .args([
            "--global",
            "task",
            "tag",
            "add",
            &uid,
            "branch=never-existed",
        ])
        .assert()
        .success();

    f.jkb()
        .args(["task", "close-merged"])
        .assert()
        .success()
        .stdout(predicate::str::contains("never-existed gone"))
        .stdout(predicate::str::contains("jkb task tag rm"));
    assert_eq!(
        f.status_of(&uid),
        "open",
        "a task whose branch is gone is held for a decision, never closed"
    );
}

/// A review of a staging branch must not credit a task whose work is not in it.
///
/// `onto=` says a task *intends* to land on a branch, not that it has. Crediting on intent stamps
/// `reviewed=` on work the review never saw and then lets `jkb task land` graft it — the one
/// direction this gate must not fail. `work_is_in` exists for exactly that, and its own doc says
/// so, yet making it return "covered" for everything killed no test at all: the containment half
/// of the gate was unverified.
#[test]
fn a_review_of_a_staging_branch_skips_work_it_does_not_contain() {
    let f = Fixture::new();
    let uid = f.add_task("still building");
    let s = f.work_onto(&uid, "stg");
    // Commits on the task's own branch, never landed onto the staging branch.
    commit_in(
        Path::new(s["worktree"].as_str().unwrap()),
        "wip.txt",
        "not landed\n",
        "wip",
    );
    f.add_finding("reviews/stg-1", "a finding");

    f.jkb()
        .args(["task", "review", "record", "--branch", "stg"])
        .args(["--findings", "reviews/stg-1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("not merged into it yet"))
        .stdout(predicate::str::contains(&uid));

    // Not credited, and therefore still refused by the land gate.
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "a task whose work is not in the reviewed branch was credited as reviewed"
    );
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("reviewed=").not());
}

/// And a task the review cannot decide about is reported as *undecidable*, naming the verb that
/// fixes it — not as "not merged yet", which asserts a fact nothing checked.
///
/// The two buckets have different remedies, and collapsing them was a pass-26 finding. Making the
/// classifier always claim a usable cut point killed no test, so the distinction it draws was
/// unverified.
#[test]
fn a_review_names_the_task_it_cannot_decide_about() {
    let f = Fixture::new();
    let uid = f.add_task("no cut point");
    let s = f.work_onto(&uid, "stg");
    let branch = s["branch"].as_str().unwrap().to_owned();
    commit_in(
        Path::new(s["worktree"].as_str().unwrap()),
        "wip.txt",
        "not landed\n",
        "wip",
    );
    // Remove the cut point the session recorded, leaving containment undecidable.
    let cut = git(&f.repo, &["rev-parse", &format!("{branch}~1")]);
    f.jkb()
        .args([
            "--global",
            "task",
            "tag",
            "rm",
            &uid,
            &format!("base={branch}:{cut}"),
        ])
        .assert()
        .success();
    f.add_finding("reviews/stg-2", "a finding");

    f.jkb()
        .args(["task", "review", "record", "--branch", "stg"])
        .args(["--findings", "reviews/stg-2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no cut point recorded"))
        .stdout(predicate::str::contains("jkb task base"));
}

/// Reviewing a task's own branch must still check the *other* branches it records.
///
/// `branch=` went single-valued (D36.6), but `tag add` is additive and the readers deliberately
/// index every value — a task really can carry two. A review of one of them has not seen the
/// other, so crediting the task on the strength of the reviewed branch alone stamps `reviewed=`
/// over work still in flight elsewhere. `others_are_covered` exists for that and had no test:
/// making it answer "covered" for everything killed nothing.
#[test]
fn a_review_of_one_branch_does_not_credit_a_tasks_other_live_branch() {
    let f = Fixture::new();
    let uid = f.add_task("two branches");
    let s = f.work(&uid);
    let reviewed = s["branch"].as_str().unwrap().to_owned();
    commit_in(
        Path::new(s["worktree"].as_str().unwrap()),
        "a.txt",
        "a\n",
        "a",
    );

    // A second, live branch with its own unmerged work, recorded on the same task.
    let fork = git(&f.repo, &["rev-parse", "main"]);
    git(&f.repo, &["checkout", "-q", "-b", "sibling"]);
    commit_in(&f.repo, "sib.txt", "sibling work\n", "sibling work");
    git(&f.repo, &["checkout", "-q", "main"]);
    f.jkb()
        .args(["--global", "task", "tag", "add", &uid, "branch=sibling"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "base", &uid, "sibling", &fork])
        .assert()
        .success();

    f.add_finding("reviews/one", "a finding");
    f.jkb()
        .args(["task", "review", "record", "--branch", &reviewed])
        .args(["--findings", "reviews/one"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&uid));

    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("reviewed=").not());
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "a review of one branch credited a task whose other branch it never saw"
    );
}

/// A land target whose checkout has uncommitted changes refuses the landing, and does not lose
/// them.
///
/// Landing rolls the target back with `reset --hard` when the gate goes red, so uncommitted work
/// sitting in that checkout would be destroyed by a landing that had nothing to do with it. The
/// sibling test covers a dirty *session*; the target is the one whose changes a rollback actually
/// reaches.
///
/// There is one rule — `staging::target_dirty_reason` — and this exercises it through the
/// command; the test below exercises it through the row. `cmd_task_land` used to carry a second,
/// independently worded copy on the far side of the land lock, and because both wordings shared
/// the phrase this test asserted on, disabling *either* left it green: two implementations of one
/// rule and neither of them covered. The lock is now taken before the checks instead, which is
/// what actually closes the window the second copy was excusing its existence with.
///
/// So the assertion is on wording only that one function produces.
#[test]
fn a_dirty_land_target_refuses_the_landing_and_keeps_its_changes() {
    let f = Fixture::new();
    let a = f.add_task("first");
    let b = f.add_task("second");
    let sa = f.work(&a);
    let sb = f.work(&b);
    commit_in(
        Path::new(sa["worktree"].as_str().unwrap()),
        "a.txt",
        "a\n",
        "a",
    );
    commit_in(
        Path::new(sb["worktree"].as_str().unwrap()),
        "b.txt",
        "b\n",
        "b",
    );

    // Landing the first task materialises the batch's own checkout under `.jkb/base`.
    f.jkb()
        .args(["task", "land", &a, "--gate", "true", "--no-review"])
        .assert()
        .success();
    let base = f.repo.join(".jkb").join("base");
    assert!(
        base.is_dir(),
        "setup: the land target's checkout should exist"
    );

    // Someone leaves uncommitted work in it.
    let stray = base.join("precious.txt");
    std::fs::write(&stray, "not committed anywhere\n").unwrap();

    f.jkb()
        .args(["task", "land", &b, "--gate", "true", "--no-review"])
        .assert()
        .failure()
        // The phrase belongs to `staging::target_dirty_reason` and to nothing else, so this
        // fails if that rule stops firing rather than being caught by a second copy of it.
        .stderr(predicate::str::contains("the checkout a land onto"))
        .stderr(predicate::str::contains("uncommitted changes"));

    assert_eq!(
        std::fs::read_to_string(&stray).unwrap(),
        "not committed anywhere\n",
        "the refused landing destroyed uncommitted work in the target's checkout"
    );
    assert_eq!(
        f.status_of(&b),
        "in_progress",
        "a refused landing must leave the task where it was"
    );
}

/// And the listing says so too — the same rule, reached the other way.
///
/// `staging ls` renders `land_blocker`, which asks `staging::target_dirty_reason`. The row and the
/// command share that one function precisely so they cannot disagree, and this is the half that
/// pins the row: without it the row went on promising a landing the command refuses, which is the
/// divergence the single read exists to prevent (D38.2).
#[test]
fn the_listing_reports_a_dirty_land_target_as_a_blocker() {
    let f = Fixture::new();
    let a = f.add_task("first");
    let b = f.add_task("second");
    let sa = f.work(&a);
    let sb = f.work(&b);
    commit_in(
        Path::new(sa["worktree"].as_str().unwrap()),
        "a.txt",
        "a\n",
        "a",
    );
    commit_in(
        Path::new(sb["worktree"].as_str().unwrap()),
        "b.txt",
        "b\n",
        "b",
    );
    f.jkb()
        .args(["task", "land", &a, "--gate", "true", "--no-review"])
        .assert()
        .success();

    let base = f.repo.join(".jkb").join("base");
    std::fs::write(base.join("precious.txt"), "not committed\n").unwrap();

    let rows = f.staging(&[]);
    let blocked_for_b = rows[0]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["uid"] == serde_json::json!(b))
        .and_then(|t| t["land_blocked"].as_str())
        .unwrap_or_default()
        .to_owned();
    assert!(
        blocked_for_b.contains("uncommitted changes"),
        "the row promised a landing the command refuses: {rows}"
    );
}

/// A branch tagged **after** its work is committed — the shape `/task-swarm` cannot avoid, since
/// it can only name a group's branch once an implementer has produced one.
///
/// Drives the real sequence: claim (`onto=`, before any branch exists), implement, tag. Returns
/// the task and the commit the branch was genuinely cut from.
///
/// The two tests below assert the write and the read **separately**, because a `#[test]` stops at
/// its first failing assertion and it was exactly the reading side that went unchecked: the
/// regression this covers was confirmed at the time by running the tagging command and reading
/// back the value it had been asked to write.
fn a_group_branch_tagged_after_its_work(f: &Fixture) -> (String, String) {
    const OWNER: &str = "swarm:integration";
    let uid = f.add_task("swarm group task");

    // Claim. The swarm claims under its run owner and records where the group lands, both before
    // any branch exists. Run as the swarm runs them, so the tagging step below is exercised
    // against a task this same owner already holds.
    git(&f.repo, &["branch", "integration", "main"]);
    f.jkb()
        .args(["task", "claim", &uid, "--owner", OWNER])
        .assert()
        .success();
    for tag in ["onto=integration", "repo=proj"] {
        f.jkb()
            .args(["--global", "task", "tag", "set", &uid, tag])
            .assert()
            .success();
    }
    let cut = git(&f.repo, &["rev-parse", "integration"]);

    // Implement. A group branch off the integration branch, with work already on it.
    git(
        &f.repo,
        &["checkout", "-q", "-b", "swarm-task/group", "integration"],
    );
    commit_in(&f.repo, "impl.txt", "work\n", "implement the group");
    git(&f.repo, &["checkout", "-q", "main"]);
    assert_ne!(
        git(&f.repo, &["rev-parse", "swarm-task/group"]),
        cut,
        "setup: the branch must already carry work by the time it is tagged"
    );

    // Tag. Exactly the command `/task-swarm` runs: it names the branch and the branch that one was
    // cut from, and supplies no commit id.
    f.jkb()
        .args(["task", "start", &uid, "--branch", "swarm-task/group"])
        .args(["--onto", "integration", "--owner", OWNER])
        .assert()
        .success();
    (uid, cut)
}

/// What gets recorded is where the branch was cut, not where its tip happens to be now.
#[test]
fn a_branch_tagged_after_its_work_records_where_it_was_cut_not_its_tip() {
    let f = Fixture::new();
    let (uid, cut) = a_group_branch_tagged_after_its_work(&f);
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "base=swarm-task/group:{cut}"
        )));
}

/// And the reader acts on it: a review of the branch the group's work has reached credits it.
///
/// This is the consequence that went unchecked. With the tip recorded as the cut point,
/// `is_merged` answers `NothingToMerge`, `review record` puts every task of every group in the
/// unlanded bucket and writes no `reviewed=`, and the land gate then refuses all of them —
/// strictly worse than having recorded no cut point at all.
#[test]
fn a_review_credits_a_group_whose_branch_was_tagged_after_its_work() {
    let f = Fixture::new();
    let (uid, _) = a_group_branch_tagged_after_its_work(&f);

    // Merge queue. The group's commits reach the integration branch.
    git(&f.repo, &["checkout", "-q", "integration"]);
    git(&f.repo, &["merge", "-q", "--ff-only", "swarm-task/group"]);
    git(&f.repo, &["checkout", "-q", "main"]);

    f.add_finding("reviews/swarm", "something to fix");
    f.jkb()
        .args(["task", "review", "record", "--branch", "integration"])
        .args(["--findings", "reviews/swarm"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&uid));
    assert_eq!(
        f.status_of(&uid),
        "needs_review",
        "a review of the branch that contains this task's work did not credit it"
    );
}

/// Deleting a branch takes its cut point with it.
///
/// `abandon --delete-branch` frees the branch *name* while leaving the task live, so the next
/// `jkb task work` cuts a **new** branch under it. The old record still resolved and still
/// differed from the new tip, so `is_merged` skipped its freshly-cut guard and `close-merged`
/// marked the task done with nothing written on it.
#[test]
fn deleting_a_branch_with_its_session_takes_its_cut_point_too() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "batch", "main"]);
    let uid = f.add_task("re-worked task");
    let branch = f.work_onto(&uid, "batch")["branch"]
        .as_str()
        .unwrap()
        .to_owned();
    f.jkb()
        .args(["task", "abandon", &uid, "--force", "--delete-branch"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("base=").not());

    // Everything moves on while the task is idle, so the branch re-cut below starts somewhere the
    // stale record does not name — which is what makes the freshly-cut guard miss it. Trunk, so
    // the re-cut branch is contained in trunk and `close-merged` would act.
    commit_in(&f.repo, "trunk.txt", "moved\n", "trunk moves on");
    git(&f.repo, &["branch", "-f", "batch", "main"]);

    f.jkb()
        .args(["task", "work", &uid, "--onto", "batch"])
        .assert()
        .success();
    assert_eq!(
        git(&f.repo, &["rev-parse", &branch]),
        git(&f.repo, &["rev-parse", "main"]),
        "setup: the branch must be re-cut somewhere its predecessor's cut point does not name"
    );

    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "an empty re-cut branch closed as merged, on the cut point of the branch it replaced"
    );
}

/// The land command and the In Flight row must answer "does this branch exist" the same way.
///
/// A branch living only on `origin/` is the ordinary state after a local ref is pruned. The row
/// counted it and the command's preflight did not, so the one shared blocker printed two opposite
/// explanations of the same task: the row said it is being built elsewhere, while the command said
/// its worktree was abandoned and told the owner to open a new session — which cuts a second
/// branch and detaches the task from its group.
#[test]
fn a_remote_only_work_branch_is_explained_the_same_way_by_the_row_and_the_command() {
    let f = Fixture::new();
    let origin = f.home.path().join("origin.git");
    git(&f.repo, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(
        &f.repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&f.repo, &["push", "-q", "origin", "main"]);

    // A swarm-shaped task: a group branch and a land target, no `.jkb/work` session. Its branch
    // is published and its local ref pruned.
    git(&f.repo, &["branch", "integration", "main"]);
    git(&f.repo, &["checkout", "-q", "-b", "grp", "integration"]);
    commit_in(&f.repo, "g.txt", "g\n", "group work");
    git(&f.repo, &["checkout", "-q", "main"]);
    git(&f.repo, &["push", "-q", "origin", "grp"]);
    git(&f.repo, &["branch", "-D", "grp"]);

    let uid = f.add_task("a group being built elsewhere");
    for tag in ["repo=proj", "onto=integration", "branch=grp"] {
        f.jkb()
            .args(["--global", "task", "tag", "set", &uid, tag])
            .assert()
            .success();
    }

    let rows = f.staging(&[]);
    let row = rows[0]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["uid"] == serde_json::json!(uid))
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        row["commits"], 1,
        "the row measured a remote-only branch with a name git cannot resolve, and read the \
         failure as zero commits: {rows}"
    );
    let blocked = row["land_blocked"].as_str().unwrap_or_default();
    assert!(
        blocked.contains("being built elsewhere"),
        "the row treated a published branch as an abandoned checkout: {rows}"
    );

    f.jkb()
        .args(["task", "land", &uid, "--gate", "true", "--no-review"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("being built elsewhere"));
}

/// `jkb task start --repo <other>` must not measure a cut point in whatever checkout the cwd is.
///
/// The database is global across repos (D32), so the command legitimately runs from anywhere. A
/// namesake branch in the repo you are standing in is not the task's branch, and its tip recorded
/// as the task's cut point — and reported as recorded — resolves to nothing when read from the
/// right checkout, so the task is held forever and the recorder never overwrites it. `jkb task
/// base` and `close-merged` already gate on standing in the task's repo; this was the third answer
/// to that question and the only one that guessed.
#[test]
fn task_start_does_not_measure_a_cut_point_in_a_foreign_repo() {
    let f = Fixture::new();
    let uid = f.add_task("worked in another repo");

    // A sibling checkout carrying a branch of the same name. The task's own repo does not have
    // it, so anything recorded can only have come from here.
    let other = f.home.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q", "-b", "main"]);
    std::fs::write(other.join("f.txt"), "x\n").unwrap();
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-qm", "base"]);
    git(&other, &["branch", "shared"]);

    let out = f
        .jkb()
        .current_dir(&other)
        .args(["task", "start", &uid, "--branch", "shared"])
        .args(["--repo", "proj", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "task start: {out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        v["base"].is_null(),
        "a namesake branch in the wrong checkout was recorded as this task's cut point: {v}"
    );
    assert_ne!(
        v["base"].as_str(),
        Some(git(&other, &["rev-parse", "shared"]).as_str()),
        "the foreign repo's commit was recorded"
    );

    // And it says which of the two reasons applies, because they have different remedies.
    f.jkb()
        .current_dir(&other)
        .args([
            "task", "start", &uid, "--branch", "shared", "--repo", "proj",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("not proj's checkout"));
}

/// A cut point is measured against the parent the caller **states**, never one read back from the
/// task's `onto=`.
///
/// Which branch this one was cut from is something the caller knows in the moment; the stored
/// facet records some earlier moment, and a stale one is worse than none. A task carrying `onto=`
/// from a previous batch, given a new branch cut from trunk, would measure their merge-base — a
/// commit well behind the new branch's tip — and an empty branch identical to trunk would then
/// skip the freshly-cut guard and close as merged with no work on it. That is the one direction
/// D34.4 forbids, so the fallback with no stated parent is the branch's own tip, which holds.
#[test]
fn a_stale_land_target_is_not_used_to_measure_a_new_branchs_cut_point() {
    let f = Fixture::new();

    // A batch from an earlier round, which has diverged from trunk...
    git(&f.repo, &["checkout", "-q", "-b", "stale-batch"]);
    commit_in(&f.repo, "old.txt", "old\n", "an earlier batch");
    git(&f.repo, &["checkout", "-q", "main"]);
    // ...and trunk has moved on since.
    commit_in(&f.repo, "new.txt", "new\n", "trunk moves on");
    let tip = git(&f.repo, &["rev-parse", "main"]);
    git(&f.repo, &["branch", "feature", "main"]);

    let uid = f.add_task("new work, old facet");
    for tag in ["repo=proj", "onto=stale-batch"] {
        f.jkb()
            .args(["--global", "task", "tag", "set", &uid, tag])
            .assert()
            .success();
    }
    f.jkb()
        .args(["task", "start", &uid, "--branch", "feature"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("base=feature:{tip}")));

    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "an empty branch closed as merged: its cut point was measured against a batch it was \
         never cut from, so the freshly-cut guard was skipped"
    );
}

/// A **wrong** parent must degrade to holding the task, not to closing it.
///
/// `--onto` is what the caller says the branch was cut from, and a caller can be wrong: a mistyped
/// name, or a session adopting a branch someone else cut from elsewhere. The merge-base then lands
/// behind the branch's real origin, so a branch carrying no work of its own reads as carrying some
/// and `merge-tree` closes it against trunk. Trunk is the backstop — commits reachable from it are
/// not this branch's doing either — and taking the later of the two fork points can only make the
/// freshly-cut guard fire more often.
#[test]
fn a_wrong_parent_branch_holds_the_task_rather_than_closing_it() {
    let f = Fixture::new();

    // A batch that forked from trunk before trunk moved on.
    git(&f.repo, &["checkout", "-q", "-b", "other-batch"]);
    commit_in(&f.repo, "other.txt", "other\n", "another batch");
    git(&f.repo, &["checkout", "-q", "main"]);
    commit_in(&f.repo, "trunk.txt", "moved\n", "trunk moves on");

    // A branch with nothing of its own, and a caller that names the wrong parent for it.
    let tip = git(&f.repo, &["rev-parse", "main"]);
    git(&f.repo, &["branch", "feature", "main"]);
    let uid = f.add_task("misattributed branch");
    f.jkb()
        .args(["task", "start", &uid, "--branch", "feature"])
        .args(["--onto", "other-batch"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("base=feature:{tip}")));

    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "a branch with no work of its own closed as merged, because its cut point was measured \
         only against a parent it was never cut from"
    );
}

/// Putting a branch on a task records where it was cut, **however** the branch got there.
///
/// `jkb task tag set <uid> branch=…` wrote the facet on its own, which is precisely the state that
/// blocked every `/task-swarm` group once the readers began refusing to act without a cut point —
/// and the swarm reached for that command because the guide recommended it. Fixing the swarm alone
/// left the hole open at the verb the next workflow reaches for, so the pairing moved into the one
/// writer instead.
#[test]
fn tagging_a_branch_onto_a_task_records_its_cut_point_too() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "handmade", "main"]);
    let tip = git(&f.repo, &["rev-parse", "handmade"]);
    let uid = f.add_task("tagged by hand");

    f.jkb()
        .args(["--global", "task", "tag", "set", &uid, "branch=handmade"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("base=handmade:{tip}")));

    // `add` keeps appending — a task can legitimately record two branches and every reader indexes
    // both — but it cannot leave the second one without a cut point either.
    git(&f.repo, &["branch", "second", "main"]);
    f.jkb()
        .args(["--global", "task", "tag", "add", &uid, "branch=second"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("branch=handmade"))
        .stdout(predicate::str::contains("branch=second"))
        .stdout(predicate::str::contains(format!("base=second:{tip}")));
}

/// And it does not measure one in whatever checkout the cwd happens to be.
///
/// The same rule `jkb task base` and `jkb task start` follow, through the same function — it had
/// three implementations, and this was the one that never asked at all.
#[test]
fn tagging_a_branch_does_not_measure_it_in_a_foreign_repo() {
    let f = Fixture::new();
    let uid = f.add_task("belongs to proj");
    f.jkb()
        .args(["--global", "task", "tag", "add", &uid, "repo=proj"])
        .assert()
        .success();

    let other = f.home.path().join("elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q", "-b", "main"]);
    std::fs::write(other.join("f.txt"), "x\n").unwrap();
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-qm", "base"]);
    git(&other, &["branch", "shared"]);

    f.jkb()
        .current_dir(&other)
        .args(["--global", "task", "tag", "set", &uid, "branch=shared"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("branch=shared"))
        .stdout(predicate::str::contains("base=").not());
}
