import { useEffect, useRef } from "react";

import { templatesActingAs, toggleMarks, type AgentTemplate, type RoleInfo, type RolesView, type ToggleMark } from "@jkb/core";

/** What a role with no agent template and no mapped agent type reads as. */
export const NO_AGENT = "no agent: done by hand";

/**
 * Grants, each toggle-controlled one marked with its toggle's name (its label on hover), then what
 * a toggle could grant this role but does not under the strategy, marked "(off in this strategy)".
 * `marks` are the role's toggles that control what this list holds.
 */
function Grants({
  granted,
  marks,
  none,
}: {
  readonly granted: readonly string[];
  readonly marks: readonly ToggleMark[];
  readonly none: string;
}): React.JSX.Element {
  const off = marks.filter((m) => !m.on);
  if (granted.length === 0 && off.length === 0) return <span className="muted">{none}</span>;
  return (
    <ul className="role-grants">
      {granted.map((g) => {
        const by = marks.find((m) => m.on && m.controls.includes(g));
        return (
          <li key={g}>
            {g}
            {by !== undefined && (
              <span className="role-toggle" data-on="true" title={by.label}>
                {by.toggle}
              </span>
            )}
          </li>
        );
      })}
      {off.map((m) => (
        <li key={`off:${m.toggle}`} className="muted" data-off="true">
          {m.controls.join(", ")}
          <span className="role-toggle" data-on="false" title={m.label}>
            {`${m.toggle} (off in this strategy)`}
          </span>
        </li>
      ))}
    </ul>
  );
}

/** The marks of the toggles that grant workflow events, and of the one that grants landing. */
function marksOf(view: RolesView, role: RoleInfo): { readonly events: ToggleMark[]; readonly land: ToggleMark[] } {
  const marks = toggleMarks(view, role);
  return {
    events: marks.filter((m) => !m.controls.includes("land")),
    land: marks.filter((m) => m.controls.includes("land")),
  };
}

/** The agents that run as a role: its templates in any agent set, then the attested agent types mapped to it. */
function Agents({
  role,
  agents,
  onOpenAgent,
}: {
  readonly role: RoleInfo;
  readonly agents: readonly AgentTemplate[];
  readonly onOpenAgent: (name: string, agentSet: string) => void;
}): React.JSX.Element {
  const templates = templatesActingAs(agents, role.name);
  if (templates.length === 0 && role.agent_types.length === 0) return <span className="muted role-no-agent">{NO_AGENT}</span>;
  return (
    <ul className="role-grants">
      {templates.map((a) => (
        <li key={`${a.workflow}/${a.name}`}>
          <button type="button" className="link-button" title={`${a.workflow}: ${a.describe}`} onClick={() => onOpenAgent(a.name, a.workflow)}>
            {a.name}
          </button>
        </li>
      ))}
      {role.agent_types.map((t) => (
        <li key={`type:${t}`} title="An attested agent type the operator maps to this role">
          <code>{t}</code> <span className="muted">(agent type)</span>
        </li>
      ))}
    </ul>
  );
}

/**
 * The Roles view (the Workflows tab's other face of the Agents pane): one row per role in jkb's
 * order, those no agent acts as included, under the selected strategy as `workflow.roles` answers
 * it — the op classes it may run in jkb, the workflow acts it may fire, whether it lands, the phases
 * where it acts next, and the agents that run as it. Permissions are per role, never per agent: an
 * agent template only names its role. Read-only: a strategy's toggles change with `workflow.define`.
 */
