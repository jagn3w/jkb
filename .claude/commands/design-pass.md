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

**a. (non-trivial) Record the decision.** Two homes, by size and scope:

- **Small & standalone → the task itself.** If the design is short and uncomplicated and the
  task doesn't belong to a larger group of related work, just stamp the decision inline on the
  task (step 3b) and **skip the change folder** — the inline `Design:` note is the record.
- **Substantial or multi-task → an openspec change folder.** Otherwise design lives in
  `openspec/changes/<change-name>/`, **one folder per group of related tasks** (mirroring
  `jkb-v1-foundation`, `jkb-v2-file-sync`, `jkb-fleet-hardening`, `jkb-task-homing`). Do **not**
  use a running `design-notes.md` log.
  - If the task belongs to an **existing** change, add the decision to that change's `design.md`.
  - Otherwise **create a new change folder** with the standard scaffolding: `.openspec.yaml`
    (`schema: spec-driven` + `created:`), `proposal.md` (Why / What Changes / Capabilities /
    Impact), `design.md` (the decisions), `tasks.md` (implementation checklist with `^anchor`
    ids), and `specs/<capability>/spec.md` (ADDED/MODIFIED Requirements, each with a WHEN/THEN
    scenario). Group unrelated tasks into **separate** folders. Don't `jkb sync` a new change's
    `tasks.md` if that would duplicate the existing origin task(s) — openspec isn't a live
    watched mount, so sync is explicit.

Each change **owns one `D<N>` number** with sub-decisions `D<N>.1`, `D<N>.2` (like D26.x,
D27.x). Continue the repo's global sequence — find the current max with
`grep -rho 'D[0-9]\+' openspec CLAUDE.md | sort -t D -k2 -n | tail -1`. Format each block so an
implementer can grep it by uid:

```
## D<N>.<M>: <short title>
Governs: <task-uid>[, <task-uid> ...]
Decision: <the decided approach, concretely — the shape/name/strategy the user chose>
Rationale: <why this over the alternatives considered>
```

The `Governs: <uid>` line is load-bearing: the swarm implementer greps design docs for the
task uid to find its decision. List every task uid the decision covers. (For the small &
standalone case, the same `Governs:`/`Decision:` content goes in the inline note instead.)

**b. (non-trivial) Stamp the design into the task body too.** So the implementer sees the
decision on the task itself, not only via a doc grep:

- **Managed task** (`task:` uid): append the design note to its body via the CLI:

  ```sh
  jkb task edit <uid> --append "Design: <one-paragraph decided approach>. See D<N>.<M> in openspec/changes/<name>/design.md."   # omit the 'See …' clause for the small & standalone case
  ```

  (Use `--stdin --append` for a multi-line note: `printf '...' | jkb task edit <uid> --stdin --append`.)

- **File-backed task** (`file://` uid): add an indented `Design:` note directly under the
  task's `^<frag>` line in its source file (continuation prose, which the tasks serializer
  preserves), then `jkb sync` to reconcile:

  ```
  - [ ] <title> ^<frag>
    Design: <one-paragraph decided approach>. See D<N>.<M> in openspec/changes/<name>/design.md.
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
