//! Shared fixture helpers for jkb-cli's integration tests.
//!
//! Each `tests/*.rs` file is its own crate, so the only way `sessions.rs` and `cli.rs` can hold
//! ONE definition of the git isolation below is a module both include. They held two — or
//! rather one and a gap: `cli.rs` had none at all, and the crate-wide spawn guard could not see
//! that, because it keyed on the literal `Command::new(` while these fixtures build their
//! process with `Command::cargo_bin("jkb")`.
//!
//! THREE COMPILATIONS NOW, not two. `src/gitrepo.rs` pulls this file in with
//! `#[cfg(test)] #[path = "../tests/common/mod.rs"]`, so the library's own fixtures
//! (`gitrepo::tests::fixture_git`, `archive::tests::fixture_git`) use these functions too. That
//! replaced a second copy of the list in `gitrepo.rs` — `FIXTURE_CONFIG` and
//! `isolate_fixture_config` — plus a test that parsed both out of their source files and compared
//! them. One source text is not a thing to keep in agreement.
//!
//! (An earlier version of this paragraph asked that the module stay `std`-only "because it is
//! compiled by a crate that has no dev-dependencies in scope". That is not true: `jkb-cli`'s
//! `[dev-dependencies]` are available to the bin target compiled with `cfg(test)`, which is this
//! compilation — `src/gitrepo.rs`'s own tests call `tempfile::tempdir()`. The claim is deleted
//! rather than softened, because a false constraint is what produced the second list in the first
//! place: a maintainer wanting `tempfile` here would have believed the boundary forbade it and
//! written another copy somewhere else.)

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
    // The library side had this right already — its own applier iterated its own list — which is
    // precisely what hid the asymmetry. SUPERSEDED in the half that named the fix: that list and
    // applier are gone, and this file is compiled into the library's test build instead, so there
    // is no second copy for either side to be right or wrong about.
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
    // Repository SELECTION — these outrank `-C`.
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    // Repository COMPONENTS. The list named only the three selectors, and the class it was
    // supposed to cover is "every variable git reads to locate any PART of a repository" — an
    // enumeration nothing bounded. Measured on an unmodified tree: with
    // `GIT_INDEX_FILE=<victim>/.git/index` exported, `./scripts/test.sh -p jkb-cli --bin jkb
    // gitrepo::tests` leaves the victim's index listing base.txt/mergecommit.txt/rebase.txt/
    // squash.txt instead of its own file, and `git status` there fails outright with
    // `fatal: unable to read <sha>`. The `GIT_OBJECT_DIRECTORY` variant leaks loose objects into
    // the victim's store. Both `assert_isolated` guards stayed green throughout, because they
    // compare against `EXPECT_DROPPED`, which spelled the same five names.
    //
    // FIXTURE SIDE ONLY. `scrub_repo_selection` deliberately stays at the three that select a
    // repository: production must not discard a component a caller legitimately handed it (git
    // exports `GIT_INDEX_FILE` to hook processes), while a fixture must not inherit anything it
    // did not choose. Two rules, not drift — the same split that keeps `GIT_CONFIG_COUNT` here
    // and not there.
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    // Configuration injection.
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
];

/// ...and the settings it must APPLY. Dropping the injected forms is only half the isolation:
/// the files stay in play until they are pointed somewhere empty, and until this list existed
/// nothing observed that half at all — measured, deleting the two `GIT_CONFIG_*` lines from
/// `isolate_git_env` left all 137 tests in the two integration crates green, the three written
/// to pin this very function included, because they checked only the removals.
///
/// SUPERSEDED, and kept because the reasoning was confident and wrong twice running. This
/// paragraph used to argue that `src/gitrepo.rs` should hold a SECOND copy of the list, on the
/// ground that an integration test is a separate crate compiled with `cfg(test)` off and cannot
/// see a `#[cfg(test)]` const — and that a test comparing the two at their source kept them
/// honest. Round 24 showed the two copies could agree while a function drifted from both; round
/// 26's fix removed the copy altogether by compiling THIS file into the library's test build
/// (`#[path]`), which the crate boundary never actually prevented. The parity test went with it:
/// it compared two pieces of text, and its failure message read as an instruction to sync them,
/// which is the edit round 25 measured as reopening the scrub hole.
///
/// What keeps this honest now is [`EXPECT_DROPPED`]/[`EXPECT_SET`] — written down, not computed —
/// and equality rather than a superset.
pub const MUST_SET: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_SYSTEM", "/dev/null"),
    ("GIT_AUTHOR_NAME", "t"),
    ("GIT_AUTHOR_EMAIL", "t@t"),
    ("GIT_COMMITTER_NAME", "t"),
    ("GIT_COMMITTER_EMAIL", "t@t"),
];

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
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
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
