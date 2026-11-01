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
import { accessSync, closeSync, constants, fstatSync, lstatSync, mkdirSync, openSync, readFileSync, readSync, statSync, writeSync } from "node:fs";
import { uptime } from "node:os";
import { basename, join } from "node:path";

import {
  EXEC_MAX_CHUNK_BYTES,
  EXEC_MAX_TAIL_BYTES,
  EXEC_MAX_TIMEOUT_MS,
  decodeExecBegun,
  decodeExecRequest,
  decodeExecRequests,
  escapeArg,
  execId,
  parseRunnerName,
  runnerName,
  type ApiError,
  type ExecPin,
  type ExecView,
  type OpRequest,
  type OpResponse,
  type Outcome,
} from "@jkb/core";

import type { ExecPreview, ExecStarted } from "../shared/bridge";
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

/** The `exec.refuse` reason a pin refusal becomes: an unpinnable tree is a pin the host cannot reproduce. */
export function refuseReasonFor(pinReason: string): string {
  return pinReason === "dir_gone" || pinReason === "audit_refused" ? pinReason : "tree_changed";
}

/** Whether two pins are the same pin, "none" included. */
function samePin(a: ExecPin | null | undefined, b: ExecPin | null | undefined): boolean {
  if (a === null || a === undefined || b === null || b === undefined) return (a ?? null) === (b ?? null);
  return a.head === b.head && a.changes === b.changes;
}

/** What the runner reaches outside itself. */
export interface ExecRunnerDeps {
  /** One op of `jkb serve`'s set, as the operator (main's client). */
  op(request: OpRequest): Promise<Outcome<OpResponse>>;
  /** `jkb exec pin --dir <hostDir> --json` by the host jkb. */
  pin(hostDir: string): Promise<PinAnswer>;
  readonly roots: TerminalRoots;
  isDirectory(path: string): boolean;
  /** `path` with every link and `..` resolved, or `undefined` when it does not exist. */
  realpath(path: string): string | undefined;
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
  readonly containerDir?: string;
  readonly spec: TerminalSpec;
  readonly pin: PinAnswer;
}

/** A preparation that stopped: the request is refused for `refuse`, or nothing is refused and `error` is said. */
type Stopped = { readonly refuse?: string; readonly error: string; readonly request?: ExecView; readonly partial?: Partial<Prepared> };

export class ExecRunner {
  /** Requests this app has begun, or is between the click and `exec.begin` for. */
  private readonly taken = new Set<string>();
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
    if (request.target === "host") {
      const file = program!.startsWith("/") ? program! : program!.includes("/") ? join(hostDir, program!) : this.deps.findHostProgram(program!);
      if (file === undefined) {
        return stop({ error: `${escapeArg(program!)} is not found where run.sh looks for programs; ask for it by path`, request, partial: { hostDir } });
      }
      argv = [file, ...args];
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
    if (!spec.ok) return stop({ error: `${id} cannot be run as a terminal: ${spec.error}`, request, partial: { hostDir } });

    const pin = await this.deps.pin(hostDir);
    const prepared: Prepared = { request, hostDir, ...(containerDir === undefined ? {} : { containerDir }), spec: spec.value, pin };
    if (pin.kind === "refused") {
      return stop({ refuse: refuseReasonFor(pin.reason), error: `the pin could not be recomputed (${pin.reason}): ${pin.message}`, request, partial: prepared });
    }
    if (pin.kind === "error") return stop({ error: pin.message, request, partial: prepared });
    // The pin's own canonical directory must be the one just resolved: a link retargeted in between
    // would pin one tree and run in another.
    if (pin.dir !== undefined && pin.dir !== hostDir) {
      return stop({ refuse: "tree_changed", error: `the directory moved while it was pinned (${hostDir}, then ${pin.dir})`, request, partial: prepared });
    }
    return { ok: true, value: prepared };
  }

  /** What Run would do now, for the queue to show before it is clicked. Changes nothing. */
  async preview(rawId: unknown): Promise<TerminalResult<ExecPreview>> {
    const id = execId(rawId);
    if (id === undefined) return { ok: false, error: `not a run request id: ${JSON.stringify(rawId)}` };
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
    const program = facts.spec?.target === "host" ? facts.spec.argv[0] : undefined;
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
  async run(owner: number, rawId: unknown): Promise<TerminalResult<ExecStarted>> {
    const id = execId(rawId);
    if (id === undefined) return { ok: false, error: `not a run request id: ${JSON.stringify(rawId)}` };
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
      const pin = p.value.pin.kind === "pinned" ? p.value.pin.pin : null;
      const started = await this.begin(id, pin);
      if (!started.ok) {
        if (started.held === true) begun = true;
        return { ok: false, error: started.error };
      }
      begun = true;
      this.issue(owner, request, spec);
      return { ok: true, value: { id, spec, timeoutMs: request.timeout_ms } };
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
  private issue(owner: number, request: ExecView, spec: TerminalSpec): void {
    const id = request.id;
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
      onOpen: (handle: OnceHandle) => {
        clearTimeout(deadline);
        stream = new ExecStream(id, this.runner, this.deps, t, true);
        timer = setTimeout(() => {
          timedOut = true;
          void handle.stop(t.killGraceMs);
        }, Math.min(request.timeout_ms, EXEC_MAX_TIMEOUT_MS));
      },
      onData: (data) => stream?.push(data),
      onExit: ({ exitCode, signal }) => {
        if (timedOut) end("timed_out", signal ?? exitCode);
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
