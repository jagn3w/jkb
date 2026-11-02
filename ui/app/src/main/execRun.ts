//! Running an approved run request (the Executions design), in main: the renderer names a request
//! id, and everything else is here — read the request, recompute its tree pin with the host's own
//! jkb (which runs the hooks-off git audit first), move it to running with `exec.begin`, build the
//! terminal spec, issue it to the window for ONE open, and stream what the terminal shows to the
//! request's topic and a log until it ends.
//
// Plain Node (no Electron), so it is tested against a stand-in daemon and a stand-in `jkb exec pin`.
// Everything outside it is injected for the same reason.
//
// What the operator approves is exactly what runs. So the renderer sends only an id: main reads the
// argv, directory and target from the record, and a window cannot turn an approval into a different
// program. The pin is recomputed by the HOST jkb at an absolute path (`HOST_JKB_IN_HOME`, the binary
// scripts/setup.sh installs and the daemon service runs) — never one found on a PATH, and never the
// container's. `exec.begin`'s compare-and-set decides the rest: a pin that differs refuses the
// request (`tree_changed`), an expiry that came due expires it, and a request already begun —
// from this window, another, or another runner — is not begun again. The per-id gate here only
// stops this app asking twice.
//
// Accepted gaps, stated by the design (decided with the operator, 2026-10-16): the run happens in
// place, so files git ignores (`target/`, `node_modules/`) are outside the pin, and a file the
// container changes between the pin check and the moment the run reads it is not caught. No private
// host copy of the tree is made.

import { execFileSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { accessSync, closeSync, constants, fstatSync, lstatSync, mkdirSync, openSync, readFileSync, readlinkSync, readSync, statSync, writeSync } from "node:fs";
import { uptime } from "node:os";
import { basename, dirname, join } from "node:path";

import {
  EXEC_MAX_CHUNK_BYTES,
  EXEC_MAX_TAIL_BYTES,
  EXEC_MAX_TIMEOUT_MS,
  decodeExecBegun,
  decodeExecRequest,
  decodeExecRequests,
  escapeArg,
  execId,
  parseBaseOutput,
  parseDiffOutput,
  parseRunnerName,
  previewVerdict,
  runnerName,
  samePin,
  TREE_CHANGED,
  type ApiError,
  type BaseAnswer,
  type DiffAnswer,
  type ExecPin,
  type ExecView,
  type OpRequest,
  type OpResponse,
  type Outcome,
} from "@jkb/core";

import type { ExecPreview, ExecStarted, ProgramFile } from "../shared/bridge";
import { containerPathOf, hostPathOf, parseSpec, type TerminalResult, type TerminalRoots, type TerminalSpec } from "../shared/terminal";
import type { RunFile } from "./update";
import { dockerSearchPath, PATH_KEEP_IN_HOME, type OnceHandle, type OnceRun } from "./terminals";

/** The host jkb main trusts for the pin, under the ACCOUNT's home: what scripts/setup.sh installs. */
export const HOST_JKB_IN_HOME = ".cargo/bin/jkb";

/** Where a run's full output goes, under the account's home: `exec-<n>.log`. */
export const EXEC_LOG_DIR_IN_HOME = ".jkb/logs/exec";

/** How long the issued terminal may wait to be opened before the run is ended as interrupted. */
export const OPEN_DEADLINE_MS = 30_000;

/** How long a timed-out host program's group gets after SIGTERM before SIGKILL. */
export const KILL_GRACE_MS = 10_000;

/** How much output waits for an unreachable daemon before the stream goes to the log only. */
export const STREAM_BUFFER_BYTES = 4 * 1024 * 1024;

/** How long an `exec.begin` whose answer was lost is waited on before the click is answered. */
export const UNKNOWN_BEGIN_WAIT_MS = 60_000;

/** How long `jkb exec pin` may take: a walk and a blake3 of the work tree. */
export const PIN_TIMEOUT_MS = 2 * 60_000;

/** How long `jkb exec base` may take: one `git ls-remote` over the network. */
export const BASE_TIMEOUT_MS = 30_000;

/** The most patch `jkb exec diff` prints for the queue (`jkb_cli::exec_cli::DIFF_MAX_BYTES`). */
export const DIFF_MAX_BYTES = 2 * 1024 * 1024;

/** Why Run waits when no diff could be shown for a request in a repository. */
export function noDiffProblem(diff: DiffAnswer | undefined): string {
  const why =
    diff === undefined
      ? "this app cannot compute a diff"
      : diff.state === "error" || diff.state === "refused"
        ? diff.message
        : "the directory is no longer in a repository";
  return `No diff could be shown, so Run waits until one can: ${why}`;
}

/** How many previews run at once (a preview pins twice, asks the remote and diffs a whole tree). */
export const PREVIEWS_AT_ONCE = 2;

/** A counting semaphore: `acquire` waits, first come first served, while `limit` are held. */
export class Semaphore {
  private held = 0;
  private readonly waiting: (() => void)[] = [];

  constructor(private readonly limit: number) {}

  acquire(): Promise<void> {
    if (this.held < this.limit) {
      this.held++;
      return Promise.resolve();
    }
    return new Promise((resolve) => this.waiting.push(resolve));
  }

  release(): void {
    const next = this.waiting.shift();
    if (next !== undefined) next();
    else this.held--;
  }
}

/** What `jkb exec pin --json` answered. */
export type PinAnswer =
  | { readonly kind: "pinned"; readonly pin: ExecPin | null; readonly dir?: string }
  | { readonly kind: "refused"; readonly reason: string; readonly message: string }
  | { readonly kind: "error"; readonly message: string };

/**
 * `jkb exec pin --json`'s output: one object, `{dir, pin, …}` when it pinned (exit 0), or `{dir,
 * refused: {reason, message}}` when it could not (exit 1). Anything else — no object, an exit with no
 * refusal — is an error the runner reports without refusing the request.
 */
export function parsePinOutput(code: number | null, stdout: string, stderr: string): PinAnswer {
  const line = stdout.trim().split("\n").pop() ?? "";
  let parsed: unknown;
  try {
    parsed = JSON.parse(line);
  } catch {
    parsed = undefined;
  }
  const o = parsed as { dir?: unknown; pin?: unknown; refused?: { reason?: unknown; message?: unknown } } | undefined;
  if (typeof o === "object" && o !== null) {
    const r = o.refused;
    if (typeof r === "object" && r !== null && typeof r.reason === "string") {
      return { kind: "refused", reason: r.reason, message: typeof r.message === "string" ? r.message : "" };
    }
    const p = o.pin as { head?: unknown; changes?: unknown } | null | undefined;
    const dir = typeof o.dir === "string" ? { dir: o.dir } : {};
    if (code === 0 && p === null) return { kind: "pinned", pin: null, ...dir };
    if (code === 0 && typeof p === "object" && p !== null && typeof p.head === "string" && typeof p.changes === "string") {
      return { kind: "pinned", pin: { head: p.head, changes: p.changes }, ...dir };
    }
  }
  const why = stderr.trim().split("\n").slice(-6).join("\n");
  return { kind: "error", message: `jkb exec pin ${code === null ? "did not finish" : `failed (exit ${code})`}${why ? `: ${why}` : ""}` };
}

/**
 * What every host run starts through, so the directory it runs in is the one that was pinned, and
 * not whatever a path names by the time the terminal opens. The container writes under the repos
 * mount: between the pin and the spawn it can replace the directory, or a component of its path,
 * with a link anywhere on the host, and a spawn's `cwd` would follow it. So `$1` (the directory) is
 * entered with `cd -P`, and `$2` — its inode, taken on the host when it was resolved and pinned — must
 * be the inode of the directory entered, or nothing runs: one line, `launcherRefusal($3)`, and exit
 * `LAUNCHER_REFUSED`. `$3` is a nonce main makes per run, so a program's own output cannot pass for
 * the launcher's refusal. Only then is the program exec'd, so its PID and process group are the
 * launcher's.
 *
 * The program it is handed never starts with `-` (`launchProgram`): `exec` in macOS's `/bin/sh`
 * (bash 3.2) reads `-a NAME` as an option and runs the word after it from PATH instead. Every tool
 * it runs is named absolutely (`/bin/ls`, as RUN_SH_PATH's rule wants: never the app's PATH);
 * everything else is a builtin. The inode is read with `ls -di`, which every POSIX `ls` has; main
 * also compares the device before the spawn.
 */
export const HOST_LAUNCHER = [
  'd=$1 want=$2 nonce=$3; shift 3',
  'refuse() { printf \'jkb: refusing to run [%s]: %s\\n\' "$nonce" "$1" >&2; exit 125; }',
  'cd -P -- "$d" 2>/dev/null || refuse "cannot enter $d"',
  'i=$(/bin/ls -di .) || refuse "cannot read the inode of $d"',
  'i=${i#"${i%%[! ]*}"}; i=${i%% *}',
  '[ "$i" = "$want" ] || refuse "$d is not the directory that was pinned"',
  'exec "$@"',
].join("\n");

/** The launcher's exit status when it refuses. */
export const LAUNCHER_REFUSED = 125;

/** How the launcher's refusal line starts, for the run's nonce. */
export function launcherRefusal(nonce: string): string {
  return `jkb: refusing to run [${nonce}]:`;
}

/**
 * The program as the launcher execs it: an absolute path as it is; a relative one as `./<path>`, so
 * it is found from the directory the launcher verified and never starts with `-`.
 */
export function launchProgram(file: string): string {
  return file.startsWith("/") ? file : `./${file}`;
}

/** A directory's identity on the host: device and inode, as decimal strings. */
export interface DirIdentity {
  readonly dev: string;
  readonly ino: string;
}

function sameIdentity(a: DirIdentity | undefined, b: DirIdentity | undefined): boolean {
  return a !== undefined && b !== undefined && a.dev === b.dev && a.ino === b.ino;
}

/** The `exec.refuse` reason a pin refusal becomes: an unpinnable tree is a pin the host cannot reproduce. */
export function refuseReasonFor(pinReason: string): string {
  return pinReason === "dir_gone" || pinReason === "audit_refused" ? pinReason : "tree_changed";
}


/** What the runner reaches outside itself. */
export interface ExecRunnerDeps {
  /** One op of `jkb serve`'s set, as the operator (main's client). */
  op(request: OpRequest): Promise<Outcome<OpResponse>>;
  /** `jkb exec pin --dir <hostDir> --json` by the host jkb. */
  pin(hostDir: string): Promise<PinAnswer>;
  /**
   * `jkb exec base --dir <hostDir> --json` by the host jkb: the base verified on the host (`git
   * ls-remote` on the audited remote URL, else the clean clone). Absent: every preview says the base
   * is unverified.
   */
  base?(hostDir: string): Promise<BaseAnswer>;
  /**
   * `jkb exec diff --dir <hostDir> --base <sha> [--program <path>] --json` by the host jkb. Absent:
   * no diff is shown.
   */
  diff?(hostDir: string, base: string, program?: string): Promise<DiffAnswer>;
  /** The program file at `path`, read without following a link (`readProgramFile`). */
  readProgram?(path: string): ProgramFile;
  readonly roots: TerminalRoots;
  isDirectory(path: string): boolean;
  /**
   * `path` with every link and `..` resolved, or `undefined` when it does not exist — the OS's own
   * answer (`realpathSync.native`), so its spelling is the one `jkb exec pin` and `pwd -P` give.
   */
  realpath(path: string): string | undefined;
  /** What `path` names now, links followed: its device and inode; `undefined` when it does not exist. */
  identity(path: string): DirIdentity | undefined;
  /** A bare program name, found by run.sh's search (path-keep, then its fixed PATH); or `undefined`. */
  findHostProgram(name: string): string | undefined;
  /** The docker client, as a container terminal finds it, or `undefined`. */
  docker(): string | undefined;
  issueOnce(owner: number, spec: TerminalSpec, run: OnceRun): () => boolean;
  processAlive(pid: number): boolean;
  /** This process, and the boot it runs in: its runner name, and how a stale one is recognised. */
  readonly pid: number;
  readonly boot: string;
  /** The account's home: the log directory is under it. */
  readonly home: string;
  log(message: string): void;
  /** Overrides for the tests. */
  readonly timing?: {
    readonly openDeadlineMs?: number;
    readonly killGraceMs?: number;
    readonly retryMs?: number;
    readonly maxRetryMs?: number;
    readonly bufferBytes?: number;
    readonly unknownWaitMs?: number;
  };
}

function transient(e: ApiError): boolean {
  return e.code === "unavailable" || e.code === "busy";
}

const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

/** What the run sequence worked out before `exec.begin`: everything Run needs, and what preview shows. */
interface Prepared {
  readonly request: ExecView;
  readonly hostDir: string;
  /** `hostDir`'s identity when it was resolved, which the pin and the launch are held to. */
  readonly identity: DirIdentity;
  /** The host program as the operator is shown it: an absolute path (a host run only). */
  readonly program?: string;
  /** The launcher's refusal nonce for this run (a host run only). */
  readonly nonce?: string;
  readonly containerDir?: string;
  readonly spec: TerminalSpec;
  readonly pin: PinAnswer;
}

/** A preparation that stopped: the request is refused for `refuse`, or nothing is refused and `error` is said. */
type Stopped = { readonly refuse?: string; readonly error: string; readonly request?: ExecView; readonly partial?: Partial<Prepared> };

export class ExecRunner {
  /** Requests this app has begun, or is between the click and `exec.begin` for. */
  private readonly taken = new Set<string>();
  /** The previews running now; the rest wait ([`PREVIEWS_AT_ONCE`]). */
  private readonly previews = new Semaphore(PREVIEWS_AT_ONCE);
  readonly runner: string;

  constructor(private readonly deps: ExecRunnerDeps) {
    this.runner = runnerName(deps.pid, deps.boot);
  }

  private get timing(): Required<NonNullable<ExecRunnerDeps["timing"]>> {
    const t = this.deps.timing ?? {};
    return {
      openDeadlineMs: t.openDeadlineMs ?? OPEN_DEADLINE_MS,
      killGraceMs: t.killGraceMs ?? KILL_GRACE_MS,
      retryMs: t.retryMs ?? 250,
      maxRetryMs: t.maxRetryMs ?? 5_000,
      bufferBytes: t.bufferBytes ?? STREAM_BUFFER_BYTES,
      unknownWaitMs: t.unknownWaitMs ?? UNKNOWN_BEGIN_WAIT_MS,
    };
  }

  private async status(id: string): Promise<Outcome<ExecView>> {
    const r = await this.deps.op({ op: "exec.status", id });
    return r.ok ? decodeExecRequest(r.value) : r;
  }

  /** The request, its directories, its spec and its pin now — or why Run cannot start. */
  private async prepare(id: string): Promise<{ ok: true; value: Prepared } | { ok: false; stopped: Stopped }> {
    const stop = (s: Stopped): { ok: false; stopped: Stopped } => ({ ok: false, stopped: s });
    const read = await this.status(id);
    if (!read.ok) return stop({ error: `could not read ${id}: ${read.error.message}` });
    const request = read.value;
    if (request.state !== "pending") return stop({ error: `${id} is ${request.state}, not pending`, request });

    // The directory is RESOLVED before anything is decided about it. `hostPathOf` maps by path text,
    // and the container writes under the repos mount: a `..` in the request, or a link it planted
    // there, would otherwise carry a host run anywhere on the host while the preview showed a path
    // under the mount. A container-side directory must resolve under the host's real repos root;
    // the resolved path is what is shown, pinned and spawned.
    const { roots } = this.deps;
    const spelled = request.dir_side === "host" ? hostPathOf(request.dir, roots) ?? request.dir : hostPathOf(request.dir, roots);
    const hostDir = spelled === undefined ? undefined : this.deps.realpath(spelled);
    if (hostDir === undefined || !this.deps.isDirectory(hostDir)) {
      return stop({ refuse: "dir_gone", error: `${request.dir} is not a directory the host can reach`, request });
    }
    const identity = this.deps.identity(hostDir);
    if (identity === undefined) return stop({ refuse: "dir_gone", error: `${hostDir} is gone`, request });
    const reposReal = this.deps.realpath(roots.hostReposReal) ?? roots.hostReposReal;
    if (request.dir_side === "container" && hostDir !== reposReal && !hostDir.startsWith(`${reposReal}/`)) {
      return stop({ refuse: "dir_gone", error: `${request.dir} resolves to ${hostDir}, outside the repos mount the container shares`, request });
    }
    let containerDir: string | undefined;
    if (request.target === "container") {
      containerDir = containerPathOf(hostDir, roots);
      if (containerDir === undefined) {
        return stop({ refuse: "dir_gone", error: `${hostDir} is not under the repos mount, so the container cannot reach it`, request });
      }
      if (this.deps.docker() === undefined) return stop({ error: "docker not found (looked where run.sh looks)", request, partial: { hostDir, containerDir } });
    }

    const [program, ...args] = request.argv;
    let argv: string[];
    let shown: string | undefined;
    let nonce: string | undefined;
    if (request.target === "host") {
      // A relative program is exec'd by the launcher from the directory it verified; it is SHOWN
      // joined to that directory. A bare name is found where run.sh looks.
      const relative = !program!.startsWith("/") && program!.includes("/");
      const file = program!.startsWith("/") ? program! : relative ? program! : this.deps.findHostProgram(program!);
      if (file === undefined) {
        return stop({ error: `${escapeArg(program!)} is not found where run.sh looks for programs; ask for it by path`, request, partial: { hostDir, identity } });
      }
      shown = relative ? join(hostDir, file) : file;
      nonce = randomUUID();
      argv = ["/bin/sh", "-c", HOST_LAUNCHER, "jkb-exec", hostDir, identity.ino, nonce, launchProgram(file), ...args];
    } else {
      argv = [...request.argv];
    }
    const spec = parseSpec({
      target: request.target,
      cwd: request.target === "host" ? hostDir : containerDir,
      argv,
      title: `${id}: ${escapeArg(basename(program!)).slice(0, 120)}`,
      sessionUuid: randomUUID(),
    });
    if (!spec.ok) return stop({ error: `${id} cannot be run as a terminal: ${spec.error}`, request, partial: { hostDir, identity } });

    const pin = await this.deps.pin(hostDir);
    const prepared: Prepared = {
      request,
      hostDir,
      identity,
      ...(shown === undefined ? {} : { program: shown }),
      ...(nonce === undefined ? {} : { nonce }),
      ...(containerDir === undefined ? {} : { containerDir }),
      spec: spec.value,
      pin,
    };
    if (pin.kind === "refused") {
      return stop({ refuse: refuseReasonFor(pin.reason), error: `the pin could not be recomputed (${pin.reason}): ${pin.message}`, request, partial: prepared });
    }
    if (pin.kind === "error") return stop({ error: pin.message, request, partial: prepared });
    // The pin's own canonical directory must be the one just resolved, and it must still be: a link
    // retargeted in between would pin one tree and run in another. Compared by identity, not by
    // spelling, so a case-insensitive file system's spelling is no false "moved".
    const now = this.deps.identity(hostDir);
    if (!sameIdentity(now, identity) || (pin.dir !== undefined && !sameIdentity(this.deps.identity(pin.dir), identity))) {
      return stop({ refuse: "tree_changed", error: `the directory moved while it was pinned (${hostDir}, then ${pin.dir})`, request, partial: prepared });
    }
    return { ok: true, value: prepared };
  }

  /**
   * What Run would do now, for the queue to show before it is clicked. Changes nothing.
   *
   * Pin, diff, pin: `prepare` takes the first pin, the diff carries the pin of exactly the tree it
   * showed, and a last pin is taken after it. Unless all three are the request's, the tree moved and
   * the verdict says Run will refuse — here, in main, so a window cannot say otherwise. The diff is
   * against the base the host verified, or, failing that, the request's own `HEAD` label, with the
   * base marked unverified.
   */
  async preview(rawId: unknown): Promise<TerminalResult<ExecPreview>> {
    const id = execId(rawId);
    if (id === undefined) return { ok: false, error: `not a run request id: ${JSON.stringify(rawId)}` };
    // At most `PREVIEWS_AT_ONCE` previews run together, the rest wait their turn: each spawns host
    // jkb and git over a whole tree, and how many requests are pending is up to the sessions.
    await this.previews.acquire();
    try {
      return await this.previewNow(id);
    } finally {
      this.previews.release();
    }
  }

  private async previewNow(id: string): Promise<TerminalResult<ExecPreview>> {
    const shown = await this.previewOnce(id);
    if (!shown.ok) return shown;
    const v = shown.value;
    if (v.request.state !== "pending" || v.hostDir === undefined || v.pin.state === "refused" || v.pin.state === "error") return shown;
    const pin1 = v.pin.state === "match" ? (v.request.pin ?? null) : v.pin.now;
    const extra = await this.reviewTree(v.request, v.hostDir, v.program);
    const pin2 = await this.deps.pin(v.hostDir);
    const pinAfter = pin2.kind === "pinned" ? pin2.pin : undefined;
    const diffPin = extra.diff?.state === "shown" ? extra.diff.pin : undefined;
    let verdict = previewVerdict(v.request.pin, [pin1, diffPin, pinAfter]);
    // The diff saw a file change under it, or the last pin could not be taken: the tree moved.
    if ((extra.diff?.state === "refused" && extra.diff.reason === "tree_changed") || pin2.kind === "refused") {
      verdict = { state: "changed", message: TREE_CHANGED };
    }
    // A request in a repository is run only after its diff was shown: a preview that could not
    // produce one (the base's objects missing, the diff failing) blocks Run as a moved tree does —
    // a `problem`, which the queue cannot click past (operator, 2026-11-01). An unverified base
    // that still produced a diff against the HEAD label does not block; it is warned about.
    const noDiff = v.request.pin !== undefined && extra.diff?.state !== "shown" ? noDiffProblem(extra.diff) : undefined;
    const problem = v.problem ?? (verdict.state === "changed" ? verdict.message : noDiff);
    return {
      ok: true,
      value: { ...v, ...extra, verdict, ...(problem === undefined ? {} : { problem }) },
    };
  }

  /** The base, the diff and the program file, for a pending request in a resolved directory. */
  private async reviewTree(
    request: ExecView,
    hostDir: string,
    program: string | undefined,
  ): Promise<Pick<ExecPreview, "base" | "diff" | "programFile">> {
    if (request.pin === undefined) return {};
    const base: BaseAnswer = this.deps.base === undefined ? { state: "unverified", message: "this app cannot verify a base" } : await this.deps.base(hostDir);
    const against = base.state === "verified" ? base.sha : request.pin.head;
    // The program file: argv[0] as a path, its directory resolved, its last component never followed.
    const argv0 = request.argv[0] ?? "";
    const file = program ?? (argv0.includes("/") ? (argv0.startsWith("/") ? hostPathOf(argv0, this.deps.roots) : join(hostDir, argv0)) : undefined);
    const fileDir = file === undefined ? undefined : this.deps.realpath(dirname(file));
    const real = file === undefined || fileDir === undefined ? undefined : join(fileDir, basename(file));
    const diff: DiffAnswer | undefined = this.deps.diff === undefined ? undefined : await this.deps.diff(hostDir, against, real);
    const top = diff?.state === "shown" ? diff.top : undefined;
    // Shown when it lies in the repository, marked with whether the pin covers it.
    let programFile: ProgramFile | undefined;
    if (real !== undefined && top !== undefined && real.startsWith(`${top}/`) && this.deps.readProgram !== undefined) {
      programFile = { ...this.deps.readProgram(real), pinned: diff?.state === "shown" && diff.programPinned === true };
    }
    return {
      base,
      ...(diff === undefined ? {} : { diff: { ...diff, against } }),
      ...(programFile === undefined ? {} : { programFile }),
    };
  }

  private async previewOnce(id: string): Promise<TerminalResult<ExecPreview>> {
    const p = await this.prepare(id);
    const facts: Partial<Prepared> = p.ok ? p.value : { ...(p.stopped.partial ?? {}) };
    const request = p.ok ? p.value.request : p.stopped.request;
    if (request === undefined) return { ok: false, error: p.ok ? "" : p.stopped.error };
    const pinNow = facts.pin;
    const pin: ExecPreview["pin"] =
      pinNow === undefined
        ? { state: "error", message: p.ok ? "" : p.stopped.error }
        : pinNow.kind === "refused"
          ? { state: "refused", reason: refuseReasonFor(pinNow.reason), message: pinNow.message }
          : pinNow.kind === "error"
            ? { state: "error", message: pinNow.message }
            : samePin(pinNow.pin, request.pin)
              ? { state: "match" }
              : { state: "differs", now: pinNow.pin };
    const problem = !p.ok ? p.stopped.error : pin.state === "differs" ? "the tree changed since the request: Run refuses it (tree_changed)" : undefined;
    const program = facts.program;
    return {
      ok: true,
      value: {
        request,
        argvShown: request.argv.map(escapeArg),
        ...(facts.hostDir === undefined ? {} : { hostDir: facts.hostDir }),
        ...(facts.containerDir === undefined ? {} : { containerDir: facts.containerDir }),
        ...(program === undefined ? {} : { program, programShown: escapeArg(program) }),
        timeoutMs: request.timeout_ms,
        pin,
        ...(problem === undefined ? {} : { problem }),
      },
    };
  }

  /**
   * Run request `rawId` for window `owner`: the spec it opens once, or why nothing was started. The
   * per-id gate is taken before the first await (`ActionGate`'s rule), and is never released once
   * `exec.begin` moved the request to running: one run per request from this app, whatever happens.
   */
  async run(owner: number, rawId: unknown, rawShown: unknown): Promise<TerminalResult<ExecStarted>> {
    const id = execId(rawId);
    if (id === undefined) return { ok: false, error: `not a run request id: ${JSON.stringify(rawId)}` };
    const shown = rawShown as { hostDir?: unknown; program?: unknown } | null;
    if (
      typeof shown !== "object" ||
      shown === null ||
      typeof shown.hostDir !== "string" ||
      (shown.program !== undefined && typeof shown.program !== "string")
    ) {
      return { ok: false, error: "Run names what its preview showed: the directory, and the host program for a host run" };
    }
    if (this.taken.has(id)) return { ok: false, error: `${id} has already been run from this app` };
    this.taken.add(id);
    let begun = false;
    try {
      const p = await this.prepare(id);
      if (!p.ok) {
        if (p.stopped.refuse !== undefined) {
          const r = await this.deps.op({ op: "exec.refuse", id, reason: p.stopped.refuse });
          if (!r.ok) return { ok: false, error: `${p.stopped.error}; and refusing ${id} failed: ${r.error.message}` };
          return { ok: false, error: `${id} was refused (${p.stopped.refuse}): ${p.stopped.error}` };
        }
        return { ok: false, error: p.stopped.error };
      }
      const { request, spec } = p.value;
      // What runs must be what the operator was shown: a directory or program that resolves
      // differently now — a link retargeted inside the mount, say — refuses the request.
      if (p.value.hostDir !== shown.hostDir || p.value.program !== shown.program) {
        const why = `it now resolves to ${p.value.hostDir}${p.value.program === undefined ? "" : ` running ${escapeArg(p.value.program)}`}, not what the preview showed`;
        const r = await this.deps.op({ op: "exec.refuse", id, reason: "tree_changed" });
        if (!r.ok) return { ok: false, error: `${why}; and refusing ${id} failed: ${r.error.message}` };
        return { ok: false, error: `${id} was refused (tree_changed): ${why}` };
      }
      const pin = p.value.pin.kind === "pinned" ? p.value.pin.pin : null;
      const started = await this.begin(id, pin);
      if (!started.ok) {
        if (started.held === true) begun = true;
        return { ok: false, error: started.error };
      }
      begun = true;
      this.issue(owner, p.value);
      const shownArgv = p.value.program === undefined ? [...request.argv] : [p.value.program, ...request.argv.slice(1)];
      return { ok: true, value: { id, spec, argv: shownArgv, timeoutMs: request.timeout_ms } };
    } finally {
      if (!begun) this.taken.delete(id);
    }
  }

  /**
   * `exec.begin`, and on a lost answer whether it took under this runner. An answer lost to an
   * unreachable daemon leaves the outcome unknown: the record is read until it says, for up to
   * `unknownWaitMs`. Still unknown then, the click is answered with that, the gate stays held
   * (`held`), and the record is read on in the background: if the begin did take, the request is
   * finished `interrupted` under this runner rather than left `running` with nothing running it.
   */
  private async begin(id: string, pin: ExecPin | null): Promise<TerminalResult<null> & { readonly held?: boolean }> {
    const r = await this.deps.op({ op: "exec.begin", id, runner: this.runner, ...(pin === null ? {} : { recomputed_pin: pin }) });
    if (r.ok) {
      const b = decodeExecBegun(r.value);
      if (!b.ok) return { ok: false, error: b.error.message };
      if (b.value.begun === "running") return { ok: true, value: null };
      const why = b.value.begun === "refused" ? `refused (${b.value.request.refuse_reason ?? "tree_changed"}): the tree changed since the request` : b.value.begun;
      return { ok: false, error: `${id} was ${why}` };
    }
    const t = this.timing;
    const until = Date.now() + t.unknownWaitMs;
    let pause = t.retryMs;
    for (;;) {
      const now = await this.status(id);
      if (now.ok) {
        if (now.value.state === "running" && now.value.runner === this.runner) return { ok: true, value: null };
        return { ok: false, error: `could not begin ${id}: ${r.error.message}` };
      }
      if (Date.now() >= until) break;
      await sleep(pause);
      pause = Math.min(pause * 2, t.maxRetryMs);
    }
    void this.settleUnknown(id);
    return {
      ok: false,
      held: true,
      error: `could not tell whether ${id} began (${r.error.message}); if it did, it is ended as interrupted once jkb serve answers`,
    };
  }

  /** Read `id` until the record answers; a begin that took under this runner is ended `interrupted`. */
  private async settleUnknown(id: string): Promise<void> {
    const t = this.timing;
    let pause = t.retryMs;
    for (;;) {
      const now = await this.status(id);
      if (now.ok) {
        if (now.value.state === "running" && now.value.runner === this.runner) {
          this.deps.log(`${id}: its begin took after all, with no terminal; ending it as interrupted`);
          await new ExecStream(id, this.runner, this.deps, t, false).finish("interrupted", undefined);
        } else {
          this.taken.delete(id);
        }
        return;
      }
      await sleep(pause);
      pause = Math.min(pause * 2, t.maxRetryMs);
    }
  }

  /** Issue `spec` to `owner` for one open, and own what follows: the deadline, the timeout, the stream. */
  private issue(owner: number, prepared: Prepared): void {
    const { request, spec, hostDir, identity, nonce } = prepared;
    const id = request.id;
    let refusedByLauncher: boolean | undefined;
    const t = this.timing;
    let stream: ExecStream | undefined;
    let timedOut = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let settled = false;
    const end = (outcome: string, exitStatus: number | undefined): void => {
      if (settled) return;
      settled = true;
      clearTimeout(deadline);
      if (timer !== undefined) clearTimeout(timer);
      const s = stream ?? new ExecStream(id, this.runner, this.deps, t, false);
      void s.finish(outcome, exitStatus);
    };
    const run: OnceRun = {
      env: { JKB_EXEC_REQUEST: id },
      // At the spawn, once more: the directory is still the one pinned (the launcher checks again).
      ...(spec.target === "host"
        ? {
            verify: () =>
              sameIdentity(this.deps.identity(hostDir), identity)
                ? undefined
                : `${hostDir} is no longer the directory that was pinned; nothing was run`,
          }
        : {}),
      onOpen: (handle: OnceHandle) => {
        clearTimeout(deadline);
        stream = new ExecStream(id, this.runner, this.deps, t, true);
        timer = setTimeout(() => {
          timedOut = true;
          void handle.stop(t.killGraceMs);
        }, Math.min(request.timeout_ms, EXEC_MAX_TIMEOUT_MS));
      },
      onData: (data) => {
        refusedByLauncher ??= nonce !== undefined && data.startsWith(launcherRefusal(nonce));
        stream?.push(data);
      },
      onExit: ({ exitCode, signal }) => {
        // The launcher refused (the directory was swapped at the last moment): nothing ran.
        if (exitCode === LAUNCHER_REFUSED && refusedByLauncher === true) end("interrupted", undefined);
        else if (timedOut) end("timed_out", signal ?? exitCode);
        else if (signal !== undefined && signal > 0) end("signalled", signal);
        else end("exited", exitCode);
      },
      onFail: (error) => {
        this.deps.log(`${id}: the terminal did not start: ${error}`);
        end("interrupted", undefined);
      },
    };
    const revoke = this.deps.issueOnce(owner, spec, run);
    const deadline = setTimeout(() => {
      if (!revoke()) return;
      this.deps.log(`${id}: the window did not open its terminal within ${Math.round(t.openDeadlineMs / 1000)}s`);
      end("interrupted", undefined);
    }, t.openDeadlineMs);
  }

  /**
   * At startup: every `running` request whose runner is another Code Factory — one from an earlier
   * boot, or one whose process is gone — died with that app, and its PTY with it. Each is finished as
   * `interrupted` in that runner's name, with what its log holds. Retries while the daemon is down,
   * `attempts` times.
   */
  async reconcile(attempts = 10): Promise<number> {
    const t = this.timing;
    for (let attempt = 0; attempt < attempts; attempt++) {
      const done = await this.reconcileOnce();
      if (done !== undefined) return done;
      await sleep(Math.min(t.maxRetryMs * 6, t.retryMs * 2 ** attempt));
    }
    return 0;
  }

  private async reconcileOnce(): Promise<number | undefined> {
    let after: number | undefined;
    const stale: ExecView[] = [];
    for (;;) {
      const r = await this.deps.op({ op: "exec.list", state: "running", ...(after === undefined ? {} : { after }) });
      if (!r.ok) {
        this.deps.log(`reconciling run requests: ${r.error.message}`);
        return undefined;
      }
      const page = decodeExecRequests(r.value);
      if (!page.ok) return 0;
      for (const v of page.value.requests) {
        const who = v.runner === undefined ? undefined : parseRunnerName(v.runner);
        if (who === undefined || v.runner === this.runner) continue;
        if (who.boot !== this.deps.boot || !this.deps.processAlive(who.pid)) stale.push(v);
      }
      if (page.value.next === undefined) break;
      after = page.value.next;
    }
    let n = 0;
    for (const v of stale) {
      const log = readLogTail(this.deps.home, v.id);
      const r = await this.deps.op({
        op: "exec.finish",
        id: v.id,
        runner: v.runner,
        outcome: "interrupted",
        tail_b64: log?.tail.toString("base64") ?? "",
        total_bytes: log?.total ?? 0,
        ...(log === undefined ? {} : { log_path: log.path }),
      });
      if (r.ok) n++;
      else this.deps.log(`finishing ${v.id} as interrupted: ${r.error.message}`);
    }
    return n;
  }
}

// ---- the log ---------------------------------------------------------------------------------

/**
 * The run's log directory under `home`, made one component at a time and refused if any component
 * below `home` is a link: `~/.jkb/logs` is writable from the dev container, so a link planted there
 * would have main create the log wherever it points. The leaf is then opened `O_EXCL|O_NOFOLLOW`.
 * Between the check and the open is the residual race, as for the daemon token (`daemon.ts`).
 */
function logDir(home: string): string | undefined {
  let at = home;
  for (const part of EXEC_LOG_DIR_IN_HOME.split("/")) {
    at = join(at, part);
    try {
      if (!lstatSync(at).isDirectory()) return undefined;
    } catch {
      try {
        mkdirSync(at, { mode: 0o700 });
      } catch {
        return undefined;
      }
      if (!lstatSync(at).isDirectory()) return undefined;
    }
  }
  return at;
}

/** The log path a request's output goes to. */
export function logPathFor(home: string, id: string): string {
  return join(home, EXEC_LOG_DIR_IN_HOME, `${id}.log`);
}

/** Open request `id`'s log for this run: a new file, mode 0600, never through a link. */
function openLog(home: string, id: string): { fd: number; path: string } | { error: string } {
  const dir = logDir(home);
  if (dir === undefined) return { error: `${EXEC_LOG_DIR_IN_HOME} under the home is not a plain directory (a link?)` };
  const path = join(dir, `${id}.log`);
  try {
    const fd = openSync(path, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | (constants.O_NOFOLLOW ?? 0), 0o600);
    return { fd, path };
  } catch (e) {
    return { error: `could not create ${path}: ${(e as { code?: string }).code ?? String(e)}` };
  }
}

/** The last `EXEC_MAX_TAIL_BYTES` of request `id`'s log and its size, read without following a link. */
function readLogTail(home: string, id: string): { tail: Buffer; total: number; path: string } | undefined {
  const path = logPathFor(home, id);
  let fd: number;
  try {
    fd = openSync(path, constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0));
  } catch {
    return undefined;
  }
  try {
    const st = fstatSync(fd);
    if (!st.isFile()) return undefined;
    const len = Math.min(st.size, EXEC_MAX_TAIL_BYTES);
    const tail = Buffer.alloc(len);
    readSync(fd, tail, 0, len, st.size - len);
    return { tail, total: st.size, path };
  } catch {
    return undefined;
  } finally {
    closeSync(fd);
  }
}

