//! Designs as the `design.*` ops answer them (D53.4–5), and the portable halves of the Document
//! pane: decoding those answers, base64 for Yjs updates, a live-update message, and the span-state
//! derivation that tells the editor which colour each piece of text is.
//
// The Rust side is the source of truth: `crates/jkb-api/src/designs.rs` (the shapes) and
// `crates/jkb-core/src/design/` (the rules). Offsets are UTF-16 code units, as Yjs and a JavaScript
// string count them, so a span's `start`/`end` index the editor's text directly.

import type { OpResponse, Outcome } from "./daemon.js";
import { failed } from "./daemon.js";

/** The four states a piece of design text is in (D53.5). Only APPROVED is recorded. */
export type SpanState = "PROPOSED" | "APPROVED" | "STAGED" | "IMPLEMENTED";

export const SPAN_STATES: readonly SpanState[] = ["PROPOSED", "APPROVED", "STAGED", "IMPLEMENTED"];

/** `jkb_api::designs::Design`: a design as `design.list` answers it. */
export interface Design {
  readonly uid: string;
  readonly title: string;
  /** `designs/<repo>`. */
  readonly namespace: string | null;
  readonly seq: number;
  /** The `mq` topic its updates are announced on. */
  readonly topic: string;
}

/** `jkb_api::designs::Piece`. */
export interface SpanPiece {
  readonly start: number;
  readonly end: number;
  readonly state: SpanState;
  /** Words removed since the approval: zero width now. */
  readonly removed: boolean;
  readonly text: string;
}

/** `jkb_api::designs::Span`. */
export interface Span {
  readonly uid: string;
  readonly reviewer: string;
  readonly anchored: boolean;
  readonly start: number;
  readonly end: number;
  readonly text: string;
  readonly state: SpanState;
  readonly demoted: boolean;
  readonly pieces: readonly SpanPiece[];
  readonly approved_by: string | null;
  readonly approved_at: string | null;
  readonly steps: readonly string[];
}

/** `jkb_api::designs::DesignDoc`: a design's text at one version, as `design.cat` answers it. */
export interface DesignDoc {
  readonly uid: string;
  readonly title: string;
  readonly text: string;
  readonly marked: string;
  /** The version token an edit's `base` (and a *Discuss*) takes. */
  readonly version: string;
  readonly seq: number;
  readonly topic: string;
  readonly spans: readonly Span[];
}

/** `design.state`: what a peer lacks, base64, and the version it brings the peer to. */
export interface DesignUpdate {
  readonly update: string;
  readonly version: string;
  readonly seq: number;
}

/** `jkb_api::designs::Written`. */
export interface DesignWritten {
  readonly uid: string;
  readonly seq: number | null;
  readonly version: string;
  readonly demoted: readonly string[];
}

/** `jkb_api::designs::Prompt`: a prompt for a Claude session, and what it was built from. */
export interface DesignPrompt {
  readonly kind: string;
  readonly uid: string;
  readonly title: string;
  readonly version: string;
  readonly start: number;
  readonly end: number;
  readonly quote: string;
  readonly occurrence: number | null;
  readonly spans: readonly string[];
  readonly prompt: string;
}

// ---- requests ---------------------------------------------------------------------------------

/** The `design.*` requests the Document pane makes, as `jkb_api::Request` serializes them. */
export const designOps = {
  list: (repo?: string) => (repo === undefined ? { op: "design.list" } : { op: "design.list", repo }),
  cat: (uid: string) => ({ op: "design.cat", uid }),
  state: (uid: string, since?: string) =>
    since === undefined ? { op: "design.state", uid } : { op: "design.state", uid, since },
  apply: (uid: string, update: string) => ({ op: "design.apply", uid, update }),
  discuss: (uid: string, base: string, start: number, end: number) => ({
    op: "design.prompt",
    ask: { kind: "discuss", uid, base, start, end },
  }),
} as const;

// ---- decoding answers -------------------------------------------------------------------------

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

const isString = (v: unknown): v is string => typeof v === "string";
const isNumber = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);
const isState = (v: unknown): v is SpanState => (SPAN_STATES as readonly unknown[]).includes(v);

function isPiece(v: unknown): v is SpanPiece {
  return (
    isObject(v) &&
    isNumber(v["start"]) &&
    isNumber(v["end"]) &&
    isState(v["state"]) &&
    typeof v["removed"] === "boolean" &&
    isString(v["text"])
  );
}

function isSpan(v: unknown): v is Span {
  return (
    isObject(v) &&
    isString(v["uid"]) &&
    isString(v["reviewer"]) &&
    typeof v["anchored"] === "boolean" &&
    isNumber(v["start"]) &&
    isNumber(v["end"]) &&
    isState(v["state"]) &&
    Array.isArray(v["pieces"]) &&
    v["pieces"].every(isPiece)
  );
}

