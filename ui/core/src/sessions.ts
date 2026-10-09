//! The Sessions tab's data (D53.9), minus the window: the session registry (`session.list`) joined
//! with the notification records (`notify.open_sessions`), the needs-input dot, the messages on the
//! `claude/notify` queue that say a record moved, and the facts *Jump to context* reads — the tasks on
//! a worktree's branch (`task.by_branch`) and what a worktree's git files say its branch is.
//
// The Rust side is the source of truth: `crates/jkb-api/src/lib.rs` (`ClaudeSession`,
// `NotifySession`, `BranchTask`), `crates/jkb-core/src/claude_session.rs` (who holds a session) and
// `crates/jkb-core/src/notify.rs` (the notification machine — a record's state is derived there and
// carried on the wire, never re-decided here).

import type { OpResponse, Outcome } from "./daemon.js";
import { failed } from "./daemon.js";

/** The topic notifications are sent on (`jkb_core::notify::TOPIC`), which the app reads with its own group. */
export const NOTIFY_TOPIC = "claude/notify";

/** `jkb_core::notify::KIND_POST` / `KIND_WITHDRAW`: the two kinds of message on [`NOTIFY_TOPIC`]. */
export const NOTIFY_KINDS = ["notify.post", "notify.withdraw"] as const;

/** Where a session's notification stands (`jkb_core::notify::NotifState`, minus `absent`, which has no record). */
export type NotifyState = "awaiting_user" | "awaiting_tool";

/** `jkb_api::NotifySession`: a notification on screen. */
export interface NotifyRecord {
  readonly session: string;
  /** The tool a permission prompt named; empty for the idle prompt. */
  readonly tool: string;
  /** `null` from a daemon that predates the field: shown, but never as the dot. */
  readonly state: NotifyState | null;
  /** Unix ms. */
  readonly updatedAt: number;
}

/** `jkb_api::ClaudeSession`: one process's hold on a session. */
export interface SessionHolder {
  readonly session: string;
  readonly pid: string;
  /** Where `pid` means something: `host[#boot][/pidns]`. */
  readonly instance: string;
  readonly cwd: string;
  /** Unix ms; `null` when the process was first seen otherwise than by its start. */
  readonly startedAt: number | null;
  readonly startSource: string | null;
  readonly seenAt: number;
  /** Unix ms; `null` while it holds the session. */
  readonly endedAt: number | null;
  readonly endReason: string | null;
}

/** One page of `session.list`. */
export interface SessionPage {
  readonly holders: readonly SessionHolder[];
  /** Send back as `after` for the next page; `null` on the last. */
  readonly next: string | null;
}

/** `jkb_api::BranchTask`: a task recording a branch. */
export interface BranchTask {
  readonly uid: string;
  readonly status: string;
  readonly onto: string | null;
}

/** The requests the Sessions tab makes, as `jkb_api::Request` serializes them. */
export const sessionOps = {
  /** The registry: live rows least recently seen first, or (`all`) every row most recently seen first. */
  list: (all: boolean, after?: string) => (after === undefined ? { op: "session.list", all } : { op: "session.list", all, after }),
  /** Every notification on screen. */
  notified: () => ({ op: "notify.open_sessions" }),
  /** A repo's tasks, by every branch each records. */
  byBranch: (repo: string) => ({ op: "task.by_branch", repo }),
} as const;

// ---- decoding answers -------------------------------------------------------------------------

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}
const isString = (v: unknown): v is string => typeof v === "string";
const isInt = (v: unknown): v is number => typeof v === "number" && Number.isInteger(v);
const optInt = (v: unknown): number | null | undefined => (v === undefined || v === null ? null : isInt(v) ? v : undefined);
const optString = (v: unknown): string | null | undefined => (v === undefined || v === null ? null : isString(v) ? v : undefined);

function holder(v: unknown): SessionHolder | undefined {
  if (!isObject(v)) return undefined;
  const { session, pid, instance, cwd, seen_at } = v;
  const startedAt = optInt(v["started_at"]);
  const endedAt = optInt(v["ended_at"]);
  const startSource = optString(v["start_source"]);
  const endReason = optString(v["end_reason"]);
  if (
    !isString(session) ||
    !isString(pid) ||
    !isString(instance) ||
    !isString(cwd) ||
    !isInt(seen_at) ||
    startedAt === undefined ||
    endedAt === undefined ||
    startSource === undefined ||
    endReason === undefined
  ) {
    return undefined;
  }
  return { session, pid, instance, cwd, startedAt, startSource, seenAt: seen_at, endedAt, endReason };
}