// ---- the stream ------------------------------------------------------------------------------

type Queued = { kind: "bytes"; bytes: Buffer } | { kind: "gap"; bytes: number };

/**
 * One run's output: to the log, to a 256 KiB ring (the row's tail), and to `exec.output` in order,
 * in chunks of at most `EXEC_MAX_CHUNK_BYTES`. Sends retry while the daemon is unreachable; what
 * waits is capped (`STREAM_BUFFER_BYTES`), and past it output goes to the log only, with one line in
 * the stream saying how much did not reach it and where it is — so the waiting session sees the gap
 * rather than a stream that merely looks complete.
 */
export class ExecStream {
  private readonly queue: Queued[] = [];
  private queued = 0;
  private inFlight: Queued | undefined;
  private pumping: Promise<void> | undefined;
  private stopped = false;
  private total = 0;
  private readonly ring: Buffer[] = [];
  private ringBytes = 0;
  private readonly log: { fd: number; path: string } | undefined;

  constructor(
    private readonly id: string,
    private readonly runner: string,
    private readonly deps: Pick<ExecRunnerDeps, "op" | "home" | "log">,
    private readonly t: { readonly retryMs: number; readonly maxRetryMs: number; readonly bufferBytes: number },
    withLog: boolean,
  ) {
    if (withLog) {
      const opened = openLog(deps.home, id);
      if ("error" in opened) deps.log(`${id}: no log: ${opened.error}`);
      else this.log = opened;
    }
  }

