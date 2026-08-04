export const meta = {
  name: 'code-review',
  description:
    'Reviews a diff along eight assumption-kind lenses plus one holistic reviewer per functional unit, deduplicates, puts every finding to three adversarial skeptics, and returns the survivors ranked and severity-calibrated. Returns structured findings, so a command can act on them.',
  whenToUse:
    'Called by /review (prints findings) and /review-log (writes them as jkb tasks). Works in any git repo; project conventions, design docs and review history are used when present and skipped when absent.',
  phases: [
    { title: 'Scout' },
    { title: 'Review' },
    { title: 'Verify' },
    { title: 'Rank' },
  ],
}

// ---------------------------------------------------------------------------
// Config (design D37). `args` comes from the calling command.
// ---------------------------------------------------------------------------
const cfg = (typeof args === 'string' ? JSON.parse(args) : args) || {}
const REPO = cfg.repo || '.'
const RANGE = cfg.range || '' // e.g. "main...HEAD"; empty = working tree vs HEAD
const EFFORT = ['low', 'medium', 'high'].includes(cfg.effort) ? cfg.effort : 'high'
const FOCUS = cfg.focus || '' // optional free-text steer from the user

// Effort scales the two expensive dimensions and nothing else: how many skeptics each
// finding must survive, and how many functional units get their own holistic reviewer.
// The eight lenses always all run — a lens skipped is a class of bug nobody looked for.
const SKEPTICS = EFFORT === 'low' ? 1 : 3
const FEATURE_CAP = EFFORT === 'low' ? 2 : EFFORT === 'medium' ? 3 : 5
// Findings each reviewer may report. Unbounded, twelve reviewers produced 69 raw findings on a
// 2.8k-line diff, and since every one is then multiplied by the skeptics, this number sets the
// cost of the whole run. It also improves what comes back: a reviewer forced to pick its five
// best reports its five best, rather than padding with everything it noticed.
const PER_REVIEWER_CAP = EFFORT === 'low' ? 3 : EFFORT === 'medium' ? 5 : 8
// Hard ceiling on how many findings enter verification. Anything beyond it is still REPORTED,
// marked unverified — dropping findings silently would make a budget look like a clean review.
const VERIFY_CAP = EFFORT === 'low' ? 15 : EFFORT === 'medium' ? 30 : 50

const diffCmd = RANGE ? `git diff ${RANGE}` : 'git diff HEAD'
const statCmd = RANGE ? `git diff --stat ${RANGE}` : 'git diff --stat HEAD'

// ---------------------------------------------------------------------------
// Schemas — structured returns, so the coordinator never parses prose.
// ---------------------------------------------------------------------------
const FINDING = {
  type: 'object',
  properties: {
    file: { type: 'string' }, // repo-relative path
    line: { type: 'integer' }, // 1-indexed; best anchor if the issue spans lines
    summary: { type: 'string' }, // one line, states the defect
    scenario: { type: 'string' }, // concrete inputs/state -> wrong result
    fix: { type: 'string' }, // the direction, not a patch
    severity: { type: 'string', enum: ['must-fix', 'concern', 'nit'] },
    kind: { type: 'string', enum: ['defect', 'quality'] },
  },
  required: ['file', 'line', 'summary', 'scenario', 'severity', 'kind'],
}

const FINDINGS = {
  type: 'object',
  properties: {
    findings: { type: 'array', items: FINDING },
    // Said out loud so "no findings" is distinguishable from "did not look".
    coverage: { type: 'string' },
  },
  required: ['findings', 'coverage'],
}

// The survey and the context gather are SEPARATE agents run in parallel. One agent doing both
// read the whole diff and searched the repo for three kinds of document, which on a 2k-line
// change ran 23 minutes and died mid-response — taking the entire review with it.
const SURVEY = {
  type: 'object',
  properties: {
    summary: { type: 'string' }, // what this change does, in a sentence
    files: { type: 'array', items: { type: 'string' } },
    features: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          name: { type: 'string' },
          purpose: { type: 'string' },
          paths: { type: 'array', items: { type: 'string' } },
          entry_points: { type: 'string' }, // how a user/caller reaches it
        },
        required: ['name', 'purpose', 'paths'],
      },
    },
  },
  required: ['summary', 'files', 'features'],
}

const CONTEXT = {
  type: 'object',
  properties: {
    conventions: { type: 'string' }, // project rules a reviewer must honour, or ""
    design: { type: 'string' }, // decisions governing this change, or ""
    patterns: { type: 'string' }, // recurring past-bug patterns here, or ""
  },
  required: ['conventions', 'design', 'patterns'],
}

const VERDICT = {
  type: 'object',
  properties: {
    refuted: { type: 'boolean' },
    reason: { type: 'string' },
  },
  required: ['refuted', 'reason'],
}

const GROUPS = {
  type: 'object',
  properties: {
    // Indices of findings that are the same defect. Singletons may be omitted.
    groups: { type: 'array', items: { type: 'array', items: { type: 'integer' } } },
    note: { type: 'string' },
  },
  required: ['groups'],
}

const RANKED = {
  type: 'object',
  properties: {
    findings: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          file: { type: 'string' },
          line: { type: 'integer' },
          summary: { type: 'string' },
          scenario: { type: 'string' },
          fix: { type: 'string' },
          severity: { type: 'string', enum: ['must-fix', 'concern', 'nit'] },
          kind: { type: 'string', enum: ['defect', 'quality'] },
          lenses: { type: 'string' }, // which reviewers found it (merged duplicates)
        },
        required: ['file', 'line', 'summary', 'scenario', 'severity', 'kind'],
      },
    },
    note: { type: 'string' },
  },
  required: ['findings'],
}

