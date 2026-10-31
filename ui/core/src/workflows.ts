//! The Workflows tab's data as the ops answer it (D53.7), minus the window: agent templates
//! (`workflow.agents` / `workflow.agent` / `workflow.agent_copy` / `workflow.agent_set`), a
//! strategy's machines (`workflow.graph`), the agent graph's layout from the templates' hand-offs,
//! and what of a machine's compiled table the Lifecycle pane draws (`machineDrawing`; the app lays
//! it out, in `machineGeometry.ts`).
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
  /** The agent set it belongs to (`task-swarm`, …). The wire keeps the name `workflow`. */
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
  /** The packaged version of this name the template in effect is built on; `null` for none. */
  readonly packaged_base: number | null;
  /** An operator copy that says exactly what the packaged template says: nothing to contribute. */
  readonly matches_packaged: boolean;
}

/** `jkb_api::workflows::AgentSetView`: an agent set and the one sentence on what it is for. */
export interface AgentSetInfo {
  readonly name: string;
  readonly describe: string;
}

/**
 * `workflow.agents`' answer: every template, every role one may act as (this jkb's, never a copy),
 * and the agent sets with their sentences — empty from a daemon that predates them, and then the tab
 * shows no sentence.
 */
export interface AgentList {
  readonly agents: readonly AgentTemplate[];
  readonly roles: readonly string[];
  readonly agent_sets: readonly AgentSetInfo[];
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
  /** Who fires it, as jkb words it for every surface: `observed`, `applied`, the roles, or `no one`. */
  readonly fired_by: string;
  /** What its guard checks, in jkb's plain words; `null` for an unguarded row (or an older jkb). */
  readonly guard: string | null;
  /** The command that fires it, `<uid>` standing for the task; `null` where jkb names none. */
  readonly command: string | null;
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
  /** One sentence on the graph: why a strategy runs it. */
  readonly describe: string;
  /** The phases it passes through, as one arrow line; `null` from a daemon that predates it. */
  readonly path: string | null;
  readonly task: string | null;
  readonly phase: string | null;
  readonly status: string | null;
  readonly workflow: MachineTable;
  readonly lifecycle: MachineTable;
  /** Every role's jkb op classes (`jkb role matrix`); empty from an older jkb. */
  readonly role_ops: Readonly<Record<string, readonly string[]>>;
  /** The strategy an unpinned task runs; `null` from a daemon that predates it. */
  readonly default: string | null;
  /** What this strategy changes from the default, one line each; `null` (unknown) from an older daemon. */
  readonly changes_from_default: readonly string[] | null;
}

/** `jkb_api::workflows::OpClassView`: an op class and what holding it lets a role do. */
export interface OpClass {
  readonly name: string;
  readonly describe: string;
}

/** `jkb_api::workflows::ToggleView`: a strategy toggle and what the strategy sets it to. */
export interface ToggleInfo {
  readonly name: string;
  /** What it grants, in a few plain words. */
  readonly label: string;
  /** The workflow events it grants, or `["land"]`. */
  readonly controls: readonly string[];
  /** The roles it may ever name. */
  readonly domain: readonly string[];
  readonly acts_in: string | null;
  /** The roles the strategy names, as stored (the operator always holds it). */
  readonly value: readonly string[];
}

/** `jkb_api::workflows::RoleView`: one role under a strategy. */
export interface RoleInfo {
  readonly name: string;
  readonly describe: string;
  readonly ops: readonly OpClass[];
  readonly may_grant: readonly string[];
  readonly granted_by: readonly string[];
  /** The workflow acts it may fire under the strategy. */
  readonly events: readonly string[];
  readonly lands: boolean;
  readonly acts_next_in: readonly string[];
  readonly fires_in: readonly string[];
  /** The toggles that can name it; `on` when the strategy does. */
  readonly toggled_by: readonly { readonly toggle: string; readonly on: boolean }[];
  /** The attested agent types mapped to it. */
  readonly agent_types: readonly string[];
}