  /** Output as the terminal shows it. */
  push(data: string): void {
    const bytes = Buffer.from(data, "utf8");
    if (bytes.length === 0) return;
    this.total += bytes.length;
    if (this.log !== undefined) {
      try {
        writeSync(this.log.fd, bytes);
      } catch (e) {
        this.deps.log(`${this.id}: writing the log: ${String(e)}`);
      }
    }
    this.ring.push(bytes);
    this.ringBytes += bytes.length;
    while (this.ringBytes - (this.ring[0]?.length ?? 0) >= EXEC_MAX_TAIL_BYTES) this.ringBytes -= this.ring.shift()!.length;
    if (this.stopped) return;
    if (this.queued + bytes.length > this.t.bufferBytes) {
      const last = this.queue[this.queue.length - 1];
      if (last?.kind === "gap" && last !== this.inFlight) last.bytes += bytes.length;
      else this.queue.push({ kind: "gap", bytes: bytes.length });
    } else {
      this.queued += bytes.length;
      let rest = bytes;
      const last = this.queue[this.queue.length - 1];
      if (last?.kind === "bytes" && last !== this.inFlight && last.bytes.length < EXEC_MAX_CHUNK_BYTES) {
        const room = EXEC_MAX_CHUNK_BYTES - last.bytes.length;
        last.bytes = Buffer.concat([last.bytes, rest.subarray(0, room)]);
        rest = rest.subarray(room);
      }
      for (let i = 0; i < rest.length; i += EXEC_MAX_CHUNK_BYTES) this.queue.push({ kind: "bytes", bytes: rest.subarray(i, i + EXEC_MAX_CHUNK_BYTES) });
    }
    this.pumping ??= this.pump().finally(() => (this.pumping = undefined));
  }