// ---------------------------------------------------------------------------
// Shared reviewer contract. Every finder gets this; it is where the noise floor
// and the read-beyond-the-diff rule live (design D37.3/D37.5).
// ---------------------------------------------------------------------------
function preamble(scout, role, mode = 'defect') {
  return `You are ${role} reviewing a code change in the git repo at ${REPO}.

THE CHANGE
${scout.summary}

Read it with: \`${statCmd}\` then \`${diffCmd}\`. Files changed:
${scout.files.map((f) => `  ${f}`).join('\n')}
${FOCUS ? `\nThe user asked you to pay particular attention to: ${FOCUS}\n` : ''}
DO NOT REVIEW THE DIFF ALONE. The best findings are invisible in the diff and only appear when
you read what it touches: the callers of a changed function, the code it calls, the other
implementation of the same rule, the other surface that reads the same fact. Open those files.
A finding you could have made without reading beyond the diff is usually a shallow one.
${scout.conventions ? `\nPROJECT CONVENTIONS (a breach of these is a real finding; and never propose an approach the project has already rejected):\n${scout.conventions}\n` : ''}${scout.design ? `\nDECIDED DESIGN GOVERNING THIS CHANGE (a contradiction of this is a finding; a decision deliberately taken is NOT a bug):\n${scout.design}\n` : ''}${scout.patterns ? `\nBUGS THIS PROJECT HAS ACTUALLY HAD (a pattern library, NOT a checklist — check whether this change repeats one, do not force a match):\n${scout.patterns}\n` : ''}
WHAT COUNTS AS A FINDING
${
  mode === 'quality'
    ? `  IN   Structure: how this code is organized, factored, named and placed. See your question
       below for the specific forms, and for the evidence each severity demands.
  OUT  Formatting and whitespace. A rename whose only argument is taste. An abstraction for a
       single use. A redesign of code the change did not touch. "This could be more idiomatic"
       with no cost attached.`
    : `  IN   Something that is wrong now, wrong on the second run, wrong on real data, or wrong on
       the error path. Two copies of one rule that can disagree. A claim the code makes and
       does not keep (a name, a doc, an error message, a test).
  OUT  Naming, formatting, "consider extracting", missing comments, hypothetical futures with
       no path to them, and anything whose only argument is taste. Do not report these at all
       — not even as nits. Another reviewer owns structure, and these crowd out real findings.`
}

For each finding give:
  file, line   the anchor, repo-relative
  summary      one line stating the defect itself, not the topic
  scenario     CONCRETE inputs or state, then the wrong result. If you cannot write this, you
               do not have a finding yet — either find the concrete case or drop it.
  fix          the direction in a sentence. Not a patch.
  severity     must-fix · concern · nit. The test for must-fix is **would you hold the merge
               for this?** — ask it of each finding individually and answer honestly; on a
               healthy change that is a small handful at most. nit is small or local, and you
               should be using it: a run where nearly everything is "concern" has told the
               reader nothing and left them to prioritize unaided. Severity is earned by how
               concrete and inevitable the harm is, never by how strongly you hold the view.
  kind         defect or quality.

Report NOTHING you are not prepared to defend against a skeptic reading the real code. A
finding that turns out to be already handled costs the reader exactly as much attention as a
real one. Returning an empty list is a perfectly good outcome — say in \`coverage\` what you
examined and why nothing engaged your question.

AT MOST ${PER_REVIEWER_CAP} FINDINGS — a ceiling, emphatically not a target. Coming back with
one finding you are sure of is a better review than ${PER_REVIEWER_CAP} you are hedging on, and
returning none is better still when there is nothing. The bar for each is: **would you raise
this with the author face to face, and be confident it was worth their time?** If you would
soften it to "you might want to consider…", it does not clear the bar. If you have more
candidates than the cap, report the ones you would defend first and say in \`coverage\` how many
you set aside and what they were. Every finding is independently re-verified by other agents
reading the code, so padding costs real money as well as attention.

READING BUDGET. Some files here are thousands of lines. Locate what you need with \`grep -n\`
and read bounded ranges around it; do not read a large file end to end. Read *widely* — many
places, each shallowly — rather than deeply into one.`
}