function isDesign(v: unknown): v is Design {
  return (
    isObject(v) &&
    isString(v["uid"]) &&
    isString(v["title"]) &&
    (v["namespace"] === null || isString(v["namespace"])) &&
    isNumber(v["seq"]) &&
    isString(v["topic"])
  );
}

function isDoc(v: unknown): v is DesignDoc {
  return (
    isObject(v) &&
    isString(v["uid"]) &&
    isString(v["title"]) &&
    isString(v["text"]) &&
    isString(v["version"]) &&
    isNumber(v["seq"]) &&
    isString(v["topic"]) &&
    Array.isArray(v["spans"]) &&
    v["spans"].every(isSpan)
  );
}

/**
 * The field `field` of a response tagged `result`, when `test` accepts it. Anything else — another
 * tag, a missing or malformed field — is an `internal` failure naming what was expected, never a
 * value the pane would then misread.
 */
function field<T>(
  outcome: Outcome<OpResponse>,
  result: string,
  pick: (r: OpResponse) => unknown,
  test: (v: unknown) => v is T,
): Outcome<T> {
  if (!outcome.ok) return outcome;
  const value = outcome.value.result === result ? pick(outcome.value) : undefined;
  if (!test(value)) return failed("internal", `jkb serve did not answer with a well-formed \`${result}\``);
  return { ok: true, value };
}

export function decodeDesigns(o: Outcome<OpResponse>): Outcome<readonly Design[]> {
  return field(o, "designs", (r) => r["designs"], (v): v is readonly Design[] => Array.isArray(v) && v.every(isDesign));
}

export function decodeDesignDoc(o: Outcome<OpResponse>): Outcome<DesignDoc> {
  return field(o, "design_text", (r) => r["design"], isDoc);
}

export function decodeDesignUpdate(o: Outcome<OpResponse>): Outcome<DesignUpdate> {
  return field(
    o,
    "design_update",
    (r) => r,
    (v): v is DesignUpdate => isObject(v) && isString(v["update"]) && isString(v["version"]) && isNumber(v["seq"]),
  );
}

export function decodeDesignWritten(o: Outcome<OpResponse>): Outcome<DesignWritten> {
  return field(
    o,
    "design_written",
    (r) => r["written"],
    (v): v is DesignWritten =>
      isObject(v) &&
      isString(v["uid"]) &&
      (v["seq"] === null || isNumber(v["seq"])) &&
      isString(v["version"]) &&
      Array.isArray(v["demoted"]) &&
      v["demoted"].every(isString),
  );
}

export function decodeDesignPrompt(o: Outcome<OpResponse>): Outcome<DesignPrompt> {
  return field(
    o,
    "design_prompt",
    (r) => r["prompt"],
    (v): v is DesignPrompt =>
      isObject(v) &&
      isString(v["uid"]) &&
      isString(v["title"]) &&
      isString(v["version"]) &&
      isString(v["quote"]) &&
      isString(v["prompt"]) &&
      v["prompt"] !== "",
  );
}

/** The repo a design's namespace (`designs/<repo>[/…]`) names, or `undefined` for any other. */
export function repoOf(namespace: string | null | undefined): string | undefined {
  const parts = (namespace ?? "").split("/");
  return parts[0] === "designs" && parts[1] !== undefined && parts[1] !== "" ? parts[1] : undefined;
}

/** The repos designs exist for, sorted, each once. */
export function designRepos(designs: readonly Design[]): string[] {
  const repos = new Set<string>();
  for (const d of designs) {
    const repo = repoOf(d.namespace);
    if (repo !== undefined) repos.add(repo);
  }
  return [...repos].sort();
}

// ---- live updates -----------------------------------------------------------------------------

/**
 * Whether `topic` is a design's live-update topic (`design/<uid with ':' spelled '.'>`,
 * `jkb_core::design::topic`). The app subscribes to nothing else on a design's behalf.
 */
export function isDesignTopic(topic: unknown): topic is string {
  return typeof topic === "string" && topic.length <= 256 && /^design\/[A-Za-z0-9._-]+$/.test(topic);
}

/** A `design/<uid>` message's payload: the update inline (base64), or `null` when it was too large. */
export interface DesignAnnouncement {
  readonly design: string;
  readonly seq: number;
  readonly update: string | null;
}

/** The payload of an `update` message on a design topic, or `undefined` when it is not one. */
export function parseAnnouncement(payload: unknown): DesignAnnouncement | undefined {
  if (!isObject(payload) || !isString(payload["design"]) || !isNumber(payload["seq"])) return undefined;
  const update = payload["update"];
  if (update !== null && update !== undefined && !isString(update)) return undefined;
  return { design: payload["design"], seq: payload["seq"], update: isString(update) ? update : null };
}

