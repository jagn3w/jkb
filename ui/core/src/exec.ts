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
 * Characters that draw nothing, that pass for an ordinary space, or that reorder or hide what is
 * drawn around them: zero-width characters, every space but U+0020, line and paragraph separators,
 * bidirectional controls, the soft hyphen, word joiners, variation selectors, the byte-order mark,
 * the interlinear annotation and tag characters. An argument holding one is not what it looks like.
 */
const INVISIBLE = /[\u00a0\u00ad\u061c\u115f\u1160\u1680\u17b4\u17b5\u180e\u2000-\u200f\u2028-\u202f\u205f-\u206f\u3000\u3164\ufe00-\ufe0f\ufeff\ufff9-\ufffb\u{e0000}-\u{e007f}]/u;

/**
 * One argument as the operator is shown it: every control character (C0, DEL, C1), every invisible
 * or reordering one, a lone surrogate and the backslash itself escaped, so what is read is what runs.
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
    else if (cp < 0x20 || (cp >= 0x7f && cp <= 0x9f) || (cp >= 0xd800 && cp <= 0xdfff) || INVISIBLE.test(ch)) {
      out += `\\u{${cp.toString(16).padStart(4, "0")}}`;
    } else out += ch;
  }
  return out;
}
