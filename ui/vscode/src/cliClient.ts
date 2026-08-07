//! The VS Code transport: a JkbClient that shells out to `jkb --json`.
//
// This is the only Node-specific piece of the client. A web app would provide an
// HTTP-backed JkbClient against the same @jkb/core interface.

import { spawn } from "node:child_process";

import type {
  JkbClient,
  ListOptions,
  MutationIntent,
  NodeDetails,
  NodeRef,
  StagingBranch,
  TreeChild,
} from "@jkb/core";

export interface CliConfig {
  /** Path to the jkb binary (on PATH by default). */
  readonly cliPath: string;
  /** Optional explicit database path (passed as `--db`). */
  readonly dbPath?: string | undefined;
}

/** Raw `jkb ls` child shape. */
interface RawChild {
  kind: string;
  ref: string;
  label: string;
  has_children: boolean;
  leaf_count: number | null;
  leaf_kinds: Record<string, number> | null;
  type: string | null;
  type_about: string | null;
  chunk_count: number | null;
  subtask_count: number | null;
  open_subtask_count: number | null;
  status: string | null;
  priority: number | null;
}

/** Raw `jkb query` result row. */
interface RawQueryItem {
  uid: string;
  kind: string;
  status: string | null;
  priority: number | null;
  snippet: string | null;
}

/** Raw `jkb item show` shape. */
interface RawItem {
  uid: string;
  kind: string;
  status: string | null;
  priority: number | null;
  due: string | null;
  mime: string | null;
  binding: string | null;
  namespace: string | null;
  content_chars: number;
  content_hash: string | null;
  created_at: string;
  updated_at: string;
  tags: { facet: string; value: string }[];
  preview: string;
  preview_truncated: boolean;
}

/** `jkb task work --json`: the isolated checkout a task is being worked in (design D36). */
export interface SessionInfo {
  readonly uid: string;
  readonly session: string;
  readonly worktree: string;
  readonly branch: string;
  readonly onto: string;
  readonly resumed: boolean;
}

/** `jkb task add --json`: the task that was created. */
export interface CreatedTask {
  readonly uid: string;
  readonly home: string;
}

export class CliJkbClient implements JkbClient {
  constructor(private readonly cfg: CliConfig) {}

  /**
   * Create a task from a raw **quick-add line**, so `!p1 @2026-08-12 #area=ui` behave exactly
   * as they do in the terminal (design D38.7). Accepting only a title would be a second,
   * poorer task grammar living in the UI.
   *
   * `home` places it in a namespace; `under` makes it a subtask of a task, inheriting that
   * task's home.
   *
   * The home goes through `--home`, never as a `+<ns>` token appended to the line: quick-add
   * re-tokenizes on whitespace, so a namespace containing a space — which a synced directory
   * named `my change` produces — would silently create a different namespace and swallow the
   * rest of the path into the title. The path here comes from a clicked tree node, not from
   * someone typing, so there is nothing to lex.
   */
  async addTask(text: string, opts: { home?: string; under?: string }): Promise<CreatedTask> {
    const args = ["--global", "task", "add", text];
    if (opts.under) args.push("--under", opts.under);
    else if (opts.home) args.push("--home", opts.home);
    return this.json<CreatedTask>(args);
  }

  /**
   * The staging branches in this repo and what is landing on each — the one read behind both
   * the branch picker and the In Flight view, so the two cannot disagree (design D38.2).
   */
  async staging(cwd: string, includeMerged = false): Promise<StagingBranch[]> {
    const args = ["staging", "ls"];
    if (includeMerged) args.push("--all");
    return this.json<StagingBranch[]>(args, cwd);
  }

  /**
   * Open (or return) the task's session — its own git worktree and branch, claimed so no
   * other terminal or swarm run starts the same task. Must run inside the repo, which is
   * why this one takes a cwd: sessions are git state, not KB state.
   */
  async openSession(uid: string, cwd: string, onto?: string): Promise<SessionInfo> {
    const args = ["task", "work", uid];
    // Omitting `--onto` is not the same as passing a value: it keeps `resolve_onto`'s
    // fallback chain, which is what "Let jkb decide" means in the picker (design D38.3).
    if (onto) args.push("--onto", onto);
    return this.json<SessionInfo>(args, cwd);
  }

  /**
   * The shell command line for `args`, carrying the same `cliPath`/`dbPath` every spawned
   * call uses.
   *
   * For commands that must run in a terminal the user watches rather than be captured — a
   * landing runs the repo's build. Composing the line by hand instead would silently target
   * the default database, marking tasks done in a KB the explorer is not even showing.
   */
  terminalCommand(args: string[]): string {
    const parts = [this.cfg.cliPath];
    if (this.cfg.dbPath) parts.push("--db", this.cfg.dbPath);
    parts.push(...args);
    return parts.map(shellQuote).join(" ");
  }

