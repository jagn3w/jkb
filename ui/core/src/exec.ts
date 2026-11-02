//! Run requests (the Executions design) as the app sees them, minus the window and minus Node: the
//! `exec.*` op shapes, which of them the renderer may never send, the output frame codec, how an
//! argument is shown to the operator, and the exit status a waiting `jkb exec request` ends with.
//
// The Rust side is the source of truth: `crates/jkb-core/src/exec.rs` holds every rule about a
// request's states (one compare-and-set per move, so a request runs at most once) and
// `crates/jkb-api/src/exec.rs` carries it. Nothing here re-decides any of it. The constants copied
// from Rust are read back out of the Rust sources by `test/exec.test.mjs`.

import type { OpRequest, OpResponse, Outcome } from "./daemon.js";
import { failed } from "./daemon.js";

/**
 * The WHATWG decoder every runtime this core runs in has (Node, Electron, a browser), declared here
 * because the core compiles against ES2022 alone, with no DOM library.
 */
declare const TextDecoder: new (
  label?: string,
  options?: { fatal?: boolean },
) => { decode(input?: Uint8Array, options?: { stream?: boolean }): string };

/**
 * The ops only main's runner sends (`ui/app/src/main/execRun.ts`). main's generic `op` bridge
 * refuses them from the renderer, so the one way to start a run is `execRun(id)`, where main reads
 * the request, recomputes the pin and builds the terminal itself: a window that could send
 * `exec.begin` could mark a request running that nothing runs, and one that could send
 * `exec.output`/`exec.finish` could put words in a run's mouth.
 */
export const EXEC_RUNNER_OPS: readonly string[] = ["exec.begin", "exec.output", "exec.finish", "exec.refuse"];

/** Why main's generic `op` bridge refuses `op` from the renderer, or `undefined` when it does not. */
export function rendererOpRefusal(op: string): string | undefined {
  if (!EXEC_RUNNER_OPS.includes(op)) return undefined;
  return `${op} is sent only by Code Factory's runner: start a run with Run, which reads the request and recomputes its pin itself`;
}

/**
 * main's generic `op` bridge: `request` sent by `send`, unless it is one of [`EXEC_RUNNER_OPS`], which
 * is refused `forbidden` without reaching the daemon.
 */
export function rendererOp(
  request: OpRequest,
  send: (request: OpRequest) => Promise<Outcome<OpResponse>>,
): Promise<Outcome<OpResponse>> {
  const refused = rendererOpRefusal(request.op);
  return refused === undefined ? send(request) : Promise.resolve(failed("forbidden", refused));
}

/** `jkb_core::exec::MAX_CHUNK_BYTES`: the most bytes one `exec.output` carries. */
export const EXEC_MAX_CHUNK_BYTES = 47 * 1024;
/** `jkb_core::exec::MAX_TAIL_BYTES`: the most output a finished request's row keeps. */
export const EXEC_MAX_TAIL_BYTES = 256 * 1024;
/** `jkb_core::exec::DEFAULT_TIMEOUT_MS`: a run's timeout when the request names none. */
export const EXEC_DEFAULT_TIMEOUT_MS = 15 * 60 * 1000;
/** `jkb_core::exec::MAX_TIMEOUT_MS`: the longest timeout a request may ask for. */
export const EXEC_MAX_TIMEOUT_MS = 2 * 60 * 60 * 1000;

/** `jkb-cli`'s exit statuses for a request that did not run to an exit of its own. */
export const EXEC_EXIT = { declined: 77, refused: 78, expired: 79, timedOut: 124 } as const;

/** A tree pin, as the wire carries it (`jkb_api::exec::Pin`). */
export interface ExecPin {
  /** `HEAD`: a label only. */
  readonly head: string;
  /** The blake3 of the pinned work tree. */
  readonly changes: string;
}

/** `jkb_api::exec::OutputView`. */
export interface ExecOutput {
  readonly tail_b64: string;
  readonly total_bytes: number;
  readonly truncated: boolean;
  readonly chunks: number;
  readonly log_path?: string;
}

