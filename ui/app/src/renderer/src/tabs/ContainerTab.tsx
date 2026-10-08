import { useCallback, useEffect, useRef, useState } from "react";

import {
  CONTAINER_ACTIONS,
  availability,
  findings,
  shortCommit,
  type ContainerAction,
  type ContainerStatus,
  type ImageStamp,
} from "@jkb/core";

import { useTerminals } from "../terminal/TerminalProvider";

type Loaded =
  | { readonly kind: "loading" }
  | { readonly kind: "loaded"; readonly value: ContainerStatus }
  | { readonly kind: "failed"; readonly message: string };

/** An image id as the tab shows it: docker's short form. */
function shortId(id: string): string {
  return id.replace(/^sha256:/, "").slice(0, 12);
}

/** When, as the operator's locale reads it, with the stored value in the title. */
function When({ at }: { readonly at: string | null }): React.JSX.Element {
  if (at === null) return <span className="muted">not stamped</span>;
  const t = new Date(at);
  return <time dateTime={at} title={at}>{Number.isNaN(t.getTime()) ? at : t.toLocaleString()}</time>;
}

/** What an image says about itself: the labels the kit's build stamps (D53.8). */
function Stamp({ image }: { readonly image: ImageStamp | null }): React.JSX.Element {
  if (image === null) return <p className="muted plan-hint">none</p>;
  return (
    <dl className="container-facts">
      <dt>Image</dt>
      <dd>
        <code title={image.id}>{shortId(image.id)}</code>
      </dd>
      <dt>Built</dt>
      <dd>
        <When at={image.builtAt} />
      </dd>
      <dt>Source</dt>
      <dd>
        <code title={image.sourceCommit ?? undefined}>{shortCommit(image.sourceCommit)}</code>
        {image.sourceBranch !== null && <span className="muted"> on {image.sourceBranch}</span>}
      </dd>
    </dl>
  );
}

const DRIFT_TEXT: Record<string, string> = {
  same: "matches",
  differs: "differs",
  unrecorded: "not recorded",
  unknown: "unknown",
};

/**
 * The Container tab (D53.8): what the dev container is, and buttons over the INSTALLED KIT's
 * `run.sh` — never the checkout's. Each button is one `run.sh` flag, run on the host in the
 * integrated terminal so its output streams there; main finds the kit and builds that terminal, and
 * the tab only names the action. What the container is comes from `run.sh --status`: its state, the
 * image's `jkb.built-at`/`jkb.source-commit`/`jkb.source-branch` labels, and its drift from the
 * declaration, in run.sh's own words.
 *
 * Re-read on demand (the refresh button) and when a button's run ends: nothing announces a change
 * to the container, and D53.1 rules out a poll loop.
 */