// ---------------------------------------------------------------------------
// The eight lenses (design D37.2). Each is one KIND OF ASSUMPTION, and its
// prompt is the question form plus the sub-questions that operationalize it.
// ---------------------------------------------------------------------------
const LENSES = [
  {
    key: 'input',
    role: 'the INPUT & BOUNDARY reviewer',
    body: `YOUR QUESTION: **what values break this?**

Find every value that crosses into the changed code — arguments, file contents, stdin, env,
config, database rows, network responses, filenames, user text — and ask what the code assumes
about it that is not guaranteed.

  · Boundaries: empty, exactly one, zero, negative, maximum, one past maximum, absent vs empty.
  · Text: non-UTF-8 bytes, unicode case (is a case-fold ASCII-only?), combining characters,
    BOM, CRLF, tabs vs spaces, leading/trailing whitespace, extremely long values.
  · Quoting and escaping: a quote inside a quoted value, an unterminated quote, a separator
    inside a field, an apostrophe or space or colon or newline inside a path.
  · SECURITY — this is your half of it: anything interpolated into another language is an
    injection site. SQL, shell, regex, HTML, format strings, glob and LIKE metacharacters,
    path traversal (\`..\`), symlinks. Ask specifically: is this value ever concatenated into a
    command, query or pattern instead of being passed as a parameter?
  · Limits imposed by something else: parameter-count caps, argument-length caps, identifier
    length, integer width and overflow, NaN and infinities.

For each, trace what actually happens — wrong answer, crash, corruption, or a silent no-op.`,
  },
  {
    key: 'state',
    role: 'the STATE & TIME reviewer',
    body: `YOUR QUESTION: **what happens the second and third time?**

A single pass through this code is not the test. Run it twice. Interrupt it and run it again.
Run it on data an older version wrote.

  · Idempotency: run the same operation twice — is the second run a no-op, or does it double
    something, append a second copy, or flip state back and forth?
  · Identity: is any id, key, name, path or slug derived from something that can CHANGE —
    content, position, ordinal, a title, a line number? If so it does not survive an edit, and
    the thing it identified becomes an orphan while a new one is minted beside it.
  · Compounding: does an error make the NEXT run worse rather than merely failing? Corruption
    that grows each cycle is the most expensive class here and the hardest to see in a diff.
  · Accumulation and cleanup: what does this create that nothing removes? Files, worktrees,
    rows, locks, temp state, references to things that were deleted or renamed.
  · Resumption: after a crash halfway, is the leftover state distinguishable from a completed
    run, or from a fresh one?
  · Old data: does this read data written before this change? Is the old shape still handled,
    and is "absent" distinguishable from "empty" and from "not yet migrated"?`,
  },
  {
    key: 'inference',
    role: 'the INFERENCE reviewer',
    body: `YOUR QUESTION: **X is treated as evidence of Y — when do they come apart?**

This is the subtlest lens and often the highest value. Code constantly concludes one thing from
another: it compares a hash and concludes the content changed; checks a pid and concludes work
is in progress; sees an ancestor commit and concludes a branch merged. Each of those is an
inference, and each can be wrong in two directions.

  · Enumerate every derived signal the changed code ACTS on, and write down the conclusion it
    draws. Then, for each, ask both:
      – when is X true but Y false?  (acts when it should not)
      – when is Y true but X false?  (fails to act when it should)
  · Usual suspects: hashes and checksums (of what exactly — bytes, or meaning? would a change
    in the *renderer* move the hash?), timestamps and clock skew, counts, existence checks,
    exit codes, "the file is there", "the process is alive", version or ancestry checks.
  · Equality: what is actually being compared — identity, value, bytes, or semantics? Is a
    normalization missing on one side?
  · SECURITY — this is your half of it: authentication is not authorization. Ask whether the
    code concludes a permission from something weaker: a token's presence rather than its
    claims, an origin or referer header, a client-supplied id, an ordering assumption. Also:
    does a comparison that guards a secret run in variable time?
  · Two signals that mean different things being conflated into one is the classic form. Look
    for a single boolean standing in for two distinct causes.`,
  },
  {
    key: 'contract',
    role: 'the CONTRACT & REACH reviewer',
    body: `YOUR QUESTION: **who else touches this fact, and do they agree?**

Nothing in a diff is alone. Find the other code that shares a fact, a rule or an invariant with
what changed, and check that they still say the same thing.

  · Callers: does every existing caller satisfy a precondition this change introduced or
    tightened? Actually grep for them — do not assume.
  · Callees: does this rely on a guarantee the called code does not actually make?
  · Duplicated authority: is this rule implemented in more than one place — two parsers, two
    id-minters, two validators, a check in the UI and a check in the server? If they can
    disagree, that is a finding even when both are currently correct.
  · Bypass: is the invariant enforced at one entry point while another path reaches the same
    state without passing it? An enforcement point that is not a choke point is not enforcement.
  · Other surfaces of the same fact: CLI, API, UI, JSON output, docs, help text. When a rule
    changes, every reader of it must change; the one that was not updated now disagrees, and
    the disagreement is worse than the old behaviour because it looks authoritative.
  · SECURITY — this is your half of it: is validation, authorization and rate limiting applied
    at EVERY entry point to this capability, or only the one in the diff? Trust boundaries: is
    data from outside treated as trusted once it is past one gate?
  · Compatibility: serialized shapes, wire formats, database schemas and public signatures —
    is an old producer or consumer broken?`,
  },
  {
    key: 'concurrency',
    role: 'the CONCURRENCY & ORDER reviewer',
    body: `YOUR QUESTION: **who else is running right now?**

Assume a second copy of this code, a second process, or a second user is running concurrently,
and that any step can be interrupted between two lines.

  · Check-then-act: is there a gap between deciding something is true and relying on it? Read
    a value, compute, write it back — what if someone wrote in between? (lost update)
  · Is the invariant inside the transaction, or is it established by a read taken before it?
    A guard computed from a stale snapshot is not a guard.
  · Locks: is the lock held across the whole region that needs it? Is it released on every
    path including errors? Can two lock acquisitions deadlock by ordering?
  · Ordering assumptions: does this rely on operations completing in the order they started,
    on a single writer, on a callback not being re-entered?
  · Shared mutable state across threads, tasks or processes — including files, and including
    "only one of these can run at a time" claims that nothing enforces.
  · If the change genuinely has no concurrency (single-threaded, one process, no shared
    resource), say so in \`coverage\` and return nothing. That is a correct answer here.`,
  },
  {
    key: 'failure',
    role: 'the FAILURE & BLAST RADIUS reviewer',
    body: `YOUR QUESTION: **what happens on the error path, and how much does it take down?**

Every operation that can fail: make it fail, and follow what happens next.

  · Partial completion: it failed halfway — what is left behind, and is that state safe for the
    next run to encounter? Is a multi-step change atomic, or can it stop in the middle?
  · Blast radius: does ONE bad item fail the whole batch, file, or run? Should it? Conversely,
    does one bad item get silently skipped when the caller needed to know?
  · Data loss: on the error path, is anything overwritten, truncated or deleted that cannot be
    recovered? Pay special attention to a read that falls back to a default and a write that
    then makes that default true.
  · Swallowed failures: errors ignored, discarded results, a catch that continues, an unwrap or
    panic in a path that can genuinely occur, a default substituted for a failure.
  · Honesty: does it report success when nothing happened? Does the error message name the real
    cause, or a plausible wrong one — a setup failure reported as a content conflict sends
    someone to fix the wrong thing.
  · Rollback: if it undoes work on failure, does the undo actually restore the prior state, and
    can the undo itself fail?`,
  },
  {
    key: 'scale',
    role: 'the SCALE & RESOURCES reviewer',
    body: `YOUR QUESTION: **what does this cost on real data?**

Test fixtures have three rows. Estimate the real magnitudes for this repo — from existing data,
config, or what the feature is for — and evaluate against those.

  · Per-item work in a loop: a query, a subprocess, a file open, a network call, or a regex
    compile per row. N+1 patterns. Say how many round-trips the real case implies.
  · Unbounded: reads with no limit, listings with no scope, recursion with no depth cap,
    accumulating an entire result set in memory, reading a whole file when a stream would do.
  · Repeated recomputation: something computed inside a loop that does not vary within it.
  · Growth: what happens at 10x and 1000x the expected size — linear, quadratic, or a hard
    failure (a limit exceeded, a timeout, memory exhaustion)?
  · Only report what will actually bite: name the magnitude and the consequence. "This is O(n²)"
    with n bounded at 5 is not a finding.`,
  },
  {
    key: 'intent',
    role: 'the INTENT reviewer',
    body: `YOUR QUESTION: **does it do what it claims?**

Everything here is a claim, and the code either keeps it or does not: names, doc comments, type
signatures, error messages, help text, and tests.

  · A name that describes something the code does not do, or does only sometimes.
  · A doc comment or help text that contradicts the code — including one that describes the
    behaviour before this change.
  · A signature or type that permits states the code cannot handle, or that fails to express a
    constraint the code depends on.
  · An error message that misstates the cause, or advice in it that will not help.
  · TESTS — give these real attention:
      – Would this test FAIL if the change it covers were reverted? If the assertion would hold
        either way, it guards nothing. Look hard at tests whose setup does not actually reach
        the branch being claimed, and at assertions that are true for a coincidental reason.
      – Does it assert the mechanism, or just that nothing crashed?
      – Does the fixture make the interesting case reachable at all?
  · A claim in the change's own description or design doc that the code does not deliver.`,
  },
]