/** `jkb_api::workflows::RolesView`: `workflow.roles`' answer. */
export interface RolesView {
  readonly strategy: string;
  readonly toggles: readonly ToggleInfo[];
  /** Every role, in jkb's order, including those no agent acts as. */
  readonly roles: readonly RoleInfo[];
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
  /**
   * An operator copy of `from` under its own name (overriding the packaged one), or `as`; with
   * `edit`, applied to the copied text in the same write (a refused edit leaves no copy).
   */
  copy: (from: string, opts: { readonly packaged?: boolean; readonly as?: string; readonly edit?: AgentEdit } = {}) => ({
    op: "workflow.agent_copy",
    from,
    packaged: opts.packaged ?? false,
    ...(opts.as === undefined ? {} : { as: opts.as }),
    ...(opts.edit === undefined ? {} : { edit: opts.edit }),
  }),
  set: (name: string, edit: AgentEdit) => ({ op: "workflow.agent_set", name, edit }),
  /** A named strategy's machines, or the default's. */
  graph: (strategy?: string) => (strategy === undefined ? { op: "workflow.graph" } : { op: "workflow.graph", strategy }),
  /** Every role under a named strategy, or the default's. */
  roles: (strategy?: string) => (strategy === undefined ? { op: "workflow.roles" } : { op: "workflow.roles", strategy }),
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

/**
 * The one request a save makes, or `undefined` when there is nothing to save. A packaged template is
 * read-only in jkb, so its save is a copy carrying the edit — one op, validated whole, never a copy
 * followed by an edit that could be refused and leave the copy behind. A copy is edited as itself.
 */
export function saveRequest(current: AgentTemplate, draft: AgentDraft) {
  const edit = editBetween(current, draft);
  const changed = Object.keys(edit).length > 0;
  if (current.source === "packaged") return workflowOps.copy(current.name, changed ? { edit } : {});
  return changed ? workflowOps.set(current.name, edit) : undefined;
}

/** Contribute has something to offer: an operator copy that is not just the packaged text. */
export function canContribute(a: AgentTemplate): boolean {
  return a.source === "operator" && !a.matches_packaged;
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
    isBool(v["behind_packaged"]) &&
    (v["packaged_base"] === null || v["packaged_base"] === undefined || isNumber(v["packaged_base"])) &&
    (v["matches_packaged"] === undefined || isBool(v["matches_packaged"]))
  );
}

function normal(a: AgentTemplate): AgentTemplate {
  return {
    ...a,
    based_on: a.based_on ?? null,
    defined_at: a.defined_at ?? null,
    packaged_version: a.packaged_version ?? null,
    packaged_base: a.packaged_base ?? null,
    matches_packaged: a.matches_packaged ?? false,
    permissions: { ...a.permissions, model: a.permissions.model ?? null },
  };
}

