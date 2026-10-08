import { useEffect, useMemo, useState } from "react";

import {
  decodeAgent,
  draftOf,
  editBetween,
  ISOLATIONS,
  placeholdersOf,
  ROLES,
  workflowOps,
  WRITES,
  type AgentDraft,
  type AgentTemplate,
  type Isolation,
  type Writes,
} from "@jkb/core";

import { useTerminals } from "../terminal/TerminalProvider";
import { contributeSpec } from "./contribute";

const op = (request: Parameters<typeof window.jkb.op>[0]) => window.jkb.op(request);

function handoffs(text: string): string[] {
  return text
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s !== "");
}

/**
 * One agent's side panel (D53.7): its template and permissions, edited in place and saved as an
 * operator copy. A packaged template is read-only in jkb, so its first save makes the copy (which
 * then overrides it — what the workflow script reads by name) and applies the edit to that; a copy
 * is edited as itself. *Revert* copies the packaged text back over the copy; *Contribute* exports the
 * saved copy to the packaged-templates file on a branch and opens a pull request, in the container.
 *
 * The permissions shown are both halves: what the role may do in jkb (its op classes, enforced by
 * `jkb serve`) and what the script lets the agent do (where it runs, its model, the most it may
 * change).
 */
export function AgentPanel({
  agent,
  onSaved,
  onNotice,
}: {
  readonly agent: AgentTemplate;
  readonly onSaved: (name: string) => void;
  readonly onNotice: (message: string, failed?: boolean) => void;
}): React.JSX.Element {
  const terminals = useTerminals();
  const [draft, setDraft] = useState<AgentDraft>(() => draftOf(agent));
  const [hands, setHands] = useState(agent.hands_off_to.join(", "));
  const [busy, setBusy] = useState(false);

  // A different agent, or a new version of this one: start from what jkb holds. Keyed on what names
  // the version, not the object, so a refresh that re-reads the same version keeps an unsaved edit.
  const held = `${agent.name}@${agent.source}@${agent.version}`;
  useEffect(() => {
    setDraft(draftOf(agent));
    setHands(agent.hands_off_to.join(", "));
  }, [held]);

  const current: AgentDraft = useMemo(() => ({ ...draft, hands_off_to: handoffs(hands) }), [draft, hands]);
  const edit = useMemo(() => editBetween(agent, current), [agent, current]);
  const dirty = Object.keys(edit).length > 0;
  const placeholders = useMemo(() => placeholdersOf(current.template), [current.template]);
  const packaged = agent.source === "packaged";

  const run = async (work: () => Promise<boolean>): Promise<void> => {
    setBusy(true);
    try {
      if (await work()) onSaved(agent.name);
    } finally {
      setBusy(false);
    }
  };

  const save = (): Promise<void> =>
    run(async () => {
      if (packaged) {
        const copied = decodeAgent(await op(workflowOps.copy(agent.name)));
        if (!copied.ok) {
          onNotice(copied.error.message, true);
          return false;
        }
      }
      if (dirty) {
        const set = decodeAgent(await op(workflowOps.set(agent.name, edit)));
        if (!set.ok) {
          // A copy made just now stays, holding the packaged text: say so rather than hide it.
          onNotice(packaged ? `Copied, but the edit was refused: ${set.error.message}` : set.error.message, true);
          return packaged;
        }
        onNotice(`${agent.name} saved as v${set.value.agent.version}.`);
      } else {
        onNotice(`${agent.name} is now an operator copy.`);
      }
      return true;
    });

  const revert = (): Promise<void> =>
    run(async () => {
      const back = decodeAgent(await op(workflowOps.copy(agent.name, { packaged: true })));
      if (!back.ok) {
        onNotice(back.error.message, true);
        return false;
      }
      onNotice(`${agent.name} is the packaged text again (v${back.value.agent.version} of the copy).`);
      return true;
    });

  const contribute = (): void => {
    const roots = terminals.roots;
    if (roots === undefined) {
      onNotice("The terminal is not ready yet.", true);
      return;
    }
    const spec = contributeSpec(agent.name, roots);
    if (spec === undefined) {
      onNotice(`\`${agent.name}\` is not a name jkb would package.`, true);
      return;
    }
    terminals.open(spec, "drawer");
  };

  const permissions = current.permissions;
  const setPermissions = (p: Partial<AgentDraft["permissions"]>): void =>
    setDraft((d) => ({ ...d, permissions: { ...d.permissions, ...p } }));

  return (
    <section className="agent-panel" aria-label={`Agent ${agent.name}`}>
      <header className="pane-bar">
        <h2 title={agent.name}>{agent.name}</h2>
        <span className="agent-source" data-source={agent.source}>
          {packaged ? "packaged" : agent.overrides_packaged ? "copy" : "operator's"} · v{agent.version}
        </span>
      </header>
      <div className="pane-body agent-fields">
        <p className="muted plan-hint">
          {agent.workflow}
          {agent.based_on !== null ? ` · copied from ${agent.based_on}` : ""}
          {agent.overrides_packaged && agent.packaged_version !== null ? ` · overrides packaged v${agent.packaged_version}` : ""}
        </p>
        {agent.behind_packaged && (
          <p className="agent-warning">The packaged template has moved on since this copy was taken.</p>
        )}
        {packaged && (
          <p className="muted plan-hint">Packaged templates are read-only: saving makes an operator copy that overrides it.</p>
        )}
        <label className="field">
          <span>Description</span>
          <input
            value={current.describe}
            disabled={busy}
            onChange={(e) => setDraft((d) => ({ ...d, describe: e.target.value }))}
          />
        </label>
        <div className="field-row">
          <label className="field">
            <span>Role</span>
            <select value={current.role} disabled={busy} onChange={(e) => setDraft((d) => ({ ...d, role: e.target.value }))}>
              {ROLES.map((r) => (
                <option key={r} value={r}>
                  {r}
                </option>
              ))}
            </select>
          </label>
          <label className="field">
            <span>Runs in</span>
            <select
              value={permissions.isolation}
              disabled={busy}
              onChange={(e) => setPermissions({ isolation: e.target.value as Isolation })}
            >
              {ISOLATIONS.map((i) => (
                <option key={i} value={i}>
                  {i === "none" ? "the checkout" : "its own worktree"}
                </option>
              ))}
            </select>
          </label>
        </div>
        <div className="field-row">
          <label className="field">
            <span>Model</span>
            <input
              value={permissions.model ?? ""}
              placeholder="the session's"
              disabled={busy}
              onChange={(e) => setPermissions({ model: e.target.value === "" ? null : e.target.value })}
            />
          </label>
          <label className="field">
            <span>May change</span>
            <select value={permissions.writes} disabled={busy} onChange={(e) => setPermissions({ writes: e.target.value as Writes })}>
              {WRITES.map((w) => (
                <option key={w} value={w}>
                  {w}
                </option>
              ))}
            </select>
          </label>
        </div>
        <p className="muted plan-hint" title="The op classes jkb serve lets this role run (`jkb role matrix`)">
          In jkb, a {agent.role} may: {agent.role_ops.join(", ") || "nothing"}
        </p>
        <label className="field">
          <span>Hands off to</span>
          <input value={hands} disabled={busy || agent.fragment} placeholder="agent names, comma-separated" onChange={(e) => setHands(e.target.value)} />
        </label>
        <label className="field field-grow">
          <span>
            Template
            {placeholders.length > 0 && <span className="muted"> · {placeholders.map((p) => `{{${p}}}`).join(" ")}</span>}
          </span>
          <textarea
            className="agent-template"
            value={current.template}
            spellCheck={false}
            disabled={busy}
            onChange={(e) => setDraft((d) => ({ ...d, template: e.target.value }))}
          />
        </label>
        <div className="task-actions">
          <button type="button" className="bar-button play-button" disabled={busy || (!packaged && !dirty)} onClick={() => void save()}>
            {packaged ? "Save as operator copy" : "Save"}
          </button>
          {dirty && (
            <button type="button" className="bar-button" disabled={busy} onClick={() => { setDraft(draftOf(agent)); setHands(agent.hands_off_to.join(", ")); }}>
              Discard
            </button>
          )}
          {agent.overrides_packaged && (
            <button type="button" className="bar-button" disabled={busy || dirty} onClick={() => void revert()}>
              Revert to packaged
            </button>
          )}
          {!packaged && (
            <button
              type="button"
              className="bar-button"
              disabled={busy || dirty}
              title={dirty ? "Save first: the contribution exports the saved copy" : "Export to the packaged templates on a branch and open a pull request (in the container)"}
              onClick={contribute}
            >
              Contribute to jkb
            </button>
          )}
        </div>
      </div>
    </section>
  );
}
