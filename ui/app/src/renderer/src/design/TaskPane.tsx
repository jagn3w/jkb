import { useCallback, useEffect, useState } from "react";

import {
  decodePlanList,
  decodeTaskDetail,
  inboxStep,
  isTerminal,
  partitionPlans,
  planOps,
  playPins,
  rebaseDraft,
  saveDraft,
  treeOrder,
  type Draft,
  type FindingSummary,
  type Pick,
  type PlanTask,
  type RepoNode,
  type TaskDetail,
} from "@jkb/core";

import { useNavigation } from "../navigation";

type Detail =
  | { readonly kind: "loading" }
  | { readonly kind: "loaded"; readonly detail: TaskDetail }
  | { readonly kind: "failed"; readonly message: string };

const op = (request: Parameters<typeof window.jkb.op>[0]) => window.jkb.op(request);

/**
 * The Tasks pane (D53.6): one task as `task.show` answers it — status, claim, strategy, its
 * transitions — with its text editable through `task.edit` (replace, or append a note) and *Play*,
 * which pins it to the strategy the operator picked (if any) and starts Claude in its own
 * worktree. A viewer/editor over the existing `task.*` ops; nothing here is the app's own state.
 *
 * A draft remembers the text it started from, and Save sends it as `task.edit`'s `expected`: a note
 * appended meanwhile (the Play session's Claude, `jkb task edit --append`) makes jkb refuse the Save
 * as `stale` instead of erasing the note. The draft is kept, the task's text as it now is is shown
 * beside it, and the operator discards the draft or keeps it over the new text knowingly.
 */
