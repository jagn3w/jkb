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

/// Run `git` in `dir` with the developer's global config neutralized. This machine sets
/// `core.hooksPath` and commit signing globally; either would fail the fixture for reasons
/// that have nothing to do with sessions.
fn git(dir: &Path, args: &[&str]) -> String {
    run_git(git_cmd(dir, args), args)
}

/// The one place the fixture's git environment is set, so [`git`] and [`git_at`] cannot drift into
/// running against different configuration.
fn git_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t");
    cmd
}

fn run_git(mut cmd: Command, args: &[&str]) -> String {
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
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

/// Re-targeting a session cannot leave the task claiming two land targets at once.
///
/// Where a branch lands is a **column** on that branch's record now, so "two targets" is not a
/// state the store can hold for one branch — a re-target overwrites it. `task work` returns the
/// same session, so both runs are about the same branch.
#[test]
fn retargeting_a_session_replaces_the_target_it_records() {
    let f = Fixture::new();
    let uid = f.add_task("retargeted task");
    let first = f.work_onto(&uid, "batch-one");
    let second = f.work_onto(&uid, "batch-two");
    assert_eq!(
        first["branch"], second["branch"],
        "setup: the second run must return the same session"
    );
    assert_eq!(second["onto"].as_str().unwrap(), "batch-two");

    // The target is a label on the task's history, so retargeting appends rather than
    // overwriting — and what any reader takes is the **latest**. Both entries are here; only one
    // of them is the answer.
    let out = f
        .jkb()
        .args(["--global", "task", "why", &uid, "--json"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let targets: Vec<&str> = v["history"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["onto"].as_str())
        .collect();
    assert_eq!(
        targets.last(),
        Some(&"batch-two"),
        "the latest is the answer"
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
/// re-tags a group on every pass, and an appending `repo=` would leave the task claiming two
/// repositories at once — while a command named `add` must not silently delete a value.
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

    // `set` collapses a facet to one value — what the remaining location facet needs.
    for v in ["proj-one", "proj-two"] {
        f.jkb()
            .args(["--global", "task", "tag", "set", &uid, &format!("repo={v}")])
            .assert()
            .success();
    }
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("repo=proj-two"))
        .stdout(predicate::str::contains("repo=proj-one").not());
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

/// A land target cannot be **written** as a tag, and a stray one can still be removed.
///
/// Where a branch lands is a fact about the **branch** and lives in that branch's record, so a
/// facet named `onto` reaches no reader at all. Refusing rather than storing it inert is a UX
/// judgement, not a data-integrity one: a stray facet cannot close a task falsely, but a user who
/// typed it expecting effect deserves an answer instead of silence.
///
/// `rm` is the other half and is deliberately **not** refused. The refusal is about setting a value
/// nothing reads; removing one is always safe, and refusing it made the only command that could
/// remove such a tag decline on the grounds that it could not exist — while two routes still create
/// one, a synced `tasks.md` line carrying `#onto=` and the facet rename exercised below.
#[test]
fn a_land_target_cannot_be_written_as_a_tag() {
    let f = Fixture::new();
    let uid = f.add_task("hand-tagged task");

    for mode in ["add", "set"] {
        f.jkb()
            .args(["--global", "task", "tag", mode, &uid, "onto=batch"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("jkb task work"));
    }
    f.jkb()
        .args(["--global", "task", "add", "planted #onto=batch"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("jkb task work"));

    // A stray `onto=` that got in by another route comes back out. Planted through `tag rename`,
    // which is a real route with no guard on the destination name — the same shape as the sync
    // one, and reachable from this test without a mount.
    let stray = f.add_task("stray onto #batchref=batch");
    f.jkb()
        .args(["--global", "tag", "rename", "batchref", "onto"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "query", "tag:onto=batch"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&stray));
    f.jkb()
        .args(["--global", "task", "tag", "rm", &stray, "onto=batch"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "query", "tag:onto=batch"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&stray).not());
    // Nothing was created: the refusal is before the write, so there is no half-made task
    // carrying the value with the modifier merely dropped.
    f.jkb()
        .args(["--global", "query", "kind:task"])
        .assert()
        .success()
        .stdout(predicate::str::contains("planted").not());
}

/// `task start --repo <other> --onto <batch>` **records the land target**, and does not drop it.
///
/// A land target is a name, not a measurement: unlike the cut point there is nothing about it a
/// foreign checkout could honestly establish, so there is nothing to withhold. The write used to
/// be gated on having a repository *root* — which `--repo <other>` deliberately does not give —
/// so the flag was accepted, ignored, and never mentioned: the task simply never appeared in
/// `jkb staging ls` and `jkb task land` failed later saying it recorded no land target.
#[test]
fn a_start_in_another_repo_still_records_the_land_target() {
    let f = Fixture::new();
    let uid = f.add_task("cross-repo task on a batch");
    f.jkb()
        .args([
            "--global",
            "task",
            "start",
            &uid,
            "--repo",
            "otherproj",
            "--branch",
            "feat/x",
            "--onto",
            "batch-1",
        ])
        .assert()
        .success();

    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("onto batch-1"))
        .stdout(predicate::str::contains("on feat/x"));
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
        .stdout(predicate::str::contains(
            "has not grafted their work onto it yet",
        ))
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

    // Merge queue. The group's commits reach the integration branch — and the queue **says so**
    // (`scripts/merge-queue.sh` runs exactly this verb), which is what makes the landing a
    // recorded event rather than something a reader has to infer from the commit graph.
    git(&f.repo, &["checkout", "-q", "integration"]);
    git(&f.repo, &["merge", "-q", "--ff-only", "swarm-task/group"]);
    git(&f.repo, &["checkout", "-q", "main"]);
    f.jkb()
        .args([
            "task",
            "landed",
            "swarm-task/group",
            "--onto",
            "integration",
        ])
        .assert()
        .success();

    f.add_finding("reviews/swarm", "something to fix");
    // Asserted on the **consequence**, not on the uid appearing in stdout. The first version
    // checked `stdout contains uid`, and `review::record` prints the uid in the *skipped* bucket
    // too — so the assertion was satisfied by the exact failure it was written to catch, and the
    // bug shipped green. It must be absent from the skipped bucket and carry `reviewed=`.
    f.jkb()
        .args(["task", "review", "record", "--branch", "integration"])
        .args(["--findings", "reviews/swarm"])
        .assert()
        .success()
        .stdout(predicate::str::contains("not tagged").not());
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("reviewed="));
    // `landed` closes it — the queue's own gate is what D38 says the swarm's review is — so the
    // review credits a task that is already `done` rather than moving it to `needs_review`.
    // What is asserted is the crediting: the task is named, and `reviewed=` is written.
    assert_eq!(f.status_of(&uid), "done");
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
        .stdout(predicate::str::contains("cut from").not());

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
    f.jkb()
        .args(["--global", "task", "tag", "set", &uid, "repo=proj"])
        .assert()
        .success();
    // The branch and its land target, through the one writer that records both — which is what
    // `/task-swarm` runs once its implementer has a branch.
    f.jkb()
        .args([
            "task",
            "start",
            &uid,
            "--branch",
            "grp",
            "--onto",
            "integration",
        ])
        .assert()
        .success();

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

/// A branch deleted by hand and re-cut by `task work` does not inherit its predecessor's cut point.
///
/// A cut point is keyed by branch *name*, and a name outlives the branch that held it. `jkb task
/// abandon` without `--delete-branch` prints "branch … kept — delete it with `git branch -D …`",
/// so this is a route jkb's own advice sends people down: the record survives the deletion, the
/// next `task work` re-cuts the name somewhere else, the stale value still resolves and still
/// differs from the new tip, and the freshly-cut guard is skipped for a branch with nothing on it.
///
/// Nothing in git distinguishes the two branches, so the only reliable signal is the moment of
/// creation — which `worktree_add` has and used to discard. `base::forget` covers the deletions
/// jkb performs; this covers the creations, and between them every branch jkb makes or destroys.
#[test]
fn a_hand_deleted_branch_recut_by_task_work_does_not_inherit_its_cut_point() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "batch", "main"]);
    let uid = f.add_task("re-cut by hand");
    let s = f.work_onto(&uid, "batch");
    let branch = s["branch"].as_str().unwrap().to_owned();

    // Abandon WITHOUT --delete-branch, then follow the advice the command itself prints.
    f.jkb()
        .args(["task", "abandon", &uid, "--force"])
        .assert()
        .success();
    git(&f.repo, &["branch", "-D", &branch]);

    // Everything moves on, so the stale record names a commit the re-cut branch is past.
    commit_in(&f.repo, "trunk.txt", "moved\n", "trunk moves on");
    git(&f.repo, &["branch", "-f", "batch", "main"]);

    f.jkb()
        .args(["task", "work", &uid, "--onto", "batch"])
        .assert()
        .success();
    assert_eq!(
        git(&f.repo, &["rev-parse", &branch]),
        git(&f.repo, &["rev-parse", "main"]),
        "setup: the branch must be re-cut past its predecessor's cut point"
    );

    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "an empty re-cut branch closed as merged, inheriting the cut point of the branch that had \
         its name before"
    );
}

/// A parent branch this repository does not have is **refused**, loudly, and nothing is stored.
///
/// The earlier behaviour was to accept it and quietly reinterpret — first as a trunk-only
/// merge-base, which for a branch cut from a *staging* branch sits behind its real origin and so
/// closes an untouched task the moment staging lands, and then as "no parent named". Refusing
/// beats both: a typo is a typo. Storing the name as a land target also drops the task out of
/// `jkb staging ls` — the one read behind the picker and In Flight — and makes `task land` fail
/// later claiming the branch "no longer exists", which is not what happened.
///
/// (`jkb task work --onto` still accepts a name that does not exist, because it *creates* it.
/// This verb only records, so there is nothing here that can make the name true.)
#[test]
fn an_unresolvable_parent_branch_is_refused_rather_than_quietly_reinterpreted() {
    let f = Fixture::new();
    // A staging branch ahead of trunk, and a branch cut from it carrying nothing of its own —
    // the shape where a trunk-only fallback used to be actively harmful.
    git(&f.repo, &["checkout", "-q", "-b", "stage"]);
    commit_in(&f.repo, "s.txt", "staging\n", "staging work");
    git(&f.repo, &["checkout", "-q", "-b", "feature"]);
    git(&f.repo, &["checkout", "-q", "main"]);

    let uid = f.add_task("mistyped parent");
    f.jkb()
        .args(["task", "start", &uid, "--branch", "feature"])
        .args(["--onto", "stgae"]) // the typo
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not a branch in proj"));

    // Nothing was stored: not the bad land target, and not a cut point measured against some
    // fallback the caller never named.
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("onto=").not())
        .stdout(predicate::str::contains("cut from").not());
}

