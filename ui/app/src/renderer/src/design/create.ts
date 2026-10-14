//! *New design* in the Design tab's sidebar, as pure steps so it is tested without React: where a
//! new design may go, which of those is offered first, and the one `design.create` that makes it.
//
// The parent goes in the same op as the creation (`under`), never as a following `design.move`: a
// design cannot be removed once made, so a refused move would leave a design at the top level that
// nothing could take back.

import { decodeDesignCreated, designOps, failed, type Design, type DesignNode, type OpRequest, type OpResponse, type Outcome, type RepoNode } from "@jkb/core";

/** One place a new design may go: the top level of a repo, or under one of its designs. */
export interface CreateTarget {
  /** Stable across re-renders: `repo:<repo>` or the parent design's uid. */
  readonly key: string;
  readonly repo: string;
  /** The parent design, or `undefined` for the repo's top level. */
  readonly under: string | undefined;
  readonly label: string;
  /** How deep it sits in the sidebar's tree: 0 for a repo. */
  readonly depth: number;
}

/** Every place a new design may go, in the sidebar's order: each repo, then its designs as nested. */
export function createTargets(tree: readonly RepoNode[]): CreateTarget[] {
  const out: CreateTarget[] = [];
  const walk = (repo: string, n: DesignNode, depth: number): void => {
    out.push({ key: n.design.uid, repo, under: n.design.uid, label: n.design.title, depth });
    for (const c of n.children) walk(repo, c, depth + 1);
  };
  for (const r of tree) {
    out.push({ key: `repo:${r.repo}`, repo: r.repo, under: undefined, label: `${r.repo} (top level)`, depth: 0 });
    for (const n of r.designs) walk(r.repo, n, 1);
  }
  return out;
}

/**
 * The place offered first: the top level of the open design's repo, else of the first repo. Never
 * under the open design itself: there is no un-nesting (`design.move` only nests), so a parent the
 * operator did not choose could not be taken back.
 */
export function defaultTarget(targets: readonly CreateTarget[], openRepo: string | undefined): CreateTarget | undefined {
  const tops = targets.filter((t) => t.under === undefined);
  return tops.find((t) => t.repo === openRepo) ?? tops[0];
}

/** `design.create` for `title` at `target`: one write, so a refused parent creates nothing. */
export async function createDesign(
  op: (request: OpRequest) => Promise<Outcome<OpResponse>>,
  title: string,
  target: Pick<CreateTarget, "repo" | "under">,
): Promise<Outcome<Design>> {
  const name = title.trim();
  if (name === "") return failed("invalid", "A design needs a title.");
  return decodeDesignCreated(await op(designOps.create(target.repo, name, target.under)));
}

/**
 * Create a design, then show it: the listing is read again, and the new design opened, unless the
 * operator opened another while it was being made (theirs stands, as after *Next*).
 */
export async function createAndOpen(steps: {
  readonly create: () => Promise<Outcome<Design>>;
  readonly reload: () => Promise<unknown>;
  /** The design open now. */
  readonly current: () => string | undefined;
  readonly open: (uid: string) => void;
}): Promise<Outcome<Design>> {
  const from = steps.current();
  const made = await steps.create();
  if (!made.ok) return made;
  await steps.reload();
  if (steps.current() === from) steps.open(made.value.uid);
  return made;
}
