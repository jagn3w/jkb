-- Workflow agent templates (design D53.7, docs/code-factory.md): the operator's copies of the
-- prompts the workflow scripts give their agents, beside `workflow_strategies`.
--
-- The PACKAGED templates are not rows here. They are compiled into the binary from
-- `crates/jkb-core/src/workflow/agents.json`, the file a contribution edits, and served beside
-- these rows as read-only ones -- the way the strategy presets are served beside
-- `workflow_strategies`. A packaged template therefore cannot be edited in place by anything, and
-- a newer jkb's packaged text reaches every database without a migration.
--
-- An operator copy of a name OVERRIDES the packaged template of that name: it is what
-- `jkb workflow agent show <name>` answers, and so what a workflow script reads. Versioned and
-- append-only, like `workflow_strategies`: editing appends a version, never changes one. NOT
-- changelogged, for the same reason as that table: a log of definitions makes no claim about the
-- present that an undo could put back, and the newest version is the definition.
CREATE TABLE workflow_agents (
    name        TEXT NOT NULL,
    version     INTEGER NOT NULL,
    -- The workflow script that runs it (`task-swarm`, `code-review`).
    workflow    TEXT NOT NULL,
    -- The D52 role its calls act as.
    role        TEXT NOT NULL CHECK (role IN
                    ('operator', 'coordinator', 'designer', 'implementer', 'reviewer',
                     'systemic_reviewer')),
    -- A shared piece other templates include (a `{{placeholder}}` the script fills with it),
    -- not an agent of its own.
    fragment    INTEGER NOT NULL DEFAULT 0 CHECK (fragment IN (0, 1)),
    describe    TEXT NOT NULL,
    -- The prompt, with `{{placeholders}}` the script fills.
    template    TEXT NOT NULL,
    -- `AgentPermissions` as JSON: isolation, model, what it may write.
    permissions TEXT NOT NULL,
    -- The agents it hands its result to, as a JSON array of names: the graph's edges.
    hands_off_to TEXT NOT NULL,
    -- `<name>@<version>` (packaged or operator) this version was copied from; NULL for an edit.
    based_on    TEXT,
    defined_at  TEXT NOT NULL,
    PRIMARY KEY (name, version)
);
