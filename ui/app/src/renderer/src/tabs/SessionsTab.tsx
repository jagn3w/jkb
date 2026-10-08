import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  decodeBranchTasks,
  decodeSessionPrompt,
  decodeTaskDetail,
  isSessionUuid,
  joinSessions,
  notifyLabel,
  planOps,
  promptOps,
  sessionOps,
  tasksOn,
  type BranchTask,
  type DesignPromptRecord,
  type GitPlace,
  type SessionRow,
  type TaskDetail,
} from "@jkb/core";

import { retarget } from "../../../shared/terminal";
import { sessionResumeSpec, titled } from "../design/launch";
import { useNavigation } from "../navigation";
import { loadHolders, type Holders } from "../sessions/data";
import { useNeedsInput } from "../sessions/NeedsInputProvider";
import { useTerminals } from "../terminal/TerminalProvider";

const op = (request: Parameters<typeof window.jkb.op>[0]) => window.jkb.op(request);

type Listing =
  | { readonly kind: "loading" }
  | { readonly kind: "loaded"; readonly value: Holders }
  | { readonly kind: "failed"; readonly message: string };

/** Unix ms, as the operator's locale reads it, with the ISO form in the title. */
function At({ ms }: { readonly ms: number | null }): React.JSX.Element {
  if (ms === null) return <span className="muted">unknown</span>;
  const d = new Date(ms);
  return (
    <time dateTime={d.toISOString()} title={d.toISOString()}>
      {d.toLocaleString()}
    </time>
  );
}

/** The last two segments of a directory: enough to tell worktrees apart in a list. */
function shortDir(cwd: string): string {
  if (cwd === "") return "(directory unknown)";
  const parts = cwd.split("/").filter((p) => p !== "");
  return parts.length <= 2 ? cwd : `…/${parts.slice(-2).join("/")}`;
}

const shortId = (session: string): string => session.slice(0, 8);

/**
 * The Sessions tab (D53.9): every Claude Code session the registry knows (`session.list`) joined with
 * its notification (`notify.open_sessions`), the ones that need the operator first, each with a red
 * dot — the same dot the tab carries. The dot is `awaiting_user`, kept current by the app's own
 * consumer group on `claude/notify` (`sessions/watch.ts`). Selecting a session previews it and
 * resolves *Jump to context*: the design prompt it was launched as, and the task(s) recording its
 * worktree's branch. A session the app started runs in one of its terminals and is shown there; any
 * other can be resumed (`claude --resume` in its directory), never re-attached.
 *
 * The registry is re-read on demand — Refresh, the *Ended too* toggle, and whenever a notification
 * moves (which the feed already announces). Nothing announces a session starting or ending, and
 * D53.1 rules out a poll loop.
 */
export function SessionsTab(): React.JSX.Element {
  const needs = useNeedsInput();
  const terminals = useTerminals();
  const [all, setAll] = useState(false);
  const [listing, setListing] = useState<Listing>({ kind: "loading" });
  const [selected, setSelected] = useState<string | undefined>(undefined);
  const generation = useRef(0);

  const load = useCallback(async (withEnded: boolean) => {
    const mine = ++generation.current;
    setListing((l) => (l.kind === "loaded" ? l : { kind: "loading" }));
    const answer = await loadHolders(op, withEnded);
    if (mine !== generation.current) return;
    setListing(answer.ok ? { kind: "loaded", value: answer.value } : { kind: "failed", message: answer.error.message });
  }, []);

  // Read again when the toggle moves, and when a notification does: a session that just asked for
  // something, or was answered, has news in the registry too.
  useEffect(() => {
    void load(all);
  }, [all, needs.records, load]);

  const rows = useMemo(
    () => joinSessions(listing.kind === "loaded" ? listing.value.holders : [], needs.records),
    [listing, needs.records],
  );
  const owned = useMemo(() => {
    const keys = new Map<string, number>();
    for (const e of terminals.state.entries) if (e.spec.sessionUuid !== undefined) keys.set(e.spec.sessionUuid, e.key);
    return keys;
  }, [terminals.state.entries]);
  const row = rows.find((r) => r.session === selected) ?? rows[0];

  const refresh = (): void => {
    needs.refresh();
    void load(all);
  };

  return (
    <div className="sessions-tab">
      <header className="design-bar">
        <h1>Sessions</h1>
        <span className="muted">
          {needs.needing.size === 0 ? "none need input" : `${needs.needing.size} need${needs.needing.size === 1 ? "s" : ""} input`}
        </span>
        <span className="spacer" />
        <label className="picker">
          <input type="checkbox" checked={all} onChange={(e) => setAll(e.target.checked)} />
          <span>Ended too</span>
        </label>
        <button type="button" className="bar-button" onClick={refresh}>
          Refresh
        </button>
      </header>
      {needs.problem !== undefined && (
        <p className="design-notice" role="status">
          {needs.problem}
        </p>
      )}
      <div className="sessions-body">
        <section className="sessions-list" aria-label="Sessions">
          {listing.kind === "failed" ? (
            <p className="muted plan-hint">Cannot list sessions: {listing.message}</p>
          ) : listing.kind === "loading" && rows.length === 0 ? (
            <p className="muted plan-hint">Loading sessions…</p>
          ) : rows.length === 0 ? (
            <p className="muted plan-hint">{all ? "No sessions recorded." : "No live sessions. Tick Ended too to see past ones."}</p>
          ) : (
            <ul role="listbox" aria-label="Sessions">
              {rows.map((r) => (
                <li
                  key={r.session}
                  role="option"
                  aria-selected={r.session === row?.session}
                  className="session-row"
                  data-live={r.live ? "true" : "false"}
                  tabIndex={0}
                  onClick={() => setSelected(r.session)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      setSelected(r.session);
                    }
                  }}
                >
                  <span className="session-mark" aria-hidden="true">
                    {r.needsInput ? <span className="needs-dot" /> : <span className="live-dot" data-live={r.live ? "true" : "false"} />}
                  </span>
                  <span className="session-dir" title={r.cwd}>
                    {shortDir(r.cwd)}
                  </span>
                  <code className="session-id" title={r.session}>
                    {shortId(r.session)}
                  </code>
                  <span className="session-meta muted">
                    {r.needsInput ? <span className="needs-text">needs input</span> : (notifyLabel(r.notify) ?? (r.live ? "live" : "ended"))}
                    {owned.has(r.session) && <span className="session-badge">app</span>}
                  </span>
                </li>
              ))}
            </ul>
          )}
          {listing.kind === "loaded" && listing.value.truncated && (
            <p className="muted plan-hint">Showing the first {listing.value.holders.length} processes the registry lists.</p>
          )}
        </section>
        <section className="sessions-preview" aria-label="Session">
          {row === undefined ? <p className="muted plan-hint">Select a session.</p> : <SessionPreview key={row.session} row={row} terminalKey={owned.get(row.session)} />}
        </section>
      </div>
    </div>
  );
}