/// A dangling `origin/HEAD` must not take the whole staging listing down with it.
///
/// The remote's default branch gets renamed, or `origin/main` gets pruned, and `origin/HEAD` is
/// left pointing at a ref that is not there. `gitrepo::trunk` took its own symref answer on trust
/// while the fallback arm verified, which was survivable while `ahead_count` quietly answered zero
/// — and stopped being survivable the moment it started refusing an operand it cannot resolve.
/// `jkb staging ls` is the ONE read behind the branch picker and In Flight (D38.2), so an error
/// there is both surfaces going dark.
#[test]
fn a_dangling_origin_head_does_not_break_the_staging_listing() {
    let f = Fixture::new();
    let origin = f.home.path().join("origin.git");
    git(&f.repo, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(
        &f.repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&f.repo, &["push", "-q", "origin", "main"]);
    git(
        &f.repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );

    let uid = f.add_task("on a batch");
    let s = f.work(&uid);
    commit_in(
        Path::new(s["worktree"].as_str().unwrap()),
        "a.txt",
        "a\n",
        "a",
    );

    // The remote's default branch goes away; the symref stays and now points at nothing.
    git(&f.repo, &["update-ref", "-d", "refs/remotes/origin/main"]);

    let rows = f.staging(&[]);
    assert!(
        !rows.as_array().unwrap().is_empty(),
        "a dangling origin/HEAD emptied the one read the picker and In Flight both use: {rows}"
    );
    let task = &rows[0]["tasks"][0];
    assert_eq!(
        task["commits"], 1,
        "the session's own commits are measurable whatever trunk is doing: {rows}"
    );
}

/// A branch cut from trunk can say so, and gets a usable cut point for it.
///
/// `--onto` names both the branch this one was cut from and the branch it lands on, and those come
/// apart at trunk: trunk is a fine measurement reference and an unacceptable land target (D34.3).
/// Refusing the flag outright left a branch genuinely cut from trunk, with commits already on it,
/// able to record only `base == tip` — permanently `NothingToMerge` — with a hand-computed
/// merge-base as the only escape.
#[test]
fn a_branch_cut_from_trunk_can_say_so_without_recording_trunk_as_a_land_target() {
    let f = Fixture::new();
    git(&f.repo, &["checkout", "-q", "-b", "feature"]);
    commit_in(&f.repo, "w.txt", "work\n", "work done before registering");
    git(&f.repo, &["checkout", "-q", "main"]);

    let uid = f.add_task("worked before registering");
    f.jkb()
        .args([
            "task", "start", &uid, "--branch", "feature", "--onto", "main",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("not recorded as a land target"));

    let show = f
        .jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success();
    // Trunk is not recorded as a land target — the whole point of the flag. A task that
    // recorded trunk would read as landing on it, and `staging ls` would offer trunk as a batch.
    show.stdout(predicate::str::contains("on feature"))
        .stdout(predicate::str::contains("onto ").not());

    // And nothing closes it: jkb did not perform this landing and no pull request proves one,
    // so the task is held and says why. That is the whole trade this change makes — a missed
    // close costs one command, and it can never bury work still in flight.
    git(&f.repo, &["merge", "-q", "--ff-only", "feature"]);
    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "a task closed on an inference nobody made"
    );
}

/// The row and the command must pick the *same* recorded branch to talk about.
///
/// They were given the same existence predicate and still disagreed, because they chose which
/// branch to ask about differently: the row preferred one that resolves, the command took whichever
/// `tag::applications` returned first — which is the lexicographically smallest. A task carrying a
/// stale `a-gone` beside a live `z-live` therefore got two opposite explanations from the one
/// shared blocker, and `land`'s advice for the branch it picked cuts a second branch and detaches
/// the task from its batch.
#[test]
fn the_row_and_the_command_talk_about_the_same_branch() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "integration", "main"]);
    git(&f.repo, &["checkout", "-q", "-b", "z-live", "integration"]);
    commit_in(&f.repo, "z.txt", "z\n", "live work");
    git(&f.repo, &["checkout", "-q", "main"]);

    let uid = f.add_task("two recorded branches");
    // The live branch and its land target, through the one writer that records both.
    f.jkb()
        .args([
            "task",
            "start",
            &uid,
            "--branch",
            "z-live",
            "--onto",
            "integration",
        ])
        .assert()
        .success();
    // …then a stale branch that sorts first, which must not be preferred over it.
    f.jkb()
        .args(["--global", "task", "tag", "add", &uid, "branch=a-gone"])
        .assert()
        .success();

    let rows = f.staging(&[]);
    let row = rows[0]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["uid"] == serde_json::json!(uid))
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        row["branch"],
        serde_json::json!("z-live"),
        "the row picked a branch that does not exist over one that does: {rows}"
    );
    let blocked = row["land_blocked"].as_str().unwrap_or_default().to_owned();

    let out = f
        .jkb()
        .args(["task", "land", &uid, "--gate", "true", "--no-review"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&blocked),
        "the row and the command explained the same task differently.\n  row: {blocked}\n  cmd: {stderr}"
    );
}

