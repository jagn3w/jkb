-- Roles, grants and task workflows (design D52, openspec/changes/jkb-rbac-workflows/).
--
-- Every table here is deliberately NOT changelogged, following `claude_sessions` and
-- `task_transitions`:
--   * `role_grants`, `agent_role_map`, `agent_bindings` are credentials and authorization state.
--     `jkb undo` reviving a revoked grant would re-arm a credential; undoing a mint would strand a
--     worker with no record of why. Revocation is its own, recursive, operation.
--   * `workflow_transitions` and `workflow_strategies` are append-only records. A log makes no claim
--     about the present, so there is nothing for an undo to put back.

-- A credential: only its blake3 hash is kept. `parent_id` is the grant that minted it; revoking a
-- grant revokes its subtree. `item_id` scopes it to one task (NULL: unscoped).
CREATE TABLE role_grants (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    token_hash  TEXT NOT NULL UNIQUE,
    role        TEXT NOT NULL CHECK (role IN
                    ('operator', 'coordinator', 'designer', 'implementer', 'reviewer',
                     'systemic_reviewer')),
    agent       TEXT NOT NULL CHECK (length(agent) > 0),
    item_id     INTEGER REFERENCES items(id) ON DELETE CASCADE,
    parent_id   INTEGER REFERENCES role_grants(id),
    granted_at  TEXT NOT NULL,
    txn_id      INTEGER,
    revoked_at  TEXT
);
CREATE INDEX idx_role_grants_item ON role_grants (item_id);
CREATE INDEX idx_role_grants_parent ON role_grants (parent_id);

-- The operator's map from a Claude Code subagent type (its definition's `name`) to the role a
-- harness-attested call from such a subagent holds (D52.9). An unmapped type holds no role.
CREATE TABLE agent_role_map (
    agent_type  TEXT PRIMARY KEY,
    role        TEXT NOT NULL CHECK (role IN
                    ('operator', 'coordinator', 'designer', 'implementer', 'reviewer',
                     'systemic_reviewer'))
);

-- Which task an attested subagent is working on: the first bind wins, so a worker cannot hop.
CREATE TABLE agent_bindings (
    session     TEXT NOT NULL,
    agent_id    TEXT NOT NULL,
    item_id     INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    bound_at    TEXT NOT NULL,
    PRIMARY KEY (session, agent_id)
);

-- A task's workflow history (D52.5). The current phase is the latest row's `to_phase`; the
-- current strategy is the latest row carrying a `spec` (the resolved StrategySpec as JSON), so a
-- redefined strategy never changes a task mid-flight.
CREATE TABLE workflow_transitions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    txn_id      TEXT NOT NULL,
    item_id     INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    at          TEXT NOT NULL,
    event       TEXT NOT NULL,
    from_phase  TEXT,
    to_phase    TEXT NOT NULL,
    -- Who acted: the role, and the principal (`operator`, `grant:<id>`, `<agent_type>@<agent_id>`).
    role        TEXT NOT NULL,
    actor       TEXT NOT NULL,
    -- The filing mark of the newest review round when this row was written: what makes "a round
    -- was filed since the task entered review" answerable.
    round_mark  INTEGER,
    reason      TEXT,
    spec        TEXT,
    evidence    TEXT
);
CREATE INDEX idx_workflow_transitions_item ON workflow_transitions (item_id, id);

-- Operator-defined strategies, versioned: redefining appends a version, never edits one.
CREATE TABLE workflow_strategies (
    name        TEXT NOT NULL,
    version     INTEGER NOT NULL,
    spec        TEXT NOT NULL,
    defined_at  TEXT NOT NULL,
    PRIMARY KEY (name, version)
);