  async listChildren(ref: NodeRef | null, opts?: ListOptions): Promise<TreeChild[]> {
    // Containment is a behaviour, not a node kind: `jkb ls` lists the children of a pure
    // namespace or of a parent task alike, so the client passes the node's address and does
    // not branch. A node that contains nothing is never asked (see the tree's hasChildren
    // guard).
    const args = ref ? ["ls", ref.kind === "item" ? ref.uid : ref.path] : ["ls"];
    if (opts?.includeTerminal) args.push("--all");
    const out = await this.json<{ children: RawChild[] }>(args);
    return out.children.map(toTreeChild);
  }

  async getDetails(ref: NodeRef): Promise<NodeDetails> {
    if (ref.kind === "namespace") {
      const out = await this.json<{ children: RawChild[] }>(["ls", ref.path]);
      const breakdown: Record<string, number> = {};
      for (const c of out.children) breakdown[c.kind] = (breakdown[c.kind] ?? 0) + 1;
      return { kind: "namespace", path: ref.path, childCount: out.children.length, breakdown };
    }
    const d = await this.json<RawItem>(["item", "show", ref.uid]);
    return {
      kind: "item",
      uid: d.uid,
      itemKind: d.kind,
      status: d.status,
      priority: d.priority,
      due: d.due,
      mime: d.mime,
      binding: d.binding,
      namespace: d.namespace,
      contentChars: d.content_chars,
      contentHash: d.content_hash,
      createdAt: d.created_at,
      updatedAt: d.updated_at,
      tags: d.tags.map((t) => ({ facet: t.facet, value: t.value })),
      preview: d.preview,
      previewTruncated: d.preview_truncated,
    };
  }

  async mutate(intent: MutationIntent): Promise<void> {
    await this.run(intentToArgs(intent));
  }

  async search(query: string): Promise<TreeChild[]> {
    const rows = await this.json<RawQueryItem[]>(["query", query, "--global"]);
    return rows.map((r) => ({
      ref: { kind: "item", uid: r.uid, itemKind: r.kind },
      label: r.snippet ?? r.uid,
      hasChildren: false,
      status: r.status,
      priority: r.priority,
    }));
  }

  // ---- process plumbing ----

  private async json<T>(args: string[], cwd?: string): Promise<T> {
    const out = await this.run([...args, "--json"], cwd);
    return JSON.parse(out) as T;
  }

  private run(args: string[], cwd?: string): Promise<string> {
    const full: string[] = [];
    if (this.cfg.dbPath) full.push("--db", this.cfg.dbPath);
    full.push(...args);
    return new Promise((resolve, reject) => {
      const child = spawn(this.cfg.cliPath, full, cwd ? { cwd } : {});
      let stdout = "";
      let stderr = "";
      child.stdout.on("data", (d) => (stdout += d));
      child.stderr.on("data", (d) => (stderr += d));
      child.on("error", (e) =>
        reject(new Error(`could not run '${this.cfg.cliPath}': ${e.message}`)),
      );
      child.on("close", (code) => {
        if (code === 0) resolve(stdout);
        else reject(new Error(stderr.trim() || `jkb exited with code ${code ?? "?"}`));
      });
    });
  }
}

/** Single-quote a string for safe use in a POSIX shell. */
function shellQuote(s: string): string {
  return `'${s.replace(/'/g, "'\\''")}'`;
}

function toTreeChild(c: RawChild): TreeChild {
  const ref: NodeRef =
    c.kind === "namespace"
      ? { kind: "namespace", path: c.ref }
      : { kind: "item", uid: c.ref, itemKind: c.kind };
  return {
    ref,
    label: c.label,
    hasChildren: c.has_children,
    leafCount: c.leaf_count,
    leafKinds: c.leaf_kinds,
    nsType: c.type,
    nsTypeAbout: c.type_about,
    chunkCount: c.chunk_count,
    subtaskCount: c.subtask_count,
    openSubtaskCount: c.open_subtask_count,
    status: c.status,
    priority: c.priority,
  };
}

/** Map an edit intent to an audited `jkb` command line. */
function intentToArgs(i: MutationIntent): string[] {
  switch (i.type) {
    case "setTaskStatus":
      return ["task", "set", i.uid, "--status", i.status];
    case "setTaskPriority":
      return ["task", "set", i.uid, "--priority", String(i.priority)];
    case "setTaskDue":
      return ["task", "set", i.uid, "--due", i.due];
    case "setTaskTitle":
      return ["task", "edit", i.uid, i.title];
    case "addTaskTag":
      return ["task", "tag", "add", i.uid, `${i.facet}=${i.value}`];
    case "renameNamespace":
      return ["ns", "mv", i.from, i.to];
    case "setItemContent":
      return ["item", "edit", i.uid, i.content];
  }
}
