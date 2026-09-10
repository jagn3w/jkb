//! Shared fixture helpers for jkb-cli's integration tests.
//!
//! Each `tests/*.rs` file is its own crate, so the only way `sessions.rs` and `cli.rs` can hold
//! ONE definition of the git isolation below is a module both include. They held two — or
//! rather one and a gap: `cli.rs` had none at all, and the crate-wide spawn guard could not see
//! that, because it keyed on the literal `Command::new(` while these fixtures build their
//! process with `Command::cargo_bin("jkb")`.

use std::process::Command;

/// Strip the caller's repository selection and configuration from a spawn.
///
/// Every `git` this process reaches — directly, or through a `jkb` that spawns its own — must
/// resolve the repository from its arguments and nothing else.
///
/// Neutralize every route by which the developer's shell reaches a `git` this fixture runs —
/// directly, or inside the `jkb` binary it spawns.
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
