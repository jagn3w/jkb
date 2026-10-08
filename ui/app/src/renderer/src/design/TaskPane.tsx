import { useCallback, useEffect, useState } from "react";

import { decodeTaskDetail, isTerminal, planOps, playPins, type PlanTask, type TaskDetail } from "@jkb/core";

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
 */
export function TaskPane({
  task,
  strategy,
  busy,
  onPlay,
  onChanged,
  onNotice,
}: {
  readonly task: PlanTask | undefined;
  readonly strategy: string | undefined;
  readonly busy: boolean;
  readonly onPlay: (task: PlanTask) => void;
  readonly onChanged: () => void;
  readonly onNotice: (message: string) => void;
}): React.JSX.Element {
  const [detail, setDetail] = useState<Detail>({ kind: "loading" });
  const [draft, setDraft] = useState<string | undefined>(undefined);
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

  const write = async (text: string, append: boolean): Promise<boolean> => {
    if (task === undefined) return false;
    setSaving(true);
    try {
      const answer = await op(planOps.edit(task.uid, text, append));
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
  const text = draft ?? loaded?.content ?? "";
  const repin = playPins([task], strategy).length > 0;

  return (
    <section className="task-pane">
      <header className="pane-bar">
        <h2 title={task.uid}>{task.title}</h2>
        <span className="spacer" />
        <button
          type="button"
          className="bar-button play-button"
          disabled={busy || isTerminal(task.status)}
          onClick={() => onPlay(task)}
          title={repin ? `Pins the task to ${strategy}, then opens its worktree with Claude` : "Opens its worktree with Claude"}
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
            {repin && <span className="muted"> · Play pins it to {strategy}</span>}
          </dd>
        </dl>
        {detail.kind === "failed" && <p className="muted plan-hint">Cannot read the task: {detail.message}</p>}
        <label className="task-notes">
          <span>Text and notes</span>
          <textarea
            value={text}
            rows={8}
            disabled={loaded === undefined || saving}
            onChange={(e) => setDraft(e.target.value)}
            aria-label="Task text"
          />
        </label>
        <div className="task-actions">
          <button
            type="button"
            className="bar-button"
            disabled={draft === undefined || draft === loaded?.content || saving}
            onClick={() => void write(text, false).then((ok) => ok && setDraft(undefined))}
          >
            Save
          </button>
          <button type="button" className="bar-button" disabled={draft === undefined || saving} onClick={() => setDraft(undefined)}>
            Revert
          </button>
        </div>
        <form
          className="task-append"
          onSubmit={(e) => {
            e.preventDefault();
            if (note.trim() === "") return;
            void write(note, true).then((ok) => ok && setNote(""));
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
