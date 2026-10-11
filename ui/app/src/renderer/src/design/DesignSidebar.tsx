import { useEffect, useRef, useState } from "react";

import type { DesignNode, RepoNode, ReviewCounts } from "@jkb/core";

/** A thin bar of the design's span states, and how much of it is unreviewed. */
function ReviewBar({ counts }: { readonly counts: ReviewCounts }): React.JSX.Element | null {
  const total = counts.proposed + counts.approved + counts.staged + counts.implemented;
  if (total === 0) return null;
  const pct = (n: number): string => `${(100 * n) / total}%`;
  const unreviewed = Math.round((100 * counts.proposed) / total);
  return (
    <span className="tree-review" title={`${unreviewed}% unreviewed`}>
      <span className="tree-bar" aria-hidden="true">
        <span data-state="IMPLEMENTED" style={{ width: pct(counts.implemented) }} />
        <span data-state="STAGED" style={{ width: pct(counts.staged) }} />
        <span data-state="APPROVED" style={{ width: pct(counts.approved) }} />
        <span data-state="PROPOSED" style={{ width: pct(counts.proposed) }} />
      </span>
      {counts.proposed > 0 && <span className="tree-unreviewed">{unreviewed === 0 ? "<1" : unreviewed}%</span>}
    </span>
  );
}

/** Which repos and designs are folded: per window, like the pane sizes (D53.1). */
const FOLDED_KEY = "jkb.app.design.folded";

function readFolded(): Set<string> {
  try {
    const raw = window.localStorage.getItem(FOLDED_KEY);
    const parsed: unknown = raw === null ? [] : JSON.parse(raw);
    return new Set(Array.isArray(parsed) ? parsed.filter((v): v is string => typeof v === "string") : []);
  } catch {
    return new Set();
  }
}

function writeFolded(folded: ReadonlySet<string>): void {
  try {
    window.localStorage.setItem(FOLDED_KEY, JSON.stringify([...folded]));
  } catch {
    // Storage unavailable: everything opens unfolded next time.
  }
}

/** The fold keys above design `uid`: its repo's and each ancestor design's. */
function pathTo(tree: readonly RepoNode[], uid: string): string[] | undefined {
  const walk = (nodes: readonly DesignNode[], above: string[]): string[] | undefined => {
    for (const n of nodes) {
      if (n.design.uid === uid) return above;
      const found = walk(n.children, [...above, n.design.uid]);
      if (found !== undefined) return found;
    }
    return undefined;
  };
  for (const r of tree) {
    const found = walk(r.designs, [`repo:${r.repo}`]);
    if (found !== undefined) return found;
  }
  return undefined;
}

function DesignRow({
  node,
  depth,
  active,
  folded,
  onToggle,
  onPick,
}: {
  readonly node: DesignNode;
  readonly depth: number;
  readonly active: string | undefined;
  readonly folded: ReadonlySet<string>;
  readonly onToggle: (key: string) => void;
  readonly onPick: (uid: string) => void;
}): React.JSX.Element {
  const { design, children } = node;
  const open = !folded.has(design.uid);
  return (
    <li role="treeitem" aria-selected={design.uid === active} aria-expanded={children.length > 0 ? open : undefined}>
      <div className="tree-row" style={{ paddingLeft: `calc(var(--space-2) + ${depth} * var(--space-3))` }}>
        {children.length > 0 ? (
          <button
            type="button"
            className="tree-fold"
            aria-label={open ? `Fold ${design.title}` : `Unfold ${design.title}`}
            onClick={() => onToggle(design.uid)}
          >
            {open ? "▾" : "▸"}
          </button>
        ) : (
          <span className="tree-fold" aria-hidden="true" />
        )}
        <button
          type="button"
          className="tree-label"
          aria-current={design.uid === active ? "page" : undefined}
          title={design.uid}
          onClick={() => onPick(design.uid)}
        >
          {design.title}
        </button>
        {design.review !== undefined && <ReviewBar counts={design.review} />}
      </div>
      {children.length > 0 && open && (
        <ul role="group">
          {children.map((c) => (
            <DesignRow
              key={c.design.uid}
              node={c}
              depth={depth + 1}
              active={active}
              folded={folded}
              onToggle={onToggle}
              onPick={onPick}
            />
          ))}
        </ul>
      )}
    </li>
  );
}

/**
 * The design list (operator feedback, 2026-10-11): a collapsible tree on the left, like Notion's —
 * repos at the top level, their designs under them, a child design under its parent. Replaces the
 * repo and design dropdowns. What is folded is the window's own convenience.
 */
export function DesignSidebar({
  tree,
  active,
  loading,
  error,
  onPick,
  onRefresh,
}: {
  readonly tree: readonly RepoNode[] | undefined;
  readonly active: string | undefined;
  readonly loading: boolean;
  readonly error: string | undefined;
  readonly onPick: (uid: string) => void;
  readonly onRefresh: () => void;
}): React.JSX.Element {
  const [folded, setFolded] = useState<ReadonlySet<string>>(readFolded);
  // The open design is always visible: its repo and every design above it unfold when it opens,
  // however it was opened (*Next*, *Jump to context*), or no row would say which design it is. Once
  // per design opened, not on every reload of the listing, so a fold the reader makes stays put.
  const revealed = useRef<string | undefined>(undefined);
  useEffect(() => {
    if (tree === undefined || active === undefined || revealed.current === active) return;
    const path = pathTo(tree, active);
    if (path === undefined) return;
    revealed.current = active;
    setFolded((prev) => {
      if (!path.some((k) => prev.has(k))) return prev;
      const next = new Set(prev);
      for (const k of path) next.delete(k);
      writeFolded(next);
      return next;
    });
  }, [tree, active]);
  const toggle = (key: string): void => {
    setFolded((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      writeFolded(next);
      return next;
    });
  };
  return (
    <nav className="design-sidebar" aria-label="Designs">
      <header className="sidebar-bar">
        <h1>Design</h1>
        <span className="spacer" />
        <button type="button" className="bar-button" onClick={onRefresh} disabled={loading} aria-label="Refresh designs">
          ↻
        </button>
      </header>
      {tree === undefined ? (
        <p className="sidebar-hint muted">{error !== undefined ? `Cannot list designs: ${error}` : "Loading designs…"}</p>
      ) : tree.length === 0 ? (
        <p className="sidebar-hint muted">No designs yet.</p>
      ) : (
        <ul className="design-tree" role="tree" aria-label="Designs by repo">
          {tree.map((r) => {
            const key = `repo:${r.repo}`;
            const open = !folded.has(key);
            return (
              <li key={r.repo} role="treeitem" aria-expanded={open} aria-selected={false}>
                <div className="tree-row tree-repo">
                  <button
                    type="button"
                    className="tree-fold"
                    aria-label={open ? `Fold ${r.repo}` : `Unfold ${r.repo}`}
                    onClick={() => toggle(key)}
                  >
                    {open ? "▾" : "▸"}
                  </button>
                  <span className="tree-repo-name">{r.repo}</span>
                  <span className="tree-count muted">{r.designs.length}</span>
                </div>
                {open && (
                  <ul role="group">
                    {r.designs.map((n) => (
                      <DesignRow
                        key={n.design.uid}
                        node={n}
                        depth={1}
                        active={active}
                        folded={folded}
                        onToggle={toggle}
                        onPick={onPick}
                      />
                    ))}
                  </ul>
                )}
              </li>
            );
          })}
        </ul>
      )}
    </nav>
  );
}
