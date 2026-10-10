---
description: Review the current change and file what it finds. This session coordinates; one jkb-reviewer reviews a small change, or up to three review the areas of a large one. The findings are filed as tasks and the round is recorded against the branch, which is what `jkb task land` checks.
argument-hint: "[range]  [-- anything to focus on]"
---

This command runs the repository's code review. This session coordinates it. For a small change,
one `jkb-reviewer` subagent reviews it, files every finding as a jkb task, and records the round
against the branch. For a large one, this session splits it across up to three reviewers and has the
merged findings filed and recorded as one round. Its calls are attested as the reviewer role, so it may
file and record, and it may not change code or task statuses. The prompts come from `jkb workflow
agent show`, so an operator copy saved in Code Factory's Workflows tab is what runs.

Arguments given: `$ARGUMENTS`

## 0. Self-review first

A review costs between one and four agents. Spend it on what is hard to spot: feature-level gaps,
cross-file contradictions, reasoning that needs a whole subsystem in view. Don't spend it on defects
a careful reading catches. Each of those costs a review to find and another to review the fix.

**Any doubt you can put into words is a test to write, not a line in the focus argument.** If you
can name the question, you can answer it yourself, usually in minutes. The focus argument is for
what you *cannot* check: a perspective you lack.

**Reach high confidence in every part of the change before launching.** Whatever you are unsure of
is exactly the thing to test. Reasoning about a behaviour is not evidence of it, and neither is a
test you have not seen fail. Read your own diff (`git diff <range>`) and check:

1. **Did every edit actually land?** Verify against a re-read of the file, not against what you
   believe you wrote.
2. **Does each comment and doc match the code beside it?** Trace anything that asserts a guarantee.
3. **Is every new branch and check reachable?** Name a concrete input that exercises it.
4. **Who else implements this rule?** **A rule every call site has to remember is itself the
   defect.** Move it into the callee, a type or the schema.
5. **Does any test exercise the path you changed?** For a regression test, revert the fix and
   confirm the test fails on the assertion it is named for.
6. **Did you add a parameter, field, or variant?** Check that every call site supplies the right
   value.
7. **Did you run it?** Execute the path you changed rather than reasoning about it.

Then run the repository's own verify command, and say what this pass caught.

## 1. Resolve what to review

- **Range.** An argument that looks like a git range or ref (`main...HEAD`, `HEAD~3..`, `abc123`)
  is the range. If there is none and the working tree is dirty, review the working tree:
  `stat_cmd="git diff --stat HEAD"`, `diff_cmd="git diff HEAD"`. Otherwise review the branch's own
  commits, `<trunk>...HEAD`, where trunk is `origin/HEAD`'s target, else `main` or `master`.
- **Focus.** Anything after `--` becomes `focus_block`, as `FOCUS: <text>`; otherwise it is empty.
- If the range has no changes, say so and stop. Over about 2,000 changed lines, suggest reviewing
  smaller ranges: a reviewer reasons worse about each line of a huge change.

Resolve the names against the **main** working copy, since a task session is a worktree that
`jkb task land` deletes:

```sh
main=$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")
repo=$(basename "$main"); branch=$(git rev-parse --abbrev-ref HEAD)
findings_ns="repos/$repo/codereviews/$(date +%Y%m%d-%H%M%S)-$(printf '%s' "$branch" | tr '/' '-')"
```

## 2. Start the round

```sh
jkb workflow agent show review-coordinator --var repo="$(git rev-parse --show-toplevel)" \
  --var range_desc="<the range, or 'the working tree'>" --var stat_cmd="<stat_cmd>" \
  --var diff_cmd="<diff_cmd>" --var findings_ns="$findings_ns" --var focus_block="<focus_block>"
```

Follow what it prints. It tells you how to fill the reviewers' prompts, when to split, and how
to file the pre-existing findings yourself.

## 3. Report

Relay the report:
- **Findings for this change:** grouped by severity, each as `file:line` · summary.
- **Pre-existing findings:** each with its backlog uid. If any of them is a must-fix, say so in a
  sentence. The branch can land over it, and somebody should still know it is there.
- **Where the findings are:** `$findings_ns`, which you can browse under `tasks/<repo>/codereviews/…`.
- **Whether the branch can land:** it can when no must-fix finding for this change is open
  (`jkb task land <uid>`). Otherwise, name what blocks it.