/** `workflow.agents`' answer. */
export function decodeAgents(o: Outcome<OpResponse>): Outcome<AgentList> {
  if (!o.ok) return o;
  const ours = o.value.result === "workflow_agents";
  const agents = ours ? o.value["agents"] : undefined;
  const roles = ours ? o.value["roles"] : undefined;
  const sets = ours ? o.value["agent_sets"] : undefined;
  const setsOk = sets === undefined || (Array.isArray(sets) && sets.every((s) => isObject(s) && isString(s["name"]) && isString(s["describe"])));
  if (!Array.isArray(agents) || !agents.every(isAgent) || !isStrings(roles) || !setsOk) {
    return failed("internal", "jkb serve did not answer with well-formed `workflow_agents`");
  }
  const agentSets = ((sets ?? []) as AgentSetInfo[]).map(({ name, describe }) => ({ name, describe }));
  return { ok: true, value: { agents: agents.map(normal), roles, agent_sets: agentSets } };
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
    isStrings(v["roles"]) &&
    isString(v["fired_by"]) &&
    isOptString(v["guard"]) &&
    isOptString(v["command"])
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
    transitions: m.transitions.map((t) => ({ ...t, to: t.to ?? null, guard: t.guard ?? null, command: t.command ?? null })),
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
    !isOptString(g["path"]) ||
    !isOptString(g["task"]) ||
    !isOptString(g["phase"]) ||
    !isOptString(g["status"]) ||
    !isMachine(g["workflow"]) ||
    !isMachine(g["lifecycle"]) ||
    !(g["role_ops"] === undefined || (isObject(g["role_ops"]) && Object.values(g["role_ops"]).every(isStrings))) ||
    !(g["default"] === undefined || isString(g["default"])) ||
    !(g["changes_from_default"] === undefined || isStrings(g["changes_from_default"]))
  ) {
    return failed("internal", "jkb serve did not answer with a well-formed `workflow_graph`");
  }
  return {
    ok: true,
    value: {
      strategy: g["strategy"],
      graph: g["graph"],
      describe: g["describe"],
      path: g["path"] ?? null,
      task: g["task"] ?? null,
      phase: g["phase"] ?? null,
      status: g["status"] ?? null,
      workflow: normalMachine(g["workflow"]),
      lifecycle: normalMachine(g["lifecycle"]),
      role_ops: (g["role_ops"] as Record<string, string[]> | undefined) ?? {},
      // Both or neither: a daemon that sends one sends the other, and a default without its diff
      // (or the reverse) is not an answer the legend can word.
      ...(isString(g["default"]) && g["default"] !== "" && isStrings(g["changes_from_default"])
        ? { default: g["default"], changes_from_default: g["changes_from_default"] }
        : { default: null, changes_from_default: null }),
    },
  };
}

function isRole(v: unknown): v is RoleInfo {
  return (
    isObject(v) &&
    isString(v["name"]) &&
    isString(v["describe"]) &&
    Array.isArray(v["ops"]) &&
    v["ops"].every((o) => isObject(o) && isString(o["name"]) && isString(o["describe"])) &&
    isStrings(v["may_grant"]) &&
    isStrings(v["granted_by"]) &&
    isStrings(v["events"]) &&
    isBool(v["lands"]) &&
    isStrings(v["acts_next_in"]) &&
    isStrings(v["fires_in"]) &&
    Array.isArray(v["toggled_by"]) &&
    v["toggled_by"].every((t) => isObject(t) && isString(t["toggle"]) && isBool(t["on"])) &&
    isStrings(v["agent_types"])
  );
}

function isToggle(v: unknown): v is ToggleInfo {
  return (
    isObject(v) &&
    isString(v["name"]) &&
    isString(v["label"]) &&
    isStrings(v["controls"]) &&
    isStrings(v["domain"]) &&
    isOptString(v["acts_in"]) &&
    isStrings(v["value"])
  );
}

/** `workflow.roles`' answer. */
export function decodeRoles(o: Outcome<OpResponse>): Outcome<RolesView> {
  if (!o.ok) return o;
  const v = o.value.result === "workflow_roles" ? o.value["roles"] : undefined;
  if (
    !isObject(v) ||
    !isString(v["strategy"]) ||
    !Array.isArray(v["toggles"]) ||
    !v["toggles"].every(isToggle) ||
    !Array.isArray(v["roles"]) ||
    !v["roles"].every(isRole)
  ) {
    return failed("internal", "jkb serve did not answer with a well-formed `workflow_roles`");
  }
  return {
    ok: true,
    value: {
      strategy: v["strategy"],
      toggles: (v["toggles"] as ToggleInfo[]).map((t) => ({ ...t, acts_in: t.acts_in ?? null })),
      roles: v["roles"] as RoleInfo[],
    },
  };
}

/** The agent templates that act as `role`, in any agent set (fragments are not agents). */
export function templatesActingAs(agents: readonly AgentTemplate[], role: string): AgentTemplate[] {
  return agents.filter((a) => a.role === role && !a.fragment);
}