/** `jkb_api::exec::ExecView`: a request, as every exec answer carries it. */
export interface ExecView {
  readonly id: string;
  readonly topic: string;
  readonly state: string;
  readonly target: string;
  readonly dir: string;
  readonly dir_side: string;
  readonly argv: readonly string[];
  readonly reason: string;
  readonly session: string;
  readonly agent_type?: string;
  readonly role: string;
  readonly task?: string;
  readonly pin?: ExecPin;
  readonly timeout_ms: number;
  readonly created_at: number;
  readonly expires_at: number;
  readonly refuse_reason?: string;
  readonly note?: string;
  readonly runner?: string;
  readonly started_at?: number;
  readonly ended_at?: number;
  readonly exit_status?: number;
  readonly outcome?: string;
  readonly output?: ExecOutput;
}

const EXEC_ID = /^exec-([1-9][0-9]{0,17})$/;

/** `exec-<n>` for a wire id (`exec-<n>`, as `jkb_core::exec::parse_id` takes it), or `undefined`. */
export function execId(value: unknown): string | undefined {
  return typeof value === "string" && EXEC_ID.test(value) ? value : undefined;
}

function isStringArray(v: unknown): v is string[] {
  return Array.isArray(v) && v.every((s) => typeof s === "string");
}

function isPin(v: unknown): v is ExecPin {
  const p = v as { head?: unknown; changes?: unknown } | null;
  return typeof p === "object" && p !== null && typeof p.head === "string" && typeof p.changes === "string";
}

/**
 * `value` as an [`ExecView`], or why it is not one. Only the fields the runner acts on are held to
 * their types; fields are only ever added on the wire, so an unknown one is ignored.
 */
export function decodeExecView(value: unknown): Outcome<ExecView> {
  const v = value as Record<string, unknown> | null;
  const bad = (why: string): Outcome<ExecView> => failed("unknown", `not a run request: ${why}`);
  if (typeof v !== "object" || v === null || Array.isArray(v)) return bad("not an object");
  if (execId(v["id"]) === undefined) return bad("no exec-<n> id");
  for (const k of ["state", "target", "dir", "dir_side", "session", "role", "topic"]) {
    if (typeof v[k] !== "string") return bad(`${k} is not a string`);
  }
  if (!isStringArray(v["argv"]) || v["argv"].length === 0) return bad("argv is not a non-empty list of strings");
  if (typeof v["timeout_ms"] !== "number") return bad("timeout_ms is not a number");
  if (v["pin"] !== undefined && v["pin"] !== null && !isPin(v["pin"])) return bad("pin is malformed");
  if (v["runner"] !== undefined && v["runner"] !== null && typeof v["runner"] !== "string") return bad("runner is not a string");
  return { ok: true, value: v as unknown as ExecView };
}

/** The request an `exec.status`, `exec.refuse`, `exec.decline` or `exec.finish` answered. */
export function decodeExecRequest(r: OpResponse): Outcome<ExecView> {
  if (r.result !== "exec_request") return failed("unknown", `expected exec_request, got ${r.result}`);
  return decodeExecView(r["request"]);
}

/** What `exec.begin` did: `running`, `refused` (the pin differed) or `expired`, and the request now. */
export function decodeExecBegun(r: OpResponse): Outcome<{ readonly begun: string; readonly request: ExecView }> {
  if (r.result !== "exec_begun" || typeof r["begun"] !== "string") return failed("unknown", `expected exec_begun, got ${r.result}`);
  const request = decodeExecView(r["request"]);
  if (!request.ok) return request;
  return { ok: true, value: { begun: r["begun"], request: request.value } };
}

/** A page of `exec.list`. */
export function decodeExecRequests(r: OpResponse): Outcome<{ readonly requests: readonly ExecView[]; readonly next?: number }> {
  if (r.result !== "exec_requests" || !Array.isArray(r["requests"])) return failed("unknown", `expected exec_requests, got ${r.result}`);
  const requests: ExecView[] = [];
  for (const raw of r["requests"] as unknown[]) {
    const one = decodeExecView(raw);
    if (!one.ok) return one;
    requests.push(one.value);
  }
  const next = typeof r["next"] === "number" ? r["next"] : undefined;
  return { ok: true, value: next === undefined ? { requests } : { requests, next } };
}

/**
 * The exit status `jkb exec request` ends with for a request in this state — `jkb_cli::exec_cli::
 * exit_code`'s map — or `undefined` while it has not ended.
 */
export function execExitCode(v: Pick<ExecView, "state" | "outcome" | "exit_status">): number | undefined {
  switch (v.state) {
    case "finished":
      if (v.outcome === "exited") return v.exit_status ?? 1;
      if (v.outcome === "signalled") return 128 + (v.exit_status ?? 0);
      return EXEC_EXIT.timedOut;
    case "declined":
      return EXEC_EXIT.declined;
    case "refused":
      return EXEC_EXIT.refused;
    case "expired":
      return EXEC_EXIT.expired;
    default:
      return undefined;
  }
}