/// A landing the queue recorded while the task was **held** still closes it once the hold lifts.
///
/// The merge queue grafts a whole branch locally, so there is no pull request to ask about — the
/// recorded landing is the only evidence that exists. A group task with an open subtask is
/// rightly not closed at that moment (D34.4), and the previous shape of this recorded the graft
/// as a `note`: a row `transition::landing` does not match and nothing else reads, indistinguishable
/// from the one `jkb task start --onto` writes. So the subtask finished, nothing re-ran the verb,
/// and `close-merged` asked GitHub for a pull request that never existed. Held for ever.
///
/// Asserted end to end and through the CLI, because the defect was that two halves each looked
/// right on their own: the row was written, and the reader could not see it.
#[test]
fn a_landing_recorded_while_a_task_was_held_closes_it_once_the_hold_lifts() {
    let f = Fixture::new();
    let parent = f.add_task("parent task");
    let s = f.work(&parent);
    let worktree = std::path::PathBuf::from(s["worktree"].as_str().unwrap());
    let onto = s["onto"].as_str().unwrap().to_owned();
    let branch = s["branch"].as_str().unwrap().to_owned();
    commit_in(&worktree, "p.txt", "parent work\n", "parent work");
    let out = f
        .jkb()
        .args([
            "--global",
            "task",
            "add",
            "child task",
            "--under",
            &parent,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "task add --under: {out:?}");
    let child = serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["uid"]
        .as_str()
        .unwrap()
        .to_owned();

    // The queue grafts the branch and reports it. The parent is held for its open subtask.
    git(&f.repo, &["merge", "-q", "--ff-only", &branch]);
    f.jkb()
        .args(["task", "landed", &branch, "--onto", &onto])
        .assert()
        .success()
        .stderr(predicate::str::contains("open subtasks"));
    assert_eq!(f.status_of(&parent), "in_progress", "it closed while held");

    // The hold lifts.
    f.jkb()
        .args(["--global", "task", "set", &child, "--status", "done"])
        .assert()
        .success();

    // `close-merged` now has to find the landing jkb recorded. There is no pull request — the
    // graft was local — so if it asks `gh` instead, the task is held for ever.
    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&parent),
        "done",
        "the recorded landing was not readable as evidence, so the task stayed held"
    );
}

