//! Shared fixture helpers for jkb-cli's integration tests.
//!
//! Each `tests/*.rs` file is its own crate, so the only way `sessions.rs` and `cli.rs` can hold
//! ONE definition of the git isolation below is a module both include. They held two — or
//! rather one and a gap: `cli.rs` had none at all, and the crate-wide spawn guard could not see
//! that, because it keyed on the literal `Command::new(` while these fixtures build their
//! process with `Command::cargo_bin("jkb")`.

use std::process::Command;

/// Neutralize every route by which the developer's shell reaches a `git` this fixture runs —
/// directly, or inside the `jkb` binary it spawns. Such a `git` must resolve its repository
/// from its arguments and nothing else.
///
/// ONE function, because two spawn sites each remembering the list is how one of them came to
/// remember only half of it. It is deliberately WIDER than `gitrepo::scrub_repo_selection`,
/// which strips repository selection and leaves configuration injection alone on purpose (this
/// project's dev container carries its `safe.directory` grants there). Production must not
/// discard a grant it needs; a fixture must not inherit configuration it did not choose. Two
/// rules, not drift.
pub fn isolate_git_env(cmd: &mut Command) {
    // Selection: these outrank `-C`. Measured — with `GIT_DIR`/`GIT_WORK_TREE` exported, the
    // bare-dotfiles shell recipe, `git -C <tmpdir> init` re-inits the OTHER repository and
    // creates nothing here, and the `add`/`commit` that follow land a commit in it. That is
    // `./scripts/check.sh` — the gate `jkb task land` and the merge queue trust — writing to a
    // repository the developer merely happens to have configured.
    cmd.env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        // Configuration: the env-injected form OUTRANKS the files neutralized below, so
        // pointing those at /dev/null is not isolation on its own. `GIT_CONFIG_COUNT` gates
        // every `GIT_CONFIG_KEY_<n>`/`VALUE_<n>` pair, so dropping it disables them all
        // without naming an unbounded set. An exported `commit.gpgsign` or `core.hooksPath`
        // would otherwise redden the gate over a fact about somebody's shell.
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t");
}

/// The variables [`isolate_git_env`] must drop, named here so a test can assert them without
/// reading the function it is checking.
pub const MUST_DROP: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
];

/// ...and the settings it must APPLY. Dropping the injected forms is only half the isolation:
/// the files stay in play until they are pointed somewhere empty, and until this list existed
/// nothing observed that half at all — measured, deleting the two `GIT_CONFIG_*` lines from
/// `isolate_git_env` left all 137 tests in the two integration crates green, the three written
/// to pin this very function included, because they checked only the removals.
///
/// `src/gitrepo.rs`'s `FIXTURE_CONFIG` is the same list for the library's own fixtures. It is
/// deliberately a SECOND list rather than a shared one: `FIXTURE_CONFIG` is `#[cfg(test)]`, and
/// an integration test is a separate crate compiled with `cfg(test)` OFF, so it cannot see it.
/// What keeps them honest is that each is asserted against the function beside it.
pub const MUST_SET: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_AUTHOR_NAME", "t"),
    ("GIT_AUTHOR_EMAIL", "t@t"),
    ("GIT_COMMITTER_NAME", "t"),
    ("GIT_COMMITTER_EMAIL", "t@t"),
];

/// Assert `cmd` carries the whole isolation — both halves of it.
pub fn assert_isolated(what: &str, cmd: &Command) {
    let envs: Vec<(String, Option<String>)> = cmd
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    for want in MUST_DROP {
        assert!(
            envs.iter().any(|(k, v)| k == want && v.is_none()),
            "{what}: {want} is not removed; envs: {envs:?}"
        );
    }
    for (key, want) in MUST_SET {
        assert!(
            envs.iter()
                .any(|(k, v)| k == key && v.as_deref() == Some(*want)),
            "{what}: {key} is not set to {want}; the developer's global git configuration \
             reaches this fixture. envs: {envs:?}"
        );
    }
}