  /** The tail the row keeps: the last `EXEC_MAX_TAIL_BYTES`. */
  tail(): Buffer {
    const all = Buffer.concat(this.ring);
    return all.subarray(Math.max(0, all.length - EXEC_MAX_TAIL_BYTES));
  }

  private notice(bytes: number): Buffer {
    const where = this.log === undefined ? "no log could be written" : `the full output is in ${this.log.path}`;
    return Buffer.from(`\r\n[jkb: ${bytes} bytes of output did not reach this stream while jkb serve could not take them; ${where}]\r\n`);
  }

  /** `op`, retried while the daemon is unreachable or busy. */
  private async send(request: OpRequest): Promise<Outcome<OpResponse>> {
    let pause = this.t.retryMs;
    for (;;) {
      const r = await this.deps.op(request);
      if (r.ok || !transient(r.error)) return r;
      await sleep(pause);
      pause = Math.min(pause * 2, this.t.maxRetryMs);
    }
  }

  private async pump(): Promise<void> {
    while (this.queue.length > 0 && !this.stopped) {
      const item = this.queue[0]!;
      this.inFlight = item;
      const bytes = item.kind === "bytes" ? item.bytes : this.notice(item.bytes);
      const r = await this.send({ op: "exec.output", id: this.id, runner: this.runner, chunk_b64: bytes.toString("base64") });
      this.inFlight = undefined;
      if (!r.ok && r.error.code !== "queue_full") {
        // Not ours to report on any more (another runner, a request that ended): the log is the record.
        this.deps.log(`${this.id}: output is going to the log only: ${r.error.message}`);
        this.stopped = true;
        this.queue.length = 0;
        this.queued = 0;
        return;
      }
      this.queue.shift();
      if (item.kind === "bytes") this.queued -= item.bytes.length;
    }
  }