/// A landing is spent once the task is put back to work — and **still closes it when it is not**.
///
/// Both directions, because either alone passes with the rule broken the other way: refusing
/// every close makes the first assertion pass, and closing everything makes the second.
///
/// `close-merged` runs unattended from the `post-merge` hook over every task at once, so this is
/// not a command anybody chose to run: you reopen a landed task, work on it, `git pull`, and it
/// is silently `done` again with a live session on it. Reproduced exactly this way.
#[test]
fn a_landing_stops_counting_once_the_task_is_put_back_to_work() {
    let f = Fixture::new();

    // Both tasks are `in_progress` with a recorded landing when `close-merged` runs, so the only
    // thing that can separate them is whether the landing still speaks for the work. Neither is
    // already in the state its assertion wants — an earlier version of this asserted `done` on a
    // task `jkb task landed` had *already* closed, which no breakage of the rule could disturb.
    let reopened = f.add_task("reopened after landing");
    let s = f.work(&reopened);
    let worktree = std::path::PathBuf::from(s["worktree"].as_str().unwrap());
    // No `git merge` anywhere here: `jkb task landed` is the merge queue reporting a graft it
    // performed and gated itself, and verifies nothing of its own — the recorded event is the
    // subject, and grafting both branches for real would only make them diverge.
    commit_in(&worktree, "a.txt", "work\n", "work");
    f.jkb()
        .args([
            "task",
            "landed",
            s["branch"].as_str().unwrap(),
            "--onto",
            s["onto"].as_str().unwrap(),
        ])
        .assert()
        .success();
    assert_eq!(f.status_of(&reopened), "done");
    // The work was wrong and has to go ahead again.
    f.jkb()
        .args([
            "--global",
            "task",
            "set",
            &reopened,
            "--status",
            "in_progress",
        ])
        .assert()
        .success();

    // The other one was *held* at landing time by an open subtask, so it is `in_progress` with a
    // landing nothing has superseded — the case `close-merged` has to close.
    let held = f.add_task("held by a subtask");
    let s = f.work(&held);
    let worktree = std::path::PathBuf::from(s["worktree"].as_str().unwrap());
    commit_in(&worktree, "b.txt", "work\n", "work");
    let out = f
        .jkb()
        .args([
            "--global", "task", "add", "child", "--under", &held, "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "task add --under: {out:?}");
    let child = serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["uid"]
        .as_str()
        .unwrap()
        .to_owned();
    f.jkb()
        .args([
            "task",
            "landed",
            s["branch"].as_str().unwrap(),
            "--onto",
            s["onto"].as_str().unwrap(),
        ])
        .assert()
        .success();
    assert_eq!(f.status_of(&held), "in_progress", "it closed while held");
    f.jkb()
        .args(["--global", "task", "set", &child, "--status", "done"])
        .assert()
        .success();

    // One `git pull`, firing the hook over both.
    f.jkb().args(["task", "close-merged"]).assert().success();

    assert_eq!(
        f.status_of(&reopened),
        "in_progress",
        "a landing recorded before the reopen closed the task again, over work in flight"
    );
    assert_eq!(
        f.status_of(&held),
        "done",
        "the staleness rule swallowed a landing nothing had superseded"
    );
}

/// Abandoning the session that produced a landing spends it — the case that broke the first
/// version of this rule, which asked for a move out of a *terminal* status. `abandon` is
/// `in_progress -> open`: neither side terminal, so a landing held by an open subtask survived
/// the abandon that destroyed its session and the task auto-closed over live work.
#[test]
fn abandoning_a_session_spends_the_landing_it_recorded() {
    let f = Fixture::new();
    let t = f.add_task("parent");
    let s = f.work(&t);
    let worktree = std::path::PathBuf::from(s["worktree"].as_str().unwrap());
    commit_in(&worktree, "a.txt", "w\n", "w");
    let out = f
        .jkb()
        .args(["--global", "task", "add", "child", "--under", &t, "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "task add --under: {out:?}");
    let child = serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["uid"]
        .as_str()
        .unwrap()
        .to_owned();
    f.jkb()
        .args([
            "task",
            "landed",
            s["branch"].as_str().unwrap(),
            "--onto",
            s["onto"].as_str().unwrap(),
        ])
        .assert()
        .success();
    assert_eq!(f.status_of(&t), "in_progress", "held by the subtask");

    // The approach was wrong: drop the session entirely, then pick the task up again.
    f.jkb().args(["task", "abandon", &t]).assert().success();
    f.jkb()
        .args(["--global", "task", "set", &child, "--status", "done"])
        .assert()
        .success();
    f.work(&t);

    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&t),
        "in_progress",
        "a landing from an abandoned session closed the task over live work"
    );
}

/// `jkb undo` of a close puts the task back to work, and the next `close-merged` must not undo
/// the undo. `undo` restores `items.status` from the changelog and `task_transitions` is not
/// changelogged, so unless `undo` records what it did the staleness rule cannot see it — and the
/// task is re-closed on the next `git pull`, in a loop `undo` itself cannot break.
#[test]
fn undoing_a_close_is_not_reversed_by_the_next_close_merged() {
    let f = Fixture::new();
    let t = f.add_task("some task");
    let s = f.work(&t);
    let worktree = std::path::PathBuf::from(s["worktree"].as_str().unwrap());
    commit_in(&worktree, "a.txt", "w\n", "w");
    f.jkb()
        .args([
            "task",
            "landed",
            s["branch"].as_str().unwrap(),
            "--onto",
            s["onto"].as_str().unwrap(),
        ])
        .assert()
        .success();
    assert_eq!(f.status_of(&t), "done");

    f.jkb().args(["--global", "undo"]).assert().success();
    assert_eq!(f.status_of(&t), "in_progress", "undo did not restore it");

    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&t),
        "in_progress",
        "close-merged reversed the undo, which is a loop undo cannot break"
    );
}

