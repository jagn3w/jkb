import { useCallback, useEffect, useRef, useState } from "react";

import {
  binPlan,
  binUnitFor,
  decodePlanTimeline,
  decodeSessionTimeline,
  decodeTaskTimeline,
  decodeTimelineOverview,
  spanText,
  swimlanes,
  timelineOps,
  type PlanTimeline,
  type SessionTimeline,
  type TaskView,
  type TimelineEvent,
  type TimelineOverview,
} from "@jkb/core";

import { useNavigation, type TimelineScope } from "../navigation";
import { LifecyclePane } from "../workflows/LifecyclePane";

const op = (request: Parameters<typeof window.jkb.op>[0]) => window.jkb.op(request);

type Loaded<T> =
  | { readonly kind: "idle" }
  | { readonly kind: "loading" }
  | { readonly kind: "loaded"; readonly value: T; readonly at: number }
  | { readonly kind: "failed"; readonly message: string };

/** The picker's three scopes; a single session is shown under Sessions. */
type Picked = "sessions" | "task" | "plan";

const SCOPES: readonly { readonly id: Picked; readonly label: string }[] = [
  { id: "sessions", label: "Sessions" },
  { id: "task", label: "Task" },
  { id: "plan", label: "Plan" },
];

/** A Unix-ms time in the operator's clock, to the second. */
export function clock(ms: number): string {
  const d = new Date(ms);
  const pad = (n: number): string => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

/** Add a page's events to what is shown, keeping jkb's order (the page is after the last one). */
function appended<T extends { readonly events: readonly TimelineEvent[]; readonly next: string | null }>(was: T, page: T): T {
  return { ...page, events: [...was.events, ...page.events] };
}

/**
 * The Timeline section of the Executions tab: the lifecycle being applied, read from facts jkb
 * already records (`timeline.*`, the Executions design). Sessions at work (and which have stalled),
 * one session's swimlanes, one task's path drawn over its strategy's graph, one plan's progress.
 *
 * Re-read on demand — the refresh button, a change of scope, a link from the Sessions or Design tab
 * — never polled: no topic carries transitions.
 */
export function TimelinePane(): React.JSX.Element {
  const { timelineRequest } = useNavigation();
  const [scope, setScope] = useState<Picked>("sessions");
  const [session, setSession] = useState<string | undefined>(undefined);
  const [taskUid, setTaskUid] = useState("");
  const [planUid, setPlanUid] = useState("");
  const [overview, setOverview] = useState<Loaded<TimelineOverview>>({ kind: "idle" });
  const [sessionView, setSessionView] = useState<Loaded<SessionTimeline>>({ kind: "idle" });
  const [taskView, setTaskView] = useState<Loaded<TaskView>>({ kind: "idle" });
  const [planView, setPlanView] = useState<Loaded<PlanTimeline>>({ kind: "idle" });
  // Each ask is numbered and only the latest answer is kept, so a slow earlier one cannot land last.
  const asked = useRef(0);

  const show = useCallback(async (what: TimelineScope, id: string) => {
    const mine = ++asked.current;
    const now = Date.now();
    const fail = (message: string) => ({ kind: "failed" as const, message });
    switch (what) {
      case "sessions": {
        setOverview({ kind: "loading" });
        const a = decodeTimelineOverview(await op(timelineOps.sessions()));
        if (mine === asked.current) setOverview(a.ok ? { kind: "loaded", value: a.value, at: a.value.now_ms } : fail(a.error.message));
        return;
      }
      case "session": {
        setSessionView({ kind: "loading" });
        const a = decodeSessionTimeline(await op(timelineOps.session(id)));
        if (mine === asked.current) setSessionView(a.ok ? { kind: "loaded", value: a.value, at: now } : fail(a.error.message));
        return;
      }
      case "task": {
        setTaskView({ kind: "loading" });
        const a = decodeTaskTimeline(await op(timelineOps.task(id)));
        if (mine === asked.current) setTaskView(a.ok ? { kind: "loaded", value: a.value, at: now } : fail(a.error.message));
        return;
      }
      case "plan": {
        setPlanView({ kind: "loading" });
        const a = decodePlanTimeline(await op(timelineOps.plan(id)));
        if (mine === asked.current) setPlanView(a.ok ? { kind: "loaded", value: a.value, at: now } : fail(a.error.message));
        return;
      }
    }
  }, []);

  // The overview on first show.
  useEffect(() => {
    void show("sessions", "");
  }, [show]);

  // A link from another tab.
  useEffect(() => {
    if (timelineRequest === undefined) return;
    const { scope: s, id } = timelineRequest;
    if (s === "sessions" || s === "session") {
      setScope("sessions");
      setSession(s === "session" ? id : undefined);
    } else if (s === "task") {
      setScope("task");
      setTaskUid(id);
    } else {
      setScope("plan");
      setPlanUid(id);
    }
    void show(s, id);
  }, [timelineRequest, show]);

  const refresh = (): void => {
    if (scope === "sessions") void show(session === undefined ? "sessions" : "session", session ?? "");
    else if (scope === "task" && taskUid.trim() !== "") void show("task", taskUid.trim());
    else if (scope === "plan" && planUid.trim() !== "") void show("plan", planUid.trim());
  };

  const openSession = (id: string): void => {
    setSession(id);
    void show("session", id);
  };
  const openTask = (uid: string): void => {
    setScope("task");
    setTaskUid(uid);
    void show("task", uid);
  };

  const more = async (): Promise<void> => {
    if (scope === "sessions" && sessionView.kind === "loaded" && sessionView.value.next !== null && session !== undefined) {
      const a = decodeSessionTimeline(await op(timelineOps.session(session, sessionView.value.next)));
      if (a.ok) setSessionView({ ...sessionView, value: appended(sessionView.value, a.value) });
    } else if (scope === "task" && taskView.kind === "loaded" && taskView.value.timeline.next !== null) {
      const was = taskView.value;
      const a = decodeTaskTimeline(await op(timelineOps.task(was.timeline.uid, was.timeline.next ?? undefined)));
      if (a.ok) setTaskView({ ...taskView, value: { ...a.value, timeline: appended(was.timeline, a.value.timeline) } });
    }
  };

  return (
    <section className="timeline-pane" aria-label="Timeline">
      <header className="pane-bar">
        <h2>Timeline</h2>
        <div className="segmented" role="radiogroup" aria-label="Scope">
          {SCOPES.map((s) => (
            <button
              key={s.id}
              type="button"
              role="radio"
              aria-checked={scope === s.id}
              className="bar-button"
              data-active={scope === s.id ? "true" : undefined}
              onClick={() => {
                setScope(s.id);
                if (s.id === "sessions") void show(session === undefined ? "sessions" : "session", session ?? "");
                else if (s.id === "task" && taskUid.trim() !== "") void show("task", taskUid.trim());
                else if (s.id === "plan" && planUid.trim() !== "") void show("plan", planUid.trim());
              }}
            >
              {s.label}
            </button>
          ))}
        </div>
        {scope !== "sessions" && (
          <form
            className="timeline-ask"
            onSubmit={(e) => {
              e.preventDefault();
              refresh();
            }}
          >
            <input
              aria-label={scope === "task" ? "Task uid" : "Plan uid"}
              placeholder={scope === "task" ? "task:…" : "plan:…"}
              value={scope === "task" ? taskUid : planUid}
              onChange={(e) => (scope === "task" ? setTaskUid(e.target.value) : setPlanUid(e.target.value))}
            />
            <button type="submit" className="bar-button">
              Show
            </button>
          </form>
        )}
        <span className="spacer" />
        <button type="button" className="bar-button" onClick={refresh} aria-label="Refresh timeline">
          Refresh
        </button>
      </header>
      <div className="pane-body">
        {scope === "sessions" &&
          (session === undefined ? (
            <Shown what={overview} name="sessions">
              {(o) => <SessionsOverview overview={o} onOpen={openSession} />}
            </Shown>
          ) : (
            <>
              <button type="button" className="link-button timeline-back" onClick={() => {
                setSession(undefined);
                void show("sessions", "");
              }}>
                ← All sessions
              </button>
              <Shown what={sessionView} name="the session">
                {(t, at) => <SessionLanes timeline={t} nowMs={at} onTask={openTask} />}
              </Shown>
            </>
          ))}
        {scope === "task" && (
          <Shown what={taskView} name="the task" idle="Name a task to see its path through its strategy.">
            {(v) => <TaskTimelineView view={v} />}
          </Shown>
        )}
        {scope === "plan" && (
          <Shown what={planView} name="the plan" idle="Name a plan, or open one from the Design tab's plans.">
            {(p, at) => <PlanChart plan={p} nowMs={at} offsetMin={-new Date(at).getTimezoneOffset()} />}
          </Shown>
        )}
        {((scope === "sessions" && session !== undefined && sessionView.kind === "loaded" && sessionView.value.next !== null) ||
          (scope === "task" && taskView.kind === "loaded" && taskView.value.timeline.next !== null)) && (
          <button type="button" className="bar-button timeline-more" onClick={() => void more()}>
            Load more events
          </button>
        )}
      </div>
    </section>
  );
}

function Shown<T>({
  what,
  name,
  idle,
  children,
}: {
  readonly what: Loaded<T>;
  readonly name: string;
  readonly idle?: string;
  readonly children: (value: T, at: number) => React.JSX.Element;
}): React.JSX.Element {
  switch (what.kind) {
    case "idle":
      return <p className="muted plan-hint">{idle ?? "Loading…"}</p>;
    case "loading":
      return <p className="muted plan-hint">Loading {name}…</p>;
    case "failed":
      return <p className="muted plan-hint">Cannot read {name}&apos;s timeline: {what.message}</p>;
    case "loaded":
      return children(what.value, what.at);
  }
}

/** The sessions at work: each with its claims, last event and whether it has stalled. */
export function SessionsOverview({
  overview,
  onOpen,
}: {
  readonly overview: TimelineOverview;
  readonly onOpen: (session: string) => void;
}): React.JSX.Element {
  return (
    <div className="timeline-overview">
      {overview.sessions.length === 0 ? (
        <p className="muted plan-hint">No live session, and no session holds a claim.</p>
      ) : (
        <table className="timeline-table">
          <thead>
            <tr>
              <th>session</th>
              <th>state</th>
              <th>claims</th>
              <th>last event</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {overview.sessions.map((s) => (
              <tr key={s.session} data-stalled={s.stalled ? "true" : undefined}>
                <td>
                  <button type="button" className="link-button" onClick={() => onOpen(s.session)} title={s.cwd ?? undefined}>
                    {s.session}
                  </button>
                </td>
                <td>{s.state}</td>
                <td>{s.claims}</td>
                <td>{s.last_event_ms === null ? <span className="muted">none</span> : clock(s.last_event_ms)}</td>
                <td>
                  {s.stalled && (
                    <span className="timeline-stalled" title="Holds a claim, has not ended, and has fired no event for 30 minutes">
                      stalled{s.stalled_for_ms !== null ? ` ${spanText(s.stalled_for_ms)}` : ""}
                    </span>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {overview.unattributed.length > 0 && (
        <section className="timeline-unattributed" aria-label="Unattributed holders">
          <h3>Unattributed holders</h3>
          <p className="muted plan-hint">Claims no Claude session can be found from: a process&apos;s, or an id in no known shape.</p>
          <ul>
            {overview.unattributed.map((h) => (
              <li key={h.uid}>
                <code>{h.uid}</code> {h.title} <span className="muted">by {h.owner}{h.claimed_at_ms !== null ? ` since ${clock(h.claimed_at_ms)}` : ""}</span>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}

const LABEL_W = 260;
const TRACK_W = 640;
const LANE_H = 26;

/** One session: a swimlane per task with a tick per event, and the events listed under it. */
export function SessionLanes({
  timeline,
  nowMs,
  onTask,
}: {
  readonly timeline: SessionTimeline;
  readonly nowMs: number;
  readonly onTask: (uid: string) => void;
}): React.JSX.Element {
  const s = timeline.summary;
  const lanes = swimlanes(timeline, nowMs);
  const span = lanes.end_ms - lanes.start_ms;
  const xOf = (t: number): number => LABEL_W + ((t - lanes.start_ms) / span) * TRACK_W;
  const rows = [{ key: "(session)", label: "session", holds: false, events: lanes.session, uid: undefined as string | undefined }].concat(
    lanes.lanes.map((l) => ({ key: l.task.uid, label: l.task.title, holds: l.task.holds_claim, events: [...l.events], uid: l.task.uid })),
  );
  const height = rows.length * LANE_H + 20;
  return (
    <div className="timeline-session">
      <p className="timeline-summary">
        <code>{s.session}</code> · {s.state} · {s.claims} claim{s.claims === 1 ? "" : "s"}
        {s.last_event_ms !== null && <> · last event {clock(s.last_event_ms)}</>}
        {s.stalled && <span className="timeline-stalled"> · stalled{s.stalled_for_ms !== null ? ` ${spanText(s.stalled_for_ms)}` : ""}</span>}
      </p>
      <svg width={LABEL_W + TRACK_W + 8} height={height} role="img" aria-label="Swimlanes" className="timeline-lanes">
        {rows.map((r, i) => (
          <g key={r.key} transform={`translate(0,${i * LANE_H})`} data-holds={r.holds ? "true" : undefined}>
            <line x1={LABEL_W} x2={LABEL_W + TRACK_W} y1={LANE_H / 2} y2={LANE_H / 2} className="lane-track" />
            <text x={4} y={LANE_H / 2 + 4} className="lane-label" onClick={r.uid === undefined ? undefined : () => onTask(r.uid as string)}>
              {r.label.length > 36 ? `${r.label.slice(0, 35)}…` : r.label}
              <title>{r.uid ?? "the session's own starts and ends"}</title>
            </text>
            {r.events.map((e) => (
              <line key={`${e.source}-${e.id}`} x1={xOf(e.at_ms)} x2={xOf(e.at_ms)} y1={5} y2={LANE_H - 5} className="lane-tick" data-source={e.source}>
                <title>{eventText(e)}</title>
              </line>
            ))}
          </g>
        ))}
        <text x={LABEL_W} y={height - 4} className="lane-axis">
          {clock(lanes.start_ms)}
        </text>
        <text x={LABEL_W + TRACK_W} y={height - 4} textAnchor="end" className="lane-axis">
          {clock(lanes.end_ms)}
        </text>
      </svg>
      <EventList events={timeline.events} />
    </div>
  );
}

function eventText(e: TimelineEvent): string {
  const move = e.to !== null ? ` ${e.from ?? "∅"} → ${e.to}` : "";
  const by = e.actor !== null ? ` by ${e.actor}${e.role !== null ? ` (${e.role})` : ""}` : "";
  return `${clock(e.at_ms)} ${e.source} ${e.event}${move}${by}${e.detail !== null ? ` [${e.detail}]` : ""}`;
}

/** Events in jkb's order, one row each. */
export function EventList({ events }: { readonly events: readonly TimelineEvent[] }): React.JSX.Element {
  if (events.length === 0) return <p className="muted plan-hint">No events recorded.</p>;
  return (
    <table className="timeline-table timeline-events">
      <thead>
        <tr>
          <th>when</th>
          <th>source</th>
          <th>event</th>
          <th>move</th>
          <th>task</th>
          <th>by</th>
          <th>detail</th>
        </tr>
      </thead>
      <tbody>
        {events.map((e) => (
          <tr key={`${e.source}-${e.id}-${e.at_ms}`} data-source={e.source}>
            <td>{clock(e.at_ms)}</td>
            <td>{e.source}</td>
            <td>{e.event}</td>
            <td>{e.to !== null ? `${e.from ?? "∅"} → ${e.to}` : ""}</td>
            <td>{e.task ?? ""}</td>
            <td>
              {e.actor ?? ""}
              {e.role !== null ? <span className="muted"> ({e.role})</span> : null}
            </td>
            <td>{[e.reason, e.detail].filter((x) => x !== null).join(" · ")}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/** One task: its path drawn over its strategy's graph, and its events. */
export function TaskTimelineView({ view }: { readonly view: TaskView }): React.JSX.Element {
  const t = view.timeline;
  return (
    <div className="timeline-task">
      <p className="timeline-summary">
        <code>{t.uid}</code> {t.title} · {t.status ?? "?"}
        {t.claim !== null && (
          <>
            {" "}
            · held by {t.claim.owner}
            {t.claim.claimed_at_ms !== null ? ` since ${clock(t.claim.claimed_at_ms)}` : ""}
          </>
        )}
        {t.landing.state !== "none" && (
          <>
            {" "}
            · {t.landing.state} {t.landing.branch ?? ""} → {t.landing.onto ?? ""}
            {t.landing.pr !== null ? ` (PR #${t.landing.pr})` : ""}
          </>
        )}
      </p>
      {t.visited.length === 0 && t.visited_status.length > 0 && (
        <p className="muted plan-hint">No workflow history: this task predates the workflow log, so only its status path is drawn.</p>
      )}
      <LifecyclePane graph={view.graph} role={undefined} visited={{ workflow: t.visited, lifecycle: t.visited_status }} />
      <EventList events={t.events} />
    </div>
  );
}

const CHART_W = 720;
const CHART_H = 180;
const BANDS = ["done", "in_progress", "open"] as const;
const BAND_LABEL: Record<(typeof BANDS)[number], string> = { done: "done", in_progress: "in progress", open: "open" };

/** One plan: open, in progress and done stacked over time, binned by hour or day; cancelled apart. */
export function PlanChart({
  plan,
  nowMs,
  offsetMin,
}: {
  readonly plan: PlanTimeline;
  readonly nowMs: number;
  readonly offsetMin: number;
}): React.JSX.Element {
  const unit = binUnitFor(plan.points, nowMs);
  const bins = binPlan(plan.points, unit, nowMs, offsetMin);
  const last = plan.points[plan.points.length - 1];
  const top = Math.max(1, ...bins.map((b) => b.open + b.in_progress + b.done));
  const w = bins.length === 0 ? 0 : CHART_W / bins.length;
  const yOf = (n: number): number => CHART_H - (n / top) * CHART_H;
  return (
    <div className="timeline-plan">
      <p className="timeline-summary">
        <code>{plan.uid}</code> {plan.title} · {plan.tasks} task{plan.tasks === 1 ? "" : "s"}
        {last !== undefined && (
          <>
            {" "}
            · now {last.open} open, {last.in_progress} in progress, {last.done} done
            {last.cancelled > 0 ? `, ${last.cancelled} cancelled` : ""}
          </>
        )}
      </p>
      {bins.length === 0 ? (
        <p className="muted plan-hint">No tasks under this plan yet.</p>
      ) : (
        <svg width={CHART_W} height={CHART_H + 20} role="img" aria-label={`Plan progress by ${unit}`} className="timeline-chart">
          {bins.map((b, i) => {
            let base = 0;
            return (
              <g key={b.start_ms} transform={`translate(${i * w},0)`}>
                {BANDS.map((band) => {
                  const n = b[band];
                  const y = yOf(base + n);
                  const h = yOf(base) - y;
                  base += n;
                  return n === 0 ? null : <rect key={band} x={0} y={y} width={Math.max(1, w - 0.5)} height={h} className="plan-band" data-band={band} />;
                })}
                <title>{`${clock(b.start_ms)}: ${b.open} open, ${b.in_progress} in progress, ${b.done} done, ${b.cancelled} cancelled`}</title>
              </g>
            );
          })}
          <text x={0} y={CHART_H + 14} className="lane-axis">
            {clock((bins[0] as (typeof bins)[number]).start_ms)}
          </text>
          <text x={CHART_W} y={CHART_H + 14} textAnchor="end" className="lane-axis">
            {clock((bins[bins.length - 1] as (typeof bins)[number]).end_ms)} · by {unit}
          </text>
        </svg>
      )}
      <ul className="plan-legend">
        {BANDS.map((band) => (
          <li key={band}>
            <span className="plan-swatch" data-band={band} /> {BAND_LABEL[band]}
          </li>
        ))}
        <li className="muted">cancelled is counted apart, not drawn</li>
      </ul>
    </div>
  );
}
