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
    // Configuration: the env-injected form OUTRANKS the files neutralized below, so pointing
    // those at /dev/null is not isolation on its own. `GIT_CONFIG_COUNT` gates every
    // `GIT_CONFIG_KEY_<n>`/`VALUE_<n>` pair, so dropping it disables them all without naming an
    // unbounded set. An exported `commit.gpgsign` or `core.hooksPath` would otherwise redden the
    // gate over a fact about somebody's shell.
    //
    // ITERATED, not restated. This function used to spell the same eleven variables that
    // `MUST_DROP`/`MUST_SET` spell, with a doc comment on those constants saying they existed so
    // a test could check this function "without reading the function it is checking" — which is
    // what made them a MIRROR rather than a source. Measured by the round-24 reviewer: adding
    // `.env_remove("GIT_OBJECT_DIRECTORY")` here and leaving `MUST_DROP` alone left all four
    // isolation guards green, including the cross-crate one written to close exactly that edit,
    // because `assert_isolated` is a superset check and the cross-crate test compares the two
    // CONSTANTS. Two comments agreeing is not evidence about two environments.
    //
    // The library side had this right already — `isolate_fixture_config` iterates
    // `FIXTURE_CONFIG` — which is precisely what hid the asymmetry.
    for key in MUST_DROP {
        cmd.env_remove(key);
    }
    for (key, value) in MUST_SET {
        cmd.env(key, value);
    }
}

/// The variables [`isolate_git_env`] must drop — and the list it iterates to drop them, so the
/// function cannot come to mean something the constant does not say. Asserting a fixture against
/// this list is then evidence about the environment that fixture builds, not about a second copy
/// of the list.
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
/// What keeps them honest is that each is ITERATED by the function beside it — `isolate_git_env`
/// here, `isolate_fixture_config` there — and that
/// `the_two_fixture_isolation_lists_describe_the_same_environment` compares the two at their
/// source. "Asserted against the function beside it" was the earlier claim and it was too weak on
/// this side: the assertion was a superset check against a hand-kept copy, so a variable added to
/// the function and not to the list passed everything.
pub const MUST_SET: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_AUTHOR_NAME", "t"),
    ("GIT_AUTHOR_EMAIL", "t@t"),
    ("GIT_COMMITTER_NAME", "t"),
    ("GIT_COMMITTER_EMAIL", "t@t"),
];

/// Assert `cmd` carries the whole isolation — both halves of it.
/// The oracle for [`isolate_git_env`], written down rather than computed.
///
/// LITERALS, and never `MUST_DROP`/`MUST_SET`, which the function iterates. An assertion that read
/// the same lists would shrink with them — measured in round 25 on the sibling rule in
/// `gitrepo.rs`: deleting one name from the production list left 260 tests passing while every
/// spawn inherited the variable. Production iterates a list; a test's expected value is written
/// down; the two are never the same source.
const EXPECT_DROPPED: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
];

const EXPECT_SET: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_AUTHOR_NAME", "t"),
    ("GIT_AUTHOR_EMAIL", "t@t"),
    ("GIT_COMMITTER_NAME", "t"),
    ("GIT_COMMITTER_EMAIL", "t@t"),
];

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
    // EQUALITY on the removed set. A superset check cannot see a list that GREW, and a fixture
    // that quietly removes more than it says is how a variable a test depends on disappears.
    let mut removed: Vec<String> = envs
        .iter()
        .filter(|(_, v)| v.is_none())
        .map(|(k, _)| k.clone())
        .collect();
    let mut want: Vec<String> = EXPECT_DROPPED.iter().map(|s| (*s).to_owned()).collect();
    removed.sort();
    want.sort();
    assert_eq!(
        removed, want,
        "{what}: the removed set must be exactly the variables a fixture must not inherit. Do \
         not reconcile this by editing MUST_DROP — that is the edit measured to reopen the hole."
    );
    // ...and a SUPERSET on the set ones, deliberately: `Fixture::jkb` legitimately adds its own
    // variables (HOSTNAME and friends), so equality here would fail on something harmless.
    for (key, want) in EXPECT_SET {
        assert!(
            envs.iter()
                .any(|(k, v)| k == key && v.as_deref() == Some(*want)),
            "{what}: {key} is not set to {want}; the developer's global git configuration \
             reaches this fixture. envs: {envs:?}"
        );
    }
}
