//! The Workflows tab's data as the ops answer it (D53.7), minus the window: agent templates
//! (`workflow.agents` / `workflow.agent` / `workflow.agent_copy` / `workflow.agent_set`), a
//! strategy's machines (`workflow.graph`), and the two layouts the tab draws — the agent graph from
//! the templates' hand-offs, and a machine from its compiled table.
//
// The Rust side is the source of truth: `crates/jkb-api/src/workflows.rs` (the shapes) and
// `crates/jkb-core/src/workflow/agents.rs` (the rules: read-only packaged templates, copies that
// override them, placeholders). Nothing here re-decides any of it.

import type { OpResponse, Outcome } from "./daemon.js";
import { failed } from "./daemon.js";

export type Isolation = "none" | "worktree";
export type Writes = "nothing" | "kb" | "git" | "code";

export const ISOLATIONS: readonly Isolation[] = ["none", "worktree"];
export const WRITES: readonly Writes[] = ["nothing", "kb", "git", "code"];
/** `jkb_core::roles::Role`, as stored. */
export const ROLES = ["operator", "coordinator", "designer", "implementer", "reviewer", "systemic_reviewer"] as const;
export type RoleName = (typeof ROLES)[number];

/** `jkb_core::workflow::agents::AgentPermissions`. */
export interface AgentPermissions {
  readonly isolation: Isolation;
  /** `null`: the session's own model. */
  readonly model: string | null;
  readonly writes: Writes;
}

/** `jkb_api::workflows::AgentView`: one template. */
export interface AgentTemplate {
  readonly name: string;
  readonly version: number;
  /** `packaged` is read-only; `operator` is an editable copy. */
  readonly source: "packaged" | "operator";
  readonly workflow: string;
  readonly role: string;
  /** The jkb op classes the role may run. */
  readonly role_ops: readonly string[];
  /** A piece other templates include; not an agent the graph draws. */
  readonly fragment: boolean;
  readonly describe: string;
  readonly template: string;
  readonly placeholders: readonly string[];
  readonly permissions: AgentPermissions;
  readonly hands_off_to: readonly string[];
  readonly based_on: string | null;
  readonly defined_at: string | null;
  readonly packaged_version: number | null;
  readonly overrides_packaged: boolean;
  readonly behind_packaged: boolean;
}

/** `jkb_api::workflows::GraphState`. */
export interface GraphState {
  readonly name: string;
  readonly initial: boolean;
  readonly settled: boolean;
  readonly awaits_input: boolean;
  readonly next_role: string | null;
  readonly next_step: string | null;
}

/** `jkb_api::workflows::GraphEdge`. */
export interface GraphEdge {
  readonly from: string;
  readonly event: string;
  /** `null`: the caller names the destination (an operator override). */
  readonly to: string | null;
  readonly reconciled: boolean;
  readonly guarded: boolean;
  readonly planned: boolean;
  /** Who the strategy lets fire it; empty for an observation (anyone's; its guard decides). */
  readonly roles: readonly string[];
}

/** `jkb_api::workflows::MachineView`. */
export interface MachineTable {
  readonly states: readonly GraphState[];
  readonly transitions: readonly GraphEdge[];
}

/** `jkb_api::workflows::GraphView`: `workflow.graph`'s answer. */
export interface WorkflowGraph {
  readonly strategy: string;
  readonly graph: string;
  readonly describe: string;
  readonly task: string | null;
  readonly phase: string | null;
  readonly status: string | null;
  readonly workflow: MachineTable;
  readonly lifecycle: MachineTable;
}

/** What a save changes: `jkb_api::workflows::AgentEdit`. */
export interface AgentEdit {
  readonly template?: string;
  readonly role?: string;
  readonly describe?: string;
  readonly permissions?: AgentPermissions;
  readonly hands_off_to?: readonly string[];
}

// ---- requests ---------------------------------------------------------------------------------