/// A task **retargeted** to another batch is not credited by a review of the one it landed on
/// before — the case that makes both of arm (c)'s conditions load-bearing.
///
/// Its sibling above drives `land_target == the reviewed branch`, which the ladder answers one
/// arm earlier, so deleting `target.is_none()` left the whole suite green: the guard read as
/// redundant, which is exactly how a later cleanup deletes it. Here the land target points at a
/// *different* branch, so arm (c) is reached and its `target.is_none()` is the only thing
/// standing between a review of `batch-a` and a task whose work has moved to `batch-b`.
#[test]
fn a_task_retargeted_to_another_batch_is_not_credited_by_the_old_one() {
    let f = Fixture::new();
    let t = f.add_task("retargeted");
    let s = f.work(&t);
    let worktree = std::path::PathBuf::from(s["worktree"].as_str().unwrap());
    let first = s["onto"].as_str().unwrap().to_owned();
    commit_in(&worktree, "a.txt", "w\n", "w");
    f.jkb()
        .args([
            "task",
            "landed",
            s["branch"].as_str().unwrap(),
            "--onto",
            &first,
        ])
        .assert()
        .success();

    // Reopened, then aimed somewhere else entirely — not abandoned, so it still aims *somewhere*.
    f.jkb()
        .args(["--global", "task", "set", &t, "--status", "in_progress"])
        .assert()
        .success();
    git(&f.repo, &["branch", "batch-b"]);
    f.jkb()
        .args(["task", "work", &t, "--onto", "batch-b"])
        .assert()
        .success();

    f.add_finding("reviews/run-1", "something to fix");
    let out = f
        .jkb()
        .args([
            "task",
            "review",
            "record",
            "--branch",
            &first,
            "--findings",
            "reviews/run-1",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        !text.contains("recorded review"),
        "a review of {first} credited a task whose work has moved to batch-b: {text}"
    );
    assert_eq!(
        f.status_of(&t),
        "in_progress",
        "it was moved to needs_review by a review of a branch it no longer aims at"
    );
}

/// A review still credits work jkb grafted onto the branch, even after the session was abandoned.
///
/// `credited_by` asks the **historical** question — did jkb ever graft this onto this branch —
/// because a graft does not un-happen: the reviewer read what is in the branch whatever the task
/// did afterwards. Asking the present-tense question instead made such a task `Credit::Unrelated`,
/// which this loop drops, so `review record` stamped nothing for it and said nothing about it,
/// and `jkb task land` refused it much later for want of a review nobody knew was missing.
#[test]
fn a_review_credits_work_grafted_before_the_session_was_abandoned() {
    let f = Fixture::new();
    let t = f.add_task("grafted then abandoned");
    let s = f.work(&t);
    let worktree = std::path::PathBuf::from(s["worktree"].as_str().unwrap());
    let onto = s["onto"].as_str().unwrap().to_owned();
    commit_in(&worktree, "a.txt", "w\n", "w");
    f.jkb()
        .args([
            "task",
            "landed",
            s["branch"].as_str().unwrap(),
            "--onto",
            &onto,
        ])
        .assert()
        .success();
    // The session is dropped afterwards — which retires the land target and, under the
    // present-tense reading, the landing too.
    f.jkb()
        .args(["--global", "task", "set", &t, "--status", "in_progress"])
        .assert()
        .success();
    f.jkb().args(["task", "abandon", &t]).assert().success();

    f.add_finding("reviews/run-1", "something to fix");
    f.jkb()
        .args([
            "task",
            "review",
            "record",
            "--branch",
            &onto,
            "--findings",
            "reviews/run-1",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(&t).and(predicate::str::contains("recorded review")));
}

/// A task reopened for a must-fix is **not** credited by a review of the branch it landed on
/// before — its fix is in a session that branch has never seen.
///
/// The two halves are one rule, and asking them in the wrong order opens the worst hole here:
/// crediting it records that a review read work it never read, and moves the task to
/// `needs_review` under whoever is working in its session. So the present-tense
/// question credits, the land target reports, and only a task aiming nowhere falls through to the
/// historical question.
#[test]
fn a_task_reopened_after_landing_is_not_credited_by_a_review_of_that_branch() {
    let f = Fixture::new();
    let t = f.add_task("landed then reopened");
    let s = f.work(&t);
    let worktree = std::path::PathBuf::from(s["worktree"].as_str().unwrap());
    let onto = s["onto"].as_str().unwrap().to_owned();
    commit_in(&worktree, "a.txt", "w\n", "w");
    f.jkb()
        .args([
            "task",
            "landed",
            s["branch"].as_str().unwrap(),
            "--onto",
            &onto,
        ])
        .assert()
        .success();
    // A must-fix comes back: the task is reopened and the fix goes into its own session, which
    // `onto` has never seen. The session was never abandoned, so it still aims here.
    f.jkb()
        .args(["--global", "task", "set", &t, "--status", "in_progress"])
        .assert()
        .success();

    f.add_finding("reviews/run-1", "something to fix");
    let out = f
        .jkb()
        .args([
            "task",
            "review",
            "record",
            "--branch",
            &onto,
            "--findings",
            "reviews/run-1",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        text.contains("has not grafted their work onto it yet"),
        "a reopened task was not reported as still owing work: {text}"
    );
    assert!(
        !text.contains("recorded review"),
        "a reopened task was credited for work this branch has never seen: {text}"
    );
    assert_eq!(
        f.status_of(&t),
        "in_progress",
        "it was moved to needs_review under a live session"
    );
}

/// A superseded landing is **context, not a verdict**: it is reported, and the run still goes on
/// to ask the other evidence.
///
/// Both halves matter and each was got wrong in turn. Reporting nothing sent `close-merged` off
/// to ask GitHub about a pull request a locally-grafted branch never had, and printed that as the
/// reason. Reporting it as *the* answer — returning early — then left a task whose work was redone
/// and merged as a pull request permanently unclosable, promising it would close when the new work
/// landed, after the new work had landed.
#[test]
fn a_spent_landing_is_context_and_does_not_stop_the_other_evidence() {
    let f = Fixture::new();
    let t = f.add_task("some task");
    let s = f.work(&t);
    let worktree = std::path::PathBuf::from(s["worktree"].as_str().unwrap());
    commit_in(&worktree, "a.txt", "w\n", "w");
    f.jkb()
        .args([
            "task",
            "landed",
            s["branch"].as_str().unwrap(),
            "--onto",
            s["onto"].as_str().unwrap(),
        ])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "set", &t, "--status", "in_progress"])
        .assert()
        .success();

    let out = f
        .jkb()
        .args(["task", "close-merged", "--dry-run"])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The **shape**, not a word: `with_context` renders `"{why}; {note}"`, so the superseded note
    // arriving after a semicolon proves it was appended to some *other* reason — which is exactly
    // what an early return cannot produce, in any environment.
    //
    // The first version asserted the reason contained "pull request", and that passed only because
    // `gh` is absent on this machine: the NotFound message happens to contain the phrase. CI runs
    // `ubuntu-latest`, where `gh` is preinstalled and a repo with no GitHub remote makes it exit
    // non-zero, so the reason becomes "`gh pr` failed: …" — no such phrase, red on a green change.
    let note = "; its earlier landing onto";
    assert!(
        text.contains(note),
        "the superseded landing did not colour another reason, so the run stopped at it: {text}"
    );
}

/// One task `close-merged` cannot decide must not stop it deciding the rest.
///
/// The run is **total**: every task in the repo gets a verdict, and a task whose branch value is
/// hostile, whose pull request is unknown, or whose `gh` is missing is *held with a reason*
/// rather than aborting the loop. This ran from a `post-merge` hook over every task at once, and
/// a single malformed row silently stopped the whole repo from closing.
///
/// The refusal at the *store* is asserted first, because that is where a hostile value is
/// actually stopped; the loop's totality is the backstop for rows that predate it.
#[test]
fn one_task_close_merged_cannot_decide_does_not_stop_the_rest() {
    let f = Fixture::new();
    // Refused at the store, so it cannot be planted through the CLI at all.
    f.jkb()
        .args(["--global", "task", "add", "hostile #branch=--upload-pack=x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used as a branch"));

    git(&f.repo, &["checkout", "-q", "-b", "done-work"]);
    commit_in(&f.repo, "d.txt", "d\n", "real work");
    git(&f.repo, &["checkout", "-q", "main"]);
    let good = f.add_task("healthy task");
    f.jkb()
        .args([
            "task",
            "start",
            &good,
            "--branch",
            "done-work",
            "--onto",
            "main",
        ])
        .assert()
        .success();

    // A row carrying such a value from before the check existed — planted the only way left,
    // straight through the tag repo.
    let bad = f.add_task("legacy hostile row");
    let db = f.db.to_str().unwrap().to_owned();
    let legacy = jkb_core::Db::open(&db).unwrap();
    let bad_id = legacy
        .read({
            let uid = bad.clone();
            move |conn| jkb_core::item::id_for_uid(conn, &uid)
        })
        .unwrap()
        .unwrap();
    legacy
        .write_txn("t", move |conn, meta| {
            jkb_core::tag::apply(conn, meta, bad_id, "repo", "proj")?;
            jkb_core::tag::apply(conn, meta, bad_id, "branch", "--upload-pack=x")
        })
        .unwrap();
    drop(legacy);

    let out = f
        .jkb()
        .args(["task", "close-merged", "--json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "the run aborted on the row it could not decide"
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let seen: Vec<&str> = v["closed"]
        .as_array()
        .unwrap()
        .iter()
        .chain(v["held"].as_array().unwrap())
        .filter_map(|t| t["uid"].as_str())
        .collect();
    assert!(
        seen.contains(&good.as_str()),
        "the healthy task got no verdict"
    );
    assert!(
        seen.contains(&bad.as_str()),
        "the undecidable task got no verdict"
    );
    // ...and every held task says why, so "in flight" is never indistinguishable from
    // "we could not tell".
    for t in v["held"].as_array().unwrap() {
        assert!(
            t["reason"].as_str().is_some_and(|r| !r.is_empty()),
            "a task was held with no reason given"
        );
    }
}

/// A land target is recorded under the name every reader looks it up by.
///
/// The `--onto` guard asked `branch_ref` — "does this revision resolve" — and `origin/<batch>`
/// resolves perfectly well, so it was accepted and stored verbatim. `jkb staging ls` keys batches
/// by the bare short names `branch_refs` returns, so the task silently dropped out of the one read
/// behind both the branch picker and In Flight: exactly the outcome the guard was added to
/// prevent. `jkb task land` then reached `adopt_remote("origin/<batch>")` and cut a junk local
/// branch by that literal name.
#[test]
fn a_land_target_is_recorded_under_the_name_the_listing_keys_on() {
    let f = Fixture::new();
    let remote = f.home.path().join("origin.git");
    git(&f.repo, &["init", "--bare", "-q", remote.to_str().unwrap()]);
    git(
        &f.repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&f.repo, &["push", "-q", "-u", "origin", "main"]);
    // A batch that lives only on the remote — the ordinary state after a local ref is pruned, and
    // the case that makes `origin/<batch>` the natural thing for a user to type.
    git(&f.repo, &["branch", "integration", "main"]);
    git(&f.repo, &["push", "-q", "origin", "integration"]);
    git(&f.repo, &["branch", "-D", "integration"]);
    git(
        &f.repo,
        &["checkout", "-q", "-b", "feat", "origin/integration"],
    );
    commit_in(&f.repo, "f.txt", "work\n", "feature work");
    git(&f.repo, &["checkout", "-q", "main"]);

    let uid = f.add_task("a batch named with its remote prefix");
    f.jkb()
        .args(["task", "start", &uid, "--branch", "feat"])
        .args(["--onto", "origin/integration"])
        .assert()
        .success();

    // Stored as the map key, not the caller's spelling.
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("onto integration"))
        .stdout(predicate::str::contains("lands on origin/integration").not());

    // And therefore visible in the one listing behind the picker and In Flight.
    let rows = f.staging(&[]);
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["branch"] == serde_json::json!("integration"))
        .cloned()
        .unwrap_or_default();
    assert!(
        row["tasks"]
            .as_array()
            .is_some_and(|ts| ts.iter().any(|t| t["uid"] == serde_json::json!(uid))),
        "the task was dropped from `staging ls` by the spelling of its land target: {rows}"
    );
}

/// A tag is not a branch, whatever `rev-parse` says about it.
///
/// It resolves, so the old guard admitted it, and the task then vanished from `staging ls` with
/// nothing created and nothing reported — the failure mode with no symptom at all.
#[test]
fn a_revision_that_is_not_a_branch_is_refused_as_a_land_target() {
    let f = Fixture::new();
    git(&f.repo, &["tag", "v1.0", "main"]);
    git(&f.repo, &["checkout", "-q", "-b", "feat"]);
    commit_in(&f.repo, "f.txt", "work\n", "feature work");
    git(&f.repo, &["checkout", "-q", "main"]);

    let uid = f.add_task("aimed at a tag");
    f.jkb()
        .args(["task", "start", &uid, "--branch", "feat", "--onto", "v1.0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not a branch"));
    // Nothing was recorded, so nothing has to be un-done.
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("lands on").not());
}

