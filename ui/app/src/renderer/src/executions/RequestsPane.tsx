import { useCallback, useEffect, useRef, useState } from "react";

import {
  NOT_IN_A_REPOSITORY,
  decodeExecRequest,
  escapeArg,
  outcomeText,
  requestView,
  tailText,
  type ExecView,
} from "@jkb/core";

import type { ExecPreview, ExecStarted } from "../../../shared/bridge";
import { useNavigation } from "../navigation";
import { useTerminals } from "../terminal/TerminalProvider";
import { DiffPre, EscapedPre, Segments } from "./Escaped";
import { ClickGate, MAX_PAGES, declineOp, history, historyFull, listAll, runBlocked } from "./queue";

const op = (request: Parameters<typeof window.jkb.op>[0]) => window.jkb.op(request);

type Loaded =
  | { readonly kind: "loading" }
  | { readonly kind: "loaded"; readonly open: readonly ExecView[]; readonly ended: readonly ExecView[]; readonly openCut: boolean }
  | { readonly kind: "failed"; readonly message: string };

/**
 * The Requests section of the Executions tab (the Executions design): every pending run request,
 * with exactly what would run — the requester, the reason, the argv one argument per line with
 * control and invisible characters escaped, the directory and target, the program file, and the diff
 * of the pinned tree against a base the host verified — and Run and Decline. Below it, the history:
 * what finished, was declined, refused or expired, with its output.
 *
 * Live by subscription, never a poll: main's group on `exec/requests` says when a request was made
 * or moved, and the queue re-reads `exec.list`. Everything a session or a repository wrote — and
 * every message that may quote it — is shown as escaped text, never as HTML.
 */
export function RequestsPane(): React.JSX.Element {
  const [loaded, setLoaded] = useState<Loaded>({ kind: "loading" });
  const [feed, setFeed] = useState<string | undefined>(undefined);
  const reads = useRef(0);
  const gate = useRef(new ClickGate());

  const reload = useCallback(async () => {
    const mine = ++reads.current;
    const [pending, running, all] = await Promise.all([
      listAll(op, "pending"),
      listAll(op, "running"),
      listAll(op, undefined, { newest: true, enough: historyFull }),
    ]);
    if (mine !== reads.current) return;
    if (!pending.ok || !running.ok || !all.ok) {
      const e = !pending.ok ? pending.error : !running.ok ? running.error : !all.ok ? all.error : undefined;
      setLoaded({ kind: "failed", message: e?.message ?? "could not read the run requests" });
      return;
    }
    setLoaded({
      kind: "loaded",
      open: [...pending.value.requests, ...running.value.requests],
      ended: history(all.value.requests),
      openCut: pending.value.cut || running.value.cut,
    });
  }, []);

  useEffect(() => {
    let live = true;
    const off = window.jkb.exec.onEvent((event) => {
      if (event.kind === "changed" || event.kind === "gap") void reload();
      if (event.kind === "error") setFeed(event.message);
      if (event.kind === "live" || event.kind === "gap") setFeed(undefined);
    });
    // Read after the group is on the topic, so no change falls between the read and the feed.
    void window.jkb.exec.subscribe().then((joined) => {
      if (!live) return;
      if (!joined.ok) setFeed(`live updates are off: ${joined.error.message}`);
      void reload();
    });
    return () => {
      live = false;
      off();
      window.jkb.exec.unsubscribe();
    };
  }, [reload]);

  return (
    <section className="requests-pane" aria-label="Requests">
      <header className="pane-bar">
        <h2>Requests</h2>
        <button type="button" onClick={() => void reload()}>
          Refresh
        </button>
        {feed !== undefined && <span className="muted"> {escapeArg(feed)}</span>}
      </header>
      <div className="pane-body">
        {loaded.kind === "loading" && <p className="muted">Reading run requests…</p>}
        {loaded.kind === "failed" && (
          <p className="requests-failed plan-hint">
            Cannot read run requests: {escapeArg(loaded.message)}. Whether jkb serve answers is shown in the top bar.
          </p>
        )}
        {loaded.kind === "loaded" && (
          <>
            {loaded.openCut && (
              <p className="request-banner" role="status">
                More requests are waiting than the {MAX_PAGES} pages read here; the oldest are shown. Answer some, then refresh.
              </p>
            )}
            {loaded.open.length === 0 && <p className="muted">No request is waiting.</p>}
            {loaded.open.map((v) => (
              <RequestCard key={v.id} request={v} gate={gate.current} onDone={() => void reload()} />
            ))}
            <h3 className="requests-history-title">History</h3>
            {loaded.ended.length === 0 && <p className="muted">Nothing has finished yet.</p>}
            {loaded.ended.map((v) => (
              <HistoryRow key={v.id} request={v} />
            ))}
          </>
        )}
      </div>
    </section>
  );
}

