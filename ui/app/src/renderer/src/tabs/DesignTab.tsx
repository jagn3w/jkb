import { EditorView } from "@codemirror/view";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  decodeDesignPrompt,
  decodeDesigns,
  designOps,
  designTree,
  repoOf,
  SPAN_STATES,
  treeOrder,
  unreviewedPassages,
  type Design,
} from "@jkb/core";

import { DesignSidebar } from "../design/DesignSidebar";
import { DocumentEditor } from "../design/DocumentEditor";
import { discussSpec, exclusive } from "../design/discuss";
import { jumpTo } from "../design/jump";
import { INITIAL_LISTING, listingFailed, listingLoaded, listingLoading, type Listing } from "../design/listing";
import { PlansView } from "../design/PlansView";
import { PromptsPane } from "../design/PromptsPane";
import { SessionRegistry, type RegistryNotice } from "../design/registry";
import { ReviewBar } from "../design/ReviewBar";
import { TasksView } from "../design/TasksView";
import { ThreadMargin } from "../design/ThreadMargin";
import { useThreads } from "../design/useThreads";
import { usePlans } from "../design/usePlans";
import type { DesignSession, SessionBridge, SyncStatus } from "../design/session";
import { useNavigation } from "../navigation";
import { useTerminals } from "../terminal/TerminalProvider";

/**
 * The last design, and the view last shown for each design: per-window conveniences (D53.1), the
 * only state the tab keeps itself.
 */
const LAST_DESIGN_KEY = "jkb.app.design.uid";
const VIEWS_KEY = "jkb.app.design.views";

/** The open design's four views, after a pull request's tabs. */
const VIEWS = ["design", "plans", "tasks", "prompts"] as const;
type View = (typeof VIEWS)[number];
const VIEW_LABEL: Record<View, string> = { design: "Design", plans: "Plans", tasks: "Tasks", prompts: "Prompts" };

function rememberedView(uid: string): View {
  try {
    const all: unknown = JSON.parse(window.localStorage.getItem(VIEWS_KEY) ?? "{}");
    const v = typeof all === "object" && all !== null ? (all as Record<string, unknown>)[uid] : undefined;
    return VIEWS.find((x) => x === v) ?? "design";
  } catch {
    return "design";
  }
}

function rememberView(uid: string, view: View): void {
  try {
    const all: unknown = JSON.parse(window.localStorage.getItem(VIEWS_KEY) ?? "{}");
    const next: Record<string, unknown> = typeof all === "object" && all !== null ? { ...(all as Record<string, unknown>) } : {};
    next[uid] = view;
    window.localStorage.setItem(VIEWS_KEY, JSON.stringify(next));
  } catch {
    // Storage unavailable: each design opens on its Design view next time.
  }
}

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
 * One open design (operator feedback, 2026-10-11): its title, and four tabs after a pull request's
 * — **Design** (the Document pane, and *See next* over what is unreviewed), **Plans** (the execution
 * plans), **Tasks** (every task under them, with the Tasks pane) and **Prompts** — each naming its
 * count. Every view stays mounted while another is shown, so the editor keeps its place and the
 * counts stay live.
 */