/** `session.list`'s answer. */
export function decodeSessionPage(o: Outcome<OpResponse>): Outcome<SessionPage> {
  if (!o.ok) return o;
  const bad = failed<SessionPage>("internal", "jkb serve did not answer with a well-formed `claude_sessions`");
  if (o.value.result !== "claude_sessions" || !Array.isArray(o.value["sessions"])) return bad;
  const holders = o.value["sessions"].map(holder);
  const next = optString(o.value["next"]);
  if (holders.some((h) => h === undefined) || next === undefined) return bad;
  return { ok: true, value: { holders: holders as SessionHolder[], next } };
}

/** `notify.open_sessions`' answer. */
export function decodeNotified(o: Outcome<OpResponse>): Outcome<readonly NotifyRecord[]> {
  if (!o.ok) return o;
  const bad = failed<readonly NotifyRecord[]>("internal", "jkb serve did not answer with a well-formed `sessions`");
  const list = o.value.result === "sessions" ? o.value["sessions"] : undefined;
  if (!Array.isArray(list)) return bad;
  const out: NotifyRecord[] = [];
  for (const v of list) {
    if (!isObject(v) || !isString(v["session"]) || !isString(v["tool"]) || !isInt(v["updated_at"])) return bad;
    const state = v["state"];
    out.push({
      session: v["session"],
      tool: v["tool"],
      state: state === "awaiting_user" || state === "awaiting_tool" ? state : null,
      updatedAt: v["updated_at"],
    });
  }
  return { ok: true, value: out };
}

/** `task.by_branch`'s answer: every task on each branch. */
export function decodeBranchTasks(o: Outcome<OpResponse>): Outcome<ReadonlyMap<string, readonly BranchTask[]>> {
  if (!o.ok) return o;
  const bad = failed<ReadonlyMap<string, readonly BranchTask[]>>("internal", "jkb serve did not answer with a well-formed `branch_tasks`");
  const tasks = o.value.result === "branch_tasks" ? o.value["tasks"] : undefined;
  if (!isObject(tasks)) return bad;
  const out = new Map<string, BranchTask[]>();
  for (const [branch, list] of Object.entries(tasks)) {
    if (!Array.isArray(list)) return bad;
    const decoded: BranchTask[] = [];
    for (const t of list) {
      const onto = isObject(t) ? optString(t["onto"]) : undefined;
      if (!isObject(t) || !isString(t["uid"]) || !isString(t["status"]) || onto === undefined) return bad;
      decoded.push({ uid: t["uid"], status: t["status"], onto });
    }
    out.set(branch, decoded);
  }
  return { ok: true, value: out };
}

/**
 * A message on [`NOTIFY_TOPIC`]: which session's notification moved. `undefined` for anything else
 * on the topic — the app only needs to know a record changed, and re-reads the records for what it
 * became, so a message it cannot read costs nothing but a re-read it would have made anyway.
 */
export function parseNotifyMessage(kind: unknown, payload: unknown): { readonly session: string } | undefined {
  if (!(NOTIFY_KINDS as readonly unknown[]).includes(kind)) return undefined;
  if (!isObject(payload) || !isString(payload["session"])) return undefined;
  return { session: payload["session"] };
}

// ---- the list ---------------------------------------------------------------------------------

/** One session as the tab lists it: its processes' holds joined with its notification. */
export interface SessionRow {
  readonly session: string;
  /** Some process still holds it — or it has a notification on screen, which only a live session posts. */
  readonly live: boolean;
  /** Its processes, most recently seen first; empty for a session known only by its notification. */
  readonly holders: readonly SessionHolder[];
  /** Where it runs: the most recently seen live holder's directory (else the most recent one's); empty when unknown. */
  readonly cwd: string;
  readonly startedAt: number | null;
  readonly seenAt: number | null;
  /** When its last holder ended; `null` while live. */
  readonly endedAt: number | null;
  readonly endReason: string | null;
  readonly notify: NotifyRecord | null;
  /** The red dot: its notification awaits the user (D53.9). */
  readonly needsInput: boolean;
}

/** Whether a notification is the needs-input dot: `awaiting_user`, as the record's derived state says. */
export function needsInput(record: NotifyRecord | null | undefined): boolean {
  return record?.state === "awaiting_user";
}

/** The sessions whose notification awaits the user: what the dot on the tab counts. */
export function needingInput(records: readonly NotifyRecord[]): Set<string> {
  return new Set(records.filter(needsInput).map((r) => r.session));
}

/**
 * The registry's rows grouped by session and joined with the notification records. Ordered by what
 * the operator acts on first: sessions needing input, then live ones, then the rest — each most
 * recently seen first. A notification whose session the registry does not list still makes a row:
 * a session waiting on the operator is never hidden by a registry row that was lost.
 */