/** At most 400 characters, then `…` (non-ASCII, so never part of shown argument text). */
function shortened(text: string): string {
  return text.length > 400 ? `${text.slice(0, 400)}…` : text;
}

function Target({ target }: { readonly target: string }): React.JSX.Element {
  return (
    <span className="target-badge" data-target={target === "host" ? "host" : "container"}>
      {target === "host" ? "host" : "container"}
    </span>
  );
}

function RequestCard({ request, gate, onDone }: { readonly request: ExecView; readonly gate: ClickGate; readonly onDone: () => void }): React.JSX.Element {
  const nav = useNavigation();
  const terminals = useTerminals();
  const view = requestView(request, Date.now());
  const [preview, setPreview] = useState<ExecPreview | undefined>(undefined);
  const [previewError, setPreviewError] = useState<string | undefined>(undefined);
  const [declining, setDeclining] = useState(false);
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(gate.holds(request.id));
  const [said, setSaid] = useState<string | undefined>(undefined);
  const [started, setStarted] = useState<ExecStarted | undefined>(undefined);
  const asks = useRef(0);

  const check = useCallback(async () => {
    const mine = ++asks.current;
    setPreview(undefined);
    setPreviewError(undefined);
    const p = await window.jkb.exec.preview(request.id);
    if (mine !== asks.current) return;
    if (p.ok) setPreview(p.value);
    else setPreviewError(p.error);
  }, [request.id]);

  useEffect(() => {
    if (request.state === "pending") void check();
  }, [check, request.state]);

  const blocked = request.state === "pending" ? runBlocked(preview) : `${request.id} is ${request.state}`;

  const run = (): void => {
    if (preview === undefined || blocked !== undefined || !gate.take(request.id)) return;
    setBusy(true);
    void (async () => {
      const r = await window.jkb.exec.run(request.id, {
        hostDir: preview.hostDir ?? "",
        ...(preview.program === undefined ? {} : { program: preview.program }),
      });
      if (r.ok) {
        setStarted(r.value);
        terminals.open(r.value.spec, "drawer", { once: true });
      } else {
        gate.release(request.id);
        setBusy(false);
        setSaid(r.error);
      }
      onDone();
    })();
  };

  const decline = (): void => {
    const d = declineOp(request.id, note);
    if (!d.ok) {
      setSaid(d.error);
      return;
    }
    if (!gate.take(request.id)) return;
    setBusy(true);
    void (async () => {
      const r = await op(d.request);
      const answered = r.ok ? decodeExecRequest(r.value) : r;
      if (!answered.ok) {
        gate.release(request.id);
        setBusy(false);
        setSaid(`Decline failed: ${answered.error.message}`);
      }
      onDone();
    })();
  };

  return (
    <article className="request-card" aria-label={`Run request ${request.id}`} data-state={request.state}>
      <header className="request-head">
        <strong>{view.id}</strong> <Target target={view.target} /> <span className="muted">{view.state}</span>
        {view.expiresIn !== undefined && <span className="muted"> · expires in {view.expiresIn}</span>}
      </header>
      <dl className="request-facts">
        <dt>Asked by</dt>
        <dd>
          <button type="button" className="link" onClick={() => nav.goTo("sessions")} title="Show the Sessions tab">
            {view.session}
          </button>{" "}
          ({view.role}
          {view.agentType !== undefined && `, ${view.agentType}`})
          {view.task !== undefined && (
            <>
              {" for "}
              <button type="button" className="link" onClick={() => nav.openTimeline("task", request.task ?? "")}>
                {view.task}
              </button>
            </>
          )}
        </dd>
        <dt>Reason</dt>
        <dd>{view.reason.length === 0 ? <span className="muted">none given</span> : <Segments segments={view.reason} />}</dd>
        <dt>Directory</dt>
        <dd>
          <code>{view.dir}</code> <span className="muted">({view.dirSide}'s path)</span>
          {preview?.hostDir !== undefined && (
            <>
              {" → on this side "}
              <code>{escapeArg(preview.hostDir)}</code>
            </>
          )}
        </dd>
        <dt>Timeout</dt>
        <dd>{view.timeout}</dd>
        <dt>Command</dt>
        <dd>
          <ol className="request-argv" aria-label="argv">
            {view.argv.map((a, i) => (
              <li key={i}>
                <code>{a.text}</code>
                {a.glyphs !== undefined && <span className="muted request-glyphs"> looks like: {a.glyphs}</span>}
              </li>
            ))}
          </ol>
          {preview?.programShown !== undefined && (
            <p className="muted">
              runs <code>{preview.programShown}</code>
            </p>
          )}
        </dd>
      </dl>
      {request.state === "pending" && <Preview preview={preview} error={previewError} />}
      {started !== undefined && (
        <p className="request-started">
          Started in the drawer: <code>{started.argv.map(escapeArg).join(" ")}</code>
        </p>
      )}
      {said !== undefined && <p className="request-said">{escapeArg(said)}</p>}
      {request.state === "pending" && started === undefined && (
        <div className="request-actions">
          <button type="button" className="primary" disabled={busy || blocked !== undefined} title={blocked === undefined ? undefined : escapeArg(blocked)} onClick={run}>
            Run
          </button>
          <button type="button" disabled={busy} onClick={() => setDeclining(!declining)}>
            Decline…
          </button>
          <button type="button" disabled={busy} onClick={() => void check()}>
            Check again
          </button>
          {blocked !== undefined && preview !== undefined && <span className="request-blocked">{escapeArg(blocked)}</span>}
        </div>
      )}
      {declining && request.state === "pending" && (
        <form
          className="request-decline"
          onSubmit={(e) => {
            e.preventDefault();
            decline();
          }}
        >
          <label>
            Note for the session (required): what to ask for instead
            <textarea value={note} required onChange={(e) => setNote(e.target.value)} rows={3} />
          </label>
          <button type="submit" disabled={busy || note.trim() === ""}>
            Decline
          </button>
        </form>
      )}
    </article>
  );
}

function Preview({ preview, error }: { readonly preview: ExecPreview | undefined; readonly error: string | undefined }): React.JSX.Element {
  if (error !== undefined) return <p className="request-said">Could not read what would run: {escapeArg(error)}</p>;
  if (preview === undefined) return <p className="muted">Pinning the tree and reading its diff…</p>;
  const { verdict, base, diff, programFile } = preview;
  return (
    <div className="request-preview">
      {verdict?.state === "changed" && <p className="request-verdict" role="alert">{escapeArg(verdict.message)}</p>}
      {verdict?.state === "no_repo" && <p className="muted">{NOT_IN_A_REPOSITORY}</p>}
      {preview.problem !== undefined && verdict?.state !== "changed" && <p className="request-verdict" role="alert">{escapeArg(preview.problem)}</p>}
      {base?.state === "unverified" && (
        <p className="request-unverified" role="alert">
          <strong>base unverified</strong>: {escapeArg(base.message)}. The diff below is against the request's own HEAD label, which the
          container could have moved.
        </p>
      )}
      {base?.state === "verified" && (
        <p className="muted">
          Base {base.sha.slice(0, 12)} ({escapeArg(base.ref)} of {escapeArg(base.url)}, verified by {base.source}).
        </p>
      )}
      {programFile !== undefined && (
        <details className="request-program" open={programFile.pinned === true}>
          <summary>
            Program file <code>{escapeArg(programFile.path)}</code>
            {programFile.pinned !== true && (
              <strong className="request-unverified"> outside the pin: can change before Run, so this is not reviewed code</strong>
            )}
          </summary>
          {programFile.kind === "file" && (
            <>
              {programFile.truncated && <p className="request-banner">Only its first 256 KiB are shown.</p>}
              <EscapedPre text={programFile.text} />
            </>
          )}
          {programFile.kind === "link" && <p>a link to <code>{escapeArg(programFile.target)}</code> (not followed)</p>}
          {programFile.kind === "absent" && <p className="muted">not there now</p>}
          {programFile.kind === "other" && <p className="muted">not a regular file</p>}
        </details>
      )}
      {diff?.state === "shown" && (
        <details className="request-diff" open>
          <summary>
            Diff of the pinned tree against {diff.against.slice(0, 12)}: {diff.files.length}
            {diff.filesTruncated ? "+" : ""} path{diff.files.length === 1 ? "" : "s"}
          </summary>
          <ul className="request-files">
            {diff.files.map((f, i) => (
              <li key={i}>
                <code className="file-status">{f.status}</code> <code>{escapeArg(f.path)}</code>
                {f.status === "I" && <span className="muted"> tracked on the base, ignored here: outside the pin, not shown</span>}
              </li>
            ))}
          </ul>
          {diff.filesTruncated && (
            <p className="request-banner" role="status">
              The list of changed paths was cut: more paths changed than are listed above.
            </p>
          )}
          {diff.truncated && (
            <p className="request-banner" role="status">
              The diff is longer than the 2 MiB shown and was cut.
              {diff.filesTruncated ? " Neither it nor the list of paths is complete." : " The list of paths above is complete."}
            </p>
          )}
          {diff.patch === "" ? <p className="muted">No changes against the base.</p> : <DiffPre patch={diff.patch} />}
        </details>
      )}
      {diff?.state === "no_repo" && <p className="muted">{NOT_IN_A_REPOSITORY}</p>}
      {diff?.state === "refused" && <p className="request-said">No diff: {escapeArg(diff.message)}</p>}
      {diff?.state === "error" && <p className="request-said">No diff: {escapeArg(diff.message)}</p>}
    </div>
  );
}

function HistoryRow({ request }: { readonly request: ExecView }): React.JSX.Element {
  const [full, setFull] = useState<ExecView | undefined>(undefined);
  const [failed, setFailed] = useState<string | undefined>(undefined);
  const view = requestView(request, Date.now());
  const open = async (): Promise<void> => {
    if (full !== undefined) return;
    const r = await op({ op: "exec.status", id: request.id });
    const d = r.ok ? decodeExecRequest(r.value) : r;
    if (d.ok) setFull(d.value);
    else setFailed(d.error.message);
  };
  const v = full ?? request;
  const tail = full === undefined ? undefined : tailText(full);
  return (
    <details className="history-row" onToggle={(e) => (e.currentTarget.open ? void open() : undefined)}>
      <summary>
        <strong>{view.id}</strong> <Target target={view.target} /> {outcomeText(v)}{" "}
        {/* `·` never occurs in shown argument text (it would be spelt `\u{00b7}`), so it separates. */}
        <code className="history-argv">{shortened(view.argv.map((a) => a.text).join(" · "))}</code>
      </summary>
      <dl className="request-facts">
        <dt>Asked by</dt>
        <dd>
          {view.session} ({view.role})
        </dd>
        <dt>Reason</dt>
        <dd>
          <Segments segments={view.reason} />
        </dd>
        {v.note !== undefined && (
          <>
            <dt>Note</dt>
            <dd>
              <EscapedPre text={v.note} className="exec-note" />
            </dd>
          </>
        )}
        {v.exit_status !== undefined && (
          <>
            <dt>Exit status</dt>
            <dd>{v.exit_status}</dd>
          </>
        )}
        {v.output?.log_path !== undefined && (
          <>
            <dt>Log</dt>
            <dd>
              <code>{escapeArg(v.output.log_path)}</code>
            </dd>
          </>
        )}
      </dl>
      {failed !== undefined && <p className="request-said">{escapeArg(failed)}</p>}
      {v.output?.truncated === true && <p className="request-banner">Only the last 256 KiB are kept here; the log has it all.</p>}
      {tail !== undefined && <EscapedPre text={tail} />}
    </details>
  );
}