  /** The run ended: send what waits, then `exec.finish`, retried until it is taken. */
  async finish(outcome: string, exitStatus: number | undefined): Promise<void> {
    while (this.pumping !== undefined) await this.pumping;
    if (this.log !== undefined) {
      try {
        closeSync(this.log.fd);
      } catch {
        // Closed already.
      }
    }
    const r = await this.send({
      op: "exec.finish",
      id: this.id,
      runner: this.runner,
      outcome,
      ...(exitStatus === undefined ? {} : { exit_status: exitStatus }),
      tail_b64: this.tail().toString("base64"),
      total_bytes: this.total,
      ...(this.log === undefined ? {} : { log_path: this.log.path }),
    });
    if (!r.ok) this.deps.log(`${this.id}: exec.finish was refused: ${r.error.message}`);
  }
}

// ---- the real machine ------------------------------------------------------------------------

/**
 * This boot's identity, so a runner name from an earlier boot is known to be dead whatever its pid:
 * Linux's `boot_id`, macOS's `kern.boottime` seconds, or the boot time to the minute.
 */
export function bootId(): string {
  try {
    const id = readFileSync("/proc/sys/kernel/random/boot_id", "utf8").trim();
    if (/^[0-9a-f-]{8,64}$/.test(id)) return id;
  } catch {
    // Not Linux.
  }
  try {
    const out = execFileSync("/usr/sbin/sysctl", ["-n", "kern.boottime"], { encoding: "utf8", timeout: 5_000 });
    const sec = /sec = (\d+)/.exec(out)?.[1];
    if (sec !== undefined) return `t${sec}`;
  } catch {
    // No sysctl.
  }
  return `m${Math.round((Date.now() / 1000 - uptime()) / 60)}`;
}

