//! A design's prompts as the ops answer them (D53.6): the Design tab's Prompts pane, minus the
//! window. One prompt per Claude Code session that worked the design — its pre-minted session uuid
//! and the directory it runs in — recorded by the launch before Claude starts, and resumed with
//! `claude --resume <uuid>` there.
//
// The Rust side is the source of truth: `crates/jkb-api/src/designs/prompts.rs` (the shapes) and
// `crates/jkb-core/src/design/prompts.rs` (the rules).

import type { OpResponse, Outcome } from "./daemon.js";
import { failed } from "./daemon.js";

/** What started a session: `jkb_core::design::Launch`. */
export type Launch = "discuss" | "play" | "task" | "new";

export const LAUNCHES: readonly Launch[] = ["discuss", "play", "task", "new"];

/** `jkb_api::designs::prompts::DesignPrompt`: one recorded session. */
export interface DesignPromptRecord {
  /** `prompt:<session>`. */
  readonly uid: string;
  readonly design: string;
  /** The Claude Code session id: what `claude --resume` takes. */
  readonly session: string;
  /** Where the session runs, and so where it is resumed. */
  readonly cwd: string;
  readonly launch: Launch;
  /** The plan or task a *Play* named; null for the design itself. */
  readonly subject: string | null;
  readonly title: string;
  readonly created_at: string;
}

/** `jkb_api::designs::prompts::NewPrompt`: a *New prompt*'s text and what it was built from. */
export interface NewPrompt {
  readonly kind: "new";
  readonly uid: string;
  readonly title: string;
  readonly prompt: string;
}

/** The requests the Prompts pane makes, as `jkb_api::Request` serializes them. */
export const promptOps = {
  /** A design's prompts, newest first. */
  list: (uid: string) => ({ op: "design.prompts", uid }),
  /** The prompt a Claude Code session was recorded with, if any (D53.9). */
  of: (session: string) => ({ op: "design.prompt_of", session }),
  /** The prompt a *New prompt* starts its session with: the operator's `text`, after the design's how-to. */
  newPrompt: (uid: string, text: string) => ({ op: "design.prompt", ask: { kind: "new", uid, text } }),
} as const;

// ---- decoding answers -------------------------------------------------------------------------

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

const isString = (v: unknown): v is string => typeof v === "string";

function isRecord(v: unknown): v is DesignPromptRecord {
  return (
    isObject(v) &&
    isString(v["uid"]) &&
    isString(v["design"]) &&
    isString(v["session"]) &&
    isString(v["cwd"]) &&
    v["cwd"].startsWith("/") &&
    (LAUNCHES as readonly unknown[]).includes(v["launch"]) &&
    (v["subject"] === null || v["subject"] === undefined || isString(v["subject"])) &&
    isString(v["title"]) &&
    isString(v["created_at"])
  );
}

/** `design.prompts`' answer: the design's prompts, newest first. */
export function decodeDesignPrompts(o: Outcome<OpResponse>): Outcome<readonly DesignPromptRecord[]> {
  if (!o.ok) return o;
  const prompts = o.value.result === "design_prompts" ? o.value["prompts"] : undefined;
  if (!Array.isArray(prompts) || !prompts.every(isRecord)) {
    return failed("internal", "jkb serve did not answer with a well-formed `design_prompts`");
  }
  return { ok: true, value: prompts.map((p) => ({ ...p, subject: p.subject ?? null })) };
}

/**
 * `design.prompt_of`'s answer: the prompt a session was recorded with — the design it worked — or
 * `null` when no launch recorded it (a session started outside the app). D53.9's *Jump to context*.
 */
export function decodeSessionPrompt(o: Outcome<OpResponse>): Outcome<DesignPromptRecord | null> {
  if (!o.ok) return o;
  if (o.value.result !== "design_prompt_of") {
    return failed("internal", "jkb serve did not answer with a well-formed `design_prompt_of`");
  }
  const p = o.value["prompt"];
  if (p === undefined || p === null) return { ok: true, value: null };
  if (!isRecord(p)) return failed("internal", "jkb serve did not answer with a well-formed `design_prompt_of`");
  return { ok: true, value: { ...p, subject: p.subject ?? null } };
}

/** `design.prompt`'s answer to a *New prompt*. */
export function decodeNewPrompt(o: Outcome<OpResponse>): Outcome<NewPrompt> {
  if (!o.ok) return o;
  const v = o.value.result === "design_new_prompt" ? o.value["prompt"] : undefined;
  if (
    !isObject(v) ||
    v["kind"] !== "new" ||
    !isString(v["uid"]) ||
    !isString(v["title"]) ||
    !isString(v["prompt"]) ||
    v["prompt"] === ""
  ) {
    return failed("internal", "jkb serve did not answer with a well-formed `design_new_prompt`");
  }
  return { ok: true, value: v as unknown as NewPrompt };
}

/** A `prompt` message on a design's topic: a launch recorded (or moved) one of its prompts. */
export interface PromptAnnouncement {
  readonly design: string;
  /** The prompt's uid. */
  readonly prompt: string;
}

/** The payload of a `prompt` message on a design topic, or `undefined` when it is not one. */
export function parsePromptAnnouncement(payload: unknown): PromptAnnouncement | undefined {
  if (!isObject(payload) || !isString(payload["design"]) || !isString(payload["prompt"])) return undefined;
  return { design: payload["design"], prompt: payload["prompt"] };
}

/**
 * What a *New prompt* is titled by (after `New · `): the first line the operator wrote, or
 * `fallback` (the design's title) when they wrote nothing.
 */
export function newPromptTitle(text: string, fallback: string): string {
  return (
    text
      .split("\n")
      .map((l) => l.trim())
      .find((l) => l !== "") ?? fallback
  );
}