function DesignView({
  design,
  session,
  repo,
  nextDesign,
  onNextDesign,
  revealFirst,
  onRevealed,
  onDiscuss,
  onNotice,
}: {
  readonly design: Design;
  readonly session: DesignSession | undefined;
  readonly repo: string | undefined;
  readonly nextDesign: Design | undefined;
  readonly onNextDesign: () => void;
  /** Move to the first unreviewed passage once it is known (arrived here by *Next*). */
  readonly revealFirst: boolean;
  readonly onRevealed: () => void;
  readonly onDiscuss: (from: number, to: number, shown: string) => void;
  readonly onNotice: (message: string) => void;
}): React.JSX.Element {
  // Arriving by *Next* is a review walk: it lands on the Design view whatever was last shown here,
  // or the passage it reveals would be off screen and `n` dead.
  const [view, setViewState] = useState<View>(() => (revealFirst ? "design" : rememberedView(design.uid)));
  const setView = (v: View): void => {
    setViewState(v);
    rememberView(design.uid, v);
  };
  const [task, setTask] = useState<string | undefined>(undefined);
  const [prompts, setPrompts] = useState<number | undefined>(undefined);
  const plans = usePlans(design.uid, repo, onNotice);
  const threads = useThreads(design, session, repo, onNotice);
  // Bumped when the editor's lines may have moved, so the thread margin is placed again; one bump
  // per frame at most.
  const [layout, setLayout] = useState(0);
  const pending = useRef(false);
  const onGeometry = useCallback(() => {
    if (pending.current) return;
    pending.current = true;
    requestAnimationFrame(() => {
      pending.current = false;
      setLayout((n) => n + 1);
    });
  }, []);

  const editor = useRef<EditorView | undefined>(undefined);
  const [editorReady, setEditorReady] = useState(false);
  const onView = useCallback((v: EditorView | undefined) => {
    editor.current = v;
    setEditorReady(v !== undefined);
  }, []);
  const getView = useCallback(() => editor.current, []);

  // Offsets only from an answer whose text is what the editor shows (`DesignSession.onScreen`):
  // after a local edit, until the next read, there are no passages rather than wrong ones.
  const current = session?.onScreen();
  const passages = useMemo(() => (current === undefined ? undefined : unreviewedPassages(current.text, current.spans)), [current]);
  const [head, setHead] = useState(0);

  useEffect(() => {
    if (!revealFirst || passages === undefined || !editorReady) return;
    const v = editor.current;
    const first = passages[0];
    if (v !== undefined && first !== undefined && first.to <= v.state.doc.length) {
      v.dispatch({ selection: { anchor: first.from, head: first.to }, effects: EditorView.scrollIntoView(first.from, { y: "center" }) });
    }
    onRevealed();
  }, [revealFirst, passages, editorReady, onRevealed]);

  const counts: Record<View, number | undefined> = {
    design: passages?.length,
    plans: plans.listing.kind === "loaded" ? plans.live.length : undefined,
    tasks: plans.listing.kind === "loaded" ? plans.open : undefined,
    prompts,
  };
  const countTitle: Record<View, string> = {
    design: "unreviewed passages",
    plans: "live plans",
    tasks: "open tasks",
    prompts: "prompts",
  };

  return (
    <div className="design-view">
      <header className="design-head">
        <div className="design-title-row">
          <h1 title={design.uid}>{design.title}</h1>
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
                  : (session.feedProblem ?? `${design.uid} · ${session.current?.version ?? ""}`)
              }
            >
              {STATUS_LABEL[session.status.kind]}
              {session.feedProblem !== undefined && session.status.kind !== "failed" ? " · not live" : ""}
            </span>
          )}
        </div>
        <div className="view-tabs" role="tablist" aria-label="Design views">
          {VIEWS.map((v) => (
            <button
              key={v}
              type="button"
              role="tab"
              id={`view-${v}`}
              aria-selected={view === v}
              aria-controls={`view-pane-${v}`}
              className="view-tab"
              onClick={() => setView(v)}
            >
              {VIEW_LABEL[v]}
              {counts[v] !== undefined && (
                <span className="view-count" data-zero={counts[v] === 0} title={`${counts[v]} ${countTitle[v]}`}>
                  {counts[v]}
                </span>
              )}
            </button>
          ))}
        </div>
      </header>
      <div className="view-panes">
        <div role="tabpanel" id="view-pane-design" aria-labelledby="view-design" hidden={view !== "design"} className="view-pane document-view">
          <ReviewBar
            passages={passages}
            view={getView}
            head={head}
            shown={view === "design"}
            nextDesign={nextDesign?.title}
            onNextDesign={onNextDesign}
          />
          <div className="design-document">
            {session === undefined ? (
              <p className="design-empty muted">Opening…</p>
            ) : (
              <div className="document-with-margin">
                <DocumentEditor
                  key={design.uid}
                  session={session}
                  onDiscuss={onDiscuss}
                  onComment={threads.comment}
                  onApprove={threads.approve}
                  onView={onView}
                  onCursor={setHead}
                  onGeometry={onGeometry}
                />
                <ThreadMargin
                  threads={threads.threads}
                  view={getView}
                  layout={layout}
                  draft={threads.draft}
                  busy={threads.busy}
                  onStart={threads.start}
                  onCancelDraft={threads.cancelDraft}
                  onReply={threads.reply}
                  onResolve={threads.resolve}
                  onAskClaude={threads.askClaude}
                />
              </div>
            )}
          </div>
        </div>
        <div role="tabpanel" id="view-pane-plans" aria-labelledby="view-plans" hidden={view !== "plans"} className="view-pane">
          <PlansView
            design={design.uid}
            plans={plans}
            onOpenTask={(uid) => {
              setTask(uid);
              setView("tasks");
            }}
          />
        </div>
        <div role="tabpanel" id="view-pane-tasks" aria-labelledby="view-tasks" hidden={view !== "tasks"} className="view-pane">
          <TasksView plans={plans} selected={task} onSelect={setTask} onNotice={onNotice} />
        </div>
        <div role="tabpanel" id="view-pane-prompts" aria-labelledby="view-prompts" hidden={view !== "prompts"} className="view-pane">
          {repo !== undefined && <PromptsPane design={design} repo={repo} onNotice={onNotice} onCount={setPrompts} />}
        </div>
      </div>
    </div>
  );
}