// ---- base64 -----------------------------------------------------------------------------------
//
// Standard base64 with padding, as the ops carry update bytes and state vectors (`base64`'s
// `STANDARD`). Written out rather than `btoa`/`Buffer`, which are a host's: this package has none.

const ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const DECODE = new Map<string, number>([...ALPHABET].map((c, i) => [c, i]));

export function toBase64(bytes: Uint8Array): string {
  let out = "";
  for (let i = 0; i < bytes.length; i += 3) {
    const a = bytes[i] ?? 0;
    const b = bytes[i + 1];
    const c = bytes[i + 2];
    const n = (a << 16) | ((b ?? 0) << 8) | (c ?? 0);
    out += ALPHABET[(n >> 18) & 63];
    out += ALPHABET[(n >> 12) & 63];
    out += b === undefined ? "=" : ALPHABET[(n >> 6) & 63];
    out += c === undefined ? "=" : ALPHABET[n & 63];
  }
  return out;
}

/** The bytes `text` encodes, or `undefined` when it is not standard padded base64. */
export function fromBase64(text: string): Uint8Array | undefined {
  if (text.length % 4 !== 0) return undefined;
  const pad = text.endsWith("==") ? 2 : text.endsWith("=") ? 1 : 0;
  const out = new Uint8Array((text.length / 4) * 3 - pad);
  let o = 0;
  for (let i = 0; i < text.length; i += 4) {
    let n = 0;
    for (let j = 0; j < 4; j++) {
      const ch = text[i + j] ?? "";
      const last = i + 4 === text.length;
      if (ch === "=" && last && j >= 4 - pad) {
        n <<= 6;
        continue;
      }
      const v = DECODE.get(ch);
      if (v === undefined) return undefined;
      n = (n << 6) | v;
    }
    for (const shift of [16, 8, 0]) {
      if (o < out.length) out[o++] = (n >> shift) & 255;
    }
  }
  return out;
}

// ---- span states ------------------------------------------------------------------------------

/** A run of text in one state: what the editor draws. Runs cover the text, in order, without gaps. */
export interface StateRun {
  readonly from: number;
  readonly to: number;
  readonly state: SpanState;
  /** The span the run is in; `undefined` for text no span covers, which reads PROPOSED. */
  readonly span?: string;
}

/** Words removed from an approved span since its approval: zero width, drawn at `at`. */
export interface RemovedRun {
  readonly at: number;
  readonly text: string;
  readonly span: string;
}

export interface StateMap {
  readonly runs: readonly StateRun[];
  readonly removed: readonly RemovedRun[];
}

/**
 * Which state each piece of a text of `length` UTF-16 units is in, from `design.cat`'s spans
 * (D53.5). Text no span covers is PROPOSED. A span's pieces carry its words' own states — a demoted
 * span's untouched words keep their approval, the words written into it read PROPOSED — and a span
 * with no pieces is drawn whole in its state. Unanchored spans are not in the text at all.
 *
 * The engine keeps spans from overlapping; a run that would overlap an earlier one anyway is clipped
 * to start after it, so each unit is drawn exactly once whatever the answer holds.
 */
export function stateRuns(length: number, spans: readonly Span[]): StateMap {
  const clamp = (n: number): number => Math.max(0, Math.min(length, Math.floor(n)));
  const covered: { from: number; to: number; state: SpanState; span: string }[] = [];
  const removed: RemovedRun[] = [];
  for (const s of spans) {
    if (!s.anchored) continue;
    const pieces = s.pieces.length > 0 ? s.pieces : [{ start: s.start, end: s.end, state: s.state, removed: false, text: s.text }];
    for (const p of pieces) {
      if (p.removed) {
        if (p.text !== "") removed.push({ at: clamp(p.start), text: p.text, span: s.uid });
        continue;
      }
      const from = clamp(p.start);
      const to = clamp(p.end);
      if (to > from) covered.push({ from, to, state: p.state, span: s.uid });
    }
  }
  covered.sort((a, b) => a.from - b.from || a.to - b.to);
  const runs: StateRun[] = [];
  let at = 0;
  for (const c of covered) {
    const from = Math.max(c.from, at);
    if (c.to <= from) continue;
    if (from > at) runs.push({ from: at, to: from, state: "PROPOSED" });
    runs.push({ from, to: c.to, state: c.state, span: c.span });
    at = c.to;
  }
  if (at < length) runs.push({ from: at, to: length, state: "PROPOSED" });
  removed.sort((a, b) => a.at - b.at);
  return { runs, removed };
}
