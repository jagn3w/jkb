---
description: Review the current change with nine specialized reviewers plus one holistic reviewer per functional unit. Prints findings; use /review-log to file them as tasks.
argument-hint: "[range]  [low|high]  [-- anything to focus on]"
---

Run this repo's own code reviewer (design D37) and report what it finds. It works in any git
repo — project conventions, design docs and review history are used when present and skipped
when absent.

Arguments given: `$ARGUMENTS`

## 0. Self-review first

The reviewer costs roughly 16 agents, 3M tokens and an hour per run, so it must be spent on what
is hard to spot rather than on what a checklist catches. Run the seven-point self-review in
`/review-log` step 0 over your own diff first, then `./scripts/check.sh`. Say what it caught.

## 1. Resolve what to review

- **Range**: an argument that looks like a git range or ref (`main...HEAD`, `HEAD~3..`,
  `abc123`) is the range. If none is given, decide:
  - working tree dirty (`git status --porcelain` non-empty) → review the working tree
    (`range: ""`), which is the common case mid-change;
  - otherwise, the branch's own commits: `<trunk>...HEAD`, where trunk is `origin/HEAD`'s
    target else `main`/`master`. If HEAD *is* trunk, review the working tree and say so.
- **Effort**: `low` (default) or `high`. There is deliberately **no medium**.
  - `low` — the nine lenses and the feature reviewers run, duplicates are consolidated, the set
    is ranked, and the findings are filed **unverified**. Whoever picks one up finds out cheaply
    whether it is real. This is the right default: measured, only ~6% of findings were refuted,
    which does not pay for skeptics up front.
  - `high` — adds adversarial verification. Every finding faces three angles and survives on a
    majority; skeptics are batched by file, so a group of findings in one region shares the cost
    of reading it. Use before merging something risky.
- **Focus**: anything after `--` is passed to every reviewer as an extra thing to watch for.

Confirm you are in a git repo (`git rev-parse --show-toplevel`) and record its absolute path.
If the resolved range has no changes, say so and stop.

## 2. Run the reviewer

Call the **Workflow** tool with `scriptPath` set to the first of these that exists —
`"$CLAUDE_CONFIG_DIR/workflows/code-review.js"`, `"$HOME/.claude/workflows/code-review.js"`,
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
**`low` ≈ 15 agents** (2 scouts, 9 lenses, ≤3 feature reviewers, consolidate, rank) and
**`high` ≈ 15 + one agent per file-batch × 3 angles**, so it scales with how many *files* carry
findings rather than with how many findings there are.

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

Then offer, without doing it: `/review-log` files these as jkb tasks under
`.codereviews/<datetime>-<branch>-<N>/`.