/**
 * How a role's grant from a toggle reads: for each toggle that can name the role, what it controls
 * that this role holds (`on`) or would hold (`off in this strategy`). A grant no toggle controls has
 * no entry.
 */
export interface ToggleMark {
  readonly toggle: string;
  readonly label: string;
  readonly on: boolean;
  /** The events (or `land`) it controls. */
  readonly controls: readonly string[];
}

export function toggleMarks(view: RolesView, role: RoleInfo): ToggleMark[] {
  return role.toggled_by.flatMap(({ toggle, on }) => {
    const t = view.toggles.find((x) => x.name === toggle);
    return t === undefined ? [] : [{ toggle, label: t.label, on, controls: t.controls }];
  });
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

/** The sentence describing agent set `name`, if the daemon sent one. */
export function agentSetSentence(sets: readonly AgentSetInfo[], name: string | undefined): string | undefined {
  return name === undefined ? undefined : sets.find((s) => s.name === name)?.describe;
}

/** The agent sets the templates belong to (each template's `workflow` field), in first-seen order. */
export function agentSetsOf(agents: readonly AgentTemplate[]): string[] {
  return [...new Set(agents.map((a) => a.workflow))];
}

/** An agent set's agent graph: its agents (fragments are not agents) and their hand-offs. */
export interface AgentGraph extends Layout {
  /** Hand-offs naming an agent that is not in this agent set: drawn nowhere, so said aloud. */
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

/** Rows of one observed event reaching one state, from two or more states: drawn as one marker. */
export interface ObservedGroup {
  readonly event: string;
  /** The per-state rows, in table order; each keeps its own `guarded` flag. */
  readonly rows: readonly GraphEdge[];
}

/** One marker on a target state: every collapsed observation that reaches it. */
export interface ObservedMarker {
  /** The target state. */
  readonly to: string;
  /** One per event that reaches it, in table order. */
  readonly groups: readonly ObservedGroup[];
}

/** What the Lifecycle pane draws of a machine, and how. */
export interface MachineDrawing {
  readonly states: readonly GraphState[];
  /** Drawn as arrows. */
  readonly edges: readonly GraphEdge[];
  /** Drawn as one marker per target state. */
  readonly markers: readonly ObservedMarker[];
  /** Self-loops and overrides (`to: null`): listed, never drawn, or they bury the graph. */
  readonly listed: readonly GraphEdge[];
}

/** The distinct source states of a marker's rows: the `N` of `observed ×N`. */
export function markerSources(m: ObservedMarker): string[] {
  return [...new Set(m.groups.flatMap((g) => g.rows.map((r) => r.from)))];
}

/**
 * Split a machine's table into what is drawn. An observation (`reconciled`) that reaches one state
 * by one event from two or more distinct states — `observed_landed` from every live phase — is one
 * marker on that state rather than one arrow per phase, which would be most of the picture. One
 * from a single state stays an arrow. Self-loops and overrides are listed. Every transition lands in
 * exactly one of `edges`, `markers[].groups[].rows` or `listed`, each in table order.
 */
export function machineDrawing(m: MachineTable): MachineDrawing {
  const drawable = (t: GraphEdge): t is GraphEdge & { readonly to: string } => t.to !== null && t.to !== t.from;
  const key = (to: string, event: string): string => JSON.stringify([to, event]);
  const sources = new Map<string, Set<string>>();
  for (const t of m.transitions) {
    if (t.reconciled && drawable(t)) {
      const k = key(t.to, t.event);
      const s = sources.get(k) ?? new Set<string>();
      s.add(t.from);
      sources.set(k, s);
    }
  }
  const edges: GraphEdge[] = [];
  const listed: GraphEdge[] = [];
  const markers = new Map<string, Map<string, GraphEdge[]>>();
  for (const t of m.transitions) {
    if (!drawable(t)) {
      listed.push(t);
    } else if (t.reconciled && (sources.get(key(t.to, t.event))?.size ?? 0) >= 2) {
      const groups = markers.get(t.to) ?? new Map<string, GraphEdge[]>();
      const rows = groups.get(t.event) ?? [];
      rows.push(t);
      groups.set(t.event, rows);
      markers.set(t.to, groups);
    } else {
      edges.push(t);
    }
  }
  return {
    states: m.states,
    edges,
    markers: [...markers].map(([to, groups]) => ({
      to,
      groups: [...groups].map(([event, rows]) => ({ event, rows })),
    })),
    listed,
  };
}

/** The two machines a strategy draws: its workflow (phases) and the task lifecycle (statuses). */
export type MachineName = "workflow" | "lifecycle";

/** What of a drawn machine the Lifecycle pane's side panel shows: a state, a drawn edge, or a marker. */
export type MachineSelection =
  | { readonly kind: "state"; readonly machine: MachineName; readonly name: string }
  | { readonly kind: "edge"; readonly machine: MachineName; readonly from: string; readonly event: string }
  | { readonly kind: "marker"; readonly machine: MachineName; readonly to: string };

/** A state's rows, as its panel lists them. */
export interface StateRows {
  /** The drawn edges out of it. */
  readonly leaves: readonly GraphEdge[];
  /** The drawn edges into it, then the rows its marker collapsed. */
  readonly arrives: readonly GraphEdge[];
  /** Its self-loops and its override: rows the picture does not draw. */
  readonly listed: readonly GraphEdge[];
}

/**
 * A state's rows, each in table order: what leaves it and what arrives along the drawn edges and its
 * marker, and its rows the picture does not draw. With the table gone this is where rows are read,
 * so every transition is in some state's lists (a collapsed row in its target's `arrives`).
 */
export function stateRows(d: MachineDrawing, name: string): StateRows {
  return {
    leaves: d.edges.filter((t) => t.from === name),
    arrives: [
      ...d.edges.filter((t) => t.to === name),
      ...d.markers.filter((m) => m.to === name).flatMap((m) => m.groups.flatMap((g) => g.rows)),
    ],
    listed: d.listed.filter((t) => t.from === name),
  };
}

/** Whether `sel` still names something drawn in `d`: after a strategy switch it may not. */
export function selectionIn(d: MachineDrawing, sel: MachineSelection): boolean {
  switch (sel.kind) {
    case "state":
      return d.states.some((s) => s.name === sel.name);
    case "edge":
      return d.edges.some((t) => t.from === sel.from && t.event === sel.event);
    case "marker":
      return d.markers.some((m) => m.to === sel.to);
  }
}

/** Whether two selections name the same part of the same machine. */
export function sameSelection(a: MachineSelection | undefined, b: MachineSelection | undefined): boolean {
  if (a === undefined || b === undefined) return a === b;
  if (a.machine !== b.machine) return false;
  switch (a.kind) {
    case "state":
      return b.kind === "state" && a.name === b.name;
    case "edge":
      return b.kind === "edge" && a.from === b.from && a.event === b.event;
    case "marker":
      return b.kind === "marker" && a.to === b.to;
  }
}

/** The events `role` may fire under the strategy (each row's `roles`), once each, in table order. */
export function eventsFiredBy(m: MachineTable, role: string): string[] {
  return [...new Set(m.transitions.filter((t) => t.roles.includes(role)).map((t) => t.event))];
}

/** A row's key, as the table keys rows: the state it applies in and the event it answers. */
export const rowKey = (from: string, event: string): string => `${from}\u0000${event}`;

/**
 * How often a task took each row, from the moves its history made (`timeline.task`'s `visited`): an
 * override is one row with several destinations, so its moves are summed.
 */
export function takenByRow(moves: readonly { readonly from: string; readonly event: string; readonly count: number }[]): Map<string, number> {
  const counts = new Map<string, number>();
  for (const v of moves) counts.set(rowKey(v.from, v.event), (counts.get(rowKey(v.from, v.event)) ?? 0) + v.count);
  return counts;
}
