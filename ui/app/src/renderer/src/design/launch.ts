//! Every Claude session the Design tab starts, and every one it resumes (D53.6), as terminal specs.
//
// The app pre-mints the session uuid and starts `claude --session-id <uuid>`; the launch records
// the design's prompt (`jkb design prompt record`) BEFORE Claude starts, from the directory Claude
// then runs in — so the link is written before the session exists, and the cwd a resume needs is
// the one the session really has. One script does it for every launch (*Discuss*, *Play*, a task's
// *Play*, *New prompt*), so no caller can start Claude without the record; a record refused (an
// unreachable daemon, say) stops the script before Claude starts, its message left on the terminal.
//
// Pure, so the commands are pinned by tests. The prompts are never assembled here (D53.5: "the
// prompt is CLI output, not app string-building"), and everything that varies is a positional
// parameter, never spliced into a script.

import type { DesignPromptRecord, Launch } from "@jkb/core";

import type { TerminalRoots, TerminalSpec } from "../../../shared/terminal";

/**
 * Record the prompt, then become Claude. Positional parameters: `$1` design, `$2` session uuid,
 * `$3` launch, `$4` subject (empty for the design itself), `$5` title, `$6` the prompt. The record
 * takes the cwd it runs in (no `--cwd`): where `claude` is about to start, and so where
 * `claude --resume` must. `claude` and `jkb` are found on the login `PATH` (a `docker exec` has the
 * image's environment, not a login's).
 */
const RECORD_THEN_CLAUDE =
  'jkb design prompt record "$1" --session "$2" --launch "$3" --subject "$4" --title "$5" >/dev/null; exec claude --session-id "$2" "$6"';

/** A launch in the design's repo: record, then Claude. */
export const LAUNCH_SCRIPT = `set -euo pipefail; ${RECORD_THEN_CLAUDE}`;

/**
 * *Play* on a task: `jkb task work` opens (or resumes) the task's own worktree and claims it, and the
 * session is recorded and started there. `jq` reads the worktree from the JSON answer (it is in the
 * image), skipping any line that is not JSON — `task work` prints a note before its answer when it
 * cancels a pending removal. A refusal, or an answer naming no worktree, stops the script before
 * anything is recorded or Claude starts (`pipefail`: `jkb`'s own failure, not only `jq`'s). `$4` is
 * the task.
 */
export const PLAY_TASK_SCRIPT = `set -euo pipefail; dir=$(jkb --json task work "$4" | jq -Rer 'fromjson? | objects | .worktree | strings'); cd "$dir"; ${RECORD_THEN_CLAUDE}`;

/**
 * Resume a recorded session where it runs: `$1` is the session uuid, `$2` the directory it ran in.
 * The terminal starts in a directory that always exists and the script moves into `$2`, so a
 * directory that is gone — a task's worktree, removed when the task landed — is said plainly and
 * ends the resume before Claude starts, instead of failing inside `docker exec -w`. It is not
 * resumed anywhere else: Claude finds a session by the directory it ran in (D53.6).
 */
export const RESUME_SCRIPT =
  'cd -- "$2" 2>/dev/null || { case $2 in /*) d=$2 ;; *) d=${PWD%/}/${2#./} ;; esac; ' +
  'printf "jkb: %s no longer exists (a task removes its worktree when it lands), so this session cannot be resumed.\\n" "$d" >&2; exit 1; }; ' +
  'exec claude --resume "$1"';

/** Where a design's repo is in the container: its directory under the repos mount. */
export function repoDir(roots: TerminalRoots, repo: string): string {
  const safe = /^[A-Za-z0-9._-]+$/.test(repo) && repo !== "." && repo !== "..";
  return safe ? `${roots.containerRepos.replace(/\/+$/, "")}/${repo}` : roots.containerRepos;
}

/** `<label> · <title>`, cut to the terminal's title limit. */
export function titled(label: string, title: string): string {
  const t = `${label} · ${title}`;
  return t.length > 200 ? `${t.slice(0, 199)}…` : t;
}

