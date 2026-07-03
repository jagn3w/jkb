export const meta = {
  name: 'task-swarm',
  description:
    'COORDINATOR fans out IMPLEMENTER worktrees per unblocked jkb task; a single serialized RESOLVER integrates each into one branch, re-dispatching (IMPLEMENTER/DESIGNER) on conflict, looping as completed tasks unblock dependents until the frontier drains',
  whenToUse:
    'Launched by the /task-swarm command after it scouts the jkb task set and creates the integration branch + worktree.',
  phases: [
    { title: 'Schedule' },
    { title: 'Implement' },
    { title: 'Resolve' },
  ],
}

// ---------------------------------------------------------------------------
// Config — supplied by the /task-swarm command via `args`. All git/branch setup
// (integration branch + its worktree) is done by the command BEFORE launch; this
// script only orchestrates agents.
// ---------------------------------------------------------------------------
const cfg = args || {}
const JKB = cfg.jkb || 'jkb' // how to invoke the jkb binary
const DB = cfg.db ? ` --db ${cfg.db}` : '' // optional --db flag
const SCOPE = cfg.scope || '' // a jkb DSL scope, e.g. "ns:codereviews/**"
const TASKS = Array.isArray(cfg.tasks) ? cfg.tasks : null // or explicit task uids
const GLOBAL = cfg.global === false ? '' : ' --global' // ignore ambient cwd scoping
const REPO = cfg.repo || '.' // the main working copy (where task files live)
const INTEGRATION = cfg.integration // integration branch name (required)
const INTEGRATION_WT = cfg.integrationWorktree // path to the integration worktree (required)
const RETRY_CAP = cfg.retryCap || 3 // per-task re-dispatch attempts before giving up
const ROUND_CAP = cfg.roundCap || 25 // safety bound on scheduler rounds

if (!INTEGRATION || !INTEGRATION_WT) {
  throw new Error(
    'task-swarm requires args.integration and args.integrationWorktree — the /task-swarm command sets these up before launching.',
  )
}

const scopeExpr = TASKS ? `tasks [${TASKS.join(', ')}]` : SCOPE || '(ambient scope)'

// ---------------------------------------------------------------------------
// Schemas — force structured returns so the coordinator never parses prose.
// ---------------------------------------------------------------------------
const FRONTIER = {
  type: 'object',
  properties: {
    ready: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          uid: { type: 'string' },
          id: { type: 'integer' },
          title: { type: 'string' },
          priority: { type: ['integer', 'null'] },
          namespace: { type: 'string' },
          source_file: { type: ['string', 'null'] }, // abs path if file-backed, else null
        },
        required: ['uid', 'title'],
      },
    },
    remaining: { type: 'integer' }, // non-terminal tasks in scope (ready OR blocked)
  },
  required: ['ready', 'remaining'],
}

const IMPL = {
  type: 'object',
  properties: {
    uid: { type: 'string' },
    branch: { type: ['string', 'null'] }, // the committed task branch, or null on failure
    outcome: { type: 'string', enum: ['ready', 'needs_design', 'failed'] },
    summary: { type: 'string' },
  },
  required: ['uid', 'outcome', 'summary'],
}

const RESOLVE = {
  type: 'object',
  properties: {
    uid: { type: 'string' },
    merged: { type: 'boolean' },
    flag: { type: ['string', 'null'], enum: ['needs_impl', 'needs_design', null] },
    notes: { type: 'string' },
  },
  required: ['uid', 'merged', 'notes'],
}

// ---------------------------------------------------------------------------
// Agent prompts
// ---------------------------------------------------------------------------
function schedulerPrompt(round) {
  const readyCmd = TASKS
    ? `list only the still-ready ones among these uids: ${TASKS.join(', ')}`
    : `run \`${JKB}${DB} task next${GLOBAL} --json '${SCOPE}' --limit 100\``
  return `You are the SCHEDULER for a task swarm (round ${round}). Determine the current READY frontier of jkb tasks and how many tasks remain.

1. Ready frontier: ${readyCmd}. \`task next\` returns only unblocked, non-terminal tasks ordered by priority then due — these are safe to start now.
2. Remaining count: run \`${JKB}${DB} query${GLOBAL} --json 'kind:task ${TASKS ? '' : SCOPE}' --limit 1000\` and count the items whose "status" is NOT "done" and NOT "cancelled". (There is no is:open filter — count from the returned status field.) If scoping by explicit uids, count those uids whose status is non-terminal instead.
3. For each ready task capture its "uid", "id", "title" (the snippet is fine), "priority", "namespace", and — if its uid starts with file:// — the absolute file path before the '#', as "source_file" (else null).

Return the structured frontier. Do not modify anything.`
}