export function ContainerTab(): React.JSX.Element {
  const terminals = useTerminals();
  const [status, setStatus] = useState<Loaded>({ kind: "loading" });
  const [notice, setNotice] = useState<string | undefined>(undefined);
  /** The terminal running a button's action, until it ends. */
  const [running, setRunning] = useState<{ readonly key: number; readonly action: ContainerAction } | undefined>(undefined);
  const live = useRef(true);

  const load = useCallback(async () => {
    setStatus((s) => (s.kind === "loaded" ? s : { kind: "loading" }));
    const answer = await window.jkb.container.status();
    if (!live.current) return;
    setStatus(answer.ok ? { kind: "loaded", value: answer.value } : { kind: "failed", message: answer.error });
  }, []);

  useEffect(() => {
    live.current = true;
    void load();
    return () => {
      live.current = false;
    };
  }, [load]);

  // A button's run has ended (or its terminal was closed): read what it left.
  const entry = running === undefined ? undefined : terminals.state.entries.find((e) => e.key === running.key);
  const ended = running !== undefined && (entry === undefined || entry.status.kind === "exited" || entry.status.kind === "failed");
  useEffect(() => {
    if (!ended) return;
    setRunning(undefined);
    void load();
  }, [ended, load]);

  const run = async (action: ContainerAction): Promise<void> => {
    const spec = CONTAINER_ACTIONS.find((a) => a.id === action);
    if (spec === undefined) return;
    if (spec.ends && !window.confirm(`${spec.label} the container? ${spec.summary}`)) return;
    setNotice(undefined);
    const answer = await window.jkb.container.spec(action);
    if (!answer.ok) {
      setNotice(answer.error);
      return;
    }
    setRunning({ key: terminals.open(answer.value, "drawer"), action });
  };

  const current = status.kind === "loaded" ? status.value : undefined;
  const said = current === undefined ? [] : findings(current);

  return (
    <div className="container-tab">
      <header className="design-bar">
        <h1>Container</h1>
        {current !== undefined && (
          <span className="muted">
            <code>{current.name}</code> from image <code>{current.image}</code>
          </span>
        )}
        <span className="spacer" />
        {CONTAINER_ACTIONS.map((a) => {
          const can = availability(a.id, current);
          return (
            <button
              key={a.id}
              type="button"
              className="bar-button"
              data-action={a.id}
              data-ends={a.ends ? "true" : undefined}
              title={can.enabled ? a.summary : `${a.summary} Unavailable: ${can.why ?? ""}.`}
              disabled={!can.enabled || running !== undefined}
              onClick={() => void run(a.id)}
            >
              {running?.action === a.id ? `${a.label}…` : a.label}
            </button>
          );
        })}
        <button type="button" className="bar-button" onClick={() => void load()} disabled={status.kind === "loading"}>
          Refresh
        </button>
      </header>
      {notice !== undefined && (
        <p className="design-notice" data-kind="failed" role="status">
          {notice}
          <span className="spacer" />
          <button type="button" className="bar-button" onClick={() => setNotice(undefined)} aria-label="Dismiss">
            ×
          </button>
        </p>
      )}
      <div className="container-body">
        {status.kind === "loading" ? (
          <p className="muted plan-hint">Asking the kit's run.sh…</p>
        ) : status.kind === "failed" ? (
          <p className="container-error" role="status">
            {status.message}
          </p>
        ) : (
          <>
            <ul className="container-findings" aria-label="Standing">
              {said.map((f) => (
                <li key={f.text} data-level={f.level}>
                  {f.text}
                </li>
              ))}
            </ul>
            {current !== undefined && current.docker === "reachable" && (
              <div className="container-grid">
                <section aria-label="The container">
                  <h2>Container</h2>
                  {current.container === null ? (
                    <p className="muted plan-hint">none</p>
                  ) : (
                    <>
                      <dl className="container-facts">
                        <dt>State</dt>
                        <dd>{current.container.state}</dd>
                        <dt>Arguments</dt>
                        <dd data-drift={current.drift.args ?? undefined}>
                          {DRIFT_TEXT[current.drift.args ?? "unknown"]}
                          <span className="muted">
                            {" "}
                            (<code title={current.container.argsHash ?? undefined}>{current.container.argsHash?.slice(0, 12) ?? "none"}</code> vs{" "}
                            <code title={current.wantArgsHash}>{current.wantArgsHash.slice(0, 12)}</code>)
                          </span>
                        </dd>
                        <dt>Image</dt>
                        <dd data-drift={current.drift.image ?? undefined}>{DRIFT_TEXT[current.drift.image ?? "unknown"]} the tag</dd>
                      </dl>
                      <h3>Its image</h3>
                      <Stamp image={current.container.image} />
                    </>
                  )}
                </section>
                <section aria-label="The image on disk">
                  <h2>
                    Image <code>{current.image}</code>
                  </h2>
                  <Stamp image={current.imageOnDisk} />
                </section>
                <section aria-label="The kit">
                  <h2>Kit</h2>
                  <dl className="container-facts">
                    <dt>Kit</dt>
                    <dd>
                      <code>{current.kit ?? "none"}</code>
                    </dd>
                    <dt>Checkout</dt>
                    <dd>
                      <code>{current.checkout ?? "none"}</code>
                    </dd>
                    <dt>Changed since</dt>
                    <dd>{current.kitChanged.length === 0 ? "nothing" : current.kitChanged.join(", ")}</dd>
                  </dl>
                </section>
              </div>
            )}
          </>
        )}
      </div>
    </div>
  );
}
