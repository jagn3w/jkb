//! *Discuss* (D53.5) as a terminal spec: a Claude session in the container, in the design's repo,
//! started with the prompt `design.prompt` built — the same text `jkb design prompt discuss` prints.
//
// Pure, so the command it runs is pinned by a test. The prompt is never assembled here: the app
// only carries it (D53.5, "the prompt is CLI output, not app string-building").

import type { DesignPrompt } from "@jkb/core";

import type { TerminalRoots, TerminalSpec } from "../../../shared/terminal";

/**
 * The script the container's login shell runs: `claude` found on the login `PATH` (a `docker exec`
 * has the image's environment, not a login's), the session id and the prompt passed as positional
 * parameters — never spliced into the script, so nothing in the prompt is shell syntax.
 */
export const DISCUSS_SCRIPT = 'exec claude --session-id "$1" "$2"';

/** Where a design's repo is in the container: its directory under the repos mount. */
export function repoDir(roots: TerminalRoots, repo: string): string {
  const safe = /^[A-Za-z0-9._-]+$/.test(repo) && repo !== "." && repo !== "..";
  return safe ? `${roots.containerRepos.replace(/\/+$/, "")}/${repo}` : roots.containerRepos;
}

/**
 * The terminal a *Discuss* opens. `sessionUuid` is minted by the caller and handed to Claude as its
 * session id, so the terminal and the session are the same thing to every later lookup — and a
 * second *Discuss* of the same id shows this terminal instead of starting another.
 */
export function discussSpec(prompt: DesignPrompt, repo: string, roots: TerminalRoots, sessionUuid: string): TerminalSpec {
  const title = `Discuss · ${prompt.title}`;
  return {
    target: "container",
    cwd: repoDir(roots, repo),
    argv: ["/bin/bash", "-lc", DISCUSS_SCRIPT, "claude", sessionUuid, prompt.prompt],
    title: title.length > 200 ? `${title.slice(0, 199)}…` : title,
    sessionUuid,
  };
}
