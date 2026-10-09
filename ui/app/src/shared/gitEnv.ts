//! The environment variables that pick a repository for git or `gh`, which a spawned git or `gh`
//! must not inherit: a launching shell that exported one would point the command at another
//! repository's state. The same lists `crates/jkb-cli/src/gitrepo.rs` (`REPO_SELECTION_VARS`) and
//! `crates/jkb-cli/src/pr.rs` (`GH_SELECTION_VARS`) scrub, and why is in
//! docs/git-hooks-installer.md; `ui/app/test/workflows.test.mjs` holds these to those.

/** Which repository, and which parts of one, git is pointed at. */
export const GIT_SELECTION: readonly string[] = [
  "GIT_DIR",
  "GIT_WORK_TREE",
  "GIT_COMMON_DIR",
  "GIT_INDEX_FILE",
  "GIT_OBJECT_DIRECTORY",
  "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

/** `gh`'s own: `GH_REPO` names a repository outright, `GH_HOST` the GitHub instance. */
export const GH_SELECTION: readonly string[] = ["GH_REPO", "GH_HOST"];
