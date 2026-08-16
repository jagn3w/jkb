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

    /// Plant a cut point directly in `branch_records`, bypassing the measurement.
    ///
    /// There is no CLI verb that accepts a commit id — deliberately, design B6 — so a *reader*
    /// test that needs a specific stored value writes the row. Everything the schema enforces
    /// still applies: the CHECK refuses anything but a full lowercase object id, so this cannot
    /// plant a state the store would not hold.
    ///
    /// `repo` is the fixture's repo key, which is what the record is keyed under.
    fn plant_cut_point(&self, branch: &str, sha: &str) {
        let db = jkb_core::Db::open(self.db.to_str().unwrap()).unwrap();
        let (repo, branch, sha) = (self.repo_key(), branch.to_owned(), sha.to_owned());
        db.write_txn("t", move |conn, meta| {
            jkb_core::branch::record_cut_point(
                conn,
                meta,
                &repo,
                &branch,
                &jkb_core::branch::Cut::Fork(sha),
                None,
                jkb_core::branch::Supersede::default(),
            )
            .map(|_| ())
        })
        .unwrap();
    }

    /// The cut point stored for `branch`, read back through core.
    ///
    /// The CLI reports a cut point only against a *task*, and the records this reads about belong
    /// to a batch branch no task is on — which is precisely the branch the ensure-on-reference
    /// writes and the one that got overwritten.
    fn cut_point_of(&self, branch: &str) -> Option<String> {
        let db = jkb_core::Db::open(self.db.to_str().unwrap()).unwrap();
        let (repo, branch) = (self.repo_key(), branch.to_owned());
        db.read(move |conn| jkb_core::branch::get(conn, &repo, &branch))
            .unwrap()
            .and_then(|r| r.cut_point)
    }

    /// The repo key the records are stored under — the basename of the fixture's checkout.
    fn repo_key(&self) -> String {
        self.repo
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
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

    let out = f
        .jkb()
        .args(["--global", "task", "show", &uid, "--json"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let targets: Vec<&str> = v["branches"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|b| b["land_target"].as_str())
        .collect();
    assert_eq!(targets, vec!["batch-two"], "one target, not two");
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

/// Abandoning one task of a swarm group leaves its siblings on the batch.
///
/// A land target is keyed `(repo, branch)`, and `/task-swarm` puts up to four tasks on one group
/// branch — so a command acting on one task reaches all four. Under the old item-keyed `onto=`
/// facet this was per task and could not touch a sibling; keyed by branch it dropped the other
/// three out of `jkb staging ls` and out of `jkb task land`, whose advice for a task with no
/// target cuts a *second* branch and detaches it from the batch.
#[test]
fn abandoning_one_task_of_a_group_keeps_the_others_on_the_batch() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "integration", "main"]);
    git(&f.repo, &["checkout", "-q", "-b", "grp", "integration"]);
    commit_in(&f.repo, "g.txt", "group work\n", "group work");
    git(&f.repo, &["checkout", "-q", "main"]);
    let a = f.add_task("group task a");
    let b = f.add_task("group task b");
    for uid in [&a, &b] {
        f.jkb()
            .args([
                "task",
                "start",
                uid,
                "--branch",
                "grp",
                "--onto",
                "integration",
            ])
            .assert()
            .success();
    }

    // The state, asserted before the sentence about it: a mutation that clears the target while
    // still printing the note would otherwise be caught only by the report, and the report is not
    // the thing that keeps the batch.
    let out = f
        .jkb()
        .args(["task", "abandon", &a, "--force"])
        .output()
        .unwrap();
    assert!(out.status.success(), "abandon: {out:?}");

    let rows = f.staging(&[]);
    let batch = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["branch"] == "integration")
        .expect("abandoning one task of the group took the whole batch out of the listing");
    let uids: Vec<&str> = batch["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["uid"].as_str())
        .collect();
    assert!(
        uids.contains(&b.as_str()),
        "the sibling lost its land target when its group-mate was abandoned: {uids:?}"
    );
    let note = String::from_utf8_lossy(&out.stderr);
    assert!(
        note.contains("land target was kept") && note.contains(&b),
        "nothing said the branch was shared, so the kept target looks like the command \
         doing nothing: {note}"
    );

    // …and once nothing live is left on the branch the clear happens as before, which is what the
    // clear is for. "Live" is the task lifecycle: an abandoned task stays open and stays on its
    // branch, so only a terminal one stops holding the batch.
    f.jkb()
        .args(["--global", "task", "set", &a, "--status", "cancelled"])
        .assert()
        .success();
    let last = f
        .jkb()
        .args(["task", "abandon", &b, "--force"])
        .output()
        .unwrap();
    assert!(last.status.success(), "abandon: {last:?}");
    assert!(
        !f.staging(&[])
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["branch"] == "integration"),
        "a batch with no live task on it is still offered as a land target"
    );
    assert!(
        !String::from_utf8_lossy(&last.stderr).contains("land target was kept"),
        "a terminal task was counted as work still on the branch"
    );
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
            "{branch}: cut from {cut_point}"
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

/// A `base=` **tag** now decides nothing, whatever is done to it.
///
/// This is the shape of the whole change, asserted from the outside. There used to be a reserved
/// facet with a store-side refusal, a privileged writer, an authored/unauthored read split, and
/// skips in both directions of the sync reconcile — six ascending choke points, the fifth write
/// route found *after* the fifth of them, and a must-fix inside the reservation's own asymmetry.
/// The fact moved to `branch_records`, so a tag called `base` is ordinary content: it can be
/// added, renamed onto or off the name, and none of it moves a landing decision.
#[test]
fn a_base_tag_is_ordinary_content_and_decides_nothing() {
    let f = Fixture::new();
    let uid = f.add_task("plain task");
    let head = git(&f.repo, &["rev-parse", "HEAD"]);

    // Renaming a facet onto the name — the fifth write route a review pass found, after the
    // store-side reservation had been added for the other four — is just a rename now.
    f.jkb()
        .args(["--global", "task", "tag", "add", &uid, "area=sync"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "tag", "rename", "area", "base"])
        .assert()
        .success();
    // …and it is freely writable directly, by every route that used to be closed off.
    f.jkb()
        .args([
            "--global",
            "task",
            "tag",
            "add",
            &uid,
            &format!("base={head}"),
        ])
        .assert()
        .success();

    // The task records a real branch with NO cut point, so the only thing that could close it is
    // the tag — which is what this asserts cannot happen. `main` is deliberately not used: trunk
    // is trivially merged into itself.
    git(&f.repo, &["branch", "feature"]);
    f.jkb()
        .args(["--global", "task", "tag", "set", &uid, "branch=feature"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "base", "--forget", "feature"])
        .assert()
        .success();
    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "open",
        "a `base=` tag closed a task whose branch has no recorded cut point"
    );
}

/// `task start --repo <other>` names **that** repository in its remedy — the one it was handed,
/// not the one the task used to be in and not a placeholder.
///
/// The reason it could get this wrong is the ordering the fix pins: `Missing::NotThisRepo` reads
/// the task's `repo=`, and the location facets used to be written *after* the branch, so on a
/// task's first `task start --repo <other>` there was no `repo=` yet and the note fell back to
/// "its repo" — on the one run that was given the answer as an argument.
#[test]
fn a_start_in_another_repo_names_that_repo_in_its_remedy() {
    let f = Fixture::new();
    let uid = f.add_task("cross-repo task");
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
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("run it again from otherproj"))
        .stdout(predicate::str::contains("its repo").not());

    // And after a *change* of repo it names the new one, not the previous. Same bug, second shape:
    // the note was written from a facet the same transaction was about to overwrite.
    f.jkb()
        .args([
            "--global",
            "task",
            "start",
            &uid,
            "--repo",
            "thirdproj",
            "--branch",
            "feat/y",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("run it again from thirdproj"));
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
        .stdout(predicate::str::contains("lands on batch-1"))
        // The cut point is still, correctly, not measured here — the two facts are independent,
        // and asserting both is what stops a fix for one silently enabling the other.
        .stdout(predicate::str::contains("feat/x: no cut point recorded"));
}

/// A cut point this repository cannot resolve is treated as **none** — the task is held, never
/// closed.
///
/// `is_merged` decides "freshly cut, nothing on it yet" by comparing the branch tip against
/// `rev-parse <base>`. When the base does not resolve, that right-hand side is `None`, the
/// comparison is false rather than unknown, and the guard is *skipped*: the branch falls through
/// to `merge-tree` and reads as merged. A missing cut point refuses to act; a garbage one closed
/// the task with the work never written.
///
/// **The branch has commits of its own**, which is what makes the reader the thing under test.
/// On an untouched branch the *writer* would supersede a stored value that is not the tip (an
/// untouched branch forked at its own tip), so the reader would never meet it — and a test that
/// cannot reach the code it names is not covering it.
///
/// `git rev-parse` is a parser, not a lookup: a full-length hex string is already a well-formed
/// object name, so it exits 0 and echoes it back for an object the clone does not have. An earlier
/// version of this test used a 16-character value, which `rev-parse` *does* reject, so it passed
/// against a check that let every fabricated 40-character sha straight through.
#[test]
fn a_cut_point_git_cannot_resolve_is_treated_as_none() {
    let f = Fixture::new();
    let uid = f.add_task("bogus base task");
    git(&f.repo, &["checkout", "-q", "-b", "feature"]);
    commit_in(&f.repo, "w.txt", "work\n", "branch work");
    git(&f.repo, &["checkout", "-q", "main"]);

    f.plant_cut_point("feature", "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
    f.jkb()
        .args(["task", "start", &uid, "--branch", "feature"])
        .assert()
        .success();

    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "a branch was closed as merged against a cut point that does not resolve here, so the \
         freshly-cut guard was skipped instead of applied"
    );
}

