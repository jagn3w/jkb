//! *Retire…* in the Document pane's header and *Restore* in the sidebar's Retired fold, as pure steps
//! so they are tested without React: what the confirm says stays, which retired designs a repo shows,
//! and the one `design.retire` or `design.restore` each sends.
//
// A design is retired, never deleted (`jkb_core::design::retire`): it leaves the listing and nothing
// else moves. Retiring is the operator's (`Admin`), and refused while a task under the design is not
// done or cancelled; the daemon's refusal names those tasks, so it is shown as it is.

import { decodeDesigns, designOps, designTree, type Design, type DesignNode, type OpRequest, type OpResponse, type Outcome, type RepoNode } from "@jkb/core";

/** How many designs are nested under `uid` in `tree`, at any depth: what a retire takes with it. */
export function nestedCount(tree: readonly RepoNode[], uid: string): number {
  const size = (n: DesignNode): number => n.children.reduce((sum, c) => sum + 1 + size(c), 0);
  const find = (nodes: readonly DesignNode[]): DesignNode | undefined => {
    for (const n of nodes) {
      if (n.design.uid === uid) return n;
      const found = find(n.children);
      if (found !== undefined) return found;
    }
    return undefined;
  };
  for (const r of tree) {
    const n = find(r.designs);
    if (n !== undefined) return size(n);
  }
  return 0;
}

/**
 * What the confirm says before a retire: what leaves, what stays, and how to take it back. A design
 * with a doc target keeps its generated file, which the operator removes.
 */
export function retireConfirm(design: Pick<Design, "title" | "doc_target">, nested: number): string[] {
  const what = nested === 0 ? `“${design.title}”` : `“${design.title}” and the ${nested === 1 ? "design" : `${nested} designs`} nested under it`;
  const lines = [
    `Retire ${what}? ${nested === 0 ? "It leaves" : "They leave"} this list, \`jkb design ls\` and \`jkb design export --all\`.`,
    "Nothing is deleted: the text, its spans and approvals, plans, tasks, threads and prompts all stay, and `jkb design cat` still reads it.",
    "Restore it from the Retired fold under its repo, or straight after with `jkb undo`.",
  ];
  const target = design.doc_target ?? undefined;
  if (target !== undefined) lines.push(`Its generated file ${target} stays in the repo until you \`git rm\` it.`);
  return lines;
}

/** `design.retire`: every design it retired, the one named first. A refusal is the daemon's, as it is. */
export async function retireDesign(op: (request: OpRequest) => Promise<Outcome<OpResponse>>, uid: string): Promise<Outcome<readonly Design[]>> {
  return decodeDesigns(await op(designOps.retire(uid)));
}

/** `design.restore`: every design it restored. Refused under a retired parent: that is restored first. */
export async function restoreDesign(op: (request: OpRequest) => Promise<Outcome<OpResponse>>, uid: string): Promise<Outcome<readonly Design[]>> {
  return decodeDesigns(await op(designOps.restore(uid)));
}

/**
 * The retired designs, read as their own listing. A daemon older than retiring refuses the field it
 * does not know: that is no retired designs, never a failed tab.
 */
export async function listRetired(op: (request: OpRequest) => Promise<Outcome<OpResponse>>): Promise<Outcome<readonly Design[]>> {
  const answer = decodeDesigns(await op(designOps.list(undefined, false, true)));
  if (!answer.ok && answer.error.code === "bad_request") return { ok: true, value: [] };
  return answer;
}

/**
 * The retired designs by repo, nested as they were retired: a design retired with its parent sits
 * under it, and only a tree's top (`restorable`) is offered *Restore* — restoring one under a retired
 * parent is refused, and restoring the top brings the rest back with it.
 */
export function retiredByRepo(retired: readonly Design[]): ReadonlyMap<string, readonly DesignNode[]> {
  return new Map(designTree(retired).map((r) => [r.repo, r.designs]));
}

/** Every repo the sidebar lists: those with live designs and those with only retired ones, sorted. */
export function sidebarRepos(tree: readonly RepoNode[], retired: ReadonlyMap<string, readonly DesignNode[]>): string[] {
  return [...new Set([...tree.map((r) => r.repo), ...retired.keys()])].sort();
}

/**
 * Retire or restore, then show the result: both listings are read again. The open design, once
 * retired, is no longer listed, so the tab falls back to another as it does for any design gone.
 */
export async function changeAndReload(steps: {
  readonly change: () => Promise<Outcome<readonly Design[]>>;
  readonly reload: () => Promise<unknown>;
}): Promise<Outcome<readonly Design[]>> {
  const done = await steps.change();
  if (done.ok) await steps.reload();
  return done;
}