function implementerPrompt(task, designHint) {
  return `You are an IMPLEMENTER in a task swarm, working in an isolated git worktree that shares the repo's object store with the main copy at ${REPO}.

TASK (uid ${task.uid}): ${task.title}
${task.source_file ? `Full detail lives in ${task.source_file} — read the line ending in \`^${task.uid.split('#').pop()}\` and its indented notes for the real requirement.` : ''}
${designHint ? `A prior pass was flagged for redesign. Take this into account:\n${designHint}` : ''}

Steps:
1. Base your work on the latest integrated state: \`git switch -c swarm-task/${task.id || 'x'}-$(printf %s "${task.uid}" | tr -c 'a-zA-Z0-9' - | cut -c1-32) ${INTEGRATION}\`. (Create a short, unique branch name off ${INTEGRATION}.)
2. Implement the task fully, following the repo's CLAUDE.md conventions. Make the real code/content change.
3. Verify with the repo's own scripts/tests (e.g. ./scripts/fix.sh, ./scripts/test.sh, ./scripts/clippy.sh, or the project's equivalent). Do not weaken tests to pass.
4. Commit everything to your branch: \`git add -A && git commit -m "swarm: ${task.title}"\`. Committing is REQUIRED — your worktree may be cleaned up, but the branch/commit persist for the resolver.

Return outcome:
- "ready" with your branch name if it's implemented, committed, and tests pass.
- "needs_design" (with a clear summary of the design problem) if the task can't be done as specified without an approach change.
- "failed" (with the reason) if you couldn't make progress. Only return "ready" if you actually committed a branch.`
}

function resolverPrompt(built, integrationWt) {
  return `You are the RESOLVER (the swarm runs at most one of you at a time). Integrate one finished task branch into ${INTEGRATION}, using the dedicated integration worktree at ${integrationWt} — do all git work THERE, never in the main copy.

TASK uid ${built.uid}, branch: ${built.branch}
Implementer summary: ${built.summary}

Steps (in ${integrationWt}):
1. \`git merge --no-ff ${built.branch}\`.
2. If there are conflicts, resolve them by understanding BOTH sides (the branch's intent and what's already integrated), then \`git add -A && git commit\`. If a conflict reveals the task's approach is fundamentally incompatible with already-merged work, \`git merge --abort\` and flag "needs_design".
3. Run the repo's build + tests. If they fail because of a small integration mismatch you can fix, fix and commit. If they fail because the implementation is broken, \`git reset --hard\` back to the pre-merge tip and flag "needs_impl".
4. Leave ${INTEGRATION} pointing at a clean, tested, merged state on success.

Return merged=true only if the branch is merged AND the integration branch builds/tests clean. Otherwise merged=false with flag "needs_impl" (re-implement) or "needs_design" (rethink approach) and notes explaining what the next pass must address. Delete the task branch's worktree if you created one; leave the branch ref.`
}

function completePrompt(task) {
  return `A swarm task is fully merged into the integration branch. Mark it done in jkb so its dependents unblock. Work in the MAIN copy at ${REPO}, not the integration worktree.

TASK uid ${task.uid}${task.source_file ? `, source file ${task.source_file}` : ''}.

- If the uid starts with file:// (source_file is set): flip its checkbox from "- [ ]" to "- [x]" on the line ending in \`^${task.uid.split('#').pop()}\` in ${task.source_file}, then run \`${JKB}${DB} sync\` so the KB status becomes done.
- If it's a managed task (no source file): the jkb CLI has no status setter; note that it must be closed via the MCP task_update tool. Do not fabricate a change.

Return a one-line confirmation of what you did.`
}