/// And a value that is not a full object id cannot be **stored** at all, whichever door it comes
/// through — the schema refuses it.
///
/// A symbolic revision is the dangerous shape precisely because it resolves in *every* clone, to
/// something different in each. It used to be refused by a check at the CLI verb, then by a check
/// inside the writer; it is now a CHECK constraint, so there is no door left to add one to.
#[test]
fn only_a_full_object_id_can_be_stored_as_a_cut_point() {
    let f = Fixture::new();
    let db = jkb_core::Db::open(f.db.to_str().unwrap()).unwrap();
    for bad in ["HEAD", "main", "deadbeefdeadbeef", "1111111"] {
        let value = bad.to_owned();
        let err = db.write_txn("t", move |conn, meta| {
            jkb_core::branch::record_cut_point(
                conn,
                meta,
                "proj",
                "feature",
                &jkb_core::branch::Cut::Fork(value),
                None,
                jkb_core::branch::Supersede::default(),
            )
            .map(|_| ())
        });
        assert!(err.is_err(), "`{bad}` was stored as a cut point");
    }
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

    // Drop it — the one verb that touches a recorded cut point, and the one whose whole purpose
    // is to leave the branch with none.
    f.jkb()
        .args(["--global", "task", "base", "--forget", "feature-x"])
        .assert()
        .success();

    f.jkb()
        .args(["task", "close-merged"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&uid))
        .stdout(predicate::str::contains("no cut point recorded"))
        // The remedy MEASURES. No verb accepts a sha any more, so no message can suggest one —
        // the sha nearest a user's hand is the branch tip, and a cut point equal to the tip reads
        // as "nothing has happened here" forever: never creditable, never landable, and never
        // corrected. That is strictly worse than the state being reported.
        .stdout(predicate::str::contains("jkb task start"))
        .stdout(predicate::str::contains("<sha>").not());
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

/// A record made in one repository is not visible to another — the key, asserted end to end.
///
/// The database is global across repos (D32), so every one of these commands legitimately runs
/// from anywhere. A namesake branch in a sibling checkout is a different branch, and the old shape
/// had no way to say so: the value was a tag on a task, and three separate commands each had their
/// own idea of when it was safe to measure. `repo` is a key column now, so the question is
/// answered by the store rather than remembered by each caller.
#[test]
fn a_cut_point_recorded_in_one_repo_is_not_lent_to_a_namesake_branch_in_another() {
    let f = Fixture::new();
    let other = f.home.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q", "-b", "main", "."]);
    std::fs::write(other.join("o.txt"), "other\n").unwrap();
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-qm", "other base"]);
    // A namesake branch there, with work on it, so it could be mistaken for the task's.
    git(&other, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(other.join("p.txt"), "more\n").unwrap();
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-qm", "other work"]);
    git(&other, &["checkout", "-q", "main"]);

    // The task's own repo, and a real measured cut point in it.
    let uid = f.add_task("cross repo task");
    git(&f.repo, &["checkout", "-q", "-b", "feature"]);
    commit_in(&f.repo, "w.txt", "work\n", "our work");
    git(&f.repo, &["checkout", "-q", "main"]);
    f.jkb()
        .args([
            "task", "start", &uid, "--branch", "feature", "--onto", "main",
        ])
        .assert()
        .success();
    let ours = git(&f.repo, &["merge-base", "feature", "main"]);
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "feature: cut from {ours}"
        )));

    // Standing in the *other* repo, nothing may be measured for this task and the record it
    // already has is untouched.
    let mut cmd = f.jkb();
    cmd.current_dir(&other)
        .args([
            "task", "start", &uid, "--branch", "feature", "--repo", "proj",
        ])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "feature: cut from {ours}"
        )));
    // …and the other repo's own record for that name does not exist at all, so nothing there
    // can be read as this task's.
    let mut show = f.jkb();
    show.current_dir(&other)
        .args(["--global", "task", "base", "--forget", "feature"])
        .assert()
        .success()
        .stdout(predicate::str::contains("had no recorded cut point"));
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
            "task/stranded-task: cut from {branch_tip}"
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
            .stdout(predicate::str::contains(format!("feature: cut from {tip}")));
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
            .stdout(predicate::str::contains(format!("mytask: cut from {tip}")));

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
        .stdout(predicate::str::contains("run it again once not-yet exists"))
        // Never a hand-typed sha, for the reason above.
        .stdout(predicate::str::contains("jkb task base").not());

    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("cut from").not());

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
    // Drop the cut point the session recorded, leaving containment undecidable.
    f.jkb()
        .args(["task", "base", "--forget", &branch])
        .assert()
        .success();
    f.add_finding("reviews/stg-2", "a finding");

    f.jkb()
        .args(["task", "review", "record", "--branch", "stg"])
        .args(["--findings", "reviews/stg-2"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no cut point recorded"))
        // MEASURE, never a hand-typed sha: the nearest one is the branch tip, which freezes the
        // task at `NothingToMerge` for good. No verb takes one any more, so no message can name
        // one — this was the third surface that had to be corrected individually.
        .stdout(predicate::str::contains("jkb task start"))
        .stdout(predicate::str::contains("<sha>").not());
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

    // A second, live branch with its own unmerged work, recorded on the same task. Its cut point
    // is planted rather than measured through `task start`, which would take the live session's
    // claim; what this test is about is the reader, and the reader needs a usable record.
    let fork = git(&f.repo, &["rev-parse", "main"]);
    git(&f.repo, &["checkout", "-q", "-b", "sibling"]);
    commit_in(&f.repo, "sib.txt", "sibling work\n", "sibling work");
    git(&f.repo, &["checkout", "-q", "main"]);
    f.jkb()
        .args(["--global", "task", "tag", "add", &uid, "branch=sibling"])
        .assert()
        .success();
    f.plant_cut_point("sibling", &fork);

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
            "swarm-task/group: cut from {cut}"
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
        .stdout(predicate::str::contains("run it again from proj"))
        .stdout(predicate::str::contains("jkb task base").not());
}

/// A cut point is measured against the parent the caller **states**, never one read back from the
/// branch's recorded land target.
///
/// Which branch this one was cut from is something the caller knows in the moment; the stored land
/// target records some earlier moment, and a stale one is worse than none. Reading it back would
/// measure a merge-base with a batch this branch was never cut from — a commit well behind its
/// tip — and the freshly-cut guard would then be skipped for work that never landed.
///
/// The branch has **commits of its own**, which is what makes this discriminating: an untouched
/// branch records its own tip whatever parent is named, so the stated-versus-stored distinction
/// would be invisible there. With work and no stated parent the honest answer is *nothing
/// recorded*, and anything else means the stored value was consulted.
#[test]
fn a_stale_land_target_is_not_used_to_measure_a_new_branchs_cut_point() {
    let f = Fixture::new();

    // A batch from an earlier round, which has diverged from trunk...
    git(&f.repo, &["checkout", "-q", "-b", "stale-batch"]);
    commit_in(&f.repo, "old.txt", "old\n", "an earlier batch");
    git(&f.repo, &["checkout", "-q", "main"]);
    // ...and trunk has moved on since.
    commit_in(&f.repo, "new.txt", "new\n", "trunk moves on");
    // A branch with real work of its own.
    git(&f.repo, &["checkout", "-q", "-b", "feature"]);
    commit_in(&f.repo, "f.txt", "feature\n", "feature work");
    git(&f.repo, &["checkout", "-q", "main"]);

    // The task's branch carries a land target from that earlier round, and its cut point is then
    // dropped — the state a re-measurement would have to fill.
    let uid = f.add_task("new work, old target");
    f.jkb()
        .args(["task", "start", &uid, "--branch", "feature"])
        .args(["--onto", "stale-batch"])
        .assert()
        .success();
    f.jkb()
        .args(["task", "base", "--forget", "feature"])
        .assert()
        .success();

    // Re-run naming NO parent. Reading the stored land target back would record a merge-base with
    // `stale-batch`; the honest answer is nothing, and the reason.
    f.jkb()
        .args(["task", "start", &uid, "--branch", "feature", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"base\":null"))
        .stdout(predicate::str::contains(
            "no-parent-named-and-branch-has-commits",
        ));
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("cut from").not());

    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "a task with no measurable cut point must be held, never closed"
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
        .stdout(predicate::str::contains(format!("feature: cut from {tip}")));

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
        .stdout(predicate::str::contains(format!(
            "handmade: cut from {tip}"
        )));

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
        .stdout(predicate::str::contains(format!("second: cut from {tip}")));
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
        .stdout(predicate::str::contains("cut from").not());
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
    let fork = git(&f.repo, &["rev-parse", "main"]);
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
    show.stdout(predicate::str::contains(format!(
        "feature: cut from {fork}"
    )))
    .stdout(predicate::str::contains("onto=").not());

    // And the point of all that: the work is real, so once it lands the task closes.
    git(&f.repo, &["merge", "-q", "--ff-only", "feature"]);
    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "done",
        "a branch cut from trunk with real work on it could not be given a usable cut point"
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

/// A branch that has done nothing records its own tip, whatever parent the caller names.
///
/// The measurement used to depend on the caller naming a *useful* reference point, and four
/// separate findings were the same consequence of that: a stale parent, a wrong one, an
/// unresolvable one, and — reachable because the CLI invites `--onto <trunk>` — a **grandparent**.
/// `main` is a truthful thing to say about a branch cut from a staging branch that was itself cut
/// from main, and it put every merge-base behind the branch's real origin, so a branch with
/// nothing on it recorded `base != tip`, skipped the freshly-cut guard, and closed as merged.
///
/// So the question is asked of git instead — has any commit here reached no other branch — and it
/// needs no reference point at all.
#[test]
fn a_branch_that_has_done_nothing_records_its_tip_whatever_parent_is_named() {
    let f = Fixture::new();
    // main → stage (one commit) → feature (nothing of its own).
    git(&f.repo, &["checkout", "-q", "-b", "stage"]);
    commit_in(&f.repo, "s.txt", "staging\n", "staging work");
    git(&f.repo, &["checkout", "-q", "-b", "feature"]);
    git(&f.repo, &["checkout", "-q", "main"]);
    let tip = git(&f.repo, &["rev-parse", "feature"]);

    let uid = f.add_task("grandparent named as the parent");
    f.jkb()
        .args([
            "task", "start", &uid, "--branch", "feature", "--onto", "main",
        ])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("feature: cut from {tip}")));

    git(&f.repo, &["merge", "-q", "--ff-only", "stage"]);
    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "a branch with nothing of its own closed as merged: naming a grandparent put every \
         merge-base behind its real origin"
    );
}