// The ninth reviewer. It is not an assumption-kind lens — it asks whether the code is well
// ORGANIZED rather than whether it is wrong — so it is listed separately, its findings are
// kind="quality", and they are verified by a different skeptic, since "is this better?" has no
// reproduction. Quality is NOT capped at nit: a structural problem can be a genuine must-fix.
// It rises only on evidence in the code, though, which the ranking pass can check without
// re-reviewing — the argument must cite where the cost is already being paid.
const STRUCTURE = {
  key: 'structure',
  role: 'the STRUCTURE & FACTORING reviewer',
  mode: 'quality',
  body: `YOUR QUESTION: **is there a better way to organize and factor this?**

You are the only reviewer allowed to talk about shape rather than correctness. Judge the code
as something people will read and change for years.

  · Cohesion — does each function, module or type do one job? Look for a unit doing several
    unrelated things, and for a job smeared across several units that must be edited together.
  · Coupling and layering — does this reach across a boundary it should not? Does a lower layer
    know about a higher one? Does a change here force an unrelated module to change?
  · Placement — is this in the module a reader would look in? Would someone find it by guessing?
  · Duplication — the same logic written twice. (If the two copies can DRIFT APART and disagree,
    that is a defect, not a nit: report it as kind="defect" and say what goes wrong when they
    diverge.)
  · Abstraction level — a function mixing policy with plumbing; a missing seam that forces
    callers to repeat a dance; equally, a seam invented for one caller that costs indirection
    and buys nothing.
  · Vocabulary — does the change introduce a second word for a concept the codebase already
    names? Two names for one thing is how a codebase becomes unlearnable.
  · API shape — is the signature honest? Too many parameters, a boolean that selects behaviour,
    primitives where a named type belongs, an argument order that invites transposition, a
    return type that cannot express a real outcome.
  · Dead or speculative — added code with no caller; generality with no second use case.
  · Idiom — does it match how the surrounding code does this? Matching the local idiom beats
    importing a "better" pattern the rest of the codebase does not use.
  · Complexity that genuinely impedes — nesting that cannot be held in the head, a function that
    needs a diagram. Not length for its own sake.

RULES THAT KEEP THIS USEFUL
  · Every finding must name what it COSTS TODAY: a reader misled, one change that will need two
    edits, a concept with two names, a caller forced to repeat something. "Cleaner" is not a
    cost. If you cannot name the cost, do not report it.
  · Propose the SMALLEST restructuring that removes the cost. You are not redesigning the change.
  · Only code this diff touched. Pre-existing structure is not this change's debt — unless the
    change made it materially worse.
  · Respect the project's conventions above. If they mandate a pattern, matching it is correct
    even where you would choose otherwise.

SEVERITY — you are NOT capped at nit, but you must EARN anything above it, and the currency is
evidence in the code rather than the strength of your opinion.
  nit       the default. A real, named cost, but small or local.
  concern   only if the cost is ALREADY BEING PAID somewhere you can point at. Cite it: the
            second place that had to change and did not, the caller that got the repeated dance
            wrong, the two names for one concept both live in the tree, the workaround that
            exists because the seam is missing. Name the file and line of the evidence.
  must-fix  only if the shape makes a correctness property UNENFORCEABLE or guarantees a defect
            on a change that is clearly coming — e.g. the invariant cannot be checked at a choke
            point because of where the code lives, or a new case must be added in three places
            with nothing that forces the third. Show the mechanism; do not assert it.
A structural finding whose argument is "this would be better" is a nit no matter how strongly
you believe it. One that says "here is where this already went wrong" is a concern. Reviewers
who inflate severity get ignored wholesale, including when they are right.`,
}

// ---------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------
function surveyPrompt() {
  return `You are the SURVEY step of a code review in the git repo at ${REPO}. You produce the reviewers' map of the change. You do NOT review — report no defects, and do not evaluate anything.

1. THE CHANGE. Run \`${statCmd}\` first, then \`${diffCmd}\`. Summarize in ONE sentence what the change does, and list every changed file (repo-relative, exactly as git prints them).

2. FUNCTIONAL UNITS. Cluster the change into the coherent CAPABILITIES it delivers — things a user or caller would name, not files. A capability usually spans several files (a command, the function behind it, its tests). At most ${FEATURE_CAP}: merge the smallest rather than exceeding it, and prefer fewer larger units to slicing one capability in two. For each give a short name, its purpose in a sentence, the paths involved, and how a caller reaches it. A pure refactor delivering no new capability returns zero units.

BUDGET — this step is a map, not a reading. Skim the diff for structure; do NOT open the changed files, do NOT chase callers, do NOT read design docs (another agent is doing that in parallel), and do NOT verify anything. Aim to finish in a handful of tool calls. Everything you leave undone, twelve reviewers will do properly with their own eyes.

If the diff is empty, return an empty file list and say so in the summary.`
}

