-- A branch is a record, not a tag value (design B-series, `openspec/changes/jkb-branch-records`).
--
-- Four facts about a *branch* -- where it was cut, which instance of the name that measurement
-- describes, which branch it lands on, and whether jkb itself merged it -- were stored as
-- **tag applications** on the tasks that happened to name the branch. Tag applications are
-- item-keyed, multi-valued, untyped and writable from any route, and every one of those four
-- properties produced its own family of defects across fifteen review passes:
--
--   * item-keyed  -> a per-branch fact had to be encoded into the value (`base=<branch>:<sha>`),
--                    and that encoding leaked to a dozen call sites with their own attribution
--                    rules
--   * multi-valued-> the documented repair (`jkb task tag set base=`) deleted *other branches'*
--                    records, and records otherwise accumulated
--   * untyped     -> `HEAD` was stored verbatim; a 40-hex string that is no commit was accepted
--   * open-write  -> five separate write routes had to be taught the rule one at a time, the
--                    fifth found *after* a store-side reservation was added for the other four
--
-- `branch_records` is keyed `(repo, branch)`, so the encoding, the attribution rules and the
-- question "which branch does this value belong to?" all cease to exist. Prefer an invariant the
-- schema enforces over one every caller must uphold (D40).
--
-- What this deliberately does NOT store is branch **existence** -- git owns that (D38.1), and
-- every reader still resolves through `gitrepo::branch_ref(s)` first. A row for a branch that no
-- longer resolves is inert.

CREATE TABLE branch_records (
    -- `INTEGER PRIMARY KEY AUTOINCREMENT`, not a `WITHOUT ROWID` composite key: `changelog::append`
    -- records `entity_id` = rowid and `undo` inverts an insert with
    -- `DELETE FROM {table} WHERE rowid = ?`.
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    -- `gitrepo::key(dir)` -- the same value the `repo=` facet carries. In the key, so "a namesake
    -- branch in a sibling checkout" stops being something each caller has to remember to ask.
    repo         TEXT NOT NULL,
    -- The short branch name, i.e. the key `gitrepo::branch_refs` returns. `origin/x` and
    -- `refs/heads/x` are not admissible values.
    branch       TEXT NOT NULL,
    -- The commit this branch forked from, measured. NULL -- *not recorded* -- is a first-class
    -- state that both readers treat as "do not act": a missing cut point holds the task and says
    -- why, while a wrong one closes it silently and permanently.
    cut_point    TEXT,
    -- The instance anchor: the branch's creation reflog entry (`old = zeros`), as its `new`
    -- revision and the entry's own timestamp. A branch name outlives the branch that held it, and
    -- nothing in git's object/ref model distinguishes a recycled name -- but the checkout-local ref
    -- journal does, because deleting a branch destroys its log and the recreated branch's log
    -- provably starts fresh. NULL means "cannot judge instance identity", which degrades every
    -- consumer to the untouched-tip predicate rather than to a judgement.
    anchor_sha   TEXT,
    anchor_ts    INTEGER,
    -- The branch this one lands on (formerly the `onto=` facet). NULL on an existing row means
    -- "lands on trunk / is on no batch"; *no row* means unknown. The facet could not tell those
    -- apart.
    land_target  TEXT,
    -- Set only where jkb itself performed the merge, and only together. `landed_head` is the
    -- branch's own tip at that moment: without it the event re-creates the same name-staleness one
    -- column over, and a namesake recreated after a landing would present its predecessor's.
    landed_at    TEXT,
    landed_onto  TEXT,
    landed_head  TEXT,
    created_at   TEXT NOT NULL,
    UNIQUE (repo, branch),
    -- A half-anchor judges nothing, so the two halves are written together or not at all.
    CHECK ((anchor_sha IS NULL) = (anchor_ts IS NULL)),
    -- Only a full object id may be stored. A symbolic revision (`HEAD`, `main`, `@`) is the
    -- dangerous value precisely because it resolves in *every* clone, to something different in
    -- each: stored and re-resolved later it names an unrelated commit, `is_merged` skips its
    -- freshly-cut guard, and a task with no work on it closes. **This CHECK is the only thing
    -- that enforces it** -- no Rust guard runs before the write, so the refusal a caller sees is
    -- a constraint violation, and that is the whole enforcement rather than a backstop behind
    -- one. `base::is_object_id` asks the same question on the *reader's* side
    -- (`repo::base_is_usable`), for values recorded before this table existed.
    --
    -- Lowercase is required because `branch::record_cut_point` lowercases what it is handed, so a
    -- value that reaches the store has already been normalized.
    CHECK (cut_point IS NULL OR (
        length(cut_point) IN (40, 64)
        AND lower(cut_point) = cut_point
        AND cut_point NOT GLOB '*[^0-9a-f]*'
    )),
    -- "Landed, target unknown" is not a state anything can act on, and a landing that does not say
    -- which tip landed cannot be attributed to the branch that carries the name later.
    CHECK ((landed_at IS NULL) = (landed_onto IS NULL)
       AND (landed_at IS NULL) = (landed_head IS NULL))
);

CREATE INDEX idx_branch_records_repo ON branch_records (repo);

-- Delete the tag applications this table replaces, and **back-fill nothing** (B7).
--
-- Back-filling would import exactly the values five review passes proved unreliable, into a store
-- whose whole purpose is that its contents can be trusted. A wrong cut point closes a task falsely
-- -- silent and permanent. No cut point holds the task and reports why -- loud and repairable. The
-- project has already chosen that direction once, at `base::Missing`.
--
-- Leaving them inert is not an option either: the reserved-facet apparatus that made `base=`
-- invisible to file sync goes with this change, so a surviving `base=` on a file-backed task would
-- begin exporting `#base=...` onto synced task lines. The hazard was never the value; it was that
-- one side of the sync stripped it and another did not. Deleting the rows and the reservation
-- together is coherent; doing either alone is not.
--
-- Migrations run outside the changelog, as `V001`'s `_sys` seed and `V008`'s type back-fill do.
DELETE FROM tag_applications WHERE facet IN ('base', 'onto');
-- The declarations go too, or `jkb tag ls` keeps advertising two facets nothing writes or reads.
DELETE FROM tag_defs WHERE facet IN ('base', 'onto');