/// One unusable branch value costs its own row and no more.
///
/// Quick-add reaches `tag::apply` below the ref check, so a value git reads as an option could be
/// planted on a task; `close-merged` then aborted on it, closing nothing for *any* task in the
/// repo — silently, because it also runs from `scripts/hooks/post-merge`. Both halves are covered
/// here: the value is refused at the store now, and a row carrying one from before is isolated.
#[test]
fn one_unusable_branch_value_does_not_stop_the_whole_close_merged_run() {
    let f = Fixture::new();
    // Refused at the store, so it cannot be planted this way any more.
    f.jkb()
        .args(["--global", "task", "add", "hostile #branch=--upload-pack=x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used as a branch"));

    // A healthy task whose branch really did land, beside a row carrying such a value from before
    // the check existed — planted the only way left, straight through the tag repo.
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
    git(&f.repo, &["merge", "-q", "--ff-only", "done-work"]);

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

    f.jkb()
        .args(["task", "close-merged"])
        .assert()
        .success()
        .stdout(predicate::str::contains("unusable"));
    assert_eq!(
        f.status_of(&good),
        "done",
        "one malformed branch tag stopped every healthy task in the repo from closing"
    );
}

/// The branch tip is a cut point only where it is provably one, and never a fallback.
///
/// This is the invariant the whole measurement rests on, so it is asserted directly rather than
/// through a consequence. `base == tip` means "nothing has happened on this branch" to every
/// reader — correct for an untouched branch and catastrophic for one with work, because the task
/// is then never creditable, never landable, and never repairable (`ensure_recorded` does not
/// overwrite). Three separate fallbacks used to return the tip, and once the untouched case was
/// hoisted above them, every one was reachable *only* when the branch had commits — that is, only
/// where the tip was the worst answer available.
///
/// Both halves matter, so both are asserted: untouched records the tip, worked-on never does.
#[test]
fn the_tip_is_recorded_only_for_a_branch_that_has_done_nothing() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "untouched", "main"]);
    let tip = git(&f.repo, &["rev-parse", "untouched"]);
    let a = f.add_task("nothing on it");
    f.jkb()
        .args(["task", "start", &a, "--branch", "untouched"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &a])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "untouched: cut from {tip}"
        )));

    // A branch with commits, and no parent named — the documented `jkb task start` invocation.
    git(&f.repo, &["checkout", "-q", "-b", "worked"]);
    commit_in(&f.repo, "w.txt", "work\n", "work done before registering");
    git(&f.repo, &["checkout", "-q", "main"]);
    let worked_tip = git(&f.repo, &["rev-parse", "worked"]);
    let b = f.add_task("commits already on it");
    f.jkb()
        .args(["task", "start", &b, "--branch", "worked", "--json"])
        .assert()
        .success()
        // Nothing recorded, and the reason says which of the four it was.
        .stdout(predicate::str::contains("\"base\":null"))
        .stdout(predicate::str::contains(
            "no-parent-named-and-branch-has-commits",
        ));
    f.jkb()
        .args(["--global", "task", "show", &b])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("worked: cut from {worked_tip}")).not());

    // …and unlike a recorded tip, that state is repairable: naming the parent measures it.
    let fork = git(&f.repo, &["merge-base", "worked", "main"]);
    f.jkb()
        .args(["task", "start", &b, "--branch", "worked", "--onto", "main"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &b])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("worked: cut from {fork}")));
}

/// A record that cannot describe this branch is discarded, whichever verb notices.
///
/// A cut point is keyed by branch NAME and a name outlives the branch that held it. There is no
/// git fact separating the old branch from the new one — but there is one that makes the record
/// provably wrong: **an untouched branch forked at its own tip**. So a recorded value other than
/// the tip, on a branch with no commits of its own, belongs to whatever had the name before.
///
/// The predecessor of this guard was a created-ness flag threaded out of `worktree_add`, which
/// `jkb task start` could not supply at all — so this asserts the `task start` route specifically.
#[test]
fn a_stale_record_is_discarded_by_task_start_not_only_by_task_work() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "batch", "main"]);
    git(&f.repo, &["branch", "feature", "batch"]);
    let uid = f.add_task("recycled name");
    f.jkb()
        .args([
            "task", "start", &uid, "--branch", "feature", "--onto", "batch",
        ])
        .assert()
        .success();

    // The branch is scrapped by hand and re-cut somewhere else, without jkb involved at all.
    git(&f.repo, &["branch", "-D", "feature"]);
    commit_in(&f.repo, "moved.txt", "moved\n", "trunk moves on");
    git(&f.repo, &["branch", "-f", "batch", "main"]);
    git(&f.repo, &["branch", "feature", "batch"]);
    let recut = git(&f.repo, &["rev-parse", "feature"]);

    f.jkb()
        .args([
            "task", "start", &uid, "--branch", "feature", "--onto", "batch",
        ])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "feature: cut from {recut}"
        )));

    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "an empty re-cut branch closed as merged, on the record of the branch that had its name"
    );
}

/// Naming a second branch on a task must not disturb the first one's record.
///
/// This was the purest finding in the whole corpus, and it was a *storage* defect rather than a
/// caller's mistake: a cut point was a tag value on a task, so the per-branch fact had to be
/// encoded into the value, and the documented repair — `jkb task tag set base=` — cleared the
/// facet's other values, which were other branches' records. The key is `(repo, branch)` now, so
/// one branch's record is not addressable from another's write.
///
/// Asserted through the CLI rather than only at the store, because the store is not where it went
/// wrong: every writer here goes through `record_branch`, and this is what makes a second one
/// harmless.
#[test]
fn recording_a_second_branch_leaves_the_first_ones_cut_point_alone() {
    let f = Fixture::new();
    let uid = f.add_task("two branch task");

    // Two branches, each with work of its own and a different fork point, so a record lent from
    // one to the other would be visible rather than coincidentally equal.
    git(&f.repo, &["checkout", "-q", "-b", "first"]);
    commit_in(&f.repo, "a.txt", "a\n", "first work");
    git(&f.repo, &["checkout", "-q", "main"]);
    let first_fork = git(&f.repo, &["merge-base", "first", "main"]);
    commit_in(&f.repo, "trunk.txt", "trunk\n", "trunk moves on");
    git(&f.repo, &["checkout", "-q", "-b", "second"]);
    commit_in(&f.repo, "b.txt", "b\n", "second work");
    git(&f.repo, &["checkout", "-q", "main"]);
    let second_fork = git(&f.repo, &["merge-base", "second", "main"]);
    assert_ne!(first_fork, second_fork, "setup: the forks must differ");

    for branch in ["first", "second"] {
        f.jkb()
            .args(["task", "start", &uid, "--branch", branch, "--onto", "main"])
            .assert()
            .success();
    }
    f.jkb()
        .args(["--global", "task", "tag", "add", &uid, "branch=first"])
        .assert()
        .success();

    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "first: cut from {first_fork}"
        )))
        .stdout(predicate::str::contains(format!(
            "second: cut from {second_fork}"
        )));
}