function contextPrompt() {
  return `You are the CONTEXT step of a code review in the git repo at ${REPO}. You gather the project's own rules so the reviewers can hold the code to them. You do NOT review and you do NOT read the diff in detail — run \`${statCmd}\` only, to know which areas are in play.

Gather what exists and skip what does not. NONE of these are required; many repos have none. Return "" for anything you do not find, and never invent one.

a. CONVENTIONS — look for CLAUDE.md, AGENTS.md, CONTRIBUTING.md, .cursorrules, or a docs style guide. Extract the RULES a reviewer must hold code to: invariants, forbidden patterns, required patterns, and any approach the project has explicitly REJECTED (so a reviewer does not propose it). Quote compactly; skip prose that is merely descriptive.

b. DESIGN — if the repo records decisions (openspec/, docs/adr/, rfcs/, a design.md), find what governs THESE changed areas. Grep for the changed modules and for any task id in recent commit messages (\`git log -3\`). Extract the decisions, so a reviewer knows what was deliberate rather than accidental.

c. PAST BUG PATTERNS — if there is review history (.codereviews/) or a run of fix commits, extract the RECURRING SHAPES of what has gone wrong in this repo, as patterns. Not a list of old findings.

BUDGET — around 40 lines per section, hard. You are writing a briefing that will be pasted into a dozen prompts, so every line costs twelve times. Be ruthless: the three most load-bearing rules beat twenty accurate ones. Finish in a handful of tool calls.`
}

function lensPrompt(scout, lens) {
  return `${preamble(scout, lens.role, lens.mode)}

${lens.body}

Work through your question systematically over the whole change. Other reviewers are covering
other questions — stay on yours; do not report a finding that belongs to another lens unless
you are confident and it is serious.`
}

function featurePrompt(scout, feature) {
  return `${preamble(scout, `a HOLISTIC reviewer of one capability: "${feature.name}"`)}

THE CAPABILITY YOU OWN
  name:         ${feature.name}
  purpose:      ${feature.purpose}
  paths:        ${feature.paths.join(', ')}
  entry points: ${feature.entry_points || '(work them out)'}

The other reviewers each ask ONE question across the whole change. You do the opposite: you
take this ONE capability and judge it whole. Read all of its code, not only the changed lines.

  · DOES IT ACTUALLY WORK? Trace the real path from the entry point to the effect, as a caller
    would exercise it, with realistic data. Not the happy path in the test — the real one.
    Features that ship broken usually do so because nobody walked the whole path once.
  · IS IT COMPLETE? A capability has surfaces: the operation, its inverse, the way you see its
    state, the way it appears in listings and output, the docs and help that mention it. When a
    rule is added in one place and the other surfaces are not updated, the result is worse than
    not shipping — the stale surface still looks authoritative.
  · DO ITS PARTS AGREE? Two halves of one capability built on different assumptions about the
    same thing — different defaults, different idea of what a field means, different error
    behaviour — is a defect even when each half is individually right.
  · IS IT THE RIGHT SHAPE? A design-level critique the single-question reviewers cannot give:
    is this the model the problem calls for, does it fight the surrounding architecture, will
    the next capability of this kind fit beside it or need a special case? Be concrete about
    the consequence; "I would have designed it differently" is not a finding.
  · DOES IT DELIVER WHAT WAS INTENDED? Compare against the change's stated purpose and any
    governing design. Something promised and quietly not built is a finding.

Report only what is wrong. Do not summarize the feature or praise it.`
}

const ANGLES = [
  {
    key: 'handled',
    ask: `Is this ALREADY HANDLED somewhere the finder did not look? Read the function containing the line, its callers, what it calls, and any validation, guard, default or normalization on the path to it. Reviewers routinely report a case that is caught one frame up. If a guard exists that makes the scenario impossible, the finding is refuted — quote the guard.`,
  },
  {
    key: 'reproduce',
    ask: `Does it ACTUALLY REPRODUCE? Take the claimed scenario and walk the real code line by line with those concrete inputs. Do the stated conditions truly produce the stated wrong result — or does the code diverge from the finder's mental model partway? If you cannot walk a concrete path to the bad outcome, the finding is refuted.`,
  },
  {
    key: 'real',
    ask: `Is it REAL AND IN SCOPE? Three ways it might not be. (a) It is a deliberate, recorded decision — check the conventions and design context and the comments around the code, which often explain exactly why it is this way. (b) It is out of this change's responsibility — pre-existing behaviour the diff merely moved, or something the caller is contractually required to handle. (c) It is speculative — the "wrong result" needs conditions that cannot arise in this system. If any holds, the finding is refuted.`,
  },
]

// A structural suggestion has no reproduction, so the defect skeptics would refute every one of
// them by construction ("I cannot walk a path to a bad outcome"). Quality findings get their own
// single skeptic, asking the question that actually kills a bad refactoring suggestion.
const QUALITY_ANGLE = {
  key: 'worth-it',
  ask: `Is this actually an IMPROVEMENT, and is it WORTH THE CHURN? Read the real code and the
code around it. Refute the finding if any of these holds: (a) it is taste, with no cost named
that a reader or a future change actually pays; (b) the current shape matches the project's
conventions or the surrounding idiom, and the proposal would make this code the odd one out;
(c) the proposed restructuring is larger than the problem, or would touch code this change did
not; (d) it is already how the code works, or the "duplication" is two things that merely look
alike and should be free to diverge; (e) the abstraction it asks for has exactly one use.
A restructuring suggestion that survives should be one a thoughtful maintainer would agree makes
the code easier to change, not merely different.

If the finding claims a severity above nit, it must cite where the cost is ALREADY being paid,
or the mechanism forcing a coming change to go wrong. **Check that citation against the real
code.** If the cited evidence is not there, the stated cost is not real and the finding is
refuted — not merely over-graded.`,
}

