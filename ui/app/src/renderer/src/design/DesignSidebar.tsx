import { useEffect, useRef, useState } from "react";

import {
  NO_REPO_LABEL,
  subtreeReview,
  unplannedOf,
  type DesignNode,
  type RepoNode,
  type ReviewCounts,
  type UnplannedList,
  type UnplannedRepo,
} from "@jkb/core";

import { createTargets, defaultTarget, type CreateTarget } from "./create";
import { sidebarRepos } from "./retire";

/** A thin bar of the span states of a design and the designs under it, and how much is unreviewed. */
function StateBar({ counts }: { readonly counts: ReviewCounts }): React.JSX.Element | null {
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
  // The whole subtree's counts; *See next* still skips by a design's own (`hasUnreviewed`).
  const review = subtreeReview(node);
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
        {review !== undefined && <StateBar counts={review} />}
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

/** A retired design in its repo's Retired fold: its title, the designs retired with it, and *Restore* on the top one. */
function RetiredRow({
  node,
  depth,
  top,
  busy,
  onRestore,
}: {
  readonly node: DesignNode;
  readonly depth: number;
  /** The top of a retired tree: the one design here that can be restored (the rest come with it). */
  readonly top: boolean;
  readonly busy: boolean;
  readonly onRestore: (uid: string) => void;
}): React.JSX.Element {
  const { design, children } = node;
  return (
    <li role="treeitem" aria-selected={false}>
      <div className="tree-row tree-retired" style={{ paddingLeft: `calc(var(--space-2) + ${depth} * var(--space-3))` }}>
        <span className="tree-fold" aria-hidden="true" />
        <span className="tree-label muted" title={design.uid}>
          {design.title}
        </span>
        {top && (
          <button
            type="button"
            className="bar-button"
            disabled={busy}
            onClick={() => onRestore(design.uid)}
            aria-label={`Restore ${design.title}`}
            title={children.length > 0 ? "Restore it and the designs retired under it" : "Restore it"}
          >
            Restore
          </button>
        )}
      </div>
      {children.length > 0 && (
        <ul role="group">
          {children.map((c) => (
            <RetiredRow key={c.design.uid} node={c} depth={depth + 1} top={false} busy={busy} onRestore={onRestore} />
          ))}
        </ul>
      )}
    </li>
  );
}

/**
 * A repo's retired designs, folded until opened: retired work is out of the way, not out of reach.
 * Each retired tree is offered *Restore* at its top.
 */
function RetiredFold({
  repo,
  designs,
  open,
  onToggle,
  onRestore,
}: {
  readonly repo: string;
  readonly designs: readonly DesignNode[];
  readonly open: boolean;
  readonly onToggle: () => void;
  readonly onRestore: (uid: string) => Promise<void>;
}): React.JSX.Element {
  const [busy, setBusy] = useState(false);
  const restore = (uid: string): void => {
    setBusy(true);
    void onRestore(uid).finally(() => setBusy(false));
  };
  const count = (ns: readonly DesignNode[]): number => ns.reduce((n, d) => n + 1 + count(d.children), 0);
  return (
    <li role="treeitem" aria-expanded={open} aria-selected={false} className="tree-retired-fold">
      <div className="tree-row" style={{ paddingLeft: "calc(var(--space-2) + var(--space-3))" }}>
        <button type="button" className="tree-fold" aria-label={open ? `Fold retired designs of ${repo}` : `Unfold retired designs of ${repo}`} onClick={onToggle}>
          {open ? "▾" : "▸"}
        </button>
        <span className="tree-label muted">Retired</span>
        <span className="tree-count muted">{count(designs)}</span>
      </div>
      {open && (
        <ul role="group" aria-label={`Retired designs of ${repo}`}>
          {designs.map((n) => (
            <RetiredRow key={n.design.uid} node={n} depth={2} top busy={busy} onRestore={restore} />
          ))}
        </ul>
      )}
    </li>
  );
}

/** What the sidebar has selected that is not a design: an unplanned task of a repo (`null`: in no repo). */
export interface UnplannedPick {
  readonly repo: string | null;
  readonly uid: string;
}

/**
 * A repo's unplanned tasks — open, under no plan step — folded until opened, like the Retired fold.
 * Choosing one opens it in the Tasks pane, where *Move to step* plans it.
 */
function UnplannedFold({
  entry,
  open,
  depth,
  selected,
  onToggle,
  onPick,
}: {
  readonly entry: UnplannedRepo;
  readonly open: boolean;
  readonly depth: number;
  readonly selected: UnplannedPick | undefined;
  readonly onToggle: () => void;
  readonly onPick: (pick: UnplannedPick) => void;
}): React.JSX.Element {
  const name = entry.repo ?? NO_REPO_LABEL;
  return (
    <li role="treeitem" aria-expanded={open} aria-selected={false} className="tree-unplanned-fold">
      <div className="tree-row" style={{ paddingLeft: `calc(var(--space-2) + ${depth} * var(--space-3))` }}>
        <button type="button" className="tree-fold" aria-label={open ? `Fold unplanned tasks of ${name}` : `Unfold unplanned tasks of ${name}`} onClick={onToggle}>
          {open ? "▾" : "▸"}
        </button>
        <span className="tree-label muted">Unplanned ({entry.count})</span>
      </div>
      {open && (
        <ul role="group" aria-label={`Unplanned tasks of ${name}`}>
          {entry.tasks.map(({ task }) => {
            const chosen = selected !== undefined && selected.repo === entry.repo && selected.uid === task.uid;
            return (
              <li key={task.uid} role="treeitem" aria-selected={chosen}>
                <div className="tree-row" style={{ paddingLeft: `calc(var(--space-2) + ${depth + 1} * var(--space-3))` }}>
                  <span className="tree-fold" aria-hidden="true" />
                  <button
                    type="button"
                    className="tree-label tree-unplanned-task"
                    aria-current={chosen ? "page" : undefined}
                    title={task.uid}
                    onClick={() => onPick({ repo: entry.repo, uid: task.uid })}
                  >
                    {task.title}
                  </button>
                </div>
              </li>
            );
          })}
          {entry.count > entry.tasks.length && (
            <li role="treeitem" aria-selected={false} className="muted tree-more">
              …and {entry.count - entry.tasks.length} more (<code>jkb task unplanned</code>)
            </li>
          )}
        </ul>
      )}
    </li>
  );
}

/**
 * *New design*: a title, and where it goes — the top level of a repo (first offered: the open
 * design's repo) or under any design. With no designs yet there is no repo to offer, so one is typed.
 * The form stays open, with what was typed, when the create is refused; the reason is the tab's notice.
 */
function NewDesignForm({
  tree,
  openRepo,
  onCreate,
  onClose,
}: {
  readonly tree: readonly RepoNode[];
  readonly openRepo: string | undefined;
  readonly onCreate: (title: string, target: Pick<CreateTarget, "repo" | "under">) => Promise<boolean>;
  readonly onClose: () => void;
}): React.JSX.Element {
  const targets = createTargets(tree);
  const [title, setTitle] = useState("");
  const [repo, setRepo] = useState("");
  const [key, setKey] = useState<string | undefined>(() => defaultTarget(targets, openRepo)?.key);
  const [busy, setBusy] = useState(false);
  // A place the listing no longer has (a refresh meanwhile) falls back to the one offered first.
  const chosen = targets.find((t) => t.key === key) ?? defaultTarget(targets, openRepo);
  const typed = repo.trim();
  const target: Pick<CreateTarget, "repo" | "under"> | undefined =
    targets.length === 0 ? (typed === "" ? undefined : { repo: typed, under: undefined }) : chosen;
  const ready = !busy && title.trim() !== "" && target !== undefined;
  const submit = async (): Promise<void> => {
    if (!ready) return;
    setBusy(true);
    try {
      if (await onCreate(title, target)) onClose();
    } finally {
      setBusy(false);
    }
  };
  return (
    <form
      className="design-new"
      aria-label="New design"
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
      onKeyDown={(e) => {
        if (e.key === "Escape" && !busy) {
          e.preventDefault();
          onClose();
        }
      }}
    >
      <input
        value={title}
        placeholder="Title"
        autoFocus
        disabled={busy}
        onChange={(e) => setTitle(e.target.value)}
        aria-label="Title"
      />
      {targets.length === 0 ? (
        <input value={repo} placeholder="Repo" disabled={busy} onChange={(e) => setRepo(e.target.value)} aria-label="Repo" />
      ) : (
        <select value={chosen?.key ?? ""} disabled={busy} onChange={(e) => setKey(e.target.value)} aria-label="Under">
          {targets.map((t) => (
            <option key={t.key} value={t.key}>
              {`${"\u00a0\u00a0".repeat(t.depth)}${t.label}`}
            </option>
          ))}
        </select>
      )}
      <div className="design-new-actions">
        <button type="button" className="bar-button" disabled={busy} onClick={onClose}>
          Cancel
        </button>
        <button type="submit" className="bar-button" disabled={!ready}>
          {busy ? "Creating…" : "Create"}
        </button>
      </div>
    </form>
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
  openRepo,
  onCreate,
  retired,
  onRestore,
  unplanned,
  unplannedPick,
  onPickUnplanned,
}: {
  readonly tree: readonly RepoNode[] | undefined;
  readonly active: string | undefined;
  readonly loading: boolean;
  readonly error: string | undefined;
  readonly onPick: (uid: string) => void;
  readonly onRefresh: () => void;
  /** The open design's repo: where *New design* offers to put one first. */
  readonly openRepo: string | undefined;
  /** Make a design and open it; `false` (refused) keeps the form open. */
  readonly onCreate: (title: string, target: Pick<CreateTarget, "repo" | "under">) => Promise<boolean>;
  /** The retired designs by repo, nested as retired (`retiredByRepo`). */
  readonly retired: ReadonlyMap<string, readonly DesignNode[]>;
  /** Restore a retired design (and those retired under it); a refusal is the tab's notice. */
  readonly onRestore: (uid: string) => Promise<void>;
  /** The unplanned tasks by repo (`task.unplanned`); `undefined` until read. */
  readonly unplanned: UnplannedList | undefined;
  /** The unplanned task open, if one is (instead of a design). */
  readonly unplannedPick: UnplannedPick | undefined;
  readonly onPickUnplanned: (pick: UnplannedPick) => void;
}): React.JSX.Element {
  const [folded, setFolded] = useState<ReadonlySet<string>>(readFolded);
  // Which repos' Retired folds are open: closed until asked, and not remembered.
  const [retiredOpen, setRetiredOpen] = useState<ReadonlySet<string>>(() => new Set());
  const toggleRetired = (repo: string): void => {
    setRetiredOpen((prev) => {
      const next = new Set(prev);
      if (next.has(repo)) next.delete(repo);
      else next.add(repo);
      return next;
    });
  };
  // Which repos' Unplanned folds are open (`""` for the tasks in no repo): closed until asked.
  const [unplannedOpen, setUnplannedOpen] = useState<ReadonlySet<string>>(() => new Set());
  const toggleUnplanned = (repo: string | null): void => {
    const key = repo ?? "";
    setUnplannedOpen((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  };
  const unplannedRepos = (unplanned?.repos ?? []).flatMap((r) => (r.repo === null ? [] : [r.repo]));
  const elsewhere = unplannedOf(unplanned, null);
  const [creating, setCreating] = useState(false);
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
        <button
          type="button"
          className="bar-button"
          onClick={() => setCreating((c) => !c)}
          disabled={tree === undefined}
          aria-label="New design"
          aria-expanded={creating}
          title="New design"
        >
          +
        </button>
        <button type="button" className="bar-button" onClick={onRefresh} disabled={loading} aria-label="Refresh designs">
          ↻
        </button>
      </header>
      {creating && tree !== undefined && (
        <NewDesignForm tree={tree} openRepo={openRepo} onCreate={onCreate} onClose={() => setCreating(false)} />
      )}
      {tree === undefined ? (
        <p className="sidebar-hint muted">{error !== undefined ? `Cannot list designs: ${error}` : "Loading designs…"}</p>
      ) : tree.length === 0 && retired.size === 0 && (unplanned?.count ?? 0) === 0 ? (
        <p className="sidebar-hint muted">No designs yet.</p>
      ) : (
        <ul className="design-tree" role="tree" aria-label="Designs by repo">
          {sidebarRepos(tree, retired, unplannedRepos).map((repo) => {
            const r = tree.find((t) => t.repo === repo) ?? { repo, designs: [] };
            const gone = retired.get(repo) ?? [];
            const loose = unplannedOf(unplanned, repo);
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
                  {loose !== undefined && loose.count > 0 && (
                    <span className="tree-unplanned" title={`${loose.count} open task(s) under no plan step`}>
                      unplanned {loose.count}
                    </span>
                  )}
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
                    {loose !== undefined && loose.count > 0 && (
                      <UnplannedFold
                        entry={loose}
                        open={unplannedOpen.has(repo)}
                        depth={1}
                        selected={unplannedPick}
                        onToggle={() => toggleUnplanned(repo)}
                        onPick={onPickUnplanned}
                      />
                    )}
                    {gone.length > 0 && (
                      <RetiredFold
                        repo={r.repo}
                        designs={gone}
                        open={retiredOpen.has(r.repo)}
                        onToggle={() => toggleRetired(r.repo)}
                        onRestore={onRestore}
                      />
                    )}
                  </ul>
                )}
              </li>
            );
          })}
          {elsewhere !== undefined && elsewhere.count > 0 && (
            <li role="treeitem" aria-expanded aria-selected={false}>
              <div className="tree-row tree-repo">
                <span className="tree-fold" aria-hidden="true" />
                <span className="tree-repo-name">{NO_REPO_LABEL}</span>
                <span className="tree-unplanned" title={`${elsewhere.count} open task(s) in no repo and under no plan step`}>
                  unplanned {elsewhere.count}
                </span>
              </div>
              <ul role="group">
                <UnplannedFold
                  entry={elsewhere}
                  open={unplannedOpen.has("")}
                  depth={1}
                  selected={unplannedPick}
                  onToggle={() => toggleUnplanned(null)}
                  onPick={onPickUnplanned}
                />
              </ul>
            </li>
          )}
        </ul>
      )}
    </nav>
  );
}