export function TaskPane({
  task,
  pick,
  busy,
  onPlay,
  onChanged,
  onNotice,
  repo,
  designs,
  listed,
  onSelect,
  onMoved,
}: {
  readonly task: PlanTask | undefined;
  /** The strategy the operator picked for a Play, if any. */
  readonly pick: Pick | undefined;
  readonly busy: boolean;
  readonly onPlay: (task: PlanTask) => void;
  readonly onChanged: () => void;
  readonly onNotice: (message: string) => void;
  /** The repo the task is shown for: Move to step's Inbox is its Open work › Inbox; none in no repo. */
  readonly repo: string | undefined;
  /** That repo's designs, for Move to step's choice of a design's step. */
  readonly designs: RepoNode | undefined;
  /** Whether a task is listed beside this pane, so choosing it selects it there. */
  readonly listed: (uid: string) => boolean;
  /** Select a listed task. */
  readonly onSelect: (uid: string) => void;
  /** After a Move to step: re-read whatever lists the task. */
  readonly onMoved?: () => void;
}): React.JSX.Element {
  const nav = useNavigation();
  const [detail, setDetail] = useState<Detail>({ kind: "loading" });
  const [draft, setDraft] = useState<Draft | undefined>(undefined);
  /** The Save was refused as stale: the task's text changed under the draft. */
  const [conflict, setConflict] = useState<string | undefined>(undefined);
  const [note, setNote] = useState("");
  const [saving, setSaving] = useState(false);

  const load = useCallback(async () => {
    if (task === undefined) return;
    const answer = decodeTaskDetail(await op(planOps.show(task.uid)));
    setDetail(answer.ok ? { kind: "loaded", detail: answer.value } : { kind: "failed", message: answer.error.message });
  }, [task]);

  useEffect(() => {
    void load();
  }, [load]);

  const append = async (text: string): Promise<boolean> => {
    if (task === undefined) return false;
    setSaving(true);
    try {
      const answer = await op(planOps.edit(task.uid, text, true));
      if (!answer.ok) {
        onNotice(`${task.uid}: ${answer.error.message}`);
        return false;
      }
      await load();
      onChanged();
      return true;
    } finally {
      setSaving(false);
    }
  };

  const save = async (d: Draft): Promise<void> => {
    if (task === undefined) return;
    setSaving(true);
    try {
      const saved = await saveDraft(op, task.uid, d);
      if (saved.kind === "failed") {
        onNotice(`${task.uid}: ${saved.message}`);
        return;
      }
      // Either way the text shown beside the draft must be the task's as it is now.
      await load();
      if (saved.kind === "stale") {
        setConflict(saved.message);
        return;
      }
      setDraft(undefined);
      setConflict(undefined);
      onChanged();
    } finally {
      setSaving(false);
    }
  };

  const discard = (): void => {
    setDraft(undefined);
    setConflict(undefined);
  };

  if (task === undefined) {
    return (
      <section className="task-pane">
        <header className="pane-bar">
          <h2>Task</h2>
        </header>
        <p className="muted plan-hint">Select a task in a plan to see its claim, notes and history.</p>
      </section>
    );
  }

  const loaded = detail.kind === "loaded" ? detail.detail : undefined;
  const text = draft?.text ?? loaded?.content ?? "";
  const open = (f: { readonly uid: string; readonly design?: string | null }): void => {
    if (listed(f.uid)) onSelect(f.uid);
    else if (f.design !== undefined && f.design !== null) nav.openDesign(f.design);
  };
  const repin = playPins([task], pick).length > 0;

  return (
    <section className="task-pane">
      <header className="pane-bar">
        <h2 title={task.uid}>{task.title}</h2>
        <span className="spacer" />
        <button
          type="button"
          className="bar-button"
          onClick={() => nav.openTimeline("task", task.uid)}
          title="Its path through its strategy, in the Executions tab"
        >
          Timeline
        </button>
        <button
          type="button"
          className="bar-button play-button"
          disabled={busy || isTerminal(task.status)}
          onClick={() => onPlay(task)}
          title={repin ? `Pins the task to ${pick?.listed}, then opens its worktree with Claude` : "Opens its worktree with Claude"}
        >
          ▶ Play
        </button>
      </header>
      <div className="pane-body">
        <dl className="task-facts">
          <dt>Task</dt>
          <dd>
            <code>{task.uid}</code>
          </dd>
          <dt>Status</dt>
          <dd>
            <span className="task-status" data-status={task.status ?? "unknown"}>
              {loaded?.status ?? task.status ?? "?"}
            </span>
          </dd>
          <dt>Claim</dt>
          <dd>{task.claimed_by ?? <span className="muted">unclaimed</span>}</dd>
          <dt>Strategy</dt>
          <dd>
            {task.strategy}
            {repin && <span className="muted"> · Play pins it to {pick?.listed}</span>}
          </dd>
        </dl>
        {loaded !== undefined && loaded.found_in.length > 0 && (
          <p className="task-found-in">
            Found in{" "}
            {loaded.found_in.map((t, i) => (
              <span key={t.uid}>
                {i > 0 ? ", " : ""}
                {listed(t.uid) ? (
                  <button type="button" className="link-button" onClick={() => onSelect(t.uid)} title={t.uid}>
                    {t.title}
                  </button>
                ) : (
                  <span title={t.uid}>{t.title}</span>
                )}
              </span>
            ))}
          </p>
        )}
        <MoveToStep
          uid={task.uid}
          repo={repo}
          designs={designs}
          onMoved={() => {
            void load();
            onChanged();
            onMoved?.();
          }}
          onNotice={onNotice}
        />
        {detail.kind === "failed" && <p className="muted plan-hint">Cannot read the task: {detail.message}</p>}
        <label className="task-notes">
          <span>Text and notes</span>
          <textarea
            value={text}
            rows={8}
            disabled={loaded === undefined || saving}
            onChange={(e) => {
              const value = e.target.value;
              setDraft((d) => ({ text: value, base: d?.base ?? loaded?.content ?? "" }));
            }}
            aria-label="Task text"
          />
        </label>
        {conflict !== undefined && draft !== undefined && loaded !== undefined && (
          <div className="task-conflict" role="alert">
            <p>The task changed since you started editing, so Save wrote nothing. Your draft is kept; the task now reads:</p>
            <pre className="task-current">{loaded.content}</pre>
            <div className="task-actions">
              <button type="button" className="bar-button" disabled={saving} onClick={discard}>
                Discard my draft
              </button>
              <button
                type="button"
                className="bar-button"
                disabled={saving}
                title="Merge what you need from the text above into your draft first: the next Save replaces it"
                onClick={() => {
                  setDraft(rebaseDraft(draft, loaded.content));
                  setConflict(undefined);
                }}
              >
                Keep my draft over it
              </button>
            </div>
          </div>
        )}
        <div className="task-actions">
          <button
            type="button"
            className="bar-button"
            disabled={draft === undefined || draft.text === draft.base || conflict !== undefined || saving}
            onClick={() => {
              if (draft !== undefined) void save(draft);
            }}
          >
            Save
          </button>
          <button type="button" className="bar-button" disabled={draft === undefined || saving} onClick={discard}>
            Revert
          </button>
        </div>
        <form
          className="task-append"
          onSubmit={(e) => {
            e.preventDefault();
            if (note.trim() === "") return;
            void append(note).then((ok) => ok && setNote(""));
          }}
        >
          <input
            value={note}
            placeholder="Append a note…"
            disabled={loaded === undefined || saving}
            onChange={(e) => setNote(e.target.value)}
            aria-label="Append a note"
          />
          <button type="submit" className="bar-button" disabled={note.trim() === "" || saving}>
            Append
          </button>
        </form>
        {loaded !== undefined && loaded.findings.length > 0 && (
          <ReviewFindings findings={loaded.findings} rounds={loaded.review_namespaces} onOpen={open} />
        )}
        {loaded !== undefined && loaded.transitions.length > 0 && (
          <>
            <h3 className="task-subhead">Transitions</h3>
            <ol className="task-transitions">
              {loaded.transitions.map((t, i) => (
                <li key={`${t.at}-${i}`}>
                  <time dateTime={t.at}>{t.at.replace("T", " ").replace(/\.\d+Z$/, "Z")}</time> {t.event} → {t.to}
                  {t.branch !== null ? <span className="muted"> · {t.branch}</span> : null}
                  {t.pr !== null ? <span className="muted"> · PR #{t.pr}</span> : null}
                </li>
              ))}
            </ol>
          </>
        )}
      </div>
    </section>
  );
}

/** The findings grouped by round: recorded rounds in `rounds` order, then any filed but not recorded. */
export function findingsByRound(
  findings: readonly FindingSummary[],
  rounds: readonly string[],
): { readonly round: string; readonly findings: readonly FindingSummary[] }[] {
  const names = [...rounds];
  for (const f of findings) {
    const r = f.round ?? "";
    if (!names.includes(r)) names.push(r);
  }
  return names
    .map((round) => ({ round, findings: findings.filter((f) => (f.round ?? "") === round) }))
    .filter((g) => g.findings.length > 0);
}

/**
 * *Review findings*: what each review of this task filed (`task.show`'s `findings`, linked by
 * `found_in`), by round, each with its status, whether it is must-fix and the step it sits under
 * now. They are not subtasks: nothing here holds the task.
 */
function ReviewFindings({
  findings,
  rounds,
  onOpen,
}: {
  readonly findings: readonly FindingSummary[];
  readonly rounds: readonly string[];
  readonly onOpen: (f: FindingSummary) => void;
}): React.JSX.Element {
  return (
    <section className="task-findings" aria-label="Review findings">
      <h3 className="task-subhead">Review findings</h3>
      {findingsByRound(findings, rounds).map((g) => (
        <div key={g.round} className="finding-round">
          <h4 title={g.round}>{g.round === "" ? "No round" : g.round}</h4>
          <ul>
            {g.findings.map((f) => (
              <li key={f.uid}>
                <button type="button" className="plan-task" onClick={() => onOpen(f)} title={f.uid}>
                  <span className="task-status" data-status={f.status ?? "unknown"}>
                    {f.status ?? "?"}
                  </span>
                  {f.must_fix && <span className="finding-must-fix">must-fix</span>}
                  <span className="task-title">{f.title}</span>
                  {(f.step_title ?? f.step) !== null && <span className="finding-step muted">{f.step_title ?? f.step}</span>}
                </button>
              </li>
            ))}
          </ul>
        </div>
      ))}
    </section>
  );
}

type StepChoice = { readonly uid: string; readonly label: string };

/**
 * *Move to step*: re-home the task under a plan step with `task.move` — first the repo's Open work ›
 * Inbox (`step:inbox@<repo>`, made on demand), then any step of a design in the repo, whose live
 * steps are read with one `design.plans` when it is chosen. A refusal is the tab's notice.
 */
function MoveToStep({
  uid,
  repo,
  designs,
  onMoved,
  onNotice,
}: {
  readonly uid: string;
  readonly repo: string | undefined;
  readonly designs: RepoNode | undefined;
  readonly onMoved: () => void;
  readonly onNotice: (message: string) => void;
}): React.JSX.Element {
  const [where, setWhere] = useState("");
  const [steps, setSteps] = useState<readonly StepChoice[] | undefined>(undefined);
  const [step, setStep] = useState("");
  const [busy, setBusy] = useState(false);
  const designList = designs === undefined ? [] : treeOrder([designs]);

  const choose = async (value: string): Promise<void> => {
    setWhere(value);
    setSteps(undefined);
    setStep("");
    if (value === "" || value === "inbox") return;
    const answer = decodePlanList(await op(planOps.plans(value)));
    if (!answer.ok) {
      onNotice(`Cannot list the steps: ${answer.error.message}`);
      return;
    }
    const choices = partitionPlans(answer.value.plans).live.flatMap((p) =>
      p.steps.map((s, i) => ({ uid: s.uid, label: `${p.title} › ${i + 1}. ${s.text}` })),
    );
    setSteps(choices);
    setStep(choices[0]?.uid ?? "");
  };

  const under = where === "inbox" && repo !== undefined ? inboxStep(repo) : where !== "" ? step : "";
  const move = async (): Promise<void> => {
    if (under === "") return;
    setBusy(true);
    try {
      const answer = await op(planOps.move(uid, under));
      if (!answer.ok) {
        onNotice(`${uid}: ${answer.error.message}`);
        return;
      }
      setWhere("");
      setSteps(undefined);
      onMoved();
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="task-move" aria-label="Move to step">
      <label className="picker">
        <span>Move to step</span>
        <select value={where} disabled={busy} onChange={(e) => void choose(e.target.value)} aria-label="Move to">
          <option value="">Choose…</option>
          {repo !== undefined && <option value="inbox">Open work › Inbox</option>}
          {designList.map((d) => (
            <option key={d.uid} value={d.uid}>
              {d.title}
            </option>
          ))}
        </select>
      </label>
      {where !== "" && where !== "inbox" && (
        <select
          value={step}
          disabled={busy || steps === undefined || steps.length === 0}
          onChange={(e) => setStep(e.target.value)}
          aria-label="Step"
        >
          {steps === undefined ? (
            <option value="">Loading steps…</option>
          ) : steps.length === 0 ? (
            <option value="">No live steps</option>
          ) : (
            steps.map((c) => (
              <option key={c.uid} value={c.uid}>
                {c.label}
              </option>
            ))
          )}
        </select>
      )}
      <button type="button" className="bar-button" disabled={busy || under === ""} onClick={() => void move()}>
        Move
      </button>
    </div>
  );
}
