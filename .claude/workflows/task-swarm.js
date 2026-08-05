export const meta = {
  name: 'task-swarm',
  description:
    'SCHEDULER groups overlapping ready jkb tasks into work-groups; one IMPLEMENTER builds each group on a clean branch; a fresh REVIEWER checks the whole group; a deterministic merge queue (no agent) rebase/fast-forwards approved branches into one feature branch and marks the group done. Pipelined (no per-round barrier), claim-guarded, looping as dependents unblock.',
  whenToUse:
    'Launched by the /task-swarm command after it scouts the jkb task set and creates the integration branch + worktree.',
  phases: [
    { title: 'Schedule' },
    { title: 'Claim' },
    { title: 'Implement' },
    { title: 'Review' },
    { title: 'Merge' },
  ],
}

// ---------------------------------------------------------------------------
// Config — supplied by the /task-swarm command via `args`. All git/branch setup
// (integration branch + its worktree) is done by the command BEFORE launch; this
// script only orchestrates agents. Roles (design D27):
//   SCHEDULER   — clusters overlapping ready tasks into work-groups (≤~4 each).
//   IMPLEMENTER — builds ALL of one group's tasks on one clean branch.
//   REVIEWER    — checks the branch against the whole group; approve / request_changes.
//   merge queue — deterministic (scripts/merge-queue.sh, no agent): rebase+ff, gate, done.
// The "coordinator" is THIS deterministic workflow JS — it owns claims, routes feedback,
// and drives the serial merge queue; it is NOT an agent.
// ---------------------------------------------------------------------------
const cfg = (typeof args === 'string' ? JSON.parse(args) : args) || {}
const JKB = cfg.jkb || 'jkb' // how to invoke the jkb binary
const DB = cfg.db ? ` --db ${cfg.db}` : '' // optional --db flag
const SCOPE = cfg.scope || '' // a jkb DSL scope, e.g. "ns:codereviews/**"
const TASKS = Array.isArray(cfg.tasks) ? cfg.tasks : null // or explicit task uids
// Design gate (D28): in scope mode the swarm only touches tasks whose design has been
// approved (tag `design=approved`, set by /design-pass). `cfg.designGate:false` disables
// it. Explicit-uid mode (TASKS) is a deliberate hand-pick and always bypasses the gate.
const DESIGN_GATE = cfg.designGate === false || TASKS ? '' : 'tag:design=approved'
const GLOBAL = cfg.global === false ? '' : ' --global' // ignore ambient cwd scoping
const REPO = cfg.repo || '.' // the main working copy (where task files live)
const INTEGRATION = cfg.integration // integration/feature branch name (required)
const INTEGRATION_WT = cfg.integrationWorktree // path to the integration worktree (required)
const RETRY_CAP = cfg.retryCap || 3 // per-GROUP feedback attempts (review + eject share it)
const ROUND_CAP = cfg.roundCap || 40 // safety bound on scheduler passes
const GROUP_CAP = cfg.groupCap || 4 // hard cap on tasks per work-group (D27.8)
// The run's claim owner (design D27.1): a liveness-checkable id for THIS run. Ideally
// `host:pid` of a process alive for the run; the /task-swarm command supplies it. Any
// owner the reclaim scan can't prove alive is treated dead by a *later* run (crash net) —
// but this run always passes OWNER to `task reclaim --keep`, so it never reclaims its own.
const OWNER = cfg.owner || `swarm:${INTEGRATION}`

if (!INTEGRATION || !INTEGRATION_WT) {
  throw new Error(
    'task-swarm requires args.integration and args.integrationWorktree — the /task-swarm command sets these up before launching.',
  )
}

// The scope terms handed to `task next`/`query`, with the design gate ANDed in (scope
// mode only). Both empty → an empty DSL string (whole ambient/global frontier, gated).
const GATED_SCOPE = [SCOPE, DESIGN_GATE].filter(Boolean).join(' ')
const scopeExpr = TASKS
  ? `tasks [${TASKS.join(', ')}]`
  : GATED_SCOPE || '(ambient scope)'
const shortUid = (uid) => (uid.includes('#') ? uid.split('#').pop() : uid.split('/').pop())
// A task's file-backed requirement id (the trailing `^id`), or null for a managed task.
const fragOf = (uid) => (uid.startsWith('file://') ? uid.split('#').pop() : null)