/// `jkb task add "… #branch=X"` records a cut point for X, like every other way of naming one.
///
/// Quick-add reached `tag::apply` directly, so this entry point silently opted out of the pairing
/// that is the whole architecture here — and made the claim "`record_branch` is the only writer of
/// `branch=`" untrue, which is worse than the missing record: the next person reads that claim in
/// the module doc and trusts it.
#[test]
fn quick_add_pairs_a_branch_with_its_cut_point_like_every_other_writer() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "planned", "main"]);
    let tip = git(&f.repo, &["rev-parse", "planned"]);
    let out = f
        .jkb()
        .args([
            "--global",
            "task",
            "add",
            "quick added #repo=proj #branch=planned",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "task add: {out:?}");
    let uid = serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["uid"]
        .as_str()
        .unwrap()
        .to_owned();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("branch=planned"))
        .stdout(predicate::str::contains(format!("planned: cut from {tip}")));
}

// ---------------------------------------------------------------------------
// Landing is an event where jkb performs it (design B4)
// ---------------------------------------------------------------------------

/// Set up a swarm-shaped task: a group branch with work, recorded against an integration branch.
/// Returns the task uid and the group branch name.
fn a_group_landing_on_an_integration_branch(f: &Fixture) -> (String, String) {
    let uid = f.add_task("group landing");
    git(&f.repo, &["branch", "integration", "main"]);
    git(&f.repo, &["checkout", "-q", "-b", "grp", "integration"]);
    commit_in(&f.repo, "g.txt", "group work\n", "group work");
    git(&f.repo, &["checkout", "-q", "main"]);
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
    (uid, "grp".to_owned())
}

/// The merge queue's verb **refuses** to record a landing that has not happened.
///
/// A landing event is a trusted fact: readers act on it without re-deriving anything from refs, so
/// the one verb that writes it from outside Rust is a new write route for exactly the class of
/// fact this whole area exists to protect. It re-establishes what it is being told by asking the
/// same question every reader asks — is this branch's work in the target — so a hand-run for work
/// that is still in flight fails.
#[test]
fn the_landing_verb_refuses_a_branch_whose_work_is_not_in_the_target() {
    let f = Fixture::new();
    let (uid, branch) = a_group_landing_on_an_integration_branch(&f);
    f.jkb()
        .args(["task", "landed", &branch, "--onto", "integration"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("is not in integration"));

    // And nothing was recorded, so the reader is not handed a landing that did not happen.
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("landed on").not());
}

/// A branch landed by jkb onto a staging branch closes its task when that staging branch reaches
/// trunk — **and not before**.
///
/// This is what the landing event buys: the number of branches whose landing has to be inferred
/// from refs drops from one per task to one per batch, and the surviving one is the branch whose
/// cut point is provable (a freshly cut batch forked at its own tip) rather than measured against
/// a moving parent.
///
/// **The group's own cut point is dropped first, and that is the whole test.** Without it this
/// passed with the credited-landing arm of `repo::landed_for_action` deleted: the group branch has
/// commits of its own and a cut point behind them, so once trunk reached the batch the *ordinary*
/// inference answered `Merged` and closed the task all by itself. Every assertion held while the
/// thing under test did nothing. With no cut point the branch's own inference must refuse to act
/// (D34.4), so only following the landing event can close this — which is exactly what the event
/// is for, and exactly the state B4 exists to rescue, since a squashed or rebased landing destroys
/// the ref inference too.
#[test]
fn a_jkb_landed_branch_closes_when_its_batch_reaches_trunk() {
    let f = Fixture::new();
    let (uid, branch) = a_group_landing_on_an_integration_branch(&f);

    // The queue's graft: the batch fast-forwards to the group's commits.
    git(&f.repo, &["checkout", "-q", "integration"]);
    git(&f.repo, &["merge", "-q", "--ff-only", &branch]);
    git(&f.repo, &["checkout", "-q", "main"]);
    f.jkb()
        .args(["task", "landed", &branch, "--onto", "integration"])
        .assert()
        .success();
    // Leave the group branch with nothing its own inference can decide on. `--forget` keeps the
    // landing deliberately (they are separate facts), which is the state this asserts about.
    f.jkb()
        .args(["--global", "task", "base", "--forget", &branch])
        .assert()
        .success();

    // The work is in the batch, and the batch has not reached trunk.
    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "a task closed while its batch was still unmerged"
    );

    git(&f.repo, &["merge", "-q", "--ff-only", "integration"]);
    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "done",
        "the batch reached trunk and the recorded landing was not followed — with no cut point of \
         its own, nothing but the landing event can decide this branch"
    );
}

/// Naming a batch as a land target never **replaces** the batch's cut point — only fills it.
///
/// The ensure-on-reference used to go through the measuring entry point, and a merge queue leaves
/// the batch in exactly the state that defeats it: the group's commits are fast-forwarded into the
/// batch but the group branch still holds them, so `has_own_commits(integration)` is truthfully
/// `false`. The measuring path reads "untouched" as proof the record belongs to a different branch
/// of the same name, and replaces the batch's real cut point with its **current tip** — after
/// which `is_merged` answers `NothingToMerge` for ever (cut point == tip) and every task already
/// landed on that batch is frozen, with `--forget` unable to repair it.
///
/// The value is asserted before the consequence, because "the second group's task did not close"
/// is also true when nothing was recorded at all.
#[test]
fn naming_a_batch_as_a_land_target_does_not_replace_its_cut_point() {
    let f = Fixture::new();
    let (first, group) = a_group_landing_on_an_integration_branch(&f);
    let cut = f
        .cut_point_of("integration")
        .expect("setup: referencing the batch recorded its cut point");

    // The merge queue's graft. The group branch survives it, which is what makes the batch read as
    // having no commits of its own.
    git(&f.repo, &["checkout", "-q", "integration"]);
    git(&f.repo, &["merge", "-q", "--ff-only", &group]);
    git(&f.repo, &["checkout", "-q", "main"]);
    f.jkb()
        .args(["task", "landed", &group, "--onto", "integration"])
        .assert()
        .success();

    // The swarm's next group names the same batch. This call is a statement about `grp-b`; what it
    // can observe about `integration` is not evidence about `integration`.
    let second = f.add_task("second group");
    git(&f.repo, &["checkout", "-q", "-b", "grp-b", "integration"]);
    commit_in(&f.repo, "h.txt", "more group work\n", "more group work");
    git(&f.repo, &["checkout", "-q", "main"]);
    f.jkb()
        .args([
            "task",
            "start",
            &second,
            "--branch",
            "grp-b",
            "--onto",
            "integration",
        ])
        .assert()
        .success();

    assert_eq!(
        f.cut_point_of("integration").as_deref(),
        Some(cut.as_str()),
        "referencing the batch again replaced its cut point with its current tip, which reads as \
         `NothingToMerge` for ever and freezes every task already landed on it"
    );

    // …and the task that had already landed on the batch still closes when the batch reaches
    // trunk, which is the whole point of the record that was being overwritten.
    f.jkb()
        .args(["--global", "task", "base", "--forget", &group])
        .assert()
        .success();
    git(&f.repo, &["merge", "-q", "--ff-only", "integration"]);
    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&first),
        "done",
        "the batch reached trunk and the task landed on it stayed open"
    );
}

/// A batch that has already been landed on records **nothing**, rather than its tip.
///
/// The other half of the fill: filling a gap is safe, but only where a cut point can honestly be
/// measured. A batch whose first group has been fast-forwarded into it has moved and yet has no
/// commits of its own, so the only obtainable value is its tip — which is the frozen
/// `NothingToMerge` state arrived at through the fill arm instead of the supersede one. Nothing
/// recorded is reported and repairable by a later run; a tip is silent and permanent.
///
/// The gate is `base::advice`, the same question every message asks before naming a measuring
/// verb, so there is one answer to "may a cut point be measured on this branch right now".
#[test]
fn a_batch_already_landed_on_records_nothing_rather_than_its_tip() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "integration", "main"]);
    // A first group, landed by hand — no jkb command has referenced the batch, so it has no record.
    git(&f.repo, &["checkout", "-q", "-b", "grp-a", "integration"]);
    commit_in(&f.repo, "g.txt", "group work\n", "group work");
    git(&f.repo, &["checkout", "-q", "integration"]);
    git(&f.repo, &["merge", "-q", "--ff-only", "grp-a"]);
    git(&f.repo, &["checkout", "-q", "main"]);
    assert_eq!(
        f.cut_point_of("integration"),
        None,
        "setup: the batch has no record yet, so this is the fill arm"
    );

    let uid = f.add_task("second group");
    git(&f.repo, &["checkout", "-q", "-b", "grp-b", "integration"]);
    commit_in(&f.repo, "h.txt", "more group work\n", "more group work");
    git(&f.repo, &["checkout", "-q", "main"]);
    f.jkb()
        .args([
            "task",
            "start",
            &uid,
            "--branch",
            "grp-b",
            "--onto",
            "integration",
        ])
        .assert()
        .success();

    assert_eq!(
        f.cut_point_of("integration"),
        None,
        "the batch's tip was recorded as its cut point, which reads as `NothingToMerge` for ever"
    );
}