type Loaded<T> = { readonly kind: "loading" } | { readonly kind: "loaded"; readonly value: T } | { readonly kind: "failed"; readonly message: string };

/** *Jump to context*: the session's design prompt, and the tasks on its worktree's branch. */
interface Context {
  readonly prompt: Loaded<DesignPromptRecord | null>;
  readonly place: Loaded<{ readonly place: GitPlace; readonly tasks: readonly BranchTask[] }>;
}

function useSessionContext(row: SessionRow): Context {
  const [prompt, setPrompt] = useState<Context["prompt"]>({ kind: "loading" });
  const [place, setPlace] = useState<Context["place"]>({ kind: "loading" });
  useEffect(() => {
    let live = true;
    void (async () => {
      const answer = decodeSessionPrompt(await op(promptOps.of(row.session)));
      if (live) setPrompt(answer.ok ? { kind: "loaded", value: answer.value } : { kind: "failed", message: answer.error.message });
    })();
    void (async () => {
      if (row.cwd === "") {
        if (live) setPlace({ kind: "failed", message: "the registry has no directory for this session" });
        return;
      }
      const where = await window.jkb.sessions.place(row.cwd);
      if (!live) return;
      if (!where.ok) {
        setPlace({ kind: "failed", message: where.error });
        return;
      }
      const tasks = decodeBranchTasks(await op(sessionOps.byBranch(where.value.repo)));
      if (!live) return;
      setPlace(
        tasks.ok
          ? { kind: "loaded", value: { place: where.value, tasks: tasksOn(tasks.value, where.value.branch) } }
          : { kind: "failed", message: tasks.error.message },
      );
    })();
    return () => {
      live = false;
    };
  }, [row.session, row.cwd]);
  return { prompt, place };
}