/// `jkb task abandon` acts on the branch the row it was clicked from names.
///
/// It was the third, unconverted implementation of "which recorded branch holds this task's
/// work": it took the session's branch or else the *first* `branch=` value, and
/// `tag::applications` orders by value. A task carrying a stale `a-gone` beside a live `z-live`
/// therefore had `--delete-branch` destroy `a-gone` and forget its cut point, while `staging ls`
/// and `jkb task land` — which share `repo::work_branch` — both said the row was about `z-live`.
#[test]
fn abandon_acts_on_the_same_branch_the_row_and_the_land_command_name() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "integration", "main"]);
    git(&f.repo, &["checkout", "-q", "-b", "z-live", "integration"]);
    commit_in(&f.repo, "z.txt", "z\n", "live work");
    git(&f.repo, &["checkout", "-q", "main"]);

    let uid = f.add_task("two recorded branches, one gone");
    f.jkb()
        .args(["task", "start", &uid, "--branch", "z-live"])
        .args(["--onto", "integration"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "tag", "add", &uid, "branch=a-gone"])
        .assert()
        .success();

    let out = f
        .jkb()
        .args([
            "task",
            "abandon",
            &uid,
            "--force",
            "--delete-branch",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "abandon: {out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["branch"],
        serde_json::json!("z-live"),
        "abandon reported a branch the user was never shown: {v}"
    );
    assert_eq!(
        v["branch_deleted"],
        serde_json::json!(true),
        "abandon deleted nothing, having picked a branch that was already gone: {v}"
    );
    assert!(
        git(&f.repo, &["branch", "--list", "z-live"]).is_empty(),
        "the branch the row named is still there"
    );
}

