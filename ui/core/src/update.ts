//! The installed copy and *Update from main* (D53.3), minus the window and the processes: where
//! the installed app and its clean clone live, what an update would take, and when running from a
//! checkout is refused.
//
// The app runs unsandboxed on the host and opens host terminals, so nothing it runs may come from
// a checkout an agent can write. It is built from a clone of `origin/main` that only this machine
// writes (`APP_SRC_IN_HOME`), by that clone's own `scripts/build-app.sh`, which records the commit
// it installed in `APP_STAMP_IN_HOME`. An update fetches `main` into the clone, shows the commits
// between that stamp and the fetched tip, and builds exactly the tip it showed.

/** Where the app's own state lives, under the account's home: the clone, the stamp, the log. */
export const APP_HOME_IN_HOME = ".local/share/jkb-app";

/** The host-side clean clone of `origin/main` the app is built from. */
export const APP_SRC_IN_HOME = `${APP_HOME_IN_HOME}/src`;

/** What `build-app.sh` writes after a successful install: `commit=<sha>`. */
export const APP_STAMP_IN_HOME = `${APP_HOME_IN_HOME}/installed`;

/** The output of the last update's build, kept for when it fails. */
export const APP_LOG_IN_HOME = `${APP_HOME_IN_HOME}/update.log`;

/** The builder, inside the clone. Run from the clone, never from a checkout. */
export const APP_BUILDER_IN_SRC = "scripts/build-app.sh";

/** The one ref an update takes. Never a branch an agent pushes, never a worktree. */
export const UPDATE_REF = "refs/remotes/origin/main";

/** The refspec that fetches `main` into `UPDATE_REF`, forced: `main` is what was landed. */
export const UPDATE_REFSPEC = `+refs/heads/main:${UPDATE_REF}`;

/** The opt-in for running the app from a checkout, as `run.sh` has `JKB_RUN_FROM_CHECKOUT`. */
export const FROM_CHECKOUT_VAR = "JKB_APP_FROM_CHECKOUT";

/**
 * Why this process may not run, or `undefined` when it may. A packaged app is the installed copy;
 * anything else is running from a checkout, which is a deliberate developer act
 * (`JKB_APP_FROM_CHECKOUT=1`) and never the default.
 */
export function checkoutRefusal(isPackaged: boolean, env: Readonly<Record<string, string | undefined>>): string | undefined {
  if (isPackaged || env[FROM_CHECKOUT_VAR] === "1") return undefined;
  return (
    `Code Factory is running from a checkout, which an agent can write, and it runs unsandboxed on this ` +
    `machine. Run the installed copy (scripts/setup.sh installs it), or set ${FROM_CHECKOUT_VAR}=1 to run ` +
    `this checkout deliberately.`
  );
}

/** A full commit id, as git prints with `%H`. */
export function isCommitId(value: unknown): value is string {
  return typeof value === "string" && /^[0-9a-f]{40}$/.test(value);
}

/** The installed commit from `build-app.sh`'s stamp, or `undefined` when there is none. */
export function parseInstalledStamp(text: string): string | undefined {
  for (const line of text.split("\n")) {
    const m = /^commit=(\S+)\s*$/.exec(line);
    if (m !== null && isCommitId(m[1])) return m[1];
  }
  return undefined;
}

export interface UpdateCommit {
  readonly id: string;
  readonly subject: string;
}

/** The `git log` format `parseCommitLog` reads: the id, a tab, the subject. */
export const COMMIT_LOG_FORMAT = "%H%x09%s";

/** `git log --format=COMMIT_LOG_FORMAT` parsed; a line that is not one is dropped. */
export function parseCommitLog(text: string): UpdateCommit[] {
  const out: UpdateCommit[] = [];
  for (const line of text.split("\n")) {
    const tab = line.indexOf("\t");
    if (tab < 0) continue;
    const id = line.slice(0, tab);
    if (isCommitId(id)) out.push({ id, subject: line.slice(tab + 1).trim() });
  }
  return out;
}

/** What a fetch found: the installed commit, `main`'s tip, and the commits between them. */
export interface UpdatePlan {
  /** The commit installed now, or `undefined` when no stamp says. */
  readonly installed: string | undefined;
  /** `origin/main`'s tip, as fetched: the commit the update builds and nothing else. */
  readonly target: string;
  /** `installed..target`, newest first. Empty when nothing is installed to count from. */
  readonly commits: readonly UpdateCommit[];
  /** Whether `installed` is not an ancestor of `target` (main was rewritten, or the stamp is foreign). */
  readonly diverged: boolean;
}

/** How many commits the confirmation lists before it says how many more there are. */
export const LISTED_COMMITS = 20;

/** The confirmation's words for `plan`, or `undefined` when there is nothing to take. */
export function updateSummary(plan: UpdatePlan): { message: string; detail: string } | undefined {
  const short = (id: string) => id.slice(0, 12);
  if (plan.installed === plan.target) return undefined;
  const lines: string[] = [];
  if (plan.installed === undefined) {
    lines.push(`No installed commit is recorded; this builds main at ${short(plan.target)}.`);
  } else {
    lines.push(`${short(plan.installed)}..${short(plan.target)}`);
    if (plan.diverged) lines.push(`The installed commit is not an ancestor of main: main was rewritten since it was built.`);
    lines.push("");
    for (const c of plan.commits.slice(0, LISTED_COMMITS)) lines.push(`${c.id.slice(0, 8)}  ${c.subject}`);
    const more = plan.commits.length - LISTED_COMMITS;
    if (more > 0) lines.push(`… and ${more} more`);
  }
  lines.push("");
  lines.push("The app is rebuilt from that commit with the frozen lockfile, swapped in, and relaunched. Open terminals close.");
  const n = plan.commits.length;
  const message =
    plan.installed === undefined ? "Build Code Factory from main?" : `Take ${n} commit${n === 1 ? "" : "s"} from main?`;
  return { message, detail: lines.join("\n") };
}
