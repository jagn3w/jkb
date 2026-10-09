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

/**
 * The lock one install holds at a time (scripts/lib.sh's `app_lock`, taken the same way): a directory
 * holding `pid` and `token`. Whoever starts an install takes it; the builder it runs recognises it by
 * the token in `APP_LOCK_TOKEN_VAR` instead of taking it again.
 */
export const APP_LOCK_IN_HOME = `${APP_HOME_IN_HOME}/lock`;

/** The variable that hands the lock's token to the builder (lib.sh reads the same name). */
export const APP_LOCK_TOKEN_VAR = "JKB_APP_LOCK_TOKEN";

/**
 * The commit a build is of, written by `build-app.sh` into the app's `out/` before packaging, so the
 * running app knows what it is: the stamp says what was last installed, which is not the same thing
 * once a copy has been swapped in under a running one.
 */
export const BUILT_COMMIT_IN_OUT = "commit";

/**
 * Where `build-app.sh` installs the app by default: `~/Applications/Code Factory.app` on macOS,
 * `<app-home>/app` elsewhere. scripts/lib.sh's `app_default_dest` says the same, and a test holds
 * the two together.
 */
export function installedAppDir(platform: string, home: string): string {
  return platform === "darwin" ? `${home}/Applications/Code Factory.app` : `${home}/${APP_HOME_IN_HOME}/app`;
}

/** The executable of the app installed at `installedAppDir` (lib.sh's `app_executable`). */
export function installedExecutable(platform: string, home: string): string {
  const dir = installedAppDir(platform, home);
  return platform === "darwin" ? `${dir}/Contents/MacOS/Code Factory` : `${dir}/code-factory`;
}

/** The builder, inside the clone. Run from the clone, never from a checkout. */
export const APP_BUILDER_IN_SRC = "scripts/build-app.sh";

/** The one ref an update takes. Never a branch an agent pushes, never a worktree. */
export const UPDATE_REF = "refs/remotes/origin/main";

/**
 * The refspec that fetches `main` into `UPDATE_REF`, forced: `main` is what was landed. Fetched with
 * `GIT_TERMINAL_PROMPT=0` on both sides (lib.sh's `app_clone_refresh`, the app's `machineRunner`), so
 * a credential prompt fails the fetch rather than hanging it.
 */
export const UPDATE_REFSPEC = `+refs/heads/main:${UPDATE_REF}`;

/** The opt-in for running the app from a checkout, as `run.sh` has `JKB_RUN_FROM_CHECKOUT`. */
export const FROM_CHECKOUT_VAR = "JKB_APP_FROM_CHECKOUT";

/**
 * Why this process may not run, or `undefined` when it may. Only the installed copy runs by default:
 * a packaged app whose executable IS `installedExecutable` (main decides that, resolving links). Any
 * other — a checkout's `out/`, or a package electron-builder left in a checkout's `dist/` — is code an
 * agent can write, and runs only as a deliberate developer act (`JKB_APP_FROM_CHECKOUT=1`).
 */
export function checkoutRefusal(
  isInstalledCopy: boolean,
  env: Readonly<Record<string, string | undefined>>,
  installedAt = "the installed copy",
): string | undefined {
  if (isInstalledCopy || env[FROM_CHECKOUT_VAR] === "1") return undefined;
  return (
    `Code Factory is not running from its installed copy (${installedAt}) but from somewhere an agent ` +
    `may be able to write, such as a checkout, and it runs unsandboxed on this machine. Run the installed ` +
    `copy (scripts/setup.sh installs it), or set ${FROM_CHECKOUT_VAR}=1 to run this copy deliberately.`
  );
}

/** A full commit id, as git prints with `%H`. */
export function isCommitId(value: unknown): value is string {
  return typeof value === "string" && /^[0-9a-f]{40}$/.test(value);
}

/** The installed commit from `build-app.sh`'s stamp (or the app's own `out/commit`), or `undefined`. */
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
  /**
   * The commit running now (built into the app), else the stamp's; `undefined` when neither says.
   * The running copy's own commit wins: it is what the user has, whatever was swapped in since.
   */
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