/// Trunk named by its remote-tracking spelling is still trunk.
///
/// The trunk comparison used to be made against the caller's own spelling, checked against both
/// trunk's short name and its full ref — two hand-guessed spellings of one branch. It is now made
/// against the canonical branch name, after resolution, so there is one comparison against one
/// name and `--onto origin/main` cannot be recorded as a land target by arriving in a third form.
#[test]
fn trunk_named_through_its_remote_copy_is_still_dropped_as_a_land_target() {
    let f = Fixture::new();
    let remote = f.home.path().join("origin.git");
    git(&f.repo, &["init", "--bare", "-q", remote.to_str().unwrap()]);
    git(
        &f.repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&f.repo, &["push", "-q", "-u", "origin", "main"]);
    git(&f.repo, &["checkout", "-q", "-b", "feature"]);
    commit_in(&f.repo, "w.txt", "work\n", "work");
    git(&f.repo, &["checkout", "-q", "main"]);

    let uid = f.add_task("cut from trunk, named through origin");
    f.jkb()
        .args(["task", "start", &uid, "--branch", "feature"])
        .args(["--onto", "origin/main"])
        .assert()
        .success()
        .stdout(predicate::str::contains("not recorded as a land target"));
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("lands on").not());
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

    // Claim. The swarm claims under its run owner and records which repo the work is in, before
    // any branch exists — where it *lands* is a fact about a branch, so it is recorded a step
    // later by the same command that names the branch. Run as the swarm runs them, so the tagging
    // step below is exercised against a task this same owner already holds.
    git(&f.repo, &["branch", "integration", "main"]);
    f.jkb()
        .args(["task", "claim", &uid, "--owner", OWNER])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "tag", "set", &uid, "repo=proj"])
        .assert()
        .success();
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
    // …and the landing is in the task's history, describing the branch it landed from. Nothing
    // is keyed by that name any more, so a branch later deleted or reused cannot make the entry
    // describe somebody else's work — which is what the `landed_head` column existed to guard
    // against when the record was keyed `(repo, branch)`.
    let out = f
        .jkb()
        .args(["task", "why", &uid, "--json"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let last = v["history"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["event"].as_str(), Some("land"));
    assert_eq!(last["branch"].as_str(), Some(branch.as_str()));
    assert_eq!(last["onto"].as_str(), Some(onto.as_str()));
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
        vec!["--global", "task", "base", "--forget", "-D"],
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

// ---------------------------------------------------------------------------
// The lifecycle machine at the CLI edge (design D48)
// ---------------------------------------------------------------------------

/// A finished task has no `start` transition, so `jkb task work` refuses it — and refuses it
/// with the machine's own sentence rather than a second one written beside it.
///
/// The rule was stated twice before: once here and once in the table. Two copies of a rule read
/// as protection while diverging from the one that actually decides, which is how one click came
/// to reopen a task that had already merged.
#[test]
fn a_finished_task_cannot_be_worked() {
    let f = Fixture::new();
    let uid = f.add_task("already finished");
    f.jkb()
        .args(["--global", "task", "set", &uid, "--status", "done"])
        .assert()
        .success();
    f.jkb()
        .args(["task", "work", &uid])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not something that can happen"));
    assert_eq!(f.status_of(&uid), "done", "the refusal changed the task");
}

/// An owner whose liveness cannot be established keeps its claim, and is **reported** rather
/// than freed (design S3.2).
///
/// The behaviour change this branch makes, and the one worth pinning end to end: the old probe
/// returned `bool` and treated an owner it could not read as dead, which silently hands a live
/// agent's task to somebody else. `doctor --fix` must leave it alone and say so.
#[test]
fn an_owner_whose_liveness_cannot_be_checked_keeps_its_claim() {
    let f = Fixture::new();
    let uid = f.add_task("held by an opaque agent");
    f.jkb()
        .args([
            "--global",
            "task",
            "claim",
            &uid,
            "--owner",
            "agent:01JBX7Q4",
        ])
        .assert()
        .success();

    // Reported in its own bucket — not as "orphaned (owner gone)", which is a different answer.
    f.jkb()
        .args(["doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("liveness cannot be checked"))
        .stdout(predicate::str::contains(&uid));

    // ...and `--fix` does not touch it: the claim is still there, still reported.
    f.jkb().args(["doctor", "--fix"]).assert().success();
    f.jkb()
        .args(["doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("agent:01JBX7Q4"))
        .stdout(predicate::str::contains("NOT auto-reclaimed"));

    // A dead *process* owner, by contrast, is proven gone and is reclaimed.
    let other = f.add_task("held by a dead process");
    f.jkb()
        .args([
            "--global",
            "task",
            "claim",
            &other,
            "--owner",
            "host:4294967290",
        ])
        .assert()
        .success();
    f.jkb()
        .args(["doctor", "--fix"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cleared 1 orphaned claim"));
}

/// `jkb task why` is the read this area most obviously lacked: what moved a task, and on what
/// evidence. Fourteen must-fix findings in the corpus are "held for ever with no way to see why".
#[test]
fn task_why_shows_what_moved_the_task_and_on_what_evidence() {
    let f = Fixture::new();
    let uid = f.add_task("a task with a history");
    let s = f.work(&uid);
    let branch = s["branch"].as_str().unwrap().to_owned();

    let out = f
        .jkb()
        .args(["--global", "task", "why", &uid, "--json"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows = v["history"].as_array().unwrap();
    assert!(!rows.is_empty(), "opening a session recorded nothing");
    let start = rows
        .iter()
        .find(|r| r["event"].as_str() == Some("start"))
        .expect("the claim that started the task is in its history");
    assert_eq!(start["to"].as_str(), Some("in_progress"));
    assert!(
        start["agent"]
            .as_str()
            .is_some_and(|a| a.starts_with("session:")),
        "the history does not say who acted"
    );
    assert!(
        rows.iter()
            .any(|r| r["branch"].as_str() == Some(branch.as_str())),
        "the branch the work is on is not recorded anywhere in the history"
    );
}

/// A parent with an open subtask is refused **before** the graft, not reported after it.
///
/// The rule was checked only inside the machine's `land` guard, which the transition applies
/// last — after the rebase, the fast-forward, the gate and the session disposal. So it did not
/// prevent the landing, it narrated one that had already happened: branch moved, worktree gone,
/// task left `in_progress`. The assertion that matters is that the **target did not move**.
#[test]
fn a_parent_with_an_open_subtask_is_refused_before_the_graft() {
    let f = Fixture::new();
    let parent = f.add_task("parent task");
    let s = f.work(&parent);
    let worktree = std::path::PathBuf::from(s["worktree"].as_str().unwrap());
    let onto = s["onto"].as_str().unwrap().to_owned();
    commit_in(&worktree, "p.txt", "parent work\n", "parent work");

    // A subtask, created after the session so the parent already has commits to land.
    f.jkb()
        .args(["--global", "task", "add", "child task", "--under", &parent])
        .assert()
        .success();

    let before = git(&f.repo, &["rev-parse", &onto]);
    f.jkb()
        .args(["task", "land", &parent, "--no-gate", "--no-review"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("open subtasks"));
    assert_eq!(
        git(&f.repo, &["rev-parse", &onto]),
        before,
        "the refusal happened after the graft had already moved the target"
    );
    assert!(
        worktree.exists(),
        "the session was disposed of before the refusal"
    );
    assert_eq!(f.status_of(&parent), "in_progress");
}