function refutePrompt(finding, angle) {
  return `You are a SKEPTIC. Your job is to REFUTE a code-review finding by reading the actual code in the repo at ${REPO}. You are not here to confirm it — a finding that survives you should have survived a real attempt to kill it.

THE FINDING
  file:     ${finding.file}:${finding.line}
  claim:    ${finding.summary}
  scenario: ${finding.scenario}
  severity: ${finding.severity}

YOUR ANGLE
${angle.ask}

Read the real code — do not reason from the finding's description of it, which is exactly where
these go wrong. But read it in BOUNDED pieces:

  · \`${RANGE ? `git diff ${RANGE} -- ${finding.file}` : `git diff HEAD -- ${finding.file}`}\` for what the change did to this file.
  · \`grep -n\` to locate the function containing line ${finding.line}, then read that range — a
    couple of hundred lines around it, not the file. Files here run to thousands of lines.
  · \`grep -n\` again for the specific callers or guards your angle needs, and read only those
    ranges.

Reading one large file end to end costs more than the finding is worth, and buys nothing over
reading the right two hundred lines.

THE BURDEN OF PROOF IS ON THE FINDING. It stands only if you can POSITIVELY ESTABLISH it against
the real code — not merely fail to disprove it. These are different bars, and the difference is
the whole job: "I could not find a guard" is not the same as "I confirmed there is no guard on
any path that reaches this", and only the second earns refuted=false.

So refuted=true covers three cases, not one:
  · you found the guard, the divergence, or the recorded decision that kills it; OR
  · you could not establish a load-bearing part of the claim — the scenario's conditions are not
    actually reachable, or you could not confirm the code does what the finding says; OR
  · you ran out of certainty. Uncertainty refutes. Say what you could not establish.

A false finding costs the reader the same attention as a true one, and a reviewer that cries
wolf stops being read at all — including when it is right.

Return refuted=true with the specific reason (the guard you found, or the part you could not
establish), or refuted=false with the concrete chain you verified in the code: which lines you
read, and why each step of the claimed failure path follows. If you cannot write that chain, the
answer is refuted.`
}

function consolidatePrompt(findings) {
  const list = findings
    .map((f, i) => `${i}. [${f.severity}] ${f.file}:${f.line} — ${(f.summary || '').slice(0, 180)}`)
    .join('\n')
  return `Group the code-review findings below by WHICH DEFECT THEY ARE, so the same one is not verified several times over. This is a cheap consolidation step: judge from the summaries alone, do not read any code, and do not evaluate whether the findings are correct.

${list}

Two findings belong in the same group when **one change would resolve both** — the same defect
reported by different reviewers in different words, or reported at two locations that are the
same underlying problem (a rule and the doc that describes it; the same mistake at two lines of
one function). Reviewers here work from different angles, so the same defect routinely arrives
described three different ways.

Do NOT group merely related things: two different bugs in one function, or two instances that
would each need their own fix, are separate. When unsure, leave them separate — wrongly merging
loses a finding, while wrongly splitting only costs one more verification.

Return \`groups\` as arrays of indices, listing only groups of two or more. Anything you do not
mention stays on its own.`
}

