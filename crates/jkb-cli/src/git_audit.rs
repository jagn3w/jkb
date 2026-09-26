//! The reap service's scan for repository config that could make the host run code (design D52.11,
//! layer 3).
//!
//! Layer 1 (read-only binds over `.git/config` and `.git/hooks`, from `.container/run.sh`) is a speed
//! bump — `.git/` is writable around it; layer 2 ([`crate::gitrepo::audit_repo_config`]) refuses to
//! run jkb's own git anywhere a program is named or the git directory is redirected. Neither covers **your** git — a terminal,
//! VS Code — in a repository cloned inside the container after it started, or in a session worktree,
//! whose `.git` file can be rewritten to point anywhere. That cannot be prevented from here, so it is
//! detected: every reap pass audits each repository under `~/repos` and its session worktrees, and a
//! finding raises one sticky notification, withdrawn when the last finding is gone.
//!
//! The residual, stated: between a plant and the next pass, git run by hand in that repository can run
//! what it names.

use std::path::{Path, PathBuf};

use anyhow::Result;
use jkb_core::Db;
use serde_json::json;

/// The notification's id and queue key: one notification for every finding, replaced as they change.
const NOTIFY_ID: &str = "jkb-git-audit";

/// The most directories one pass audits — far past any real `~/repos`, and a bound on a tree someone
/// filled with directories to make the scan slow.
const MAX_DIRS: usize = 2000;

/// Every repository directly under `root`, and each one's `.jkb/work/*` session worktrees.
fn candidates(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for repo in entries.flatten().map(|e| e.path()) {
        if !repo.join(".git").exists() {
            continue;
        }
        out.push(repo.clone());
        if let Ok(work) = std::fs::read_dir(repo.join(".jkb/work")) {
            out.extend(
                work.flatten()
                    .map(|e| e.path())
                    .filter(|w| w.join(".git").exists()),
            );
        }
        if out.len() >= MAX_DIRS {
            break;
        }
    }
    out.truncate(MAX_DIRS);
    out.sort();
    out
}

/// Audit every candidate under `root`; each finding is the directory and why.
#[must_use]
pub(crate) fn scan(root: &Path) -> Vec<(PathBuf, String)> {
    candidates(root)
        .into_iter()
        .filter_map(|dir| {
            // jkb's own refusal first; then what only YOUR git would reach — the submodules, which
            // jkb's git never enters and so never refuses.
            let why = if let Err(e) = crate::gitrepo::check_repo_config(&dir) {
                format!("{e:#}")
            } else {
                let found = crate::gitrepo::module_findings(&dir);
                if found.is_empty() {
                    return None;
                }
                format!("its submodules: {}", found.join("; "))
            };
            Some((dir, why))
        })
        .collect()
}

/// A pass's findings, as one comparable string — the service says a thing once, and again only when
/// it changes.
#[must_use]
pub(crate) fn summary(findings: &[(PathBuf, String)]) -> String {
    findings
        .iter()
        .map(|(d, _)| d.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Post the notification for `findings`, or withdraw it when there are none.
///
/// # Errors
/// A database or queue failure (the topic is created by `setup.sh`).
pub(crate) fn notify(db: &Db, findings: &[(PathBuf, String)]) -> Result<()> {
    use jkb_core::notify::{KIND_POST, KIND_WITHDRAW, POST_TTL_MS, TITLE, TOPIC};
    let (kind, payload, ttl) = if findings.is_empty() {
        (
            KIND_WITHDRAW,
            json!({ "id": NOTIFY_ID, "session": NOTIFY_ID }),
            None,
        )
    } else {
        let body = format!(
            "A repository's git setup could make git you run there execute a program (a key in its own \
             config, a hook in a submodule's git directory, or a git directory redirected elsewhere): \
             {}. The reap log names the key or file; fix it on the host.",
            summary(findings)
        );
        (
            KIND_POST,
            json!({
                "id": NOTIFY_ID,
                "session": NOTIFY_ID,
                "title": TITLE,
                "subtitle": "jkb: planted git setup",
                "body": body.chars().take(jkb_core::notify::MAX_BODY_CHARS).collect::<String>(),
            }),
            Some(POST_TTL_MS),
        )
    };
    let now = jkb_core::mq::now_ms();
    db.write_txn("reap", move |conn, meta| {
        jkb_core::mq::send(
            conn,
            meta,
            TOPIC,
            &jkb_core::mq::Draft {
                key: NOTIFY_ID.to_owned(),
                kind: kind.to_owned(),
                payload,
                ttl_ms: ttl,
                producer: "reap".to_owned(),
            },
            now,
        )
        .map_err(|e| jkb_core::Error::Types(jkb_types::Error::Validation(e.to_string())))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::scan;
    use crate::gitrepo::fixture_env::isolate_git_env;

    /// The one git spawn in these tests, isolated from the caller's repository and config.
    fn fixture_git(dir: &std::path::Path, args: &[&str]) -> std::process::Command {
        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C").arg(dir).args(args);
        isolate_git_env(&mut cmd);
        cmd
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        assert!(
            fixture_git(dir, args).status().unwrap().success(),
            "git {args:?}"
        );
    }

    #[test]
    fn the_audit_fixture_does_not_reach_another_repository() {
        crate::gitrepo::fixture_env::assert_isolated(
            "git_audit fixture",
            &fixture_git(std::path::Path::new("/somewhere"), &["status"]),
        );
    }

    #[test]
    fn a_planted_repository_or_session_worktree_is_found_and_a_clean_one_is_not() {
        let t = tempfile::tempdir().unwrap();
        let clean = t.path().join("clean");
        let planted = t.path().join("planted");
        for r in [&clean, &planted] {
            std::fs::create_dir_all(r).unwrap();
            git(r, &["init", "-q", "-b", "main"]);
        }
        git(&planted, &["config", "core.sshCommand", "/evil"]);
        // A session worktree whose `.git` file points at a git directory with its own config.
        let rogue = t.path().join("rogue-gitdir");
        std::fs::create_dir_all(&rogue).unwrap();
        git(&rogue, &["init", "-q", "--bare"]);
        git(&rogue, &["config", "core.fsmonitor", "/evil"]);
        let sess = clean.join(".jkb/work/sess");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(sess.join(".git"), format!("gitdir: {}\n", rogue.display())).unwrap();
        std::fs::create_dir_all(t.path().join("not-a-repo")).unwrap();

        let found: Vec<_> = scan(t.path()).into_iter().map(|(d, _)| d).collect();
        assert_eq!(found, vec![sess, planted]);
    }
}
