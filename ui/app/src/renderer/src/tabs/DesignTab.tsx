import { useCallback, useEffect, useMemo, useState } from "react";

import { decodeDesignPrompt, decodeDesigns, designOps, designRepos, repoOf, SPAN_STATES, type Design } from "@jkb/core";

import { DocumentEditor } from "../design/DocumentEditor";
import { discussSpec } from "../design/discuss";
import { PlanColumn } from "../design/PlanColumn";
import { PromptsPane } from "../design/PromptsPane";
import { DesignSession, type SyncStatus } from "../design/session";
import { useTerminals } from "../terminal/TerminalProvider";

/** The last repo and design, per-window conveniences (D53.1): the only state the tab keeps itself. */
const LAST_REPO_KEY = "jkb.app.design.repo";
const LAST_DESIGN_KEY = "jkb.app.design.uid";

function remembered(key: string): string | undefined {
  try {
    return window.localStorage.getItem(key) ?? undefined;
  } catch {
    return undefined;
  }
}

function remember(key: string, value: string | undefined): void {
  try {
    if (value === undefined) window.localStorage.removeItem(key);
    else window.localStorage.setItem(key, value);
  } catch {
    // Storage unavailable: the tab opens on the first repo next time.
  }
}

type Listing =
  | { readonly kind: "loading" }
  | { readonly kind: "loaded"; readonly designs: readonly Design[] }
  | { readonly kind: "failed"; readonly message: string };

const STATUS_LABEL: Record<SyncStatus["kind"], string> = {
  connecting: "Loading…",
  live: "Saved",
  saving: "Saving…",
  retrying: "Reconnecting…",
  failed: "Stopped",
};

/** The open design's session, re-rendered on every change of status or spans. */
function useSession(design: Design | undefined): DesignSession | undefined {
  const [session, setSession] = useState<DesignSession | undefined>(undefined);
  const [, bump] = useState(0);
  useEffect(() => {
    if (design === undefined) {
      setSession(undefined);
      return undefined;
    }
    const s = new DesignSession(bridgeOf(), design.uid, design.topic);
    setSession(s);
    const off = s.onChange(() => bump((n) => n + 1));
    void s.open();
    return () => {
      off();
      s.dispose();
    };
  }, [design?.uid, design?.topic]);
  return session;
}

function bridgeOf(): ConstructorParameters<typeof DesignSession>[0] {
  return {
    op: (request) => window.jkb.op(request),
    subscribe: (topic) => window.jkb.design.subscribe(topic),
    unsubscribe: (topic) => window.jkb.design.unsubscribe(topic),
    onEvent: (listener) => window.jkb.design.onEvent(listener),
  };
}

/**
 * The Design tab (D53.4–6): pick a repo and one of its designs, and edit it live. The Document pane
 * is the design's CRDT text in CodeMirror, synced with jkb as it is typed, coloured by span state;
 * selecting text offers *Discuss*, which opens Claude beside it. Beside it, the Execution Plan and
 * Tasks panes — the design's plans, their steps and tasks, and *Play* — and the Prompts pane: every
 * Claude session that worked the design, resumable, and *New prompt*.
 */
