//! *Contribute to jkb* (D53.7): a template the operator edited, exported into the packaged-templates
//! file on a new branch, pushed, and offered as a pull request — run in the container, as every git
//! write is, from the jkb checkout under the repos mount.
//
// Pure, so the command is pinned by a test. The work happens in a fresh worktree off
// `origin/main`, never in the operator's checkout: a contribution must not pick up whatever is on
// the branch they happen to have checked out, nor leave their tree dirty. Everything that varies is
// a positional parameter, never spliced into the script.

import { isAgentName } from "@jkb/core";

import { GH_SELECTION, GIT_SELECTION } from "../../../shared/gitEnv";
import type { TerminalRoots, TerminalSpec } from "../../../shared/terminal";
import { repoDir } from "../design/launch";

/** The repo the packaged templates live in: jkb's own checkout under the repos mount. */
export const JKB_REPO = "jkb";

/** Where the packaged templates are, relative to the repository root (`agents::PACKAGED_FILE`). */
export const PACKAGED_FILE = "crates/jkb-core/src/workflow/agents.json";

/**
 * `$1` is the template's name. Branch `agent-template/<name>-<UTC stamp>` off a freshly fetched
 * `origin/main`, in a worktree beside the checkout's own under `.jkb/work`; export the template there
 * (`jkb workflow agent export`: the copy in effect, as the next packaged version — refused when the
 * copy is built on another version than `origin/main` packages); commit only that file; push; open
 * the pull request. A failure stops the script where it is, its message on the terminal.
 *
 * - **Nothing writes `.git/config`**, which the container binds read-only: the branch is made
 *   `--no-track` and pushed without `-u`, either of which would record an upstream there and fail.
 * - **No repository is inherited**: the variables that point git or `gh` at another repository are
 *   unset first (`GIT_SELECTION`, `GH_SELECTION`), as `jkb`'s own git and `gh` spawns scrub them.
 * - **Whatever happens, the worktree and the local branch go** (an `EXIT` trap), so a failed
 *   contribution leaves nothing in `.jkb/work` for the next click to pile onto. Once pushed, the
 *   branch lives on `origin`, and a failed `gh pr create` says how to open the pull request by hand.
 */
export const CONTRIBUTE_SCRIPT = [
  "set -euo pipefail",
  `unset ${[...GIT_SELECTION, ...GH_SELECTION].join(" ")}`,
  'name="$1"',
  'top="$PWD"',
  'branch="agent-template/$name-$(date -u +%Y%m%d%H%M%S)"',
  'wt="$top/.jkb/work/${branch//\\//-}"',
  "pushed=0",
  'cleanup() { s=$?; cd "$top"; if [ -d "$wt" ]; then git worktree remove --force "$wt" >/dev/null 2>&1 || true; fi; git branch -D "$branch" >/dev/null 2>&1 || true; if [ "$s" != 0 ] && [ "$pushed" = 1 ]; then echo "pushed $branch, but no pull request was opened: gh pr create --base main --head $branch" >&2; fi; exit "$s"; }',
  "trap cleanup EXIT",
  "git fetch origin main",
  'git worktree add --no-track -b "$branch" "$wt" origin/main',
  'cd "$wt"',
  'jkb workflow agent export "$name"',
  `git add -- ${PACKAGED_FILE}`,
  'git commit -m "workflow agents: contribute $name"',
  'git push origin "$branch"',
  "pushed=1",
  'gh pr create --base main --head "$branch" --title "workflow agents: contribute $name" --body "The operator\'s edited \\`$name\\` agent template, exported from the Code Factory\'s Workflows tab (docs/code-factory.md, D53.7)."',
  'echo "contributed $name on $branch"',
].join("; ");

/** The terminal *Contribute to jkb* opens, or `undefined` for a name jkb would refuse. */
export function contributeSpec(name: string, roots: TerminalRoots): TerminalSpec | undefined {
  if (!isAgentName(name)) return undefined;
  return {
    target: "container",
    cwd: repoDir(roots, JKB_REPO),
    argv: ["/bin/bash", "-lc", CONTRIBUTE_SCRIPT, "contribute", name],
    title: `Contribute · ${name}`,
  };
}