/** The runner name main records with `exec.begin`: `code-factory:<pid>@<boot>`. */
export function runnerName(pid: number, boot: string): string {
  return `code-factory:${pid}@${boot}`;
}

/** A runner name [`runnerName`] made, taken apart; `undefined` for any other runner's. */
export function parseRunnerName(name: string): { readonly pid: number; readonly boot: string } | undefined {
  const m = /^code-factory:([1-9][0-9]{0,9})@(.+)$/.exec(name);
  return m === null ? undefined : { pid: Number(m[1]), boot: m[2] as string };
}

// ---- the frame codec -------------------------------------------------------------------------

const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const B64_INDEX: ReadonlyMap<string, number> = new Map([...B64].map((c, i) => [c, i]));

/** `bytes` as standard base64 (with padding), as `jkb_api::exec::bytes` decodes it. */
export function bytesToBase64(bytes: Uint8Array): string {
  let out = "";
  let i = 0;
  for (; i + 2 < bytes.length; i += 3) {
    const n = ((bytes[i] as number) << 16) | ((bytes[i + 1] as number) << 8) | (bytes[i + 2] as number);
    out += B64[(n >> 18) & 63]! + B64[(n >> 12) & 63]! + B64[(n >> 6) & 63]! + B64[n & 63]!;
  }
  const rest = bytes.length - i;
  if (rest === 1) {
    const n = (bytes[i] as number) << 16;
    out += `${B64[(n >> 18) & 63]!}${B64[(n >> 12) & 63]!}==`;
  } else if (rest === 2) {
    const n = ((bytes[i] as number) << 16) | ((bytes[i + 1] as number) << 8);
    out += `${B64[(n >> 18) & 63]!}${B64[(n >> 12) & 63]!}${B64[(n >> 6) & 63]!}=`;
  }
  return out;
}

/** Standard base64 back to bytes, or `undefined` for text that is not base64. */
export function base64ToBytes(text: string): Uint8Array | undefined {
  if (text.length % 4 !== 0 || !/^[A-Za-z0-9+/]*={0,2}$/.test(text)) return undefined;
  const pad = text.endsWith("==") ? 2 : text.endsWith("=") ? 1 : 0;
  const out = new Uint8Array((text.length / 4) * 3 - pad);
  let o = 0;
  for (let i = 0; i < text.length; i += 4) {
    let n = 0;
    for (let j = 0; j < 4; j++) {
      const c = text[i + j] as string;
      n = (n << 6) | (c === "=" ? 0 : (B64_INDEX.get(c) as number));
    }
    for (const shift of [16, 8, 0]) {
      if (o < out.length) out[o++] = (n >> shift) & 255;
    }
  }
  return out;
}

/** `bytes` in `exec.output` chunks of at most `max` bytes, each base64: cut anywhere, a UTF-8 sequence included. */
export function encodeOutputChunks(bytes: Uint8Array, max: number = EXEC_MAX_CHUNK_BYTES): string[] {
  const out: string[] = [];
  for (let i = 0; i < bytes.length; i += max) out.push(bytesToBase64(bytes.subarray(i, i + max)));
  return out;
}

/** An `exec.output` message's payload (`{n, b64}`) as its chunk number and bytes, or `undefined`. */
export function decodeOutputFrame(payload: unknown): { readonly n: number; readonly bytes: Uint8Array } | undefined {
  const p = payload as { n?: unknown; b64?: unknown } | null;
  if (typeof p !== "object" || p === null || !Number.isInteger(p.n) || typeof p.b64 !== "string") return undefined;
  const bytes = base64ToBytes(p.b64);
  return bytes === undefined ? undefined : { n: p.n as number, bytes };
}

/**
 * Output frames back to text, in order. A chunk is cut at a byte count, so a UTF-8 sequence may be
 * split across two: the decoder holds the first half until the second arrives, and only `end` says
 * what a dangling half was.
 */
export class OutputText {
  private readonly decoder = new TextDecoder("utf-8", { fatal: false });

  /** The text `bytes` completes. */
  push(bytes: Uint8Array): string {
    return this.decoder.decode(bytes, { stream: true });
  }

  /** Whatever was held back, flushed (a dangling half sequence becomes U+FFFD). */
  end(): string {
    return this.decoder.decode();
  }
}