function SessionPreview({ row, terminalKey }: { readonly row: SessionRow; readonly terminalKey: number | undefined }): React.JSX.Element {
  const terminals = useTerminals();
  const nav = useNavigation();
  const context = useSessionContext(row);
  const [task, setTask] = useState<Loaded<TaskDetail> | undefined>(undefined);
  const [notice, setNotice] = useState<string | undefined>(undefined);
  const prompt = context.prompt.kind === "loaded" ? context.prompt.value : null;

  const showTerminal = (): void => {
    if (terminalKey === undefined) return;
    const entry = terminals.state.entries.find((e) => e.key === terminalKey);
    if (entry?.placement === "popover") terminals.toDrawer(terminalKey);
    else terminals.select(terminalKey);
    terminals.session(terminalKey)?.focus();
  };

  const resume = (): void => {
    const roots = terminals.roots;
    if (roots === undefined) {
      setNotice("The terminal is not ready yet.");
      return;
    }
    if (row.live && !window.confirm("This session is still running in another process. Resume it here as well? Two processes on one session both write its transcript.")) {
      return;
    }
    const title = titled("Resume", prompt?.title ?? shortId(row.session));
    terminals.open(sessionResumeSpec({ session: row.session, cwd: row.cwd || prompt?.cwd || roots.containerRepos, title }, roots), "drawer");
  };

  const shellAt = (root: string): void => {
    const roots = terminals.roots;
    if (roots === undefined) return;
    // `root` is the host's spelling (main read git there); the shell runs in the container.
    terminals.open(retarget({ target: "host", cwd: root, argv: [], title: "shell" }, "container", roots), "drawer");
  };

  const showTask = async (uid: string): Promise<void> => {
    setTask({ kind: "loading" });
    const answer = decodeTaskDetail(await op(planOps.show(uid)));
    setTask(answer.ok ? { kind: "loaded", value: answer.value } : { kind: "failed", message: answer.error.message });
  };

  return (
    <div className="session-preview">
      <header className="pane-bar">
        <h2>
          {row.needsInput && <span className="needs-dot" role="img" aria-label="needs input" />}
          {prompt?.title ?? shortDir(row.cwd)}
        </h2>
        <span className="spacer" />
        {terminalKey !== undefined ? (
          <button type="button" className="bar-button" onClick={showTerminal} title="The app's terminal running this session">
            Show terminal
          </button>
        ) : (
          <button
            type="button"
            className="bar-button"
            disabled={!isSessionUuid(row.session)}
            onClick={resume}
            title={isSessionUuid(row.session) ? `claude --resume ${row.session}` : "Only a session with a uuid can be resumed"}
          >
            Resume
          </button>
        )}
      </header>
      {notice !== undefined && (
        <p className="design-notice" role="status">
          {notice}
        </p>
      )}
      <div className="pane-body">
        <dl className="container-facts">
          <dt>Session</dt>
          <dd>
            <code>{row.session}</code>
          </dd>
          <dt>State</dt>
          <dd data-needs={row.needsInput ? "true" : undefined}>
            {notifyLabel(row.notify) ?? (row.live ? "live" : `ended${row.endReason !== null ? ` (${row.endReason})` : ""}`)}
          </dd>
          <dt>Directory</dt>
          <dd>
            <code>{row.cwd || "unknown"}</code>
          </dd>
          <dt>Started</dt>
          <dd>
            <At ms={row.startedAt} />
          </dd>
          <dt>Last seen</dt>
          <dd>
            <At ms={row.seenAt} />
          </dd>
          {row.endedAt !== null && (
            <>
              <dt>Ended</dt>
              <dd>
                <At ms={row.endedAt} />
              </dd>
            </>
          )}
          <dt>Owner</dt>
          <dd>{terminalKey !== undefined ? "this app — re-attached if the container is rebuilt" : "another process (viewable and resumable, not re-attached)"}</dd>
        </dl>

        <h3>Context</h3>
        {context.prompt.kind === "loading" ? (
          <p className="muted plan-hint">Looking up its design…</p>
        ) : context.prompt.kind === "failed" ? (
          <p className="muted plan-hint">Cannot look up its design: {context.prompt.message}</p>
        ) : prompt !== null ? (
          <div className="session-context">
            <span className="prompt-launch" data-launch={prompt.launch}>
              {prompt.launch}
            </span>
            <span title={prompt.design}>{prompt.title}</span>
            <button type="button" className="bar-button" onClick={() => nav.openDesign(prompt.design)}>
              Open design
            </button>
          </div>
        ) : (
          <p className="muted plan-hint">Not launched from a design.</p>
        )}
        {context.place.kind === "loading" ? (
          <p className="muted plan-hint">Reading its worktree…</p>
        ) : context.place.kind === "failed" ? (
          <p className="muted plan-hint">No worktree: {context.place.message}</p>
        ) : (
          <>
            <div className="session-context">
              <span>
                <code>{context.place.value.place.repo}</code>
                {context.place.value.place.branch !== null ? (
                  <>
                    {" "}
                    on <code>{context.place.value.place.branch}</code>
                  </>
                ) : (
                  <span className="muted"> (detached)</span>
                )}
              </span>
              <button type="button" className="bar-button" onClick={() => shellAt(context.place.kind === "loaded" ? context.place.value.place.root : "")}>
                Shell here
              </button>
            </div>
            {context.place.value.tasks.length === 0 ? (
              <p className="muted plan-hint">No task records this branch.</p>
            ) : (
              <ul className="session-tasks">
                {context.place.value.tasks.map((t) => (
                  <li key={t.uid}>
                    <code>{t.uid}</code> <span className="muted">{t.status}</span>
                    <button type="button" className="bar-button" onClick={() => void showTask(t.uid)}>
                      Show task
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </>
        )}
        {task !== undefined && (
          <div className="session-task">
            {task.kind === "loading" ? (
              <p className="muted plan-hint">Reading the task…</p>
            ) : task.kind === "failed" ? (
              <p className="muted plan-hint">Cannot read the task: {task.message}</p>
            ) : (
              <>
                <p>
                  <code>{task.value.uid}</code> <span className="muted">{task.value.status ?? ""}</span>
                </p>
                <pre className="session-task-text">{task.value.content}</pre>
              </>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