export function joinSessions(holders: readonly SessionHolder[], notified: readonly NotifyRecord[]): SessionRow[] {
  const by = new Map<string, SessionHolder[]>();
  for (const h of holders) {
    const list = by.get(h.session);
    if (list === undefined) by.set(h.session, [h]);
    else list.push(h);
  }
  const notes = new Map(notified.map((n) => [n.session, n]));
  const sessions = new Set([...by.keys(), ...notes.keys()]);
  const rows: SessionRow[] = [];
  for (const session of sessions) {
    const held = [...(by.get(session) ?? [])].sort((a, b) => b.seenAt - a.seenAt);
    const live = held.filter((h) => h.endedAt === null);
    const notify = notes.get(session) ?? null;
    const lead = live[0] ?? held[0];
    const starts = held.map((h) => h.startedAt).filter((t): t is number => t !== null);
    const lastEnd = live.length > 0 ? undefined : [...held].sort((a, b) => (b.endedAt ?? 0) - (a.endedAt ?? 0))[0];
    rows.push({
      session,
      live: live.length > 0 || (held.length === 0 && notify !== null),
      holders: held,
      cwd: lead?.cwd ?? "",
      startedAt: starts.length > 0 ? Math.min(...starts) : null,
      seenAt: lead?.seenAt ?? notify?.updatedAt ?? null,
      endedAt: lastEnd?.endedAt ?? null,
      endReason: lastEnd?.endReason ?? null,
      notify,
      needsInput: needsInput(notify),
    });
  }
  const rank = (r: SessionRow): number => (r.needsInput ? 0 : r.live ? 1 : 2);
  return rows.sort((a, b) => rank(a) - rank(b) || (b.seenAt ?? 0) - (a.seenAt ?? 0) || a.session.localeCompare(b.session));
}

/** How a row's notification reads, or `undefined` when it has none. */
export function notifyLabel(record: NotifyRecord | null): string | undefined {
  if (record === null) return undefined;
  switch (record.state) {
    case "awaiting_user":
      return "needs input";
    case "awaiting_tool":
      return record.tool === "" ? "awaiting permission" : `awaiting permission: ${record.tool}`;
    case null:
      return "notified";
  }
}

// ---- jump to context --------------------------------------------------------------------------

/** What a worktree's git files say: the checkout's root, its repo key and its branch. */
export interface GitPlace {
  /** The checkout's root (the directory holding `.git`), in the filesystem it was read in. */
  readonly root: string;
  /**
   * The repo key — the MAIN checkout's basename (a linked worktree's is the directory holding its
   * `commondir`), as `repo_ctx` derives the `repo=` tag: `gitrepo::key(gitrepo::main_root(cwd))`.
   */
  readonly repo: string;
  /** The checked-out branch; `null` when HEAD is detached. */
  readonly branch: string | null;
}

/**
 * Where a `.git` FILE points (a linked worktree's `gitdir: <path>`), or `undefined` when the text is
 * not one. A relative path is relative to the directory holding the file, which the caller resolves.
 */
export function parseGitdirFile(text: string): string | undefined {
  const line = text.split("\n")[0]?.trim() ?? "";
  const m = /^gitdir:\s*(.+)$/.exec(line);
  return m?.[1] === undefined || m[1].includes("\0") ? undefined : m[1].trim();
}

/** The branch a `HEAD` file names, `null` for a detached HEAD, `undefined` for text that is neither. */
export function parseHead(text: string): string | null | undefined {
  const line = text.split("\n")[0]?.trim() ?? "";
  const ref = /^ref:\s*refs\/heads\/(.+)$/.exec(line);
  if (ref?.[1] !== undefined) return ref[1];
  return /^[0-9a-f]{40}([0-9a-f]{24})?$/.test(line) ? null : undefined;
}

/** The repo key of a main checkout rooted at `root`: its basename (`gitrepo::key`). */
export function repoKeyOf(root: string): string | undefined {
  const name = root.replace(/\/+$/, "").split("/").pop() ?? "";
  return name === "" || name === "." || name === ".." ? undefined : name;
}

/** The tasks recording `branch`, or none. */
export function tasksOn(byBranch: ReadonlyMap<string, readonly BranchTask[]>, branch: string | null): readonly BranchTask[] {
  return branch === null ? [] : (byBranch.get(branch) ?? []);
}

/**
 * Whether `session` is a lowercase uuid — the only id a resume is built from (jkb stores session ids
 * lowercase, as `claude --resume` takes them). Anything else, including an id that begins with `-`
 * and would be read as a flag, is never put on a command line.
 */
export function isSessionUuid(session: string): boolean {
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(session);
}