/**
 * The Design tab (D53.4–6): the designs as a tree on the left, and the one open on the right in its
 * four views. The Document pane is the design's CRDT text in CodeMirror, synced with jkb as it is
 * typed, coloured by span state; selecting text offers *Discuss*, which opens Claude beside it.
 */
export function DesignTab(): React.JSX.Element {
  const terminals = useTerminals();
  const [listing, setListing] = useState<Listing>(INITIAL_LISTING);
  const [uid, setUid] = useState<string | undefined>(() => remembered(LAST_DESIGN_KEY));
  const [notice, setNotice] = useState<string | undefined>(undefined);
  /** The design *Next* moved to, whose first unreviewed passage is shown once it is read. */
  const [revealIn, setRevealIn] = useState<string | undefined>(undefined);

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

  const pickDesign = useCallback((next: string): void => {
    setUid(next);
    remember(LAST_DESIGN_KEY, next);
  }, []);

  // A design another tab asked for (*Jump to context*, D53.9): opened once the listing has it. A
  // design newer than the listing is read again once, then said to be missing.
  const { designRequest } = useNavigation();
  const [handled, setHandled] = useState<{ readonly seq: number; readonly reloaded: boolean }>({ seq: 0, reloaded: false });
  useEffect(() => {
    if (designRequest === undefined || designRequest.seq === handled.seq) return;
    const jump = jumpTo(listing, designRequest.uid, handled.reloaded);
    switch (jump.kind) {
      case "wait":
        return;
      case "open":
        pickDesign(jump.uid);
        break;
      case "reload":
        setHandled({ seq: handled.seq, reloaded: true });
        void load();
        return;
      case "refused":
        setNotice(jump.notice);
        break;
    }
    setHandled({ seq: designRequest.seq, reloaded: false });
  }, [designRequest, handled, listing, load, pickDesign]);

  const tree = useMemo(() => (listing.designs === undefined ? undefined : designTree(listing.designs)), [listing.designs]);
  const order = useMemo(() => (tree === undefined ? [] : treeOrder(tree)), [tree]);
  const design = order.find((d) => d.uid === uid) ?? order[0];
  const at = design === undefined ? -1 : order.indexOf(design);
  const nextDesign = at >= 0 ? order[at + 1] : undefined;
  const repo = repoOf(design?.namespace);
  const opened = useSession(design);
  // Until the effect has swapped sessions, the one open is the previous design's: never show it
  // under this one's name.
  const session = opened !== undefined && opened.uid === design?.uid ? opened : undefined;

  // What the registry says about designs the pane has left — still saving, not saved, a *Discuss*
  // not started — whichever way the pane left them (the sidebar, *Jump to context*, the tab closing).
  // Its own list, each kept until dismissed: never the one `notice` slot every *Discuss* clears.
  const [designNotices, setDesignNotices] = useState<readonly RegistryNotice[]>(() => registryOf().notices);
  useEffect(() => {
    const reg = registryOf();
    setDesignNotices(reg.notices);
    return reg.onNotices(() => setDesignNotices(reg.notices));
  }, []);

  const discuss = async (from: number, to: number, shown: string): Promise<void> => {
    if (session === undefined || design === undefined || repo === undefined) return;
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
    terminals.open(discussSpec(answer.value, repo, roots, crypto.randomUUID()), "popover");
  };
  // One *Discuss* at a time, across re-renders: the latest closure runs, the guard is the one made
  // on mount.
  const latestDiscuss = useRef(discuss);
  latestDiscuss.current = discuss;
  const onDiscuss = useMemo(() => exclusive((from: number, to: number, shown: string) => latestDiscuss.current(from, to, shown)), []);
  const onRevealed = useCallback(() => setRevealIn(undefined), []);

  return (
    <div className="design-tab">
      <DesignSidebar
        tree={tree}
        active={design?.uid}
        loading={listing.loading}
        error={listing.error}
        onPick={pickDesign}
        onRefresh={() => void load()}
      />
      <div className="design-main">
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
            <DesignView
              key={design.uid}
              design={design}
              session={session}
              repo={repo}
              nextDesign={nextDesign}
              onNextDesign={() => {
                if (nextDesign === undefined) return;
                setRevealIn(nextDesign.uid);
                pickDesign(nextDesign.uid);
              }}
              revealFirst={revealIn === design.uid}
              onRevealed={onRevealed}
              onDiscuss={(f, t, shown) => void onDiscuss(f, t, shown)}
              onNotice={setNotice}
            />
          )}
        </div>
      </div>
    </div>
  );
}