// ---- showing an argument ---------------------------------------------------------------------

/**
 * The characters an argument may not show as themselves, by Unicode property rather than by a list,
 * because a list is always short (a review round found U+1D173, U+034F, U+180B, U+FFA0 and U+0600
 * past the first one): controls (Cc), format characters (Cf), private use (Co), unassigned code
 * points (Cn), lone surrogates (Cs), line and paragraph separators (Zl, Zp), every
 * default-ignorable code point, and every space separator but U+0020 (Zs). An argument holding one is
 * not what it looks like. Rust's `{:?}`, which `jkb exec status` prints with, escapes these too.
 */
const HIDDEN = /[\p{Cc}\p{Cf}\p{Co}\p{Cn}\p{Cs}\p{Zl}\p{Zp}\p{Default_Ignorable_Code_Point}]|(?!\x20)\p{Zs}/u;

/**
 * One argument as the operator is shown it: every character [`HIDDEN`] names and the backslash itself
 * escaped, so what is read is what runs.
 * Printable text, any script, is kept as it is; a newline, tab and carriage return show as `\n`,
 * `\t` and `\r`, anything else as `\u{<hex>}`.
 */
export function escapeArg(arg: string): string {
  let out = "";
  for (const ch of arg) {
    const cp = ch.codePointAt(0) as number;
    if (ch === "\\") out += "\\\\";
    else if (ch === "\n") out += "\\n";
    else if (ch === "\t") out += "\\t";
    else if (ch === "\r") out += "\\r";
    else if (HIDDEN.test(ch)) {
      out += `\\u{${cp.toString(16).padStart(4, "0")}}`;
    } else out += ch;
  }
  return out;
}

/**
 * One argument as the request queue shows it, where the operator decides: [`escapeArg`]'s text with
 * every non-ASCII character also spelt `\u{…}` — so a homoglyph (Cyrillic `а` for Latin `a`) cannot
 * pass for the letter it looks like — and a trailing space as `␠`. Unambiguous because `␠` itself is
 * non-ASCII, so a literal one is spelt `\u{2420}`; an empty argument is `∅` for the same reason.
 * `glyphs` is the argument as [`escapeArg`] shows it, alongside, when the two differ by more than the
 * trailing spaces: what the characters look like.
 */
export interface ShownArg {
  readonly text: string;
  readonly glyphs?: string;
}

export function showArg(arg: string): ShownArg {
  if (arg === "") return { text: "∅" };
  const trailing = arg.length - arg.replace(/ +$/, "").length;
  const body = arg.slice(0, arg.length - trailing);
  const marks = "␠".repeat(trailing);
  let text = "";
  for (const ch of escapeArg(body)) {
    const cp = ch.codePointAt(0) as number;
    text += cp > 0x7e ? `\\u{${cp.toString(16).padStart(4, "0")}}` : ch;
  }
  text += marks;
  const glyphs = escapeArg(body) + marks;
  return glyphs === text ? { text } : { text, glyphs };
}

/** A run of shown text: as it is, or an escape standing for characters that may not show as themselves. */
export interface Segment {
  readonly text: string;
  readonly escaped: boolean;
}

/**
 * Text that is not an argument — a diff, a run's output, a reason — as segments for an escaped
 * `<pre>`: the characters [`escapeArg`] escapes become `\u{…}` (or `\r`) segments marked `escaped`,
 * which the renderer draws apart, so a literal `\u{202e}` in the text is never mistaken for an escape.
 * Newlines and tabs stay as they are; `\r\n` becomes a newline. Never HTML: the renderer puts each
 * segment in as text.
 */
export function escapeSegments(text: string): Segment[] {
  const out: Segment[] = [];
  let plain = "";
  const push = (seg: string): void => {
    if (plain !== "") out.push({ text: plain, escaped: false });
    plain = "";
    out.push({ text: seg, escaped: true });
  };
  const chars = [...text.replaceAll("\r\n", "\n")];
  for (const ch of chars) {
    if (ch === "\n" || ch === "\t") plain += ch;
    else if (ch === "\r") push("\\r");
    else if (HIDDEN.test(ch)) push(`\\u{${(ch.codePointAt(0) as number).toString(16).padStart(4, "0")}}`);
    else plain += ch;
  }
  if (plain !== "") out.push({ text: plain, escaped: false });
  return out;
}