export function RolesTable({
  view,
  agents,
  selected,
  onSelect,
  onOpenAgent,
}: {
  readonly view: RolesView;
  /** Every agent template, in every agent set (`workflow.agents`). */
  readonly agents: readonly AgentTemplate[];
  readonly selected: string | undefined;
  readonly onSelect: (role: string) => void;
  readonly onOpenAgent: (name: string, agentSet: string) => void;
}): React.JSX.Element {
  return (
    <table className="roles-table" aria-label={`Roles under ${view.strategy}`}>
      <thead>
        <tr>
          <th scope="col">Role</th>
          <th scope="col">May run in jkb</th>
          <th scope="col">May fire</th>
          <th scope="col">Lands</th>
          <th scope="col">Acts next in</th>
          <th scope="col">Agents</th>
        </tr>
      </thead>
      <tbody>
        {view.roles.map((r) => {
          const marks = marksOf(view, r);
          return (
            <tr key={r.name} data-role={r.name} data-selected={selected === r.name ? "true" : undefined}>
              <th scope="row">
                <button type="button" className="link-button" aria-pressed={selected === r.name} title={r.describe} onClick={() => onSelect(r.name)}>
                  {r.name}
                </button>
              </th>
              <td>
                {r.ops.length === 0 ? (
                  <span className="muted">nothing</span>
                ) : (
                  <ul className="role-grants">
                    {r.ops.map((o) => (
                      <li key={o.name} title={o.describe}>
                        {o.name}
                      </li>
                    ))}
                  </ul>
                )}
              </td>
              <td>
                <Grants granted={r.events} marks={marks.events} none="no acts: it observes" />
              </td>
              <td>
                <Grants granted={r.lands ? ["land"] : []} marks={marks.land} none="no" />
              </td>
              <td>{r.acts_next_in.length === 0 ? <span className="muted">nowhere</span> : r.acts_next_in.join(", ")}</td>
              <td>
                <Agents role={r} agents={agents} onOpenAgent={onOpenAgent} />
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}

/** A labelled value in the panel's facts list. */
function Fact({ name, children }: { readonly name: string; readonly children: React.ReactNode }): React.JSX.Element {
  return (
    <>
      <dt>{name}</dt>
      <dd>{children}</dd>
    </>
  );
}

const listed = (items: readonly string[], none: string): React.ReactNode =>
  items.length === 0 ? <span className="muted">{none}</span> : items.join(", ");

/**
 * One role's side panel, read-only: what it is for, each op class it may run in plain words, the
 * acts it may fire and whether it lands (each toggle-controlled grant marked), where it acts, the
 * toggles that can change it under the strategy, who grants it, and the agents that run as it.
 */
export function RolePanel({
  view,
  role,
  agents,
  onClose,
  onOpenAgent,
}: {
  readonly view: RolesView;
  readonly role: RoleInfo;
  readonly agents: readonly AgentTemplate[];
  readonly onClose: () => void;
  readonly onOpenAgent: (name: string, agentSet: string) => void;
}): React.JSX.Element {
  const marks = marksOf(view, role);
  const heading = useRef<HTMLHeadingElement>(null);
  const panel = useRef<HTMLElement>(null);
  // As the machine panel does: a role picked from a link elsewhere takes focus to the heading, so
  // Escape reaches the panel.
  useEffect(() => {
    const at = document.activeElement;
    if (at === null || at === document.body || panel.current?.contains(at) === true) heading.current?.focus();
  }, [role.name]);
  const title = `Role ${role.name}`;
  return (
    <section
      ref={panel}
      className="machine-panel role-panel"
      aria-label={title}
      onKeyDown={(e) => {
        if (e.key === "Escape") onClose();
      }}
    >
      <header className="pane-bar">
        <h2 title={title} ref={heading} tabIndex={-1}>
          {title}
        </h2>
        <span className="spacer" />
        <button type="button" className="bar-button" onClick={onClose} aria-label="Close">
          ×
        </button>
      </header>
      <div className="pane-body">
        <p className="role-describe">{role.describe}</p>
        <div className="machine-rows">
          <h3>May run in jkb</h3>
          {role.ops.length === 0 ? (
            <p className="muted">nothing</p>
          ) : (
            <ul className="role-ops">
              {role.ops.map((o) => (
                <li key={o.name}>
                  <code>{o.name}</code> {o.describe}
                </li>
              ))}
            </ul>
          )}
        </div>
        <div className="machine-rows">
          <h3>{`Under ${view.strategy}`}</h3>
        </div>
        <dl className="machine-facts">
          <Fact name="may fire">
            <Grants granted={role.events} marks={marks.events} none="no acts: it observes" />
          </Fact>
          <Fact name="lands">
            <Grants granted={role.lands ? ["land"] : []} marks={marks.land} none="no" />
          </Fact>
          <Fact name="acts next in">{listed(role.acts_next_in, "no phase")}</Fact>
          <Fact name="fires in">{listed(role.fires_in, "no phase")}</Fact>
          <Fact name="toggles">
            {role.toggled_by.length === 0 ? (
              <span className="muted">no toggle can change it</span>
            ) : (
              <ul className="role-grants">
                {toggleMarks(view, role).map((m) => (
                  <li key={m.toggle} data-on={m.on ? "true" : "false"}>
                    <code>{m.toggle}</code> ({m.label}): {m.on ? "on" : "off in this strategy"}
                  </li>
                ))}
              </ul>
            )}
          </Fact>
          <Fact name="may grant">{listed(role.may_grant, "no role")}</Fact>
          <Fact name="granted by">{listed(role.granted_by, "no role")}</Fact>
          <Fact name="agents">
            <Agents role={role} agents={agents} onOpenAgent={onOpenAgent} />
          </Fact>
        </dl>
      </div>
    </section>
  );
}
