//! Finding your way around designs: the sidebar's tree of repos and designs, and the unreviewed
//! passages *See next* walks. Pure, so the app and its tests read the same answers.

import { repoOf, stateRuns, type Design, type ReviewCounts, type Span } from "./design.js";

// ---- the sidebar tree -------------------------------------------------------------------------

/** A design in the sidebar, with the designs nested under it. */
export interface DesignNode {
  readonly design: Design;
  readonly children: readonly DesignNode[];
}

/** One repo's designs, top-level first; a child design sits under its parent. */
export interface RepoNode {
  readonly repo: string;
  readonly designs: readonly DesignNode[];
}

const byTitle = (a: Design, b: Design): number => a.title.localeCompare(b.title) || a.uid.localeCompare(b.uid);

/**
 * The sidebar's tree: repos sorted by name, each holding its designs by title, with a design whose
 * `parent` is another design of the same repo nested under it. A design whose parent is not in the
 * listing (another repo, or not listed) is shown at the top level rather than lost, and a cycle,
 * which the engine refuses, is broken the same way: no design is ever unreachable from the tree.
 */
export function designTree(designs: readonly Design[]): RepoNode[] {
  const byRepo = new Map<string, Design[]>();
  for (const d of designs) {
    const repo = repoOf(d.namespace);
    if (repo === undefined) continue;
    const list = byRepo.get(repo) ?? [];
    list.push(d);
    byRepo.set(repo, list);
  }
  return [...byRepo.keys()].sort().map((repo) => {
    const inRepo = byRepo.get(repo) ?? [];
    const uids = new Set(inRepo.map((d) => d.uid));
    const kids = new Map<string, Design[]>();
    const roots: Design[] = [];
    for (const d of inRepo) {
      const parent = d.parent ?? undefined;
      if (parent !== undefined && parent !== d.uid && uids.has(parent)) {
        const list = kids.get(parent) ?? [];
        list.push(d);
        kids.set(parent, list);
      } else {
        roots.push(d);
      }
    }
    const placed = new Set<string>();
    const node = (d: Design): DesignNode => {
      placed.add(d.uid);
      const children = (kids.get(d.uid) ?? []).filter((c) => !placed.has(c.uid)).sort(byTitle);
      return { design: d, children: children.map(node) };
    };
    const top = roots.sort(byTitle).map(node);
    // Whatever a cycle kept from being reached is shown at the top level.
    // Each is checked as it comes up, since placing one places the rest of its cycle.
    const stranded: DesignNode[] = [];
    for (const d of [...inRepo].sort(byTitle)) if (!placed.has(d.uid)) stranded.push(node(d));
    return { repo, designs: [...top, ...stranded] };
  });
}

/** The tree's designs in the order the sidebar shows them, depth first. */
export function treeOrder(tree: readonly RepoNode[]): Design[] {
  const out: Design[] = [];
  const walk = (n: DesignNode): void => {
    out.push(n.design);
    n.children.forEach(walk);
  };
  for (const r of tree) r.designs.forEach(walk);
  return out;
}

/**
 * A design's review counts with every descendant's added in: what its sidebar row shows, so a
 * folded parent still says how much under it is left to review. A design whose counts are unknown
 * (a daemon that predates them) adds nothing; a subtree with none known is `undefined`.
 */
export function subtreeReview(node: DesignNode): ReviewCounts | undefined {
  const parts = [node.design.review, ...node.children.map(subtreeReview)].filter((c): c is ReviewCounts => c !== undefined);
  if (parts.length === 0) return undefined;
  return parts.reduce((a, c) => ({
    proposed: a.proposed + c.proposed,
    approved: a.approved + c.approved,
    staged: a.staged + c.staged,
    implemented: a.implemented + c.implemented,
  }));
}

// ---- unreviewed passages ----------------------------------------------------------------------

/** A stretch of text a reviewer has not approved: a range of the document, in UTF-16 units. */
export interface Passage {
  readonly from: number;
  readonly to: number;
}

const BLANK = /\s/;

/**
 * The passages of `text` that read PROPOSED, in order: text no span covers and every demoted span,
 * from the same `stateRuns` the editor draws, so the walk can never disagree with the colours. Each
 * is trimmed of surrounding whitespace, and one that is only whitespace (the blank lines between two
 * approved spans) is not a passage: there is nothing in it to review.
 */
export function unreviewedPassages(text: string, spans: readonly Span[]): Passage[] {
  const out: Passage[] = [];
  for (const run of stateRuns(text.length, spans).runs) {
    if (run.state !== "PROPOSED") continue;
    let from = run.from;
    let to = run.to;
    while (from < to && BLANK.test(text.charAt(from))) from += 1;
    while (to > from && BLANK.test(text.charAt(to - 1))) to -= 1;
    if (to <= from) continue;
    const last = out[out.length - 1];
    // Two PROPOSED runs that touch (uncovered text against a demoted span) are one passage.
    if (last !== undefined && text.slice(last.to, from).trim() === "") out[out.length - 1] = { from: last.from, to };
    else out.push({ from, to });
  }
  return out;
}

/**
 * The passage *Next* (`dir` 1) or *Previous* (`dir` -1) moves to from position `at`: the first
 * passage starting after `at`, or the last one starting before it. `undefined` past either end.
 */
export function stepPassage(passages: readonly Passage[], at: number, dir: 1 | -1): number | undefined {
  if (dir === 1) {
    const i = passages.findIndex((p) => p.from > at);
    return i === -1 ? undefined : i;
  }
  for (let i = passages.length - 1; i >= 0; i -= 1) {
    const p = passages[i];
    if (p !== undefined && p.from < at) return i;
  }
  return undefined;
}

/** Which passage `at` is in, or `undefined` when it is in none. */
export function passageAt(passages: readonly Passage[], at: number): number | undefined {
  const i = passages.findIndex((p) => p.from <= at && at <= p.to);
  return i === -1 ? undefined : i;
}