/** The requests the Workflows tab makes, as `jkb_api::Request` serializes them. */
export const workflowOps = {
  agents: () => ({ op: "workflow.agents" }),
  agent: (name: string, packaged = false) => ({ op: "workflow.agent", name, packaged }),
  /** An operator copy of `from` under its own name (overriding the packaged one), or `as`. */
  copy: (from: string, opts: { readonly packaged?: boolean; readonly as?: string } = {}) => ({
    op: "workflow.agent_copy",
    from,
    packaged: opts.packaged ?? false,
    ...(opts.as === undefined ? {} : { as: opts.as }),
  }),
  set: (name: string, edit: AgentEdit) => ({ op: "workflow.agent_set", name, edit }),
  /** A named strategy's machines, or the default's. */
  graph: (strategy?: string) => (strategy === undefined ? { op: "workflow.graph" } : { op: "workflow.graph", strategy }),
} as const;

/** `jkb_core::workflow::agents::validate_name`: what a copy may be called. */
export function isAgentName(name: string): boolean {
  return /^[a-z][a-z0-9-]{0,63}$/.test(name);
}

/**
 * What a save sends: only the fields that differ from `current`, so an unchanged field never
 * travels. Empty when nothing changed (a save then has nothing to do).
 */
export function editBetween(current: AgentTemplate, draft: AgentDraft): AgentEdit {
  const out: { -readonly [K in keyof AgentEdit]: AgentEdit[K] } = {};
  if (draft.template !== current.template) out.template = draft.template;
  if (draft.role !== current.role) out.role = draft.role;
  if (draft.describe !== current.describe) out.describe = draft.describe;
  const p = draft.permissions;
  const q = current.permissions;
  if (p.isolation !== q.isolation || p.model !== q.model || p.writes !== q.writes) out.permissions = p;
  if (draft.hands_off_to.join(",") !== current.hands_off_to.join(",")) out.hands_off_to = draft.hands_off_to;
  return out;
}

/** The fields the side panel edits. */
export interface AgentDraft {
  readonly template: string;
  readonly role: string;
  readonly describe: string;
  readonly permissions: AgentPermissions;
  readonly hands_off_to: readonly string[];
}

export function draftOf(a: AgentTemplate): AgentDraft {
  return {
    template: a.template,
    role: a.role,
    describe: a.describe,
    permissions: a.permissions,
    hands_off_to: a.hands_off_to,
  };
}

/** The placeholders a template names (`{{name}}`), sorted, as jkb finds them; malformed ones are jkb's to refuse. */
export function placeholdersOf(template: string): string[] {
  const names = new Set<string>();
  for (const m of template.matchAll(/\{\{([a-z_][a-z0-9_]*)\}\}/g)) names.add(m[1] as string);
  return [...names].sort();
}

// ---- decoding answers -------------------------------------------------------------------------

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}
const isString = (v: unknown): v is string => typeof v === "string";
const isOptString = (v: unknown): v is string | null => v === null || v === undefined || isString(v);
const isNumber = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);
const isBool = (v: unknown): v is boolean => typeof v === "boolean";
const isStrings = (v: unknown): v is string[] => Array.isArray(v) && v.every(isString);

function isPermissions(v: unknown): v is AgentPermissions {
  return (
    isObject(v) &&
    (ISOLATIONS as readonly unknown[]).includes(v["isolation"]) &&
    (WRITES as readonly unknown[]).includes(v["writes"]) &&
    isOptString(v["model"])
  );
}

function isAgent(v: unknown): v is AgentTemplate {
  return (
    isObject(v) &&
    isString(v["name"]) &&
    isNumber(v["version"]) &&
    (v["source"] === "packaged" || v["source"] === "operator") &&
    isString(v["workflow"]) &&
    isString(v["role"]) &&
    isStrings(v["role_ops"]) &&
    isBool(v["fragment"]) &&
    isString(v["describe"]) &&
    isString(v["template"]) &&
    isStrings(v["placeholders"]) &&
    isPermissions(v["permissions"]) &&
    isStrings(v["hands_off_to"]) &&
    isOptString(v["based_on"]) &&
    isOptString(v["defined_at"]) &&
    (v["packaged_version"] === null || v["packaged_version"] === undefined || isNumber(v["packaged_version"])) &&
    isBool(v["overrides_packaged"]) &&
    isBool(v["behind_packaged"])
  );
}