// ---------------------------------------------------------------------------
// Schemas — force structured returns so the coordinator never parses prose.
// ---------------------------------------------------------------------------
const TASK_ITEM = {
  type: 'object',
  properties: {
    uid: { type: 'string' },
    id: { type: ['integer', 'null'] },
    title: { type: 'string' },
    priority: { type: ['integer', 'null'] },
    namespace: { type: ['string', 'null'] },
    source_file: { type: ['string', 'null'] }, // abs path if file-backed, else null
  },
  required: ['uid', 'title'],
}

// SCHEDULER returns work-GROUPS (design D27.8), not a flat frontier.
const SCHEDULE = {
  type: 'object',
  properties: {
    groups: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          tasks: { type: 'array', items: TASK_ITEM, minItems: 1 },
          rationale: { type: 'string' }, // why these were grouped (overlap signal)
        },
        required: ['tasks'],
      },
    },
    remaining: { type: 'integer' }, // non-terminal tasks in scope (ready OR blocked)
  },
  required: ['groups', 'remaining'],
}

const IMPL = {
  type: 'object',
  properties: {
    branch: { type: ['string', 'null'] }, // the committed group branch, or null on failure
    outcome: { type: 'string', enum: ['ready', 'failed'] },
    summary: { type: 'string' },
  },
  required: ['outcome', 'summary'],
}

const REVIEW = {
  type: 'object',
  properties: {
    verdict: { type: 'string', enum: ['approve', 'request_changes'] },
    notes: { type: 'string' }, // → the SAME implementer, if request_changes
    handoff: { type: 'string' }, // → the NEXT (fresh) reviewer: what was flagged and why
  },
  required: ['verdict', 'notes'],
}

const MERGE = {
  type: 'object',
  properties: {
    landed: { type: 'boolean' },
    detail: { type: 'string' }, // the script's one-line status (or eject reason)
  },
  required: ['landed', 'detail'],
}

const ACK = {
  type: 'object',
  properties: { ok: { type: 'boolean' }, detail: { type: 'string' } },
  required: ['ok'],
}

// ---------------------------------------------------------------------------
// Agent prompts
// ---------------------------------------------------------------------------
function schedulerPrompt(round) {
  const readyCmd = TASKS
    ? `list only the still-ready ones among these uids: ${TASKS.join(', ')} (a task is ready only if unblocked AND unclaimed)`
    : `run \`${JKB}${DB} task next${GLOBAL} --json '${GATED_SCOPE}' --limit 100\``
  const gateNote = DESIGN_GATE
    ? ` The scope carries \`${DESIGN_GATE}\` — the design gate (D28): only design-approved tasks are swarmable, so un-triaged tasks are correctly invisible here.`
    : ''
  return `You are the SCHEDULER for a task swarm (pass ${round}), the head of each round (design D27.8). Read the current READY frontier and cluster overlapping tasks into WORK-GROUPS.

1. Ready frontier: ${readyCmd}. \`task next\` returns only unblocked, non-terminal, UNCLAIMED tasks (claimed = already in flight, excluded) ordered by priority then due — safe to start now.${gateNote}
2. Remaining count: run \`${JKB}${DB} query${GLOBAL} --json 'kind:task ${TASKS ? '' : GATED_SCOPE}' --limit 1000\` and count items whose "status" is NOT "done" and NOT "cancelled" (those are terminal; everything else — open/in_progress/needs_review — is still remaining work). If scoping by explicit uids, count only those uids.
3. For each ready task capture "uid", "id", "title", "priority", "namespace", and — if the uid starts with file:// — the absolute file path before '#' as "source_file" (else null).
4. PRECOMPUTE OVERLAP SIGNALS, then CLUSTER (this judgement is why you are an agent):
   a. For each ready task, read its body (\`${JKB}${DB} task show <uid> --json\`; for file-backed tasks the real requirement is the line ending in \`^<frag>\` in its source_file) and extract the concrete files/paths/symbols/crate it will touch. This extraction is the deterministic signal.
   b. Group tasks that CLEARLY overlap — shared target files/paths, shared symbols, same crate/module, or near-duplicate descriptions — so one implementer builds them together on one branch and resolves the overlap directly (instead of it surfacing as a merge conflict).
   c. HARD CAP: at most ${GROUP_CAP} tasks per group. Split larger overlaps into multiple groups even if they overlap somewhat — the merge queue absorbs the residual. Non-overlapping tasks are SINGLETON groups (one task). Over-grouping serializes independent work; under-grouping causes conflicts — group only clear overlaps.
   d. Group ONLY within THIS ready frontier (never include a task that isn't ready right now). Give each group a one-line "rationale" naming the shared surface (or "independent" for singletons).

Return the groups and the remaining count. Do not modify anything.`
}