/// A landing whose target has **no record of its own** is held, not closed.
///
/// The event says the work is in `S`; deciding whether `S` reached trunk needs `S`'s cut point,
/// and with none the policy is the same as everywhere else — do not act. In the ordinary flows
/// this state is unreachable, because recording a land target ensures the target's own row; the
/// test builds it by recording a branch without one.
#[test]
fn a_landing_onto_a_batch_with_no_record_is_held() {
    let f = Fixture::new();
    let uid = f.add_task("landed onto an unknown batch");
    git(&f.repo, &["branch", "integration", "main"]);
    git(&f.repo, &["checkout", "-q", "-b", "grp", "integration"]);
    commit_in(&f.repo, "g.txt", "group work\n", "group work");
    git(&f.repo, &["checkout", "-q", "main"]);
    // Measured against **trunk**, which `land_target_for` deliberately does not record as a land
    // target (D34.3) — so `grp` gets a cut point and nothing ever ensures a row for `integration`.
    f.jkb()
        .args(["task", "start", &uid, "--branch", "grp", "--onto", "main"])
        .assert()
        .success();
    git(&f.repo, &["checkout", "-q", "integration"]);
    git(&f.repo, &["merge", "-q", "--ff-only", "grp"]);
    git(&f.repo, &["checkout", "-q", "main"]);
    git(&f.repo, &["merge", "-q", "--ff-only", "integration"]);
    f.jkb()
        .args(["task", "landed", "grp", "--onto", "integration"])
        .assert()
        .success();

    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "a landing was credited onto a branch with no recorded cut point"
    );
}

/// A branch re-pointed after a jkb landing is **not** credited with that landing.
///
/// Without this the event re-creates the exact staleness the record is keyed by name to avoid: the
/// row is keyed by branch name, a name outlives its branch, and a namesake would present its
/// predecessor's landing — closing a task with nothing on it through the *trusted* path. A land
/// does not move the branch ref, so `tip == landed_head` holds until something re-points it.
///
/// The re-pointed branch carries work trunk does not have, so the fallback inference answers
/// "unmerged" — which is what makes this test able to fail: if `landed_head` were ignored, the
/// event would credit the batch, which *has* reached trunk, and the task would close.
#[test]
fn a_branch_repointed_after_landing_is_not_credited_with_it() {
    let f = Fixture::new();
    let (uid, branch) = a_group_landing_on_an_integration_branch(&f);
    git(&f.repo, &["checkout", "-q", "integration"]);
    git(&f.repo, &["merge", "-q", "--ff-only", &branch]);
    git(&f.repo, &["checkout", "-q", "main"]);
    f.jkb()
        .args(["task", "landed", &branch, "--onto", "integration"])
        .assert()
        .success();
    // As in the test above: with a cut point of its own the group branch closes by ordinary
    // inference, and the sanity check below would then hold whether or not the event is consulted.
    f.jkb()
        .args(["--global", "task", "base", "--forget", &branch])
        .assert()
        .success();
    git(&f.repo, &["merge", "-q", "--ff-only", "integration"]);

    // Sanity: while the branch still points at what landed, the event is credited — and with no
    // cut point of its own, nothing else could have closed it.
    //
    // Asserted as **"would close"**, not as the uid appearing anywhere: a task with no cut point
    // is also printed, by name, in the `unknown` bucket, so a bare uid match passed with the
    // credited-landing arm deleted. The bucket is the assertion.
    f.jkb()
        .args(["task", "close-merged", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("would close {uid}")));

    // Now re-point it at unmerged work, as a recreated namesake would be.
    git(&f.repo, &["checkout", "-q", "-b", "elsewhere", "main"]);
    commit_in(&f.repo, "other.txt", "not landed\n", "other work");
    git(&f.repo, &["checkout", "-q", "main"]);
    git(&f.repo, &["branch", "-f", &branch, "elsewhere"]);

    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "a re-pointed branch inherited its predecessor's landing event and closed a task whose \
         current work is not in trunk"
    );
}

/// A branch whose work was merged away: **held** where the instance anchor cannot be verified,
/// correctly closed by content where it can.
///
/// This test pinned one direction — held — and the edit is deliberate. Read the retain-license
/// paragraph of `openspec/changes/jkb-branch-records/design.md` (B5) before changing it again.
///
/// The rule it pinned: "no commits of its own" is evidence a recorded cut point belongs to
/// whatever had the branch name before, so the record is discarded and re-measured to the tip,
/// after which nothing can close. That is also true of a branch whose commits were
/// fast-forwarded into its batch, and *refs alone cannot tell those apart* — so the missed close
/// was the accepted cost of never taking the other direction, which buries work (D34.4).
///
/// The checkout-local **ref journal** does tell them apart. A merged-away branch was never
/// deleted, so its creation entry still matches the anchor stored with the record, and its tip was
/// reached only by `commit`-class entries; a recycled name's log starts fresh, and every verb that
/// re-points a branch writes a `Reset`-class entry. So where the anchor verifies, the record is
/// **retained** and content decides — which closes this correctly. Where it cannot (reflogs off,
/// the log expired, a different checkout) nothing changed: discard, re-measure, hold.
///
/// Both arms are asserted, because the licence is only sound if its failure lands on the old
/// behaviour rather than on a new close.
#[test]
fn a_branch_whose_work_was_merged_away_closes_only_when_its_instance_is_verifiable() {
    for reflogs in [true, false] {
        let f = Fixture::new();
        if !reflogs {
            // No ref journal at all — the "cannot judge instance identity" state, and the one
            // every other failure mode degrades to.
            git(&f.repo, &["config", "core.logAllRefUpdates", "false"]);
            std::fs::remove_dir_all(f.repo.join(".git/logs")).ok();
        }
        git(&f.repo, &["branch", "batch", "main"]);
        git(&f.repo, &["checkout", "-q", "-b", "feature", "batch"]);
        commit_in(&f.repo, "w.txt", "work\n", "real work");
        git(&f.repo, &["checkout", "-q", "main"]);

        let uid = f.add_task("work that lands by fast-forward");
        f.jkb()
            .args([
                "task", "start", &uid, "--branch", "feature", "--onto", "batch",
            ])
            .assert()
            .success();

        // The batch fast-forwards onto the branch's own commits, then trunk takes the batch — so
        // the branch has no unique commit left anywhere.
        git(&f.repo, &["checkout", "-q", "batch"]);
        git(&f.repo, &["merge", "-q", "--ff-only", "feature"]);
        git(&f.repo, &["checkout", "-q", "main"]);
        git(&f.repo, &["merge", "-q", "--ff-only", "batch"]);

        // Anything that re-runs the writer now sees a branch that looks untouched.
        f.jkb()
            .args([
                "task", "start", &uid, "--branch", "feature", "--onto", "batch",
            ])
            .assert()
            .success();

        f.jkb().args(["task", "close-merged"]).assert().success();
        let expected = if reflogs { "done" } else { "in_progress" };
        assert_eq!(
            f.status_of(&uid),
            expected,
            "reflogs={reflogs}: with a verifiable instance anchor the merged-away branch closes \
             by content; without one the record is discarded and the task is held, which is the \
             accepted direction because the same git facts describe a recycled name"
        );
    }
}

/// A branch re-pointed with `branch -f` is **never** retained, even though its creation entry
/// still matches.
///
/// The anchor proves the ref journal belongs to this instance; it does not prove the branch has
/// not been moved. `branch -f` and `checkout -B` preserve the log and append a `Reset`-class
/// entry, so the licence requires *both* halves: a matching creation entry **and** nothing but
/// `commit`-class entries since. Without the second half a hand-repointed branch would keep a
/// record describing where it used to be.
#[test]
fn a_repointed_branch_does_not_keep_its_old_cut_point() {
    let f = Fixture::new();
    // A branch cut from trunk with work on it, and a recorded fork point.
    git(&f.repo, &["checkout", "-q", "-b", "feature"]);
    commit_in(&f.repo, "w.txt", "work\n", "real work");
    git(&f.repo, &["checkout", "-q", "main"]);
    commit_in(&f.repo, "t.txt", "trunk\n", "trunk moves on");
    let uid = f.add_task("repointed branch");
    f.jkb()
        .args([
            "task", "start", &uid, "--branch", "feature", "--onto", "main",
        ])
        .assert()
        .success();
    let fork = git(&f.repo, &["merge-base", "feature", "main"]);
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "feature: cut from {fork}"
        )));

    // Re-pointed by hand at trunk: it now has no commits of its own, and its recorded cut point
    // describes a branch shape that no longer exists.
    git(&f.repo, &["branch", "-f", "feature", "main"]);
    let tip = git(&f.repo, &["rev-parse", "feature"]);
    assert_ne!(tip, fork, "setup: the re-point must move the branch");
    f.jkb()
        .args(["task", "start", &uid, "--branch", "feature"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("feature: cut from {tip}")))
        .stdout(predicate::str::contains(format!("feature: cut from {fork}")).not());
}

/// The **no-writer window**: a branch deleted and recreated by hand, with no jkb command in
/// between, is held rather than closed.
///
/// This is the residual the supersede arm alone cannot reach — it fires when a *writer* runs, and
/// here `close-merged` is the first thing to look at the branch. The read-side anchor check closes
/// it: the recreated branch's log starts fresh, so its creation entry differs from the one stored
/// with the record, which is positive proof of recycling. It acts only toward hold, so an absent
/// or truncated log leaves the read exactly as it was.
#[test]
fn a_hand_recreated_branch_is_held_even_with_no_jkb_write_in_between() {
    let f = Fixture::new();
    // A branch with work, landed into trunk by a merge commit — so on content alone it reads as
    // merged, and only the record separates "landed" from "recreated empty".
    git(&f.repo, &["checkout", "-q", "-b", "feature"]);
    commit_in(&f.repo, "w.txt", "work\n", "real work");
    git(&f.repo, &["checkout", "-q", "main"]);
    let uid = f.add_task("recycled by hand");
    f.jkb()
        .args([
            "task", "start", &uid, "--branch", "feature", "--onto", "main",
        ])
        .assert()
        .success();

    // Deleted and recreated under the same name, entirely outside jkb.
    git(&f.repo, &["branch", "-D", "feature"]);
    commit_in(&f.repo, "t.txt", "trunk\n", "trunk moves on");
    git(&f.repo, &["branch", "feature", "main"]);

    // No jkb write in between: `close-merged` is the first thing to see the new branch.
    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "a task closed against the record of the branch that previously had this name — the new \
         branch has nothing on it"
    );
}