/** What a launch starts: a session on `design`, its prompt, and what it is recorded as. */
export interface LaunchAsk {
  /** The design the session works. */
  readonly design: string;
  readonly launch: Launch;
  /** The plan or task it is started on; the task for a task's *Play*. */
  readonly subject?: string;
  /**
   * `<label> · <title>` is the tab's title and the record's, as the Prompts pane lists it: what was
   * done (`Discuss`, `Play`, `New`) and what it was done to.
   */
  readonly label: string;
  readonly title: string;
  /** The prompt `design.prompt` built. */
  readonly prompt: string;
}

/**
 * The terminal a launch opens: in the container, in the design's repo, recording the session and
 * starting Claude with `prompt`. A task's *Play* moves into its worktree first. `sessionUuid` is
 * minted by the caller, so the terminal, the record and the session are the same thing to every
 * later lookup — and a second open of the same id shows this terminal instead of starting another.
 */
export function launchSpec(ask: LaunchAsk, repo: string, roots: TerminalRoots, sessionUuid: string): TerminalSpec {
  const title = titled(ask.label, ask.title);
  const script = ask.launch === "task" ? PLAY_TASK_SCRIPT : LAUNCH_SCRIPT;
  return {
    target: "container",
    cwd: repoDir(roots, repo),
    argv: ["/bin/bash", "-lc", script, "claude", ask.design, sessionUuid, ask.launch, ask.subject ?? "", title, ask.prompt],
    title,
    sessionUuid,
  };
}

function under(path: string, root: string): boolean {
  const r = root.replace(/\/+$/, "");
  return path === r || path.startsWith(`${r}/`);
}

/**
 * The terminal that resumes a recorded prompt: `claude --resume <uuid>` in the cwd it was recorded
 * with. Sessions start in the container, so it resumes there; a session that was moved to the host
 * by the terminal's toggle recorded a host path, which is carried back through the repos mount.
 */
export function resumeSpec(prompt: DesignPromptRecord, roots: TerminalRoots): TerminalSpec {
  return sessionResumeSpec({ session: prompt.session, cwd: prompt.cwd, title: titled("Resume", prompt.title) }, roots);
}

/**
 * `claude --resume <session>` in the container, in `cwd` — carried back through the repos mount when
 * it is a host path — titled `title`. What every resume runs: a recorded prompt's (above), a session
 * the Sessions tab lists, and a terminal re-attached after a rebuild (D53.9). The terminal starts
 * where nothing can be missing and `RESUME_SCRIPT` moves into `cwd`, refusing plainly when it is
 * gone.
 */
export function sessionResumeSpec(
  ask: { readonly session: string; readonly cwd: string; readonly title: string },
  roots: TerminalRoots,
): TerminalSpec {
  const ctr = roots.containerRepos.replace(/\/+$/, "");
  const host = roots.hostRepos.replace(/\/+$/, "");
  const hostOnly = !under(ask.cwd, ctr) && under(ask.cwd, host);
  const cwd = hostOnly ? ctr + ask.cwd.slice(host.length) : ask.cwd;
  // Under the repos mount the terminal starts at its root — which exists on both sides, so the
  // target toggle still carries the resume across — and the script moves into the directory,
  // relative to it. Elsewhere it starts at `/` and moves to the absolute path.
  const [start, dir] = under(cwd, ctr) ? [roots.containerRepos, `./${cwd.slice(ctr.length + 1)}`] : ["/", cwd];
  return {
    target: "container",
    cwd: start,
    argv: ["/bin/bash", "-lc", RESUME_SCRIPT, "claude", ask.session, dir],
    title: ask.title,
    sessionUuid: ask.session,
  };
}

/**
 * The directory a resume terminal moves into — `sessionResumeSpec`'s inverse — or `undefined` for a
 * spec that is no resume. A resume's own `cwd` is where it *starts* (the repos mount's root), never
 * the session's directory, so anything that needs where the session runs asks this.
 */
export function resumedDir(spec: TerminalSpec): string | undefined {
  const [, , script, , , dir] = spec.argv;
  if (script !== RESUME_SCRIPT || dir === undefined) return undefined;
  if (dir.startsWith("/")) return dir;
  const rel = dir.replace(/^\.\/?/, "").replace(/\/+$/, "");
  const start = spec.cwd.replace(/\/+$/, "");
  return rel === "" ? spec.cwd : `${start}/${rel}`;
}