function normal(a: AgentTemplate): AgentTemplate {
  return {
    ...a,
    based_on: a.based_on ?? null,
    defined_at: a.defined_at ?? null,
    packaged_version: a.packaged_version ?? null,
    permissions: { ...a.permissions, model: a.permissions.model ?? null },
  };
}

/** `workflow.agents`' answer. */
export function decodeAgents(o: Outcome<OpResponse>): Outcome<readonly AgentTemplate[]> {
  if (!o.ok) return o;
  const agents = o.value.result === "workflow_agents" ? o.value["agents"] : undefined;
  if (!Array.isArray(agents) || !agents.every(isAgent)) {
    return failed("internal", "jkb serve did not answer with well-formed `workflow_agents`");
  }
  return { ok: true, value: agents.map(normal) };
}

/** `workflow.agent`'s, `workflow.agent_copy`'s or `workflow.agent_set`'s answer. */
export function decodeAgent(o: Outcome<OpResponse>): Outcome<{ readonly agent: AgentTemplate; readonly wrote: boolean }> {
  if (!o.ok) return o;
  const v = o.value;
  if (v.result !== "workflow_agent" || !isAgent(v["agent"])) {
    return failed("internal", "jkb serve did not answer with a well-formed `workflow_agent`");
  }
  return { ok: true, value: { agent: normal(v["agent"]), wrote: v["wrote"] === true } };
}

function isState(v: unknown): v is GraphState {
  return (
    isObject(v) &&
    isString(v["name"]) &&
    isBool(v["initial"]) &&
    isBool(v["settled"]) &&
    isBool(v["awaits_input"]) &&
    isOptString(v["next_role"]) &&
    isOptString(v["next_step"])
  );
}

function isEdge(v: unknown): v is GraphEdge {
  return (
    isObject(v) &&
    isString(v["from"]) &&
    isString(v["event"]) &&
    isOptString(v["to"]) &&
    isBool(v["reconciled"]) &&
    isBool(v["guarded"]) &&
    isBool(v["planned"]) &&
    isStrings(v["roles"])
  );
}

function isMachine(v: unknown): v is MachineTable {
  return (
    isObject(v) &&
    Array.isArray(v["states"]) &&
    v["states"].every(isState) &&
    Array.isArray(v["transitions"]) &&
    v["transitions"].every(isEdge)
  );
}

function normalMachine(m: MachineTable): MachineTable {
  return {
    states: m.states.map((s) => ({ ...s, next_role: s.next_role ?? null, next_step: s.next_step ?? null })),
    transitions: m.transitions.map((t) => ({ ...t, to: t.to ?? null })),
  };
}

/** `workflow.graph`'s answer. */
export function decodeWorkflowGraph(o: Outcome<OpResponse>): Outcome<WorkflowGraph> {
  if (!o.ok) return o;
  const g = o.value.result === "workflow_graph" ? o.value["graph"] : undefined;
  if (
    !isObject(g) ||
    !isString(g["strategy"]) ||
    !isString(g["graph"]) ||
    !isString(g["describe"]) ||
    !isOptString(g["task"]) ||
    !isOptString(g["phase"]) ||
    !isOptString(g["status"]) ||
    !isMachine(g["workflow"]) ||
    !isMachine(g["lifecycle"])
  ) {
    return failed("internal", "jkb serve did not answer with a well-formed `workflow_graph`");
  }
  return {
    ok: true,
    value: {
      strategy: g["strategy"],
      graph: g["graph"],
      describe: g["describe"],
      task: g["task"] ?? null,
      phase: g["phase"] ?? null,
      status: g["status"] ?? null,
      workflow: normalMachine(g["workflow"]),
      lifecycle: normalMachine(g["lifecycle"]),
    },
  };
}

// ---- layouts ----------------------------------------------------------------------------------

/** A node placed on a grid: `layer` is its column, `row` its place within it. */
export interface Placed {
  readonly id: string;
  readonly layer: number;
  readonly row: number;
}

/** A directed edge between two placed nodes; `back` when it runs to an earlier or the same layer. */
export interface Link {
  readonly from: string;
  readonly to: string;
  readonly label: string;
  readonly back: boolean;
}

