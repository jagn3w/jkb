import { useCallback, useEffect, useState } from "react";

import {
  decodeTaskDetail,
  isTerminal,
  planOps,
  playPins,
  rebaseDraft,
  saveDraft,
  type Draft,
  type Pick,
  type PlanTask,
  type TaskDetail,
} from "@jkb/core";

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
}: {
  readonly task: PlanTask | undefined;
  /** The strategy the operator picked for a Play, if any. */
  readonly pick: Pick | undefined;
  readonly busy: boolean;
  readonly onPlay: (task: PlanTask) => void;
  readonly onChanged: () => void;
  readonly onNotice: (message: string) => void;
}): React.JSX.Element {
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
  const repin = playPins([task], pick).length > 0;

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
