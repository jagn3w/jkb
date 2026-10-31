//! What fills the Workflows tab's side panel, as plain data, so its rules are tested without a window.

import type { MachineSelection, RoleInfo, RolesView } from "@jkb/core";

/** An answer being read, read, or refused. */
export type Loaded<T> =
  | { readonly kind: "loading" }
  | { readonly kind: "loaded"; readonly value: T }
  | { readonly kind: "failed"; readonly message: string };

/** What fills the side panel: the selected agent, a role, or a part of the Lifecycle pane's picture. */
export type Side = { readonly kind: "agent" } | { readonly kind: "role"; readonly name: string } | MachineSelection;

export const AGENT: Side = { kind: "agent" };

/** What the Agents pane shows: an agent set's agents as a graph, or every role. */
export type AgentsView = "agents" | "roles";

/** The side panel's machine selection, if that is what it holds. */
export const machinePart = (side: Side): MachineSelection | undefined => (side.kind === "agent" || side.kind === "role" ? undefined : side);

/** The side panel after the Agents pane switches view: back on Agents, a role gives way to the agent. */
export function sideForView(side: Side, view: AgentsView): Side {
  return view === "agents" && side.kind === "role" ? AGENT : side;
}

/**
 * What a role selection draws: nothing (no role is selected), its panel, or — while the roles are
 * read, when reading them failed, or when the strategy has no such role — a placeholder saying so,
 * which can be closed. A selected role never leaves the side panel blank.
 */
export type RoleSide =
  | { readonly kind: "none" }
  | { readonly kind: "shown"; readonly view: RolesView; readonly role: RoleInfo }
  | { readonly kind: "pending"; readonly name: string; readonly message: string };

export function roleSide(side: Side, roles: Loaded<RolesView>): RoleSide {
  if (side.kind !== "role") return { kind: "none" };
  const name = side.name;
  switch (roles.kind) {
    case "loading":
      return { kind: "pending", name, message: "Loading the roles…" };
    case "failed":
      return { kind: "pending", name, message: `Cannot list the roles: ${roles.message}` };
    case "loaded": {
      const role = roles.value.roles.find((r) => r.name === name);
      return role === undefined
        ? { kind: "pending", name, message: `${roles.value.strategy} has no role ${name}.` }
        : { kind: "shown", view: roles.value, role };
    }
  }
}