export interface Layout {
  readonly nodes: readonly Placed[];
  readonly links: readonly Link[];
  readonly layers: number;
  /** The most nodes in one layer. */
  readonly rows: number;
}

/**
 * Lay `ids` out in layers by shortest distance from `roots` along `edges` (breadth first), each
 * layer in the order the ids were given. A node no root reaches starts a layer of its own after the
 * rest, so nothing is dropped. Pure and deterministic: the same table always draws the same picture.
 */
export function layered(
  ids: readonly string[],
  edges: readonly { readonly from: string; readonly to: string; readonly label: string }[],
  roots: readonly string[],
): Layout {
  const known = new Set(ids);
  const depth = new Map<string, number>();
  const queue: string[] = [];
  for (const r of roots) {
    if (known.has(r) && !depth.has(r)) {
      depth.set(r, 0);
      queue.push(r);
    }
  }
  const walk = (): void => {
    while (queue.length > 0) {
      const at = queue.shift() as string;
      const d = depth.get(at) as number;
      for (const e of edges) {
        if (e.from === at && known.has(e.to) && !depth.has(e.to)) {
          depth.set(e.to, d + 1);
          queue.push(e.to);
        }
      }
    }
  };
  walk();
  for (const id of ids) {
    if (!depth.has(id)) {
      const next = Math.max(-1, ...depth.values()) + 1;
      depth.set(id, next);
      queue.push(id);
      walk();
    }
  }
  const perLayer = new Map<number, number>();
  const nodes = ids.map((id) => {
    const layer = depth.get(id) as number;
    const row = perLayer.get(layer) ?? 0;
    perLayer.set(layer, row + 1);
    return { id, layer, row };
  });
  const at = new Map(nodes.map((n) => [n.id, n]));
  const links = edges
    .filter((e) => known.has(e.from) && known.has(e.to))
    .map((e) => ({
      from: e.from,
      to: e.to,
      label: e.label,
      back: (at.get(e.to) as Placed).layer <= (at.get(e.from) as Placed).layer,
    }));
  return {
    nodes,
    links,
    layers: Math.max(0, ...nodes.map((n) => n.layer + 1)),
    rows: Math.max(0, ...perLayer.values()),
  };
}

/** The workflows the templates belong to, in first-seen order. */
export function workflowsOf(agents: readonly AgentTemplate[]): string[] {
  return [...new Set(agents.map((a) => a.workflow))];
}

/** A workflow's agent graph: its agents (fragments are not agents) and their hand-offs. */
export interface AgentGraph extends Layout {
  /** Hand-offs naming an agent that is not in this workflow: drawn nowhere, so said aloud. */
  readonly dangling: readonly { readonly from: string; readonly to: string }[];
}

export function agentGraph(agents: readonly AgentTemplate[], workflow: string): AgentGraph {
  const own = agents.filter((a) => a.workflow === workflow && !a.fragment);
  const ids = own.map((a) => a.name);
  const names = new Set(ids);
  const edges = own.flatMap((a) => a.hands_off_to.map((to) => ({ from: a.name, to, label: "" })));
  const incoming = new Set(edges.map((e) => e.to));
  const roots = ids.filter((id) => !incoming.has(id));
  const layout = layered(ids, edges, roots.length > 0 ? roots : ids.slice(0, 1));
  return {
    ...layout,
    dangling: edges.filter((e) => !names.has(e.to)).map(({ from, to }) => ({ from, to })),
  };
}

/** The pseudo-state an override's edge runs to: a destination the operator names. */
export const STATED = "(stated)";

/**
 * A machine laid out from its table: states by distance from the initial one. Self-loops and
 * overrides (`to: null`, every live state has one) are left to the caller to list rather than draw,
 * or they bury the graph.
 */
export function machineLayout(m: MachineTable): Layout {
  const ids = m.states.map((s) => s.name);
  const edges = m.transitions
    .filter((t) => t.to !== null && t.to !== t.from)
    .map((t) => ({ from: t.from, to: t.to as string, label: t.event }));
  const roots = m.states.filter((s) => s.initial).map((s) => s.name);
  return layered(ids, edges, roots);
}