/// The read-side anchor check fires through **`review record`**, not only through `close-merged`.
///
/// The two readers share one implementation (`repo::landed_for_action`), and a mutation of
/// `base::stale_instance` kills the `close-merged` test — but "the seam is shared" is an argument,
/// not a test, and this is the reader whose failure mode is the *quieter* of the two: crediting a
/// recycled branch stamps `reviewed=<sha>` for work no review saw, which then opens the land gate.
///
/// The branch is deleted and recreated by hand with no jkb write in between, so the writer-side
/// supersede arm never runs.
#[test]
fn a_review_does_not_credit_a_branch_recreated_under_its_name() {
    let f = Fixture::new();
    let uid = f.add_task("recycled under review");
    git(&f.repo, &["branch", "stg", "main"]);
    git(&f.repo, &["checkout", "-q", "-b", "feature", "stg"]);
    commit_in(&f.repo, "w.txt", "work\n", "real work");
    git(&f.repo, &["checkout", "-q", "main"]);
    f.jkb()
        .args([
            "task", "start", &uid, "--branch", "feature", "--onto", "stg",
        ])
        .assert()
        .success();

    // The work lands on the staging branch, so a review of `stg` legitimately covers it.
    git(&f.repo, &["checkout", "-q", "stg"]);
    git(&f.repo, &["merge", "-q", "--ff-only", "feature"]);
    git(&f.repo, &["checkout", "-q", "main"]);

    // Sanity, and what makes the assertion below able to fail: as it stands the review credits it.
    f.add_finding("reviews/stg-a", "a finding");
    f.jkb()
        .args(["task", "review", "record", "--branch", "stg"])
        .args(["--findings", "reviews/stg-a"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&uid));
    assert_eq!(f.status_of(&uid), "needs_review", "setup: it should credit");

    // Now the branch is deleted and recreated under the same name, entirely outside jkb, and the
    // task is put back where a second review would find it.
    f.jkb()
        .args(["--global", "task", "set", &uid, "--status", "in_progress"])
        .assert()
        .success();
    git(&f.repo, &["branch", "-D", "feature"]);
    // Recreated at the staging branch's tip — **already-landed content**, which is the shape a
    // recycled name actually takes. That placement is what makes this test able to fail: the tip
    // now differs from the recorded cut point, so `is_merged`'s freshly-cut guard does NOT fire,
    // and `merge-tree` answers with the staging branch's own tree, i.e. Merged. The anchor is then
    // the only thing between this task and a `reviewed=` it never earned.
    //
    // An earlier version recreated the branch at `main`, where the tip happened to equal the
    // recorded cut point — so the freshly-cut guard held it and disabling the anchor killed
    // nothing. Only mutation was ever going to surface that.
    git(&f.repo, &["branch", "feature", "stg"]);
    assert_ne!(
        git(&f.repo, &["rev-parse", "feature"]),
        git(&f.repo, &["merge-base", "feature", "main"]),
        "setup: the namesake's tip must differ from the recorded cut point, or the freshly-cut \
         guard decides this instead of the anchor"
    );

    f.add_finding("reviews/stg-b", "another finding");
    f.jkb()
        .args(["task", "review", "record", "--branch", "stg"])
        .args(["--findings", "reviews/stg-b"])
        .assert()
        .success();
    assert_eq!(
        f.status_of(&uid),
        "in_progress",
        "a review credited a branch recreated under the reviewed name — `reviewed=` would then \
         open the land gate for work no review saw"
    );
}

/// `jkb doctor` reports reflog retention entries for branches nothing records any more.
///
/// The entries are written beside a record to keep the instance anchor from expiring, and removed
/// when the branch is forgotten. The residue is a branch recorded and never forgotten: inert
/// config, but config the user did not ask for, and this project's rule is that jkb does not leave
/// things in other people's repositories silently.
///
/// Both directions, because a report that names everything is as useless as one that names
/// nothing: a **recorded** branch's entry must not be listed.
#[test]
fn doctor_reports_retention_entries_for_branches_nothing_records() {
    let f = Fixture::new();
    let uid = f.add_task("recorded branch");
    git(&f.repo, &["branch", "live", "main"]);
    f.jkb()
        .args(["task", "start", &uid, "--branch", "live"])
        .assert()
        .success();
    // An entry for a branch no record claims — what `abandon --delete-branch` would have removed.
    git(
        &f.repo,
        &[
            "config",
            "--local",
            "gc.refs/heads/ghost.reflogExpire",
            "never",
        ],
    );

    f.jkb()
        .args(["doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no longer recorded"))
        // The remedy is spelled out: the user did not write this entry and should not have to
        // work out the key.
        .stdout(predicate::str::contains("gc.refs/heads/ghost.reflogExpire"))
        // BOTH keys. Retention writes `reflogExpire` and `reflogExpireUnreachable`, while the
        // scan matches only the first — so a remedy naming one key left the other in
        // `.git/config` and the next run reported "all recorded" over the top of it, guaranteeing
        // the residue this check exists to stop.
        .stdout(predicate::str::contains(
            "gc.refs/heads/ghost.reflogExpireUnreachable",
        ))
        // …and the recorded branch's own entry is not reported as residue.
        .stdout(predicate::str::contains("gc.refs/heads/live").not());

    // `--fix` removes it, through the same verb `base::forget` uses — and the entry really is
    // gone from the config, not merely unreported.
    git(
        &f.repo,
        &[
            "config",
            "--local",
            "gc.refs/heads/ghost.reflogExpireUnreachable",
            "never",
        ],
    );
    f.jkb()
        .args(["doctor", "--fix"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ghost — removed"));
    let left = std::process::Command::new("git")
        .current_dir(&f.repo)
        .args([
            "config",
            "--local",
            "--get-regexp",
            "^gc[.]refs/heads/ghost",
        ])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&left.stdout).trim().is_empty(),
        "`doctor --fix` left a retention entry behind: {}",
        String::from_utf8_lossy(&left.stdout)
    );
}

/// A bare `jkb undo` after `jkb task start` reverts **that** transaction, not an older one.
///
/// `task start` writes a `branch_records` row, logged as an `insert`. An insert into a table
/// `undo`'s allowlist did not name made `undo_last` skip the whole transaction — so this did not
/// fail, it quietly reverted the previous one and reported success, deleting the task itself. The
/// allowlist is derived from `changelog::Entity::insert_inverse` now; this is the end-to-end shape
/// of the same regression.
#[test]
fn undo_after_task_start_reverts_the_start_and_not_the_task_that_preceded_it() {
    let f = Fixture::new();
    let uid = f.add_task("undo after start");
    git(&f.repo, &["branch", "feature-u", "main"]);
    f.jkb()
        .args(["task", "start", &uid, "--branch", "feature-u"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("feature-u: cut from"));

    f.jkb().args(["undo"]).assert().success();

    // The start is undone: no branch, no record. The **task** is untouched — reverting it instead
    // is the failure, and it is silent.
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("feature-u").not());
}

/// The landing verb's refusal must not send you to measure a branch whose work has just landed.
///
/// This is the family rule at `base::MEASURE_VERB`: the refusal fires at the one moment a branch
/// is provably empty — the queue has just fast-forwarded the target onto its commits — so naming
/// `jkb task start … --onto` there records the branch **tip** as its cut point, which reads as
/// "nothing has happened here" for ever and freezes the task at `NothingToMerge`.
///
/// Both directions, and the fixture is built to distinguish them: the same refusal on a branch
/// that has genuinely never moved *does* name the verb, because there the tip provably is its fork
/// point. A test that only checked for absence would pass against a message that never mentions
/// the verb at all.
#[test]
fn the_landing_refusal_names_a_measuring_verb_only_where_measuring_is_safe() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "integration", "main"]);
    git(&f.repo, &["checkout", "-q", "-b", "grp", "integration"]);
    commit_in(&f.repo, "g.txt", "group work\n", "group work");
    git(&f.repo, &["checkout", "-q", "integration"]);
    git(&f.repo, &["merge", "-q", "--ff-only", "grp"]);
    git(&f.repo, &["checkout", "-q", "main"]);

    // A hand-run queue: the graft happened, and nothing ever recorded a cut point for `grp`.
    f.jkb()
        .args(["task", "landed", "grp", "--onto", "integration"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("has moved since it was cut"))
        .stderr(predicate::str::contains("jkb task start").not());

    // The other side of the same question: a branch that has never moved can be measured, so the
    // refusal says so.
    git(&f.repo, &["branch", "fresh", "main"]);
    f.jkb()
        .args(["task", "landed", "fresh", "--onto", "integration"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "jkb task start <uid> --branch fresh --onto <parent>",
        ));
}

