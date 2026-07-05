---
description: Walk open, un-triaged jkb tasks under a path and settle each task's design WITH the user, then tag it design=approved so the swarm may implement it.
argument-hint: "<jkb-path>  (a namespace/directory; omit to triage tasks/inbox)"
---

You are running a **design pass**. The swarm (`/task-swarm`) will only implement tasks
tagged `design=approved` (the design gate, D28) — because its implementers run headless and
**cannot ask the user** about undecided architecture. Your job here, in this interactive
session, is to be the place where the user weighs in: walk the open tasks that have **not**
been triaged yet, settle each one's design **with the user**, record the decision where an
implementer will find it, and tag the task `design=approved`.

Path given: `$ARGUMENTS` (if empty, default `PATH=tasks/inbox`).

Use the `jkb` binary on PATH (fall back to `./target/debug/jkb`). Normalize `$ARGUMENTS`
into `PATH` as in `/next-task` (trim, strip leading/trailing `/` and a trailing `/**`).

## 1. Find the un-triaged tasks

```sh
# open, non-terminal tasks under PATH that are NOT yet design-approved:
jkb query --global --json 'kind:task status:open ns:<PATH>/**' --limit 1000
jkb query --global --json 'kind:task status:open ns:<PATH>/** tag:design=approved' --limit 1000
```

The un-triaged set is the first list minus the second (tasks lacking `design=approved`).
Also include `status:in_progress` if the user wants (default: just `open`). Order them by
priority (`!p1` first), then due. If the un-triaged set is empty, report "All tasks under
`<PATH>` are design-approved." and stop.

Print a short roster first: N un-triaged tasks, their titles and priorities, so the user
sees the scope of the pass.

## 2. Triage each task — the core loop

Go one task at a time, highest priority first. For each:

**a. Read it fully.** `jkb task show <uid> --json`. For a file-backed task
(`uid` starts with `file://<abs-path>#<frag>`), also read the source line ending in
`^<frag>` and its indented notes. Open any source files/paths the task names.

**b. Judge the design surface — is this trivial?** A task is *trivial* (no design needed)
when the approach is obvious and there is genuinely one sensible way to do it: a typo/doc
fix, a mechanical rename, a dependency bump, a localized bug fix with one clear cause. A
task has *real design surface* when there's a decision the user should own: a new
API/trait/enum shape, a schema/migration change, a cross-cutting refactor, anything with
several defensible approaches or that sets a convention. **When unsure, treat it as
non-trivial** — the cost of asking is low, the cost of a wrong headless guess is high.

**c. Trivial → fast-track.** Briefly tell the user "`<title>` looks trivial: `<one-line
approach>` — approving." Batch several trivial ones into a single confirmation when they're
clearly minor. Then tag and move on (step 3). Do **not** write a design doc for these.

**d. Real design surface → settle it WITH the user.** This is the whole point of the
command. Present the task and the genuine decision(s) it raises, then use the
**AskUserQuestion** tool for the real choices (approach A vs B, trait vs enum, where a
seam lives, migration strategy). Give a recommendation as the first option. Discuss
follow-ups in plain text if the user pushes back. Keep going until the approach is settled.
Then **record it** (step 3d) before tagging.

Never invent the decision yourself and tag it approved — that defeats the gate. If the user
is unavailable to decide, leave the task un-tagged and move on.

## 3. Record + approve each task

**a. (non-trivial) Write the decision to a design doc.** Append a decision block to the
running design log `openspec/design-notes.md` (create it if absent), or — if the task
belongs to an existing openspec change — to that change's `design.md`. Number decisions
`D<N>` continuing the repo's existing sequence (the last one is D28 for this gate; check
with `grep -rho 'D[0-9]\+' openspec | sort -t D -k2 -n | tail -1`). Format so an implementer
can grep it by uid:

```
## D<N>: <short title>
Governs: <task-uid>[, <task-uid> ...]
Decision: <the decided approach, concretely — the shape/name/strategy the user chose>
Rationale: <why this over the alternatives considered>
```

The `Governs: <uid>` line is load-bearing: the swarm implementer greps design docs for the
task uid to find its decision. List every task uid the decision covers.

**b. (non-trivial) Stamp the design into the task body too.** So the implementer sees the
decision on the task itself, not only via a doc grep:

- **Managed task** (`task:` uid): append the design note to its body via the CLI:

  ```sh
  jkb task edit <uid> --append "Design: <one-paragraph decided approach>. See D<N> in openspec/design-notes.md."
  ```

  (Use `--stdin --append` for a multi-line note: `printf '...' | jkb task edit <uid> --stdin --append`.)

- **File-backed task** (`file://` uid): add an indented `Design:` note directly under the
  task's `^<frag>` line in its source file (continuation prose, which the tasks serializer
  preserves), then `jkb sync` to reconcile:

  ```
  - [ ] <title> ^<frag>
    Design: <one-paragraph decided approach>. See D<N> in openspec/design-notes.md.
  ```

  (You could also `jkb task edit` a file-backed task, but editing the file keeps disk and KB
  in step; `task edit` warns you to `jkb sync` afterward.)

**c. Tag it approved (every triaged task, trivial or not).** This is the gate flip:

```sh
jkb task tag add <uid> design=approved
```

The tag is applied to the item, independent of any file, so it works for both managed and
file-backed tasks and survives sync. Only tag AFTER the decision is recorded (or after
confirming trivial).

## 4. Report

Summarize the pass: how many tasks you triaged, how many were **approved trivially** vs
**designed** (with the `D<N>` numbers you added), and any you **left un-tagged** (and why —
e.g. the user deferred the decision). Remind the user that only the now-approved tasks are
visible to `/task-swarm <PATH>`; anything still un-tagged will be held back until a future
design pass.