/** A duration in the queue's words: `90s`, `15m`, `2h 5m`. */
export function durationText(ms: number): string {
  const s = Math.round(ms / 1000);
  if (s < 120) return `${s}s`;
  const m = Math.round(s / 60);
  if (m < 120) return `${m}m`;
  return `${Math.floor(m / 60)}h${m % 60 === 0 ? "" : ` ${m % 60}m`}`;
}

/** A pending request as the queue shows it. Every string the requester chose is escaped here. */
export interface RequestView {
  readonly id: string;
  readonly state: string;
  readonly session: string;
  readonly role: string;
  readonly agentType?: string;
  readonly task?: string;
  /** The reason, escaped. */
  readonly reason: readonly Segment[];
  readonly target: "host" | "container" | string;
  /** The directory as asked, and which side's path it is. */
  readonly dir: string;
  readonly dirSide: string;
  readonly timeout: string;
  readonly argv: readonly ShownArg[];
  /** The pin it was asked with, or none (not in a repository). */
  readonly pin?: ExecPin;
  /** How long until it expires unanswered (pending only). */
  readonly expiresIn?: string;
}

export function requestView(v: ExecView, now: number): RequestView {
  return {
    id: v.id,
    state: v.state,
    session: escapeArg(v.session),
    role: escapeArg(v.role),
    ...(v.agent_type === undefined ? {} : { agentType: escapeArg(v.agent_type) }),
    ...(v.task === undefined ? {} : { task: escapeArg(v.task) }),
    reason: escapeSegments(v.reason),
    target: v.target,
    dir: escapeArg(v.dir),
    dirSide: v.dir_side,
    timeout: durationText(v.timeout_ms),
    argv: v.argv.map(showArg),
    ...(v.pin === undefined ? {} : { pin: v.pin }),
    ...(v.state === "pending" ? { expiresIn: durationText(Math.max(0, v.expires_at - now)) } : {}),
  };
}

/** How a request that left the queue ended, in one line: `exited 0`, `declined`, `refused (tree_changed)`. */
export function outcomeText(v: Pick<ExecView, "state" | "outcome" | "exit_status" | "refuse_reason">): string {
  switch (v.state) {
    case "finished":
      if (v.outcome === "exited") return `exited ${v.exit_status ?? "?"}`;
      if (v.outcome === "signalled") return `killed by signal ${v.exit_status ?? "?"}`;
      if (v.outcome === "timed_out") return "timed out";
      return v.outcome ?? "finished";
    case "refused":
      return `refused (${v.refuse_reason ?? "?"})`;
    default:
      return v.state;
  }
}

/** The states the history lists: everything that left the queue for good. */
export const EXEC_ENDED_STATES: readonly string[] = ["finished", "declined", "refused", "expired"];

/** A finished request's stored tail as text (its bytes decoded as UTF-8), or `undefined`. */
export function tailText(v: ExecView): string | undefined {
  const b64 = v.output?.tail_b64;
  if (b64 === undefined || b64 === "") return undefined;
  const bytes = base64ToBytes(b64);
  if (bytes === undefined) return undefined;
  const t = new OutputText();
  return t.push(bytes) + t.end();
}

// ---- the preview: the base, the diff, the verdict --------------------------------------------

/** What `jkb exec base --json` answered. */
export type BaseAnswer =
  | { readonly state: "verified"; readonly sha: string; readonly ref: string; readonly source: string; readonly url: string }
  | { readonly state: "unverified"; readonly message: string }
  | { readonly state: "no_repo" };

const OID = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/;

function lastJson(stdout: string): Record<string, unknown> | undefined {
  const line = stdout.trim().split("\n").pop() ?? "";
  try {
    const v: unknown = JSON.parse(line);
    return typeof v === "object" && v !== null && !Array.isArray(v) ? (v as Record<string, unknown>) : undefined;
  } catch {
    return undefined;
  }
}

/** `jkb exec base --json`'s output. A fault (no object, an exit with no answer) is `unverified` with why. */
export function parseBaseOutput(code: number | null, stdout: string, stderr: string): BaseAnswer {
  const o = lastJson(stdout);
  if (code === 0 && o !== undefined) {
    if ("repo" in o && o["repo"] === null) return { state: "no_repo" };
    const b = o["base"] as Record<string, unknown> | undefined;
    if (typeof b === "object" && b !== null && typeof b["sha"] === "string" && OID.test(b["sha"]) && typeof b["ref"] === "string") {
      return { state: "verified", sha: b["sha"], ref: b["ref"], source: String(b["source"] ?? ""), url: String(b["url"] ?? "") };
    }
    const u = o["unverified"] as Record<string, unknown> | undefined;
    if (typeof u === "object" && u !== null) return { state: "unverified", message: String(u["message"] ?? "") };
  }
  const why = stderr.trim().split("\n").slice(-4).join("\n");
  return { state: "unverified", message: `jkb exec base ${code === null ? "did not finish" : `failed (exit ${code})`}${why ? `: ${why}` : ""}` };
}