/// Degradation is toward today's behaviour, never toward a judgement.
///
/// Every way the ref journal can be unavailable — never written, or truncated so its oldest
/// surviving entry is not a creation entry — makes both anchor checks decline. Declining means the
/// reader proceeds exactly as it did before the anchor existed, and the writer falls back to the
/// untouched-tip predicate: discard-and-hold. Neither can mint a close.
#[test]
fn an_unreadable_ref_journal_declines_rather_than_deciding() {
    for damage in ["absent", "truncated"] {
        let f = Fixture::new();
        git(&f.repo, &["checkout", "-q", "-b", "feature"]);
        commit_in(&f.repo, "w.txt", "work\n", "real work");
        git(&f.repo, &["checkout", "-q", "main"]);
        let uid = f.add_task("no readable journal");
        f.jkb()
            .args([
                "task", "start", &uid, "--branch", "feature", "--onto", "main",
            ])
            .assert()
            .success();
        let fork = git(&f.repo, &["merge-base", "feature", "main"]);

        let log = f.repo.join(".git/logs/refs/heads/feature");
        if damage == "absent" {
            std::fs::remove_file(&log).unwrap();
        } else {
            // Drop the creation entry, leaving a log whose oldest line is a `commit` entry —
            // which is exactly what expiry produces, and what announces its own truncation.
            let text = std::fs::read_to_string(&log).unwrap();
            let rest: Vec<&str> = text.lines().skip(1).collect();
            std::fs::write(&log, format!("{}\n", rest.join("\n"))).unwrap();
        }

        // The reader still acts on the record it has: declining is not refusing.
        f.jkb().args(["task", "close-merged"]).assert().success();
        assert_eq!(
            f.status_of(&uid),
            "in_progress",
            "{damage}: an unmerged branch must stay in flight"
        );
        f.jkb()
            .args(["--global", "task", "show", &uid])
            .assert()
            .success()
            .stdout(predicate::str::contains(format!(
                "feature: cut from {fork}"
            )));

        // And the branch really does land, so this is not asserting that nothing ever closes.
        git(
            &f.repo,
            &["merge", "-q", "--no-ff", "-m", "merge", "feature"],
        );
        f.jkb().args(["task", "close-merged"]).assert().success();
        assert_eq!(
            f.status_of(&uid),
            "done",
            "{damage}: an unreadable journal blocked a genuine close"
        );
    }
}

/// The reflog retention entry is written beside a record, and removed when the branch is
/// forgotten.
///
/// The instance anchor is only as durable as the reflog, so coverage is a condition this
/// establishes rather than assumes: `gc.refs/heads/<branch>.reflogExpire = never` holds the
/// creation entry through config-driven expiry. Exact-ref, so no branch naming scheme is needed.
#[test]
fn recording_a_branch_retains_its_ref_journal() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "feature", "main"]);
    // The control: same repository, same expiry, no record and therefore no retention entry. Its
    // creation entry must be gone afterwards, or the positive assertion below proves nothing —
    // `git reflog expire` leaves an empty log **file** behind either way, so asserting the file
    // exists passed with `retain_reflog` reduced to a no-op.
    git(&f.repo, &["branch", "unrecorded", "main"]);
    let uid = f.add_task("retained");
    f.jkb()
        .args(["task", "start", &uid, "--branch", "feature"])
        .assert()
        .success();
    let key = "gc.refs/heads/feature.reflogExpire";

    // The **entry** the anchor is read from, not the file it lives in.
    let creation_entry = |branch: &str| -> Option<String> {
        let log = std::fs::read_to_string(f.repo.join(format!(".git/logs/refs/heads/{branch}")))
            .unwrap_or_default();
        // A creation entry is the one whose `old` revision is all zeros — the same fact
        // `gitrepo::ref_journal` keys on, and the reason expiry cannot remove it silently.
        log.lines()
            .find(|l| {
                l.split(' ')
                    .next()
                    .is_some_and(|old| !old.is_empty() && old.chars().all(|c| c == '0'))
            })
            .map(str::to_owned)
    };
    let anchor = creation_entry("feature").expect("setup: the branch has a creation entry");
    assert!(
        creation_entry("unrecorded").is_some(),
        "setup: the control branch has a creation entry to lose"
    );

    // It survives a config-driven expiry of everything else.
    git(&f.repo, &["config", "gc.reflogExpire", "now"]);
    git(&f.repo, &["reflog", "expire", "--all"]);
    assert_eq!(
        creation_entry("unrecorded"),
        None,
        "setup: the expiry did not remove an unretained creation entry, so this run cannot show \
         that retention did anything"
    );
    assert_eq!(
        creation_entry("feature").as_deref(),
        Some(anchor.as_str()),
        "the retention entry did not hold the creation entry through `reflog expire --all`, so \
         the instance anchor is not durable"
    );

    // The mechanism, asserted after the property it is supposed to produce — so a `retain_reflog`
    // reduced to a no-op fails on the durability above rather than here, where the panic would say
    // only that a config key is missing. `--default` so an unset key exits 0 and this assertion is
    // what reports it: the helper asserts on git's exit status, so a bare `--get` would panic with
    // git's silence instead.
    assert_eq!(
        git(
            &f.repo,
            &["config", "--local", "--default", "", "--get", key]
        ),
        "never",
        "no retention entry was written beside the record"
    );

    // Deleting the branch takes the record and the entry with it.
    f.jkb()
        .args(["task", "abandon", &uid, "--force", "--delete-branch"])
        .assert()
        .success();
    assert!(
        git(
            &f.repo,
            &["config", "--local", "--default", "", "--get", key]
        )
        .is_empty(),
        "the retention entry outlived the branch it was for"
    );
}

/// A cut point already in the store that git cannot resolve is still ignored by the reader.
///
/// The writer will not produce this state — `rejected` refuses an inadmissible measurement and the
/// schema refuses a malformed value — so it only arises from a record made in another clone, or
/// one whose commit has since been garbage-collected. The reader-side guard is what covers those,
/// and removing it would close them wrongly, so it keeps its own test now that the writer no
/// longer produces the state.
///
/// The branch here is untouched, so nothing this run does could measure over the planted value:
/// `close-merged` never writes, which is exactly what makes this the reader's test.
#[test]
fn an_unresolvable_cut_point_already_in_the_store_is_ignored_by_the_reader() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "feature", "main"]);
    let uid = f.add_task("legacy bogus base");

    let legacy = jkb_core::Db::open(f.db.to_str().unwrap()).unwrap();
    let id = legacy
        .read({
            let uid = uid.clone();
            move |conn| jkb_core::item::id_for_uid(conn, &uid)
        })
        .unwrap()
        .unwrap();
    legacy
        .write_txn("t", move |conn, meta| {
            jkb_core::tag::apply(conn, meta, id, "repo", "proj")?;
            jkb_core::tag::apply(conn, meta, id, "branch", "feature")
        })
        .unwrap();
    drop(legacy);
    // A well-formed object id this repository does not have — `rev-parse` parses it happily, so
    // only a verifying lookup rejects it.
    f.plant_cut_point("feature", "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef");

    f.jkb().args(["task", "close-merged"]).assert().success();
    assert_eq!(
        f.status_of(&uid),
        "open",
        "an empty branch closed as merged: its cut point did not resolve, so the freshly-cut \
         guard was skipped instead of applied"
    );
}

/// Naming a branch as its own parent records nothing, rather than recording its tip.
///
/// `--onto feature` on branch `feature`, or the likelier slip `--onto origin/feature` on a pushed
/// branch, makes every merge-base come back as the tip — and a tip on a branch with work is the
/// one value that must never be stored, because `is_merged` then answers `NothingToMerge` forever.
///
/// This is the third route by which a tip reached the store (after a fallback, and after an
/// adopted legacy value beating the measurement), which is why the rule is enforced in one place
/// that every writer passes through rather than fixed a fourth time at the site.
#[test]
fn naming_a_branch_as_its_own_parent_records_nothing() {
    let f = Fixture::new();
    git(&f.repo, &["checkout", "-q", "-b", "feature"]);
    commit_in(&f.repo, "w.txt", "work\n", "real work");
    git(&f.repo, &["checkout", "-q", "main"]);
    let tip = git(&f.repo, &["rev-parse", "feature"]);

    let uid = f.add_task("self-referential parent");
    f.jkb()
        .args(["task", "start", &uid, "--branch", "feature"])
        .args(["--onto", "feature", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"base\":null"));
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("feature: cut from {tip}")).not());

    // Nothing recorded is repairable; a recorded tip is not. Naming the real parent measures it.
    let fork = git(&f.repo, &["merge-base", "feature", "main"]);
    f.jkb()
        .args([
            "task", "start", &uid, "--branch", "feature", "--onto", "main",
        ])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "feature: cut from {fork}"
        )));
}

