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

export class CliJkbClient implements JkbClient {
  constructor(private readonly cfg: CliConfig) {}

  async listChildren(ref: NodeRef | null, opts?: ListOptions): Promise<TreeChild[]> {
    // A namespace expands to `jkb ls`; a task expands to its subtasks. Both commands emit
    // the same `{children}` shape, so the two node kinds cost a different command rather
    // than a different model.
    const args =
      ref && ref.kind === "item"
        ? ["task", "subtasks", ref.uid]
        : ref
          ? ["ls", ref.path]
          : ["ls"];
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

  private async json<T>(args: string[]): Promise<T> {
    const out = await this.run([...args, "--json"]);
    return JSON.parse(out) as T;
  }

  private run(args: string[]): Promise<string> {
    const full: string[] = [];
    if (this.cfg.dbPath) full.push("--db", this.cfg.dbPath);
    full.push(...args);
    return new Promise((resolve, reject) => {
      const child = spawn(this.cfg.cliPath, full);
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