export function DesignTab(): React.JSX.Element {
  const terminals = useTerminals();
  const [listing, setListing] = useState<Listing>({ kind: "loading" });
  const [repo, setRepo] = useState<string | undefined>(() => remembered(LAST_REPO_KEY));
  const [uid, setUid] = useState<string | undefined>(() => remembered(LAST_DESIGN_KEY));
  const [notice, setNotice] = useState<string | undefined>(undefined);

  const load = useCallback(async () => {
    setListing({ kind: "loading" });
    try {
      const answer = decodeDesigns(await window.jkb.op(designOps.list()));
      setListing(answer.ok ? { kind: "loaded", designs: answer.value } : { kind: "failed", message: answer.error.message });
    } catch (e) {
      setListing({ kind: "failed", message: e instanceof Error ? e.message : String(e) });
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const designs = listing.kind === "loaded" ? listing.designs : [];
  const repos = useMemo(() => designRepos(designs), [designs]);
  const activeRepo = repo !== undefined && repos.includes(repo) ? repo : repos[0];
  const inRepo = useMemo(() => designs.filter((d) => repoOf(d.namespace) === activeRepo), [designs, activeRepo]);
  const design = inRepo.find((d) => d.uid === uid) ?? inRepo[0];
  const opened = useSession(design);
  // Until the effect has swapped sessions, the one open is the previous design's: never show it
  // under this one's name.
  const session = opened !== undefined && opened.uid === design?.uid ? opened : undefined;

  const pickRepo = (next: string): void => {
    setRepo(next);
    remember(LAST_REPO_KEY, next);
    setUid(undefined);
    remember(LAST_DESIGN_KEY, undefined);
  };
  const pickDesign = (next: string): void => {
    setUid(next);
    remember(LAST_DESIGN_KEY, next);
  };

  const onDiscuss = useCallback(
    async (from: number, to: number, shown: string): Promise<void> => {
      if (session === undefined || design === undefined || activeRepo === undefined) return;
      setNotice(undefined);
      const roots = terminals.roots;
      if (roots === undefined) {
        setNotice("The terminal is not ready yet.");
        return;
      }
      // The selection is offsets into the text on screen; it is sent with the version whose text is
      // exactly that, once every edit has been saved.
      const settled = await session.settledVersion();
      if (settled === undefined || settled.text !== shown) {
        setNotice("The text changed while the selection was being sent — select it again.");
        return;
      }
      const answer = decodeDesignPrompt(await window.jkb.op(designOps.discuss(design.uid, settled.version, from, to)));
      if (!answer.ok) {
        setNotice(answer.error.message);
        return;
      }
      terminals.open(discussSpec(answer.value, activeRepo, roots, crypto.randomUUID()), "popover");
    },
    [session, design, activeRepo, terminals],
  );

  return (
    <div className="design-tab">
      <header className="design-bar">
        <h1>Design</h1>
        <label className="picker">
          <span>Repo</span>
          <select
            value={activeRepo ?? ""}
            disabled={repos.length === 0}
            onChange={(e) => pickRepo(e.target.value)}
            aria-label="Repo"
          >
            {repos.map((r) => (
              <option key={r} value={r}>
                {r}
              </option>
            ))}
          </select>
        </label>
        <label className="picker">
          <span>Design</span>
          <select
            value={design?.uid ?? ""}
            disabled={inRepo.length === 0}
            onChange={(e) => pickDesign(e.target.value)}
            aria-label="Design"
          >
            {inRepo.map((d) => (
              <option key={d.uid} value={d.uid}>
                {d.title}
              </option>
            ))}
          </select>
        </label>
        <button type="button" className="bar-button" onClick={() => void load()} disabled={listing.kind === "loading"}>
          Refresh
        </button>
        <span className="spacer" />
        <ul className="state-legend" aria-label="Span states">
          {SPAN_STATES.map((s) => (
            <li key={s} data-state={s}>
              {s.toLowerCase()}
            </li>
          ))}
        </ul>
        {session !== undefined && (
          <span
            className="sync-status"
            data-state={session.status.kind}
            title={
              session.status.kind === "retrying" || session.status.kind === "failed"
                ? session.status.message
                : (session.feedProblem ?? `${design?.uid ?? ""} · ${session.current?.version ?? ""}`)
            }
          >
            {STATUS_LABEL[session.status.kind]}
            {session.feedProblem !== undefined && session.status.kind !== "failed" ? " · not live" : ""}
          </span>
        )}
      </header>
      {notice !== undefined && (
        <p className="design-notice" role="status">
          {notice}
          <button type="button" className="bar-button" onClick={() => setNotice(undefined)} aria-label="Dismiss">
            ×
          </button>
        </p>
      )}
      {session?.status.kind === "failed" && (
        <p className="design-notice" role="alert" data-kind="failed">
          {session.status.message} — nothing more is saved until the design is reopened.
        </p>
      )}
      <div className="design-body">
        {listing.kind === "failed" ? (
          <p className="design-empty">Cannot list designs: {listing.message}</p>
        ) : listing.kind === "loading" && designs.length === 0 ? (
          <p className="design-empty muted">Loading designs…</p>
        ) : design === undefined ? (
          <div className="design-empty">
            <p>No designs yet.</p>
            <p className="muted">
              Create one with <code>jkb design create &lt;title&gt; --repo &lt;repo&gt;</code>, then Refresh.
            </p>
          </div>
        ) : (
          <div className="design-split">
            <div className="design-document">
              {session === undefined ? (
                <p className="design-empty muted">Opening…</p>
              ) : (
                <DocumentEditor key={design.uid} session={session} onDiscuss={(f, t, shown) => void onDiscuss(f, t, shown)} />
              )}
            </div>
            {activeRepo !== undefined && (
              <PlanColumn key={design.uid} design={design.uid} repo={activeRepo} onNotice={setNotice}>
                <PromptsPane design={design} repo={activeRepo} onNotice={setNotice} />
              </PlanColumn>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