/** One changed path in a diff: git's status letter (`I`: tracked on the base, ignored here, outside the pin). */
export interface DiffFile {
  readonly status: string;
  readonly path: string;
}

/** What `jkb exec diff --json` answered. */
export type DiffAnswer =
  | {
      readonly state: "shown";
      readonly base: string;
      readonly top: string;
      readonly pin: ExecPin;
      readonly files: readonly DiffFile[];
      readonly filesTruncated: boolean;
      readonly patch: string;
      readonly truncated: boolean;
      /** For the program asked about: whether the pin covers it (absent when none was asked about). */
      readonly programPinned?: boolean;
    }
  | { readonly state: "no_repo" }
  | { readonly state: "refused"; readonly reason: string; readonly message: string }
  | { readonly state: "error"; readonly message: string };

/** `jkb exec diff --json`'s output. */
export function parseDiffOutput(code: number | null, stdout: string, stderr: string): DiffAnswer {
  const o = lastJson(stdout);
  if (o !== undefined) {
    const r = o["refused"] as Record<string, unknown> | undefined;
    if (typeof r === "object" && r !== null && typeof r["reason"] === "string") {
      return { state: "refused", reason: r["reason"], message: String(r["message"] ?? "") };
    }
    if (code === 0 && "repo" in o && o["repo"] === null) return { state: "no_repo" };
    const files = o["files"];
    const pin = o["pin"];
    if (
      code === 0 &&
      isPin(pin) &&
      typeof o["patch"] === "string" &&
      typeof o["base"] === "string" &&
      Array.isArray(files) &&
      files.every((f) => typeof f === "object" && f !== null && typeof f.status === "string" && typeof f.path === "string")
    ) {
      return {
        state: "shown",
        base: o["base"],
        top: String(o["top"] ?? ""),
        pin,
        files: (files as DiffFile[]).map((f) => ({ status: f.status, path: f.path })),
        filesTruncated: o["files_truncated"] === true,
        patch: o["patch"],
        truncated: o["truncated"] === true,
        ...(typeof o["program_pinned"] === "boolean" ? { programPinned: o["program_pinned"] } : {}),
      };
    }
  }
  const why = stderr.trim().split("\n").slice(-6).join("\n");
  return { state: "error", message: `jkb exec diff ${code === null ? "did not finish" : `failed (exit ${code})`}${why ? `: ${why}` : ""}` };
}

/** Whether two pins are the same pin, "none" (no repository) included. */
export function samePin(a: ExecPin | null | undefined, b: ExecPin | null | undefined): boolean {
  if (a === null || a === undefined || b === null || b === undefined) return (a ?? null) === (b ?? null);
  return a.head === b.head && a.changes === b.changes;
}

/** The sentence the queue shows when Run would refuse: the tree moved since the request. */
export const TREE_CHANGED = "Tree changed since the request: Run will refuse";

/**
 * The preview's verdict on the tree: the pin taken before the diff, the diff's own and the one taken
 * after (pin → diff → pin) must all be the request's. `null` in `seen` is "no repository"; an
 * `undefined` one was not taken (an error said elsewhere) and is skipped.
 */
export function previewVerdict(
  requested: ExecPin | null | undefined,
  seen: readonly (ExecPin | null | undefined)[],
): { readonly state: "match" } | { readonly state: "no_repo" } | { readonly state: "changed"; readonly message: string } {
  const taken = seen.filter((p) => p !== undefined);
  if (taken.some((p) => !samePin(p, requested))) return { state: "changed", message: TREE_CHANGED };
  return requested === undefined || requested === null ? { state: "no_repo" } : { state: "match" };
}

/** What the queue says where a diff would be, for a directory in no repository. */
export const NOT_IN_A_REPOSITORY = "Not in a repository: no tree pin, no diff.";

/** `jkb_core::exec::INDEX_TOPIC`: where jkb announces each request made and each move. */
export const EXEC_INDEX_TOPIC = "exec/requests";
