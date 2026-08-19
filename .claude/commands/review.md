---
description: Review the current change. By default up to three reviewers split it by feature area and each asks every lens question; `medium` fans the nine lenses out instead, `high` adds adversarial skeptics. Prints findings; use /jkb-review-log to file them as tasks.
argument-hint: "[range]  [low|medium|high]  [-- anything to focus on]"
---

Run this repo's own code reviewer (design D37) and report what it finds. It works in any git
repo — project conventions, design docs and review history are used when present and skipped
when absent.

Arguments given: `$ARGUMENTS`

## 0. Self-review first

The reviewer costs a dozen or more agents and millions of tokens per run, so it has to be spent
on what is hard to spot rather than on what a careful reading catches. Read your own diff first
and check the seven points in `/jkb-review-log` step 0 — edits landed, comments match the code, new
branches reachable, the same rule honoured at its other call sites, the changed path covered by a
test, new parameters supplied everywhere, and the thing actually run. Then run the repo's verify
command, and say what the self-review caught.

Reach **high confidence in every part** of the change first: whatever you are unsure of is the
thing to test, and naming a gap in the report does not substitute for closing it.

## 1. Resolve what to review

- **Range**: an argument that looks like a git range or ref (`main...HEAD`, `HEAD~3..`,
  `abc123`) is the range. If none is given, decide:
  - working tree dirty (`git status --porcelain` non-empty) → review the working tree
    (`range: ""`), which is the common case mid-change;
  - otherwise, the branch's own commits: `<trunk>...HEAD`, where trunk is `origin/HEAD`'s
    target else `main`/`master`. If HEAD *is* trunk, review the working tree and say so.
- **Effort**: `low` (default), `medium` or `high`. The axis is **breadth of fan-out**, not how
  hard anyone looks — every lens question is asked at every tier.
  - `low` — up to **three** reviewers, split by feature area, each asking every lens question
    against **one** reading of its code. Findings are filed **unverified**. This is the default
    because the fanned-out tier cost ~3M tokens an hour per run while nine agents each re-read
    the same diff, producing near-duplicate findings that then had to be merged back together.
  - `medium` — the previous default: nine lens reviewers (one question, whole diff) plus one
    holistic reviewer per functional unit. Nine independent readings catch things one reader
    misses, at several times the cost. Reach for it on a large or unfamiliar change.
  - `high` — `medium` plus adversarial verification. A defect finding faces three angles, a
    quality one faces the single "is it worth the churn?" angle, and each survives on a majority
    of the verdicts cast about it; skeptics are batched by file, so findings in one region share
    the cost of reading it. Use before merging something risky.
  - Findings are unverified at `low` and `medium`. Measured, only ~6% were ever refuted, which
    does not pay for skeptics up front — whoever picks one up finds out cheaply whether it is
    real.
- **Focus**: anything after `--` is passed to every reviewer as an extra thing to watch for.

Confirm you are in a git repo (`git rev-parse --show-toplevel`) and record its absolute path.
If the resolved range has no changes, say so and stop.

## 2. Run the reviewer

Call the **Workflow** tool with `scriptPath` set to the first of these that exists —
`"$CLAUDE_CONFIG_DIR/workflows/jkb-code-review.js"`, `"$HOME/.claude/workflows/jkb-code-review.js"`,
`./.claude/workflows/code-review.js` — and `args` as an **actual JSON object** (a stringified
one makes every field `undefined`):

```json
{
  "repo": "<abs path to the repo root>",
  "range": "<range, or \"\" for the working tree>",
  "effort": "low",
  "focus": "<focus text, or \"\">"
}
```

This deliberately opts into multi-agent orchestration. Roughly, for a 1,000-line diff:
**`low` ≈ 6 agents** (2 scouts, ≤3 area reviewers, rank), **`medium` ≈ 15** (2 scouts, 9 lenses,
≤3 feature reviewers, consolidate, rank), and **`high` ≈ 15 + one agent per file-batch × 3
angles**, so verification scales with how many *files* carry findings rather than how many
findings there are.

Before launching, get the size (`git diff --shortstat <range>`) and say what it will cost. Over
~2,000 changed lines, say it is cheaper *and a better review* to run several smaller ranges — a
reviewer reasoning about 3,000 lines at once reasons worse about each of them. It runs in the
background and notifies on completion.

## 3. Report

The workflow returns `{findings, raw, refuted, reviewers, features, context, note}`. Print:

- one line of provenance: range, effort, how many reviewers ran, which functional units were
  identified, and which context it found (conventions / design / patterns) — a review that
  found no conventions file reviewed a different, weaker thing, and the reader should know;
- `raw → refuted → findings`, so the filtering is visible;
- each finding: **severity** · `file:line` · the summary, then the scenario and the fix
  direction, most severe first. Group by severity.
- whether the run was **verified** (`verified: true`) or filed unverified. At `low` every
  finding carries `unverified: true` and that is expected — say once that nothing was
  skeptic-checked, rather than repeating it per finding. At `high`, flag the individual ones
  that still carry it: those were past the verify cap or lost their skeptics.
- if `findings` is empty, say whether that is because nothing was found or because everything
  was refuted (`note` says which).

Do not re-review, re-rank or add findings of your own — they have already been through
verification and calibration. If you disagree with one, say so as a comment rather than editing
the list.

Then offer, without doing it: `/jkb-review-log` files these as jkb tasks under
`.codereviews/<datetime>-<branch>-<N>/`.