// ---------------------------------------------------------------------------
// Coordinator loop
// ---------------------------------------------------------------------------
log(`swarm start · scope ${scopeExpr} · integration ${INTEGRATION} (${INTEGRATION_WT})`)

const done = new Set()
const retries = new Map() // uid -> attempts spent
const designHint = new Map() // uid -> notes to feed the next pass
const gaveUp = []
const merged = []
let round = 0

const attempt = (uid) => retries.get(uid) || 0
const bump = (uid) => retries.set(uid, attempt(uid) + 1)

while (round < ROUND_CAP) {
  round++
  if (budget.total && budget.remaining() < 60_000) {
    log(`stopping: ~${Math.round(budget.remaining() / 1000)}k tokens left, below the per-round reserve`)
    break
  }

  // 1. SCHEDULE — ask jkb for the current ready frontier and remaining count.
  phase('Schedule')
  const frontier = await agent(schedulerPrompt(round), {
    label: `schedule#${round}`,
    phase: 'Schedule',
    schema: FRONTIER,
  })
  if (!frontier) {
    log('scheduler returned nothing — stopping')
    break
  }

  const ready = frontier.ready.filter((t) => !done.has(t.uid) && attempt(t.uid) < RETRY_CAP)
  const overCap = frontier.ready.filter((t) => !done.has(t.uid) && attempt(t.uid) >= RETRY_CAP)
  overCap.forEach((t) => {
    if (!gaveUp.includes(t.uid)) gaveUp.push(t.uid)
  })

  log(`round ${round}: ${ready.length} ready · ${frontier.remaining} remaining · ${done.size} merged · ${gaveUp.length} gave up`)

  if (ready.length === 0) {
    if (frontier.remaining === 0) {
      log('frontier drained — all tasks complete')
    } else {
      log(`no ready tasks but ${frontier.remaining} remain (blocked or retry-capped) — stopping`)
    }
    break
  }

  // 2. IMPLEMENT — one isolated worktree per ready task, concurrently (runtime caps
  //    the actual parallelism). A design-flagged task gets its hint fed back in.
  phase('Implement')
  const built = (
    await parallel(
      ready.map((t) => () =>
        agent(implementerPrompt(t, designHint.get(t.uid)), {
          label: `impl:${t.uid}`,
          phase: 'Implement',
          schema: IMPL,
          isolation: 'worktree',
        }).then((r) => (r ? { ...r, task: t } : null)),
      ),
    )
  ).filter(Boolean)

  // 3. RESOLVE — strictly serial: at most ONE resolver at a time integrates into the
  //    branch. On success, mark the task done in jkb so dependents unblock next round.
  phase('Resolve')
  for (const b of built) {
    if (b.outcome === 'failed') {
      bump(b.uid)
      log(`impl failed ${b.uid} (attempt ${attempt(b.uid)}/${RETRY_CAP}): ${b.summary}`)
      continue
    }
    if (b.outcome === 'needs_design' || !b.branch) {
      bump(b.uid)
      designHint.set(b.uid, b.summary)
      log(`impl flagged design ${b.uid} (attempt ${attempt(b.uid)}/${RETRY_CAP})`)
      continue
    }

    const r = await agent(resolverPrompt(b, INTEGRATION_WT), {
      label: `resolve:${b.uid}`,
      phase: 'Resolve',
      schema: RESOLVE,
    })
    if (r && r.merged) {
      done.add(b.uid)
      merged.push(b.uid)
      designHint.delete(b.uid)
      await agent(completePrompt(b.task), { label: `done:${b.uid}`, phase: 'Resolve' })
      log(`merged ${b.uid}`)
    } else {
      bump(b.uid)
      const flag = (r && r.flag) || 'needs_impl'
      if (flag === 'needs_design') designHint.set(b.uid, (r && r.notes) || b.summary)
      log(`resolver flagged ${b.uid} → ${flag} (attempt ${attempt(b.uid)}/${RETRY_CAP})`)
    }
  }
}

return {
  scope: scopeExpr,
  integration_branch: INTEGRATION,
  rounds: round,
  merged,
  gave_up: gaveUp,
}