function rankPrompt(findings) {
  return `You are the RANKING pass of a code review in the repo at ${REPO}. Every finding below already survived adversarial verification. Your job is calibration and consolidation — NOT re-reviewing, and NOT finding anything new.

Each finding carries \`skeptics\`: what the skeptics who failed to refute it actually verified.
Read those. A skeptic that let a finding stand often still corrected part of it — narrowed its
scope, disproved one of its supporting claims, or said the severity is overstated. That is the
most reliable evidence you have, because it came from someone who read the code trying to kill
the finding. Apply those corrections: narrow the summary, drop a scenario detail the skeptic
disproved, move the severity. Where a skeptic contradicts the finder, believe the skeptic.

FINDINGS
${JSON.stringify(findings, null, 2)}

1. MERGE DUPLICATES. Different reviewers describe the same defect in different words and often
   at different lines. Merge any that would be fixed by the same change into one finding: keep
   the clearest summary, the most concrete scenario, the best-anchored file:line, and record the
   contributing reviewers in \`lenses\`. Do not merge two genuinely different defects that happen
   to sit in the same function.

2. CALIBRATE SEVERITY across the whole set. Each finder saw only its own findings, so their
   severities are not comparable — restate them on one scale:
     must-fix  wrong now, wrong on the next run, loses data, or corrupts state.
     concern   wrong under conditions that will occur in normal use.
     nit       small or local.
   A **quality** finding (kind="quality") is NOT capped — a structural problem can genuinely be
   a concern or a must-fix — but it has to have earned it, and you can check that without
   reading the code: above nit it must CITE where the cost is already being paid (a specific
   place that had to change and did not, a caller that got it wrong, two live names for one
   concept) or show the mechanism by which a coming change is forced to go wrong. A quality
   finding above nit whose argument reduces to "this would be better" — however well argued —
   is a nit. **Demote it.**
   Be willing to move things DOWN generally. If everything is must-fix, nothing is.

   THE TEST FOR must-fix IS: **would you hold the merge for this?** Ask it of each one
   individually and answer honestly. On a healthy change the answer is yes for a handful at
   most — if you are marking more than about a fifth of the set must-fix, you are not
   calibrating, you are relaying. The previous run of this reviewer put 34 of 45 findings on
   "concern", which is the same failure one tier down: a severity every finding shares carries
   no information and leaves the reader to prioritize unaided, which is the job you are here to
   do.

3. ORDER STRICTLY, most damaging first — a total order across the whole set, not just grouped by
   tier. The reader works down this list and stops when they run out of time, so position 1 must
   genuinely be the thing most worth their next hour.

4. SHARPEN each summary to one line that states the DEFECT — what is wrong — rather than the
   topic. "X is never removed, so a second Y wedges" beats "cleanup issue in X". Keep the
   scenario concrete. Do not lengthen anything.

Return the consolidated, calibrated, ordered list, plus a \`note\` giving what you merged and —
in one sentence — why the must-fix set is the size it is. If it is empty, say that plainly;
a change with nothing worth holding the merge for is a normal and good outcome.`
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------
log(`code review · ${RANGE || 'working tree'} · effort ${EFFORT} · ${LENSES.length} lenses, ${SKEPTICS} skeptic(s) per finding`)

phase('Scout')
// Two bounded agents in parallel rather than one that does everything: the combined version
// read the whole diff AND searched the repo for three kinds of document, and on a 2k-line
// change it ran 23 minutes and died mid-response, taking the review with it.
const [survey, context] = await parallel([
  () => agent(surveyPrompt(), { label: 'survey', phase: 'Scout', schema: SURVEY }),
  () => agent(contextPrompt(), { label: 'context', phase: 'Scout', schema: CONTEXT }),
])

// A dead survey and an empty diff are different facts and must not report as the same one.
// Saying "empty diff" when the scout crashed is a specific wrong cause for a generic failure —
// it sends the reader to check their range instead of re-running.
if (!survey) {
  log('ABORTED: the survey step failed, so there is no map of the change to review')
  return { findings: [], reviewers: 0, raw: 0, refuted: 0, error: 'survey failed', note: 'survey step failed — nothing was reviewed; re-run' }
}
if (survey.files.length === 0) {
  log('nothing to review — the diff is empty')
  return { findings: [], reviewers: 0, raw: 0, refuted: 0, note: 'empty diff' }
}
if (!context) log('NOTE: context gathering failed — reviewing without project conventions, design or past patterns')

const scout = { ...survey, conventions: '', design: '', patterns: '', ...(context || {}) }
log(
  `${scout.files.length} file(s) · ${scout.features.length} functional unit(s)` +
    ` · context: ${[scout.conventions && 'conventions', scout.design && 'design', scout.patterns && 'patterns'].filter(Boolean).join(', ') || 'none found'}`,
)

// FAN OUT. Eight lenses (horizontal: one question, whole diff) plus one holistic reviewer per
// functional unit (vertical: one capability, end to end) — design D37.1.
phase('Review')
const reviewers = [
  ...[...LENSES, STRUCTURE].map((l) => ({ label: `lens:${l.key}`, prompt: lensPrompt(scout, l), tag: l.key })),
  ...scout.features.slice(0, FEATURE_CAP).map((f) => ({
    label: `feature:${f.name}`.slice(0, 40),
    prompt: featurePrompt(scout, f),
    tag: `feature:${f.name}`,
  })),
]
if (scout.features.length > FEATURE_CAP) {
  log(`NOTE: ${scout.features.length - FEATURE_CAP} functional unit(s) beyond the cap of ${FEATURE_CAP} got no holistic reviewer`)
}

// A barrier here is deliberate and is the case that justifies one: deduplication needs the
// FULL result set before the expensive verification stage, or the same defect found by three
// reviewers is put to nine skeptics.
const reports = await parallel(
  reviewers.map((r) => () =>
    agent(r.prompt, { label: r.label, phase: 'Review', schema: FINDINGS }).then((res) =>
      res ? { ...res, tag: r.tag } : null,
    ),
  ),
)

const raw = []
for (const rep of reports.filter(Boolean)) {
  for (const f of rep.findings || []) raw.push({ ...f, lens: rep.tag })
}
log(`${raw.length} raw finding(s) from ${reports.filter(Boolean).length}/${reviewers.length} reviewers`)

// Order by severity BEFORE deduplicating or capping. Both of those keep whatever comes first,
// and arrival order is reviewer order — which is arbitrary. Left unsorted, collapsing a
// duplicate keeps whichever reviewer happened to run first rather than the graver reading of
// the same defect, and the verify cap spends its budget on an early reviewer's nits while a
// later reviewer's must-fix goes unverified.
const SEV_RANK = { 'must-fix': 0, concern: 1, nit: 2 }
const bySeverityFirst = (a, b) => (SEV_RANK[a.severity] ?? 3) - (SEV_RANK[b.severity] ?? 3)
raw.sort(bySeverityFirst)

// Deterministic dedup: same file and line, or a summary already seen. Near-duplicates that
// survive this are merged by the ranking pass, which can read them together and judge.
const seen = new Set()
const deduped = raw.filter((f) => {
  const key = `${f.file}:${f.line}`
  const alt = `${f.file}:${(f.summary || '').toLowerCase().slice(0, 60)}`
  if (seen.has(key) || seen.has(alt)) return false
  seen.add(key)
  seen.add(alt)
  return true
})
if (deduped.length !== raw.length) log(`${raw.length - deduped.length} exact duplicate(s) collapsed`)
if (deduped.length === 0) {
  return { findings: [], reviewers: reviewers.length, raw: raw.length, refuted: 0, note: 'no findings' }
}

// CONSOLIDATE. Deterministic dedup only catches an identical file:line or summary, but the same
// defect routinely arrives from three angles in three wordings at three locations — and each
// copy would otherwise buy its own skeptics, which is the expensive stage. One cheap agent
// judging from summaries alone collapses them first. It is best-effort: a failure here costs
// duplicate verification, never a lost finding.
phase('Verify')
let candidates = deduped
if (deduped.length > 3) {
  const grouped = await agent(consolidatePrompt(deduped), {
    label: 'consolidate',
    phase: 'Verify',
    schema: GROUPS,
  })
  const merged = []
  const claimed = new Set()
  for (const group of (grouped && grouped.groups) || []) {
    // Trust nothing about the indices: out-of-range or already-used ones are dropped rather
    // than allowed to drop a finding or duplicate one.
    const members = [...new Set(group)]
      .filter((i) => Number.isInteger(i) && i >= 0 && i < deduped.length && !claimed.has(i))
    if (members.length < 2) continue
    members.forEach((i) => claimed.add(i))
    const picked = members.map((i) => deduped[i]).sort(bySeverityFirst)
    // Keep the gravest reading, and record every lens that saw it — agreement across angles is
    // itself evidence, and the ranking pass is told to weigh it.
    merged.push({ ...picked[0], lens: picked.map((f) => f.lens).join(' + ') })
  }
  const singles = deduped.filter((_, i) => !claimed.has(i))
  candidates = [...merged, ...singles].sort(bySeverityFirst)
  if (candidates.length !== deduped.length) {
    log(`${deduped.length - candidates.length} near-duplicate(s) consolidated before verification`)
  }
}

// VERIFY. Each finding faces SKEPTICS fresh skeptics with different angles; it survives on a
// majority failing to refute it (design D37.3).
// Verification dominates the cost of a review — it is the one stage multiplied by the number of
// findings — so it is STAGED rather than flat. One decisive skeptic runs first and kills the
// cheap mistakes; only findings that survive it AND are worth the money face the other two.
// 2-of-3 is preserved for those: stage one standing plus at least one of stage two.
const toVerify = candidates.slice(0, VERIFY_CAP)
const unverified = candidates.slice(VERIFY_CAP)
if (unverified.length) {
  log(`NOTE: ${unverified.length} finding(s) past the verify cap of ${VERIFY_CAP} are reported UNVERIFIED, not dropped`)
}

const judged = await parallel(
  toVerify.map((f) => () => {
    // Quality findings face the "is this worth it?" skeptic instead of the defect ones, which
    // would refute every structural suggestion for having no reproduction.
    const first = f.kind === 'quality' ? QUALITY_ANGLE : ANGLES[1] // 'reproduce' is the decisive one
    return agent(refutePrompt(f, first), {
      label: `refute:${first.key}:${f.file.split('/').pop()}`.slice(0, 40),
      phase: 'Verify',
      schema: VERDICT,
    }).then((v1) => {
      // Refuted at stage one, or a quality finding (which only ever gets its one skeptic), or
      // a nit (not worth two more agents) — settle here.
      const escalate =
        SKEPTICS > 1 &&
        v1 &&
        !v1.refuted &&
        f.kind !== 'quality' &&
        f.severity !== 'nit' &&
        (!budget.total || budget.remaining() > 80_000)
      if (!escalate) return { votes: [v1].filter(Boolean), stage1: v1 }
      return parallel(
        [ANGLES[0], ANGLES[2]].map((a) => () =>
          agent(refutePrompt(f, a), {
            label: `refute:${a.key}:${f.file.split('/').pop()}`.slice(0, 40),
            phase: 'Verify',
            schema: VERDICT,
          }),
        ),
      ).then((rest) => ({ votes: [v1, ...rest].filter(Boolean), stage1: v1 }))
    }).then(({ votes }) => {
      const real = votes
      // A skeptic that died counts as no opinion; with none surviving, keep the finding and
      // let the human judge rather than dropping it silently.
      const stood = real.filter((v) => !v.refuted)
      const survives = real.length === 0 || stood.length >= Math.ceil(real.length / 2)
      // Carry what the surviving skeptics verified into the finding. A skeptic that fails to
      // refute still routinely corrects one — narrows its scope, disproves a supporting claim,
      // or says the severity is overstated — and that correction is the best evidence in the
      // run, because it came from someone reading the code in order to kill it. Discarding it
      // was throwing away the most careful work in the pipeline.
      const enriched = {
        ...f,
        skeptics: stood.map((v) => v.reason).filter(Boolean).join(' || ').slice(0, 1200),
      }
      return { finding: enriched, survives, votes: real }
    })
  }),
)

const verified = judged.filter(Boolean).filter((j) => j.survives).map((j) => j.finding)
const killed = judged.filter(Boolean).length - verified.length
// Findings past the verify cap are carried through unverified rather than dropped, and say so.
const survivors = [...verified, ...unverified.map((f) => ({ ...f, unverified: true }))]
log(`${verified.length} survived verification, ${killed} refuted${unverified.length ? `, ${unverified.length} unverified` : ''}`)
if (survivors.length === 0) {
  return { findings: [], reviewers: reviewers.length, raw: raw.length, refuted: killed, note: 'every finding was refuted' }
}

// RANK. One pass over all survivors together: merge near-duplicates and put severity on a
// single scale, which no individual finder could do (design D37.4).
phase('Rank')
// The fallback is a real ordering, not the arrival order. The ranking agent is one API call
// away from failing (it did, on the first full run, to a session limit), and handing back an
// unsorted list makes a review look uncalibrated when only its last step was lost.
const bySeverity = (list, note) => ({ findings: [...list].sort(bySeverityFirst), note })
const ranked =
  EFFORT === 'low'
    ? bySeverity(survivors, 'ranking pass skipped at low effort; sorted by severity, not merged')
    : (await agent(rankPrompt(survivors), { label: 'rank', phase: 'Rank', schema: RANKED })) ||
      bySeverity(survivors, 'ranking pass failed; sorted by severity, not merged or recalibrated')

const counts = ranked.findings.reduce((acc, f) => ({ ...acc, [f.severity]: (acc[f.severity] || 0) + 1 }), {})
log(
  `done · ${ranked.findings.length} finding(s): ` +
    `${counts['must-fix'] || 0} must-fix, ${counts.concern || 0} concern, ${counts.nit || 0} nit`,
)

return {
  range: RANGE || 'working tree',
  effort: EFFORT,
  reviewers: reviewers.length,
  features: scout.features.map((f) => f.name),
  context: {
    conventions: Boolean(scout.conventions),
    design: Boolean(scout.design),
    patterns: Boolean(scout.patterns),
  },
  raw: raw.length,
  refuted: killed,
  findings: ranked.findings,
  note: ranked.note || '',
}
