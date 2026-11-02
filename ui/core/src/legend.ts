//! The Workflows tab's legend: the model's words, one line each, and what the drawn strategy
//! changes from the default. The terms are the one copy every surface reads; the changes are jkb's
//! own lines (`StrategySpec::changes_from`), never recomputed here.

import { sameStrategy, strategyLabel } from "./plan.js";
import type { WorkflowGraph } from "./workflows.js";

/** One term of the legend and its one-line definition. */
export interface WorkflowTerm {
  readonly term: string;
  readonly line: string;
}

/** The words the Workflows tab draws with, in the order the legend lists them. */
export const WORKFLOW_TERMS: readonly WorkflowTerm[] = [
  {
    term: "Role",
    line: "What jkb lets a caller do (operator, coordinator, designer, implementer, reviewer, systemic_reviewer); jkb enforces it on every op.",
  },
  { term: "Agent", line: "A prompt template a coordinator session gives itself or a worker; it acts as one role." },
  { term: "Agent set", line: "The agents one coordinator script runs together (task-swarm, code-review)." },
  { term: "Graph", line: "The phases a task passes through and the events that move it." },
  {
    term: "Strategy",
    line: "A graph plus who may approve designs and who may land, and the review knobs; a task is pinned to one, or follows the default.",
  },
];

/**
 * What the legend says about the drawn strategy against the default:
 * - `unknown`: the daemon predates `changes_from_default`, so that half of the legend is hidden;
 * - `default`: the drawn strategy is the default;
 * - `same`: another strategy that grants and checks what the default does;
 * - `changes`: jkb's lines, one per change.
 */
export type StrategyChanges =
  | { readonly kind: "unknown" }
  | { readonly kind: "default"; readonly label: string }
  | { readonly kind: "same"; readonly label: string; readonly defaultLabel: string }
  | { readonly kind: "changes"; readonly label: string; readonly defaultLabel: string; readonly lines: readonly string[] };

/** The legend's second half for `graph`, labelled as the pickers label strategies. */
export function strategyChanges(graph: WorkflowGraph): StrategyChanges {
  const { changes_from_default: lines, default: def } = graph;
  if (lines === null || def === null) return { kind: "unknown" };
  const label = strategyLabel(graph.strategy);
  const defaultLabel = strategyLabel(def);
  if (lines.length > 0) return { kind: "changes", label, defaultLabel, lines };
  return sameStrategy(graph.strategy, def) ? { kind: "default", label } : { kind: "same", label, defaultLabel };
}
