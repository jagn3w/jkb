---
description: Review the current change with nine specialized reviewers plus one holistic reviewer per functional unit, adversarially verified. Prints findings; use /review-log to file them as tasks.
argument-hint: "[range]  [low|medium|high]  [-- anything to focus on]"
---

Run this repo's own code reviewer (design D37) and report what it finds. It works in any git
repo — project conventions, design docs and review history are used when present and skipped
when absent.

Arguments given: `$ARGUMENTS`

## 1. Resolve what to review

- **Range**: an argument that looks like a git range or ref (`main...HEAD`, `HEAD~3..`,
  `abc123`) is the range. If none is given, decide:
  - working tree dirty (`git status --porcelain` non-empty) → review the working tree
    (`range: ""`), which is the common case mid-change;
  - otherwise, the branch's own commits: `<trunk>...HEAD`, where trunk is `origin/HEAD`'s
    target else `main`/`master`. If HEAD *is* trunk, review the working tree and say so.
- **Effort**: `low` | `medium` | `high`, default **`medium`**. It scales findings per reviewer
  (3/5/8), how many functional units get a holistic reviewer (2/3/5), the verify cap (15/30/50)
  and whether verification escalates past one skeptic. The nine lenses always all run.
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
  "effort": "medium",
  "focus": "<focus text, or \"\">"
}
```

This deliberately opts into multi-agent orchestration: 9 lenses + up to 5 feature reviewers,
then staged verification of each finding. **Budget roughly 30 agents per 1,000 diff lines at
`medium`.** Before launching, get the size (`git diff --shortstat <range>`) and say what it will
cost. Over ~2,000 changed lines, tell the user it is cheaper *and a better review* to run
several smaller ranges — a reviewer reasoning about 3,000 lines at once reasons worse about each
of them — and offer `low` as the alternative. It runs in the background and notifies on
completion.

## 3. Report

The workflow returns `{findings, raw, refuted, reviewers, features, context, note}`. Print:

- one line of provenance: range, effort, how many reviewers ran, which functional units were
  identified, and which context it found (conventions / design / patterns) — a review that
  found no conventions file reviewed a different, weaker thing, and the reader should know;
- `raw → refuted → findings`, so the filtering is visible;
- each finding: **severity** · `file:line` · the summary, then the scenario and the fix
  direction, most severe first. Group by severity. Mark any carrying `unverified: true` as such
  — they were past the verify cap and no skeptic read them.
- if `findings` is empty, say whether that is because nothing was found or because everything
  was refuted (`note` says which).

Do not re-review, re-rank or add findings of your own — they have already been through
verification and calibration. If you disagree with one, say so as a comment rather than editing
the list.

Then offer, without doing it: `/review-log` files these as jkb tasks under
`.codereviews/<datetime>-<branch>-<N>/`.
