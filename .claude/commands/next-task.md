---
description: Fetch the highest-priority ready jkb task at a given path and work on it end-to-end
argument-hint: "<jkb-path>  (a namespace/directory, or the path to a single task)"
---

You are working the **next task** in a jkb knowledge base. Take the jkb path below,
find the single highest-priority *ready* task under it, do the work it describes, then
mark it done. Do the steps in order.

Path given: `$ARGUMENTS`

Use the `jkb` binary on your PATH (fall back to `./target/debug/jkb` if `jkb` isn't
found). All queries below pass `--global` so the explicit `ns:` scope is honoured
regardless of the current directory.

## 1. Require a path

If `$ARGUMENTS` is empty, stop and ask the user which jkb path to work (e.g.
`tasks/inbox`, `codereviews/**`, or a specific section). Do not guess.

Otherwise normalize it into `PATH`: trim surrounding whitespace and any leading/trailing
`/`, and strip a trailing `/**` if the user already added one.

## 2. Fetch the next task

```sh
jkb task next --global --json 'ns:<PATH>/**' --limit 1
```

`task next` already returns only **ready** tasks — unblocked (no unfinished `depends_on`),
not `done`/`cancelled` — ordered by priority then due date. So the first (only) element is
the highest-priority actionable task. `!p1` beats `!p3`; unprioritized tasks sort last.

- The `ns:<PATH>/**` subtree scope covers both cases the user may pass: a
  **directory/namespace** (many tasks → you get the top one) and a **single task's
  namespace path** (you get that task).
- If the array is empty, report `No ready tasks under <PATH>.` and stop (mention that
  blocked or completed tasks are excluded, and they can broaden the path or check
  `jkb query --global 'kind:task ns:<PATH>/**'`).

Read the returned JSON object's fields: `id`, `uid`, `namespace`, `priority`, `status`,
`snippet`.

## 3. Gather full context

The `snippet` is truncated — get the whole task before acting:

- **File-backed task** (`uid` starts with `file://`): the uid is `file://<abs-path>#<local_id>`.
  Read `<abs-path>` and locate the task line ending in `^<local_id>`; read it plus the
  indented continuation prose beneath it (notes, rationale, failure scenarios) and its
  section header for context.
- **Managed task** (`uid` starts with `task:` or similar): the `snippet` is essentially the
  whole task; if you need siblings/context, run
  `jkb query --global --json "kind:task ns:<namespace>"`.
- If the task text references source locations (`path:line`, a crate, a function), open
  those files. For code-review findings, the task names the exact file:line to fix.

## 4. Do the work

Carry out what the task describes, fully and following repo conventions in `CLAUDE.md`
(no `unsafe` beyond the one allowed site, no `unwrap`/`expect` outside tests, parameterized
SQL, changelog on mutations, `thiserror` in libs). Make the actual code/content change —
don't just describe it. When the change touches Rust, verify with the repo scripts before
calling it done:

```sh
./scripts/fix.sh      # fmt + check
./scripts/test.sh     # or a targeted `cargo test -p <crate>`
./scripts/clippy.sh   # clippy -D warnings
```

If the task is ambiguous or turns out to be wrong/obsolete, stop and report what you found
rather than forcing a change.

## 5. Mark it done

Only after the work is complete and verified:

- **File-backed task:** flip its checkbox from `- [ ]` to `- [x]` on the line bearing
  `^<local_id>` in the source file (`<abs-path>` from the uid), then reconcile so the KB
  status follows:

  ```sh
  jkb sync            # reconciles every mount; or `jkb sync <mount-root-ns>` for just one
  ```

- **Managed task (no backing file):** the `jkb` CLI has no status-setter (only `task add`
  / `task next`). Leave the status as-is and tell the user it must be closed via the MCP
  `task_update` tool or by editing its source; do not fabricate a status change.

## 6. Report

Summarize in a few lines: which task you worked (`id`, priority, one-line title), what you
changed (files touched, tests/clippy result), and how you marked it done (checkbox+sync, or
that it couldn't be closed via CLI). If you intentionally left it open, say why.