/// The one repair verb takes **no commit id**, and forgetting is what it does.
///
/// `jkb task base <uid> <branch> <sha>` produced three findings across three review passes, all
/// the same shape: the sha nearest a user's hand is `git rev-parse <branch>` — the tip — and a cut
/// point equal to the tip reads as "nothing has happened here" forever, after which the task can
/// neither be credited by a review nor land, with no repair path. Each was fixed by rewording a
/// message. Forgetting always repairs, because the next `task start` measures again.
#[test]
fn the_repair_verb_forgets_rather_than_accepting_a_sha() {
    let f = Fixture::new();
    git(&f.repo, &["checkout", "-q", "-b", "feature"]);
    commit_in(&f.repo, "w.txt", "work\n", "work");
    git(&f.repo, &["checkout", "-q", "main"]);
    let tip = git(&f.repo, &["rev-parse", "feature"]);
    let fork = git(&f.repo, &["merge-base", "feature", "main"]);

    let uid = f.add_task("repairable");
    f.jkb()
        .args([
            "task", "start", &uid, "--branch", "feature", "--onto", "main",
        ])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "feature: cut from {fork}"
        )));

    // No positional sha is accepted at all — the argument form is gone, so no message can name it.
    f.jkb()
        .args(["--global", "task", "base", &uid, "feature", &tip])
        .assert()
        .failure();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "feature: cut from {fork}"
        )));

    // Forgetting drops it, and the next measurement puts it back.
    f.jkb()
        .args(["task", "base", "--forget", "feature"])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains("cut from").not());
    f.jkb()
        .args([
            "task", "start", &uid, "--branch", "feature", "--onto", "main",
        ])
        .assert()
        .success();
    f.jkb()
        .args(["--global", "task", "show", &uid])
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "feature: cut from {fork}"
        )));
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
        .stdout(predicate::str::contains("lands on integration"))
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

/// A `rev-list` git refused is not "this branch has done nothing".
///
/// "Untouched" is the single answer that makes the branch **tip** admissible as its cut point, and
/// a cut point equal to the tip reads as `NothingToMerge` forever — never creditable, never
/// landable, and never corrected, since `ensure_recorded` declines to overwrite. So a traversal
/// that could not run must record nothing and say why, exactly as every other failed measurement
/// here does. Same rule as `ahead_count`: a question that could not be asked must not be spelled
/// the same as an answer of no.
#[test]
fn a_ref_walk_git_refused_records_nothing_rather_than_the_tip() {
    let f = Fixture::new();
    git(&f.repo, &["checkout", "-q", "-b", "feat"]);
    commit_in(&f.repo, "f.txt", "work\n", "feature work");
    git(&f.repo, &["checkout", "-q", "main"]);
    let tip = git(&f.repo, &["rev-parse", "feat"]);
    // A ref pointing at an object this repository does not have — what an interrupted fetch or a
    // stale `packed-refs` leaves behind. It fails `rev-list --branches` and nothing else.
    std::fs::write(
        f.repo.join(".git/refs/heads/broken"),
        format!("{}1\n", "0".repeat(39)),
    )
    .unwrap();

    let uid = f.add_task("measured where git cannot walk the refs");
    let out = f
        .jkb()
        .args(["task", "start", &uid, "--branch", "feat"])
        .args(["--onto", "main", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "task start: {out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_ne!(
        v["base"],
        serde_json::json!(tip),
        "the tip of a branch full of work was recorded as its cut point, which freezes it: {v}"
    );
    assert_eq!(
        v["base"],
        serde_json::Value::Null,
        "something was recorded from a measurement git could not make: {v}"
    );
    assert_eq!(
        v["base_missing_because"],
        serde_json::json!("git-could-not-answer-here"),
        "the reason reported was not the one the writer had: {v}"
    );
}

/// `close-merged` reports each branch under the fault it actually has.
///
/// A task may legitimately record several branches, and folding them with an AND over "usable"
/// and an OR over "recorded" mislabelled every task whose branches differ: one branch with a good
/// cut point beside one with none routed the whole task to "its recorded cut point does not
/// resolve", which was true of neither half, and named a repair that would do nothing.
#[test]
fn close_merged_reports_each_branch_under_its_own_fault() {
    let f = Fixture::new();
    // alpha: untouched, so its cut point is measured and resolves.
    git(&f.repo, &["branch", "alpha", "main"]);
    // beta: has commits and is named with no parent, so nothing could be measured for it.
    git(&f.repo, &["checkout", "-q", "-b", "beta", "main"]);
    commit_in(&f.repo, "b.txt", "b\n", "beta work");
    git(&f.repo, &["checkout", "-q", "main"]);
    // gamma: same, and then given a cut point this repository cannot resolve.
    git(&f.repo, &["checkout", "-q", "-b", "gamma", "main"]);
    commit_in(&f.repo, "g.txt", "g\n", "gamma work");
    git(&f.repo, &["checkout", "-q", "main"]);

    let uid = f.add_task("branches that differ in fault");
    f.jkb()
        .args(["task", "start", &uid, "--branch", "alpha", "--onto", "main"])
        .assert()
        .success();
    for b in ["beta", "gamma"] {
        f.jkb()
            .args([
                "--global",
                "task",
                "tag",
                "add",
                &uid,
                &format!("branch={b}"),
            ])
            .assert()
            .success();
    }
    // Planted after the tagging, so the measurement (which records nothing for a branch with
    // commits and no parent) cannot overwrite it.
    f.plant_cut_point("gamma", &"c".repeat(40));

    let out = f
        .jkb()
        .args(["task", "close-merged", "--dry-run", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "close-merged: {out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let branches = |bucket: &str| {
        v[bucket]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["uid"] == serde_json::json!(uid))
            .map(|r| r["branch"].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        branches("undecidable"),
        vec!["beta".to_owned()],
        "the branch with no cut point at all was not reported as such: {v}"
    );
    assert_eq!(
        branches("unresolvable"),
        vec!["gamma".to_owned()],
        "the branch whose recorded cut point does not resolve was not reported as such: {v}"
    );
}

/// A landing onto the very branch a review is being recorded for **is** the answer.
///
/// Walking past it to ask "and is that branch contained in itself?" needs the target's own cut
/// point, which the review path has no reason to hold — a batch measured in a repository with no
/// discoverable trunk records none — and the reader then declined to credit work jkb had itself
/// just grafted onto that branch. Before the landing event existed this asked about the task's own
/// branch and answered `Merged`, so following the event must not make the answer worse.
#[test]
fn a_review_credits_a_landing_onto_the_branch_being_reviewed() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "integration", "main"]);
    git(&f.repo, &["checkout", "-q", "-b", "grp", "integration"]);
    commit_in(&f.repo, "g.txt", "group work\n", "group work");
    git(&f.repo, &["checkout", "-q", "main"]);

    let uid = f.add_task("landed onto the reviewed branch");
    f.jkb()
        .args(["task", "start", &uid, "--branch", "grp"])
        .args(["--onto", "integration"])
        .assert()
        .success();
    // The state the fix is about: the target has a row and a land target, and no cut point.
    f.jkb()
        .args(["task", "base", "--forget", "integration"])
        .assert()
        .success();

    git(&f.repo, &["checkout", "-q", "integration"]);
    git(&f.repo, &["merge", "-q", "--ff-only", "grp"]);
    git(&f.repo, &["checkout", "-q", "main"]);
    f.jkb()
        .args(["task", "landed", "grp", "--onto", "integration"])
        .assert()
        .success();

    f.add_finding("reviews/batch", "something to fix");
    f.jkb()
        .args(["task", "review", "record", "--branch", "integration"])
        .args(["--findings", "reviews/batch"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&uid));
    assert_eq!(
        f.status_of(&uid),
        "needs_review",
        "a review of the branch jkb landed this task's work onto did not credit it"
    );
}

/// The landing verb says when the event it just recorded can never decide anything.
///
/// `close-merged` follows a landing to its target and then asks whether *that* branch reached
/// trunk, which needs the target's own cut point; with none it holds the task and reports it as
/// still in flight, which is indistinguishable from the truth. The verb deliberately does not
/// measure one itself: a cut point is only provable while a branch is untouched, and a landing is
/// the moment the target stops being — the queue's first entry fast-forwards it onto commits its
/// source branch still holds, so `has_own_commits` truthfully says "nothing of its own" and the
/// tip, the one value that freezes a task for good, becomes admissible.
#[test]
fn the_landing_verb_reports_a_target_that_cannot_credit_it() {
    let f = Fixture::new();
    git(&f.repo, &["branch", "integration", "main"]);
    git(&f.repo, &["checkout", "-q", "-b", "grp", "integration"]);
    commit_in(&f.repo, "g.txt", "group work\n", "group work");
    git(&f.repo, &["checkout", "-q", "main"]);
    let uid = f.add_task("landed onto an unrecorded batch");
    // `--onto main` measures against trunk and deliberately records no land target (D34.3), so
    // nothing ever ensures a row for `integration`.
    f.jkb()
        .args(["task", "start", &uid, "--branch", "grp", "--onto", "main"])
        .assert()
        .success();
    git(&f.repo, &["checkout", "-q", "integration"]);
    git(&f.repo, &["merge", "-q", "--ff-only", "grp"]);
    git(&f.repo, &["checkout", "-q", "main"]);

    let out = f
        .jkb()
        .args(["task", "landed", "grp", "--onto", "integration", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "task landed: {out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["creditable"],
        serde_json::json!(false),
        "an event that can never be credited was reported as though it could: {v}"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no usable cut point"),
        "nothing said the landing will not close its tasks: {out:?}"
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