function implementerPrompt(group, priorBranch, reviewHint) {
  const list = group.tasks
    .map(
      (t) =>
        `  - ${t.uid} — ${t.title}${t.source_file ? ` (detail: the line ending \`^${fragOf(t.uid)}\` and its indented notes in ${t.source_file})` : ''}`,
    )
    .join('\n')
  const branchName = `swarm-task/${shortUid(group.tasks[0].uid)}`.replace(/[^A-Za-z0-9/_-]/g, '-').slice(0, 48)
  return `You are the IMPLEMENTER for ONE work-group in a task swarm, in an isolated git worktree that shares the repo's object store with the main copy at ${REPO}. You build this group and STAY WITH IT through review/fix — you keep the context of the code you just wrote.

WORK-GROUP (${group.tasks.length} task${group.tasks.length > 1 ? 's' : ''}), implement ALL of them on ONE branch:
${list}
${reviewHint ? `\nFEEDBACK to address this pass (from the reviewer or a merge-queue eject):\n${reviewHint}\n${priorBranch ? `Continue from your previous branch ${priorBranch} — pull the updated ${INTEGRATION} first (\`git rebase ${INTEGRATION}\`) so you build on the latest integrated state.` : ''}` : ''}

Steps:
1. ${priorBranch ? `Reuse your branch ${priorBranch} (create it off ${INTEGRATION} if it's gone), and rebase it onto the current ${INTEGRATION} tip.` : `Branch off the latest integrated state: \`git switch -c ${branchName} ${INTEGRATION}\` (pick a short unique name if that one exists).`}
2. READ THE APPROVED DESIGN FIRST (D28). Each task here is design-approved — the decided approach was worked out with the user and recorded, NOT left for you to invent. For each task: (a) read its body for an inline "Design:" note (\`${JKB}${DB} task show <uid> --json\` for managed tasks; for a file-backed task, the "Design:" block indented beneath its \`^${'<frag>'}\` line in the source file); (b) also grep the repo's design docs for a decision that governs it: \`grep -rl '<task-uid>' openspec --include=design.md 2>/dev/null\` and read the decision block that names it. FOLLOW the recorded decision exactly — do not re-litigate settled choices or substitute your own architecture. If a task has NO recorded design anywhere (no inline note, no governing doc block) and its approach is genuinely non-obvious, return "failed" with reason "missing design for <uid>" rather than guessing (it should not have been gated through).
3. Implement EVERY task in the group fully, following the repo's CLAUDE.md conventions. Make the real change. Stay within the group's scope — implement all of these tasks and NOTHING beyond their union (no drift, no unrelated edits).
4. Verify with the repo's own scripts/tests (e.g. ./scripts/fix.sh, ./scripts/test.sh, ./scripts/clippy.sh, or the project's equivalent). Do not weaken tests to pass.
5. Commit with a NORMAL, PROFESSIONAL commit message describing the change on its own terms — NO "swarm:" prefix, NO task uid, NO trailer, no reference to the swarm (this branch lands in a shared codebase). You MAY squash to one clean commit. Committing is REQUIRED — the worktree may be cleaned up, but the branch/commit persist for the reviewer and merge queue.

Return outcome "ready" with your branch name if all group tasks are implemented, committed, and tests pass; else "failed" with the reason. Only return "ready" if you actually committed a branch.`
}

function reviewerPrompt(group, branch, priorHandoff) {
  const list = group.tasks
    .map((t) => `  - ${t.uid} — ${t.title}${t.source_file ? ` (spec: \`^${fragOf(t.uid)}\` in ${t.source_file})` : ''}`)
    .join('\n')
  return `You are a REVIEWER in a task swarm — a FRESH reviewer for this pass (fresh eyes, no anchoring). Review ONE implementer branch against its WHOLE work-group, in your own isolated git worktree (it shares the object store with ${REPO}). Do NOT integrate — that is the deterministic merge queue's job.

BRANCH: ${branch}
WORK-GROUP (review against EVERY task):
${list}
${priorHandoff ? `\nPRIOR REVIEWER HANDOFF (what was flagged before and should be re-verified):\n${priorHandoff}\n` : ''}
Setup: in your worktree, \`git switch ${branch}\` (or check it out) so you can read the diff (\`git diff ${INTEGRATION}..${branch}\`) AND run the gate against the branch's code. Change nothing on the branch.

Checks:
1. The diff satisfies the spec/acceptance criteria of EVERY task in the group (file-backed → the requirement text at each \`^id\`; managed → \`${JKB}${DB} task show <uid>\`).
2. Scope: the branch implements ALL of the group's tasks and NOTHING beyond their union — no drift, no unrelated edits.
3. Tests + scripts are green: run the repo's ./scripts/* (build/test/clippy) on the checked-out branch.

Verdict:
- "approve" if all three hold → the branch enters the merge queue.
- "request_changes" (with concrete "notes") if anything fails → the SAME implementer will fix it. Always also write a "handoff": what you required and what the next reviewer should re-verify.

Return the verdict, notes, and handoff. Change nothing.`
}

function mergeRunnerPrompt(branch) {
  return `You are a MECHANICAL merge-queue runner — NO reasoning, NO conflict resolution. Run ONE script and report its result. Do all git work in the integration worktree at ${INTEGRATION_WT}.

Run EXACTLY: \`cd ${INTEGRATION_WT} && ${REPO}/scripts/merge-queue.sh ${branch} ${INTEGRATION} ${INTEGRATION_WT}\` (use ./scripts/merge-queue.sh if that path is right for this repo).

The script rebases ${branch} onto the current ${INTEGRATION} tip, fast-forwards (linear, no merge commit), and runs the gate. Do NOT resolve conflicts, edit code, or retry — just run it once and read its exit code:
- exit 0 → landed=true, detail = the script's "landed: …" line.
- exit 1 or 2 → landed=false, detail = the script's "eject: …" line (rebase conflict or red gate).
- exit 3 → landed=false, detail = the setup error.

Return {landed, detail}. Do not touch the main copy at ${REPO}.`
}

function completePrompt(group) {
  const fileBacked = group.tasks.filter((t) => t.source_file)
  const managed = group.tasks.filter((t) => !t.source_file)
  const parts = []
  if (managed.length) {
    parts.push(
      `Managed tasks (set status done via the CLI — no git artifact): ${managed
        .map((t) => `\`${JKB}${DB} task set ${t.uid} --status done\``)
        .join(' ; ')}`,
    )
  }
  if (fileBacked.length) {
    parts.push(
      `File-backed tasks: set the checkbox to "- [x]" on the line ending \`^<frag>\` in each source file, then \`${JKB}${DB} sync\` so the KB status becomes done:\n${fileBacked
        .map((t) => `    - ${t.source_file}  (\`^${fragOf(t.uid)}\`)`)
        .join('\n')}`,
    )
  }
  return `A swarm work-group's branch has LANDED on the feature branch ${INTEGRATION}. Mark EVERY task in the group DONE in jkb — landing is what completes them, and \`done\` unblocks their dependents (design D27.6/D27.7). Work in the MAIN copy at ${REPO}. Status is KB-local only — write NOTHING to git here.

${parts.join('\n\n')}

Return ok=true with a one-line confirmation. Do not fabricate changes.`
}

function claimPrompt(group, verb) {
  // On claim, also record WHERE the work is landing. `onto=` is the integration branch, and
  // it is the same facet `jkb task work` writes for a hand-driven session — so `jkb staging
  // ls` sees swarm work and manual work in one view instead of only the half it was told
  // about (design D38.1/D38.2). `tag set` rather than `tag add`: a second `onto=` is a
  // contradiction, and the swarm re-tags a group on every pass.
  const locate =
    verb === 'claim'
      ? [`repo=$(basename "$(git -C ${REPO} rev-parse --show-toplevel)")`]
          .concat(
            group.tasks.flatMap((t) => [
              `${JKB}${DB} task tag set ${t.uid} onto=${INTEGRATION}`,
              `${JKB}${DB} task tag set ${t.uid} repo="$repo"`,
            ]),
          )
          .join(' && ')
      : null
  const cmds = [
    group.tasks.map((t) => `${JKB}${DB} task ${verb} ${t.uid} --owner '${OWNER}'`).join(' && '),
    locate,
  ]
    .filter(Boolean)
    .join(' && ')
  const flip = verb === 'claim' ? ' (claiming also flips each task to in_progress)' : ''
  return `Mechanical step — run these jkb commands in the main copy at ${REPO} and report. ${verb === 'claim' ? 'CLAIM' : 'RELEASE'} this work-group's tasks for owner '${OWNER}'${flip}:

${cmds}

Run them, then return ok=true (detail = any command that returned false/failed). This is bookkeeping — change no code, touch no git.`
}

// Record the implementer's branch on every task in the group, so `jkb staging ls` can show
// the sub-branch and its commits exactly as it does for a hand-driven session (D38.2).
function branchTagPrompt(group, branch) {
  const cmds = group.tasks.map((t) => `${JKB}${DB} task tag set ${t.uid} branch=${branch}`).join(' && ')
  return `Mechanical step — in the main copy at ${REPO}, record this group's working branch:

${cmds}

Return ok=true (detail = any that failed). Bookkeeping only — change no code, touch no git.`
}

function statusPrompt(group, status) {
  const cmds = group.tasks.map((t) => `${JKB}${DB} task set ${t.uid} --status ${status}`).join(' && ')
  return `Mechanical step — in the main copy at ${REPO}, set this group's tasks to status "${status}" (KB-local; write nothing to git):

${cmds}

Return ok=true (detail = any that failed). Change no code.`
}

function reclaimPrompt() {
  return `Mechanical startup crash-recovery step (design D27.6.6b) — in the main copy at ${REPO}, run ONCE:

${JKB}${DB} task reclaim --keep '${OWNER}'

This clears claims left by CRASHED PRIOR runs (owner pid gone) before the first frontier read, while preserving THIS run's own claims (owner '${OWNER}' is kept). The ONGOING ~60s reclaim is handled by the /task-swarm command's sidecar, not here. Return ok=true with the reclaimed count. Change no code, touch no git.`
}

// ---------------------------------------------------------------------------
// Coordinator loop — pipelined (D27.9): each group flows Implement → Review → Merge
// independently (no per-round barrier); the merge queue is the one serial stage. As
// groups land and unblock dependents, a fresh SCHEDULER pass surfaces newly-ready groups
// and feeds them into the same pipeline. Loop until nothing is ready or in flight.
// ---------------------------------------------------------------------------
log(`swarm start · scope ${scopeExpr} · integration ${INTEGRATION} (${INTEGRATION_WT}) · owner ${OWNER}`)

const landed = [] // uids of completed (landed + marked done) tasks
const gaveUp = [] // uids the swarm exhausted RETRY_CAP on
const dispatched = new Set() // group signatures already in flight or finished
const inFlight = new Set() // live group-chain promises
const stats = { groups: 0, land: 0, eject: 0, requestChanges: 0 }
let round = 0

// The serial merge queue: one integration at a time (D27.6). Every approved branch chains
// onto `mergeLock`, so at most one merge-queue run is ever in flight.
let mergeLock = Promise.resolve()
async function enqueueMerge(branch) {
  const run = mergeLock.then(() =>
    agent(mergeRunnerPrompt(branch), {
      label: `merge:${branch}`,
      phase: 'Merge',
      schema: MERGE,
      model: 'haiku', // mechanical: runs one script, reports exit status — no reasoning
    }),
  )
  mergeLock = run.then(
    () => {},
    () => {},
  ) // keep the chain alive regardless of this run's outcome
  return (await run) || { landed: false, detail: 'merge runner returned nothing' }
}

const groupSig = (group) => group.tasks.map((t) => t.uid).sort().join('|')

// Process one work-group end-to-end: claim → (implement → review)* → merge → done/release.
async function processGroup(group) {
  const label = shortUid(group.tasks[0].uid) + (group.tasks.length > 1 ? `+${group.tasks.length - 1}` : '')
  // Claim every task in the group BEFORE building it, so a concurrent SCHEDULER pass sees
  // them as in-flight (claimed) and never re-hands them out (D27.1/D27.8).
  await agent(claimPrompt(group, 'claim'), { label: `claim:${label}`, phase: 'Claim', schema: ACK, model: 'haiku' })
  try {
    let attempt = 0
    let branch = null
    let reviewHint = null
    let priorHandoff = null
    while (attempt < RETRY_CAP) {
      attempt++
      // IMPLEMENT. The SAME implementer role stays with the group across the loop; the
      // Workflow primitive can't literally continue an agent, so we re-seed it with its
      // prior branch + the feedback (its own build context), which is the faithful
      // approximation of "keep the implementer, feed it the notes" (D27.8).
      const impl = await agent(implementerPrompt(group, branch, reviewHint), {
        label: `impl:${label}#${attempt}`,
        phase: 'Implement',
        schema: IMPL,
        isolation: 'worktree',
      })
      if (!impl || impl.outcome !== 'ready' || !impl.branch) {
        log(`group ${label}: implement failed (attempt ${attempt}/${RETRY_CAP}): ${impl ? impl.summary : 'no result'}`)
        continue
      }
      branch = impl.branch

      // Record the branch on the group's tasks so `jkb staging ls` shows the sub-branch and
      // its commits, exactly as it does for a hand-driven session (D38.2). Set once the
      // implementer has actually produced one — before that there is nothing true to record.
      await agent(branchTagPrompt(group, branch), {
        label: `tag:${label}#${attempt}`,
        phase: 'Implement',
        schema: ACK,
        model: 'haiku',
      })

      // Entering review: the WHOLE group is `needs_review` (transient — a reviewer is
      // reviewing; it no longer unblocks dependents, D27.7).
      await agent(statusPrompt(group, 'needs_review'), {
        label: `nr:${label}#${attempt}`,
        phase: 'Review',
        schema: ACK,
        model: 'haiku',
      })

      // REVIEW — a FRESH reviewer each pass, seeded with the prior handoff (objectivity +
      // memory, D27.5).
      const review = await agent(reviewerPrompt(group, branch, priorHandoff), {
        label: `review:${label}#${attempt}`,
        phase: 'Review',
        schema: REVIEW,
        isolation: 'worktree', // its own checkout of the branch to run the gate
      })
      if (!review || review.verdict === 'request_changes') {
        stats.requestChanges++
        reviewHint = (review && review.notes) || 'The reviewer requested changes but gave no notes; re-verify the group.'
        priorHandoff = (review && review.handoff) || priorHandoff
        await agent(statusPrompt(group, 'in_progress'), {
          label: `ip:${label}#${attempt}`,
          phase: 'Review',
          schema: ACK,
          model: 'haiku',
        })
        log(`group ${label}: request_changes (attempt ${attempt}/${RETRY_CAP})`)
        continue
      }

      // APPROVED → the serial merge queue (deterministic, no agent reasoning).
      const merge = await enqueueMerge(branch)
      if (merge.landed) {
        stats.land++
        await agent(completePrompt(group), { label: `done:${label}`, phase: 'Merge', schema: ACK, model: 'haiku' })
        group.tasks.forEach((t) => landed.push(t.uid))
        log(`group ${label}: landed → done · ${merge.detail}`)
        return
      }
      // EJECT (rebase conflict or red gate) → SAME implementer pulls the updated feature
      // branch, reproduces, fixes, and resubmits to a fresh reviewer (D27.5/D27.6).
      stats.eject++
      priorHandoff = (review && review.handoff) || priorHandoff
      reviewHint = `Merge-queue eject: ${merge.detail}. Pull/rebase onto the updated feature branch ${INTEGRATION}, reproduce the failure, fix it, and resubmit.`
      await agent(statusPrompt(group, 'in_progress'), {
        label: `ip:${label}#${attempt}`,
        phase: 'Merge',
        schema: ACK,
        model: 'haiku',
      })
      log(`group ${label}: merge eject (attempt ${attempt}/${RETRY_CAP}) · ${merge.detail}`)
    }
    // Exhausted the retry budget — reset the group to `open` so a human/later run can pick
    // it up, and record the give-up.
    await agent(statusPrompt(group, 'open'), { label: `giveup:${label}`, phase: 'Merge', schema: ACK, model: 'haiku' })
    group.tasks.forEach((t) => gaveUp.push(t.uid))
    log(`group ${label}: gave up after ${RETRY_CAP} attempts`)
  } finally {
    // Release the group's claims on settle — success, give-up, OR a thrown/aborted chain —
    // so a crashed group still frees its claims (the coordinator is the liveness authority
    // for in-flight tasks; `doctor`/`task reclaim` are the crash-recovery net, D27.1).
    await agent(claimPrompt(group, 'release'), {
      label: `release:${label}`,
      phase: 'Merge',
      schema: ACK,
      model: 'haiku',
    })
  }
}

function startGroup(group) {
  stats.groups++
  const sig = groupSig(group)
  // Never let a thrown group-chain reject `Promise.race(inFlight)` and abort the loop —
  // swallow it here (the chain's own `finally` already released its claims).
  const p = processGroup(group)
    .catch((e) => log(`group ${shortUid(group.tasks[0].uid)} errored: ${e}`))
    .finally(() => inFlight.delete(p))
  inFlight.add(p)
  return sig
}

// A SCHEDULER pass over the CURRENT ready frontier → new work-groups (never cross-round).
async function schedule() {
  round++
  // Startup crash-recovery scan (D27.6.6b), first pass only: clear claims left by dead
  // PRIOR runs before the first frontier read, keeping our own owner. The ONGOING ~60s
  // periodic reclaim is a true wall-clock timer owned by the /task-swarm command's sidecar
  // process (workflow JS has no clock/background timer), so we do NOT repeat it each pass.
  if (round === 1) {
    await agent(reclaimPrompt(), { label: 'reclaim#startup', phase: 'Schedule', schema: ACK, model: 'haiku' })
  }
  const s = await agent(schedulerPrompt(round), { label: `schedule#${round}`, phase: 'Schedule', schema: SCHEDULE })
  return s || { groups: [], remaining: 0 }
}

// Initial fan-out.
phase('Schedule')
let sched = await schedule()
log(`pass ${round}: ${sched.groups.length} group(s) · ${sched.remaining} remaining`)
for (const g of sched.groups) {
  const sig = groupSig(g)
  if (!dispatched.has(sig)) {
    dispatched.add(sig)
    startGroup(g)
  }
}

// Dynamic pipeline: as groups settle and unblock dependents, re-schedule and feed the
// newly-ready groups in. Bounded by ROUND_CAP and the token budget.
while (inFlight.size > 0 && round < ROUND_CAP) {
  if (budget.total && budget.remaining() < 60_000) {
    log(`stopping new work: ~${Math.round(budget.remaining() / 1000)}k tokens left; draining ${inFlight.size} in-flight`)
    break
  }
  await Promise.race(inFlight) // wait for at least one group to settle
  const s = await schedule()
  let added = 0
  for (const g of s.groups) {
    const sig = groupSig(g)
    // Skip groups already in flight/finished, or any task already dispatched (claims keep
    // the frontier from re-offering in-flight tasks, but guard against races anyway).
    if (dispatched.has(sig)) continue
    if (g.tasks.some((t) => landed.includes(t.uid) || gaveUp.includes(t.uid))) continue
    dispatched.add(sig)
    startGroup(g)
    added++
  }
  if (added === 0 && s.groups.length === 0 && s.remaining === 0) {
    log('frontier drained — all tasks complete')
  }
}

// Let every in-flight group finish its chain.
await Promise.allSettled([...inFlight])

log(
  `swarm done · passes ${round} · groups ${stats.groups} · landed ${stats.land} · ejects ${stats.eject} · request_changes ${stats.requestChanges}`,
)

return {
  scope: scopeExpr,
  integration_branch: INTEGRATION,
  passes: round,
  groups: stats.groups,
  // Landed on the feature branch and marked done in jkb; dependents unblocked.
  completed: landed,
  gave_up: gaveUp,
  merge_queue: { landed: stats.land, ejects: stats.eject },
  reviews: { request_changes: stats.requestChanges },
}