/** `jkb exec pin` by the host jkb at `home`'s `HOST_JKB_IN_HOME`, run by `run` (`machineRunner`). */
export function machinePin(home: string, run: RunFile): (hostDir: string) => Promise<PinAnswer> {
  const jkb = join(home, HOST_JKB_IN_HOME);
  return async (hostDir) => {
    try {
      accessSync(jkb, constants.X_OK);
      if (!statSync(jkb).isFile()) throw new Error("not a file");
    } catch {
      return { kind: "error", message: `no jkb at ${jkb} — scripts/setup.sh installs it; the pin is recomputed only by the host's own jkb` };
    }
    const r = await run(jkb, ["--json", "exec", "pin", "--dir", hostDir], PIN_TIMEOUT_MS);
    return parsePinOutput(r.code, r.stdout, r.stderr);
  };
}

/** `jkb exec base` by the host jkb, the clean clone (`cloneDir`) consulted when the remote cannot be asked. */
export function machineBase(home: string, run: RunFile, cloneDir: string): (hostDir: string) => Promise<BaseAnswer> {
  const jkb = join(home, HOST_JKB_IN_HOME);
  return async (hostDir) => {
    if (!hostJkbOk(jkb)) return { state: "unverified", message: `no jkb at ${jkb}` };
    const trust = trustedRemotes(home).flatMap((url) => ["--trust", url]);
    const r = await run(jkb, ["--json", "exec", "base", "--dir", hostDir, ...(isDir(cloneDir) ? ["--clone", cloneDir] : []), ...trust], BASE_TIMEOUT_MS);
    return parseBaseOutput(r.code, r.stdout, r.stderr);
  };
}

