import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { decodeDesignPrompt, decodeDesigns, designOps, designRepos, repoOf, SPAN_STATES, type Design } from "@jkb/core";

import { DocumentEditor } from "../design/DocumentEditor";
import { discussSpec, exclusive } from "../design/discuss";
import { INITIAL_LISTING, listingFailed, listingLoaded, listingLoading, type Listing } from "../design/listing";
import { PlanColumn } from "../design/PlanColumn";
import { PromptsPane } from "../design/PromptsPane";
import { SessionRegistry, type RegistryNotice } from "../design/registry";
import type { DesignSession, SessionBridge, SyncStatus } from "../design/session";
import { useNavigation } from "../navigation";
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

const STATUS_LABEL: Record<SyncStatus["kind"], string> = {
  connecting: "Loading…",
  live: "Saved",
  saving: "Saving…",
  retrying: "Reconnecting…",
  failed: "Stopped",
};

/** The window's one registry of design sessions (`registry.ts`): one session per design, reused. */
let registry: SessionRegistry | undefined;
function registryOf(): SessionRegistry {
  registry ??= new SessionRegistry(bridgeOf());
  return registry;
}

/**
 * The open design's session, re-rendered on every change of status or spans. The pane attaches to
 * the registry's session and detaches when it stops showing it; the session itself outlives the pane
 * until what it holds is sent.
 */
function useSession(design: Design | undefined): DesignSession | undefined {
  const [session, setSession] = useState<DesignSession | undefined>(undefined);
  const [, bump] = useState(0);
  const title = design?.title ?? "";
  useEffect(() => {
    if (design === undefined) {
      setSession(undefined);
      return undefined;
    }
    const reg = registryOf();
    const s = reg.attach({ uid: design.uid, topic: design.topic, title });
    setSession(s);
    const off = s.onChange(() => bump((n) => n + 1));
    return () => {
      off();
      reg.detach(s);
    };
    // The title only names the design in notices: a rename does not reopen it.
  }, [design?.uid, design?.topic]);
  return session;
}

function bridgeOf(): SessionBridge {
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
  const [listing, setListing] = useState<Listing>(INITIAL_LISTING);
  const [repo, setRepo] = useState<string | undefined>(() => remembered(LAST_REPO_KEY));
  const [uid, setUid] = useState<string | undefined>(() => remembered(LAST_DESIGN_KEY));
  const [notice, setNotice] = useState<string | undefined>(undefined);

  const load = useCallback(async () => {
    setListing(listingLoading);
    try {
      const answer = decodeDesigns(await window.jkb.op(designOps.list()));
      setListing((prev) => (answer.ok ? listingLoaded(answer.value) : listingFailed(prev, answer.error.message)));
    } catch (e) {
      setListing((prev) => listingFailed(prev, e instanceof Error ? e.message : String(e)));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // A design another tab asked for (*Jump to context*, D53.9): opened once the listing has it. A
  // design newer than the listing is read again once, then said to be missing.
  const { designRequest } = useNavigation();
  const [handled, setHandled] = useState<{ readonly seq: number; readonly reloaded: boolean }>({ seq: 0, reloaded: false });
  useEffect(() => {
    if (designRequest === undefined || designRequest.seq === handled.seq || listing.loading) return;
    const found = listing.designs?.find((d) => d.uid === designRequest.uid);
    if (found !== undefined) {
      const r = repoOf(found.namespace);
      if (r !== undefined) {
        setRepo(r);
        remember(LAST_REPO_KEY, r);
      }
      setUid(found.uid);
      remember(LAST_DESIGN_KEY, found.uid);
      setHandled({ seq: designRequest.seq, reloaded: false });
    } else if (!handled.reloaded) {
      setHandled({ seq: handled.seq, reloaded: true });
      void load();
    } else {
      setNotice(`${designRequest.uid} is not a design this daemon lists.`);
      setHandled({ seq: designRequest.seq, reloaded: false });
    }
  }, [designRequest, handled, listing, load]);

  const designs = useMemo(() => listing.designs ?? [], [listing.designs]);
  const repos = useMemo(() => designRepos(designs), [designs]);
  const activeRepo = repo !== undefined && repos.includes(repo) ? repo : repos[0];
  const inRepo = useMemo(() => designs.filter((d) => repoOf(d.namespace) === activeRepo), [designs, activeRepo]);
  const design = inRepo.find((d) => d.uid === uid) ?? inRepo[0];
  const opened = useSession(design);
  // Until the effect has swapped sessions, the one open is the previous design's: never show it
  // under this one's name.
  const session = opened !== undefined && opened.uid === design?.uid ? opened : undefined;

  // What the registry says about designs the pane has left — still saving, not saved, a *Discuss*
  // not started — whichever way the pane left them (a picker, *Jump to context*, the tab closing).
  // Its own list, each kept until dismissed: never the one `notice` slot every *Discuss* clears.
  const [designNotices, setDesignNotices] = useState<readonly RegistryNotice[]>(() => registryOf().notices);
  useEffect(() => {
    const reg = registryOf();
    setDesignNotices(reg.notices);
    return reg.onNotices(() => setDesignNotices(reg.notices));
  }, []);

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

  const discuss = async (from: number, to: number, shown: string): Promise<void> => {
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
    if (!settled.ok) {
      const why = settled.why;
      switch (why.kind) {
        case "closed":
          // The pane left the design while its edits were still being saved: said once, in the
          // design's own notices, so it is there whichever design is shown now.
          registryOf().notify(
            design.uid,
            "closed",
            `A Discuss of ${design.title} was not started: the design was left before its edits were saved. Select the text again.`,
          );
          return;
        case "failed":
          setNotice(`${why.message} — reopen the design to discuss it.`);
          return;
        case "unread":
          setNotice(`Cannot read the design: ${why.message}`);
          return;
        case "moved":
          break;
      }
    }
    if (!settled.ok || settled.doc.text !== shown) {
      setNotice("The text changed while the selection was being sent — select it again.");
      return;
    }
    const answer = decodeDesignPrompt(await window.jkb.op(designOps.discuss(design.uid, settled.doc.version, from, to)));
    if (!answer.ok) {
      setNotice(answer.error.message);
      return;
    }
    terminals.open(discussSpec(answer.value, activeRepo, roots, crypto.randomUUID()), "popover");
  };
  // One *Discuss* at a time, across re-renders: the latest closure runs, the guard is the one made
  // on mount.
  const latestDiscuss = useRef(discuss);
  latestDiscuss.current = discuss;
  const onDiscuss = useMemo(() => exclusive((from: number, to: number, shown: string) => latestDiscuss.current(from, to, shown)), []);

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
        <button type="button" className="bar-button" onClick={() => void load()} disabled={listing.loading}>
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
      {designNotices.map((n) => (
        <p key={n.id} className="design-notice" role={n.kind === "lost" ? "alert" : "status"} data-kind={n.kind}>
          {n.message}
          <button type="button" className="bar-button" onClick={() => registryOf().dismiss(n.id)} aria-label="Dismiss">
            ×
          </button>
        </p>
      ))}
      {session?.status.kind === "failed" && (
        <p className="design-notice" role="alert" data-kind="failed">
          {session.status.message} — nothing more is saved until the design is reopened.
        </p>
      )}
      {listing.error !== undefined && listing.designs !== undefined && (
        <p className="design-notice" role="status">
          Cannot refresh the designs: {listing.error}
        </p>
      )}
      <div className="design-body">
        {listing.designs === undefined && listing.error !== undefined ? (
          <p className="design-empty">Cannot list designs: {listing.error}</p>
        ) : listing.designs === undefined ? (
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