/** `jkb exec diff` by the host jkb, the clean clone offered as a second source of the base's objects. */
export function machineDiff(home: string, run: RunFile, cloneDir: string): (hostDir: string, base: string, program?: string) => Promise<DiffAnswer> {
  const jkb = join(home, HOST_JKB_IN_HOME);
  return async (hostDir, base, program) => {
    if (!hostJkbOk(jkb)) return { state: "error", message: `no jkb at ${jkb}` };
    const r = await run(
      jkb,
      [
        "--json", "exec", "diff", "--dir", hostDir, "--base", base, "--max", String(DIFF_MAX_BYTES),
        ...(isDir(cloneDir) ? ["--objects-from", cloneDir] : []),
        ...(program === undefined ? [] : ["--program", program]),
      ],
      PIN_TIMEOUT_MS,
    );
    return parseDiffOutput(r.code, r.stdout, r.stderr);
  };
}

/**
 * The remotes the operator trusts beyond the clean clone's: one URL per line of
 * `~/.jkb/trusted-remotes` under the ACCOUNT's home (blank lines and `#` comments skipped). The dev
 * container mounts only `~/.jkb/claude-memory` and `~/.jkb/logs`, so it cannot write this file; it
 * is read without following a link, and anything not a plain file reads as no list.
 */
export function trustedRemotes(home: string): string[] {
  const path = join(home, TRUSTED_REMOTES_IN_HOME);
  let fd: number;
  try {
    fd = openSync(path, constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0));
  } catch {
    return [];
  }
  try {
    const st = fstatSync(fd);
    if (!st.isFile() || st.size > 64 * 1024) return [];
    const buf = Buffer.alloc(st.size);
    readSync(fd, buf, 0, st.size, 0);
    return buf
      .toString("utf8")
      .split("\n")
      .map((l) => l.trim())
      .filter((l) => l !== "" && !l.startsWith("#") && !l.startsWith("-"));
  } finally {
    closeSync(fd);
  }
}

/** The operator's list of trusted remotes, under the account's home. */
export const TRUSTED_REMOTES_IN_HOME = ".jkb/trusted-remotes";

function hostJkbOk(jkb: string): boolean {
  try {
    accessSync(jkb, constants.X_OK);
    return statSync(jkb).isFile();
  } catch {
    return false;
  }
}

function isDir(path: string): boolean {
  try {
    return statSync(path).isDirectory();
  } catch {
    return false;
  }
}

/** How much of a program file the queue shows. */
export const PROGRAM_SHOWN_BYTES = 256 * 1024;

/**
 * The program file at `path` as the queue shows it, read without following a link: a link is shown
 * as one (its target's text), never read through.
 */
export function readProgramFile(path: string): ProgramFile {
  let st;
  try {
    st = lstatSync(path);
  } catch {
    return { path, kind: "absent" };
  }
  if (st.isSymbolicLink()) {
    try {
      return { path, kind: "link", target: readlinkSync(path) };
    } catch {
      return { path, kind: "absent" };
    }
  }
  if (!st.isFile()) return { path, kind: "other" };
  let fd: number;
  try {
    fd = openSync(path, constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0) | (constants.O_NONBLOCK ?? 0));
  } catch {
    return { path, kind: "absent" };
  }
  try {
    const f = fstatSync(fd);
    if (!f.isFile()) return { path, kind: "other" };
    const len = Math.min(f.size, PROGRAM_SHOWN_BYTES);
    const buf = Buffer.alloc(len);
    const n = readSync(fd, buf, 0, len, 0);
    return { path, kind: "file", text: buf.subarray(0, n).toString("utf8"), truncated: f.size > PROGRAM_SHOWN_BYTES };
  } finally {
    closeSync(fd);
  }
}

/** A bare program name, found as run.sh finds its own: each absolute line of path-keep, then its fixed PATH. */
export function machineFindProgram(home: string): (name: string) => string | undefined {
  return (name) => {
    if (name.includes("/") || name === "") return undefined;
    let keep: string | undefined;
    try {
      keep = readFileSync(join(home, PATH_KEEP_IN_HOME), "utf8");
    } catch {
      keep = undefined;
    }
    for (const dir of dockerSearchPath(keep)) {
      const file = join(dir, name);
      try {
        accessSync(file, constants.X_OK);
        if (statSync(file).isFile()) return file;
      } catch {
        // Not here.
      }
    }
    return undefined;
  };
}
