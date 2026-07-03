---
description: Run /code-review and log each finding as a task in .codereviews/<datetime>-<branch>-<N>/tasks.md
argument-hint: "[low|medium|high|max] [-- extra /code-review args]"
---

You are running a **logged code review**: wrap `/code-review`, then persist every
finding as an actionable task in a per-run folder. Do these steps in order.

## 1. Create the review folder

Run exactly this (from the repo root) and use the printed path as `REVIEW_DIR`:

```sh
mkdir -p .codereviews
b=$(git rev-parse --abbrev-ref HEAD | tr '/' '-')
d=$(date +%Y%m%d-%H%M%S)
n=$(( $(ls -d .codereviews/*-"$b"-* 2>/dev/null | wc -l | tr -d ' ') + 1 ))
dir=".codereviews/$d-$b-$n"
mkdir -p "$dir"
echo "$dir"
```

The folder name is `<datetime>-<branch>-<reviewNumber>`, where `reviewNumber` is the
count of prior reviews for this branch plus one.

## 2. Run the review

Invoke the **code-review** skill on the current diff. Effort = the first argument if it
is one of `low|medium|high|max`, otherwise `high`. Pass any remaining arguments through
to the skill. Do NOT add `--comment`/`--fix` unless the user included them.

Arguments given: `$ARGUMENTS`

Collect the verified findings (summary, file, line, severity/verdict, failure scenario).

## 3. Log findings as tasks

Write `REVIEW_DIR/tasks.md` as a **jkb `tasks`-serializer** Markdown doc so it can be
mounted and managed as tasks later:

```
# Code review — <branch> @ <YYYY-MM-DD HH:MM>

Base: <base ref or SHA> · HEAD: <short SHA> · Effort: <effort> · Findings: <N>

## High

- [ ] <one-line finding summary> — <file>:<line> !p1
  <failure scenario / why it's wrong, as indented continuation prose>

## Medium

- [ ] <summary> — <file>:<line> !p2
  <failure scenario>

## Low

- [ ] <summary> — <file>:<line> !p3
  <failure scenario>
```

Rules for the doc:
- One `- [ ]` checkbox task per finding; group findings under a `## <Severity>` header.
- Map severity to a **trailing** `!p1`/`!p2`/`!p3` (high/medium/low) so jkb parses it as
  a priority. Keep everything else (file:line, summary) *before* it, and put no other
  `!p`/`#f=v`/`+ns`/`^id` tokens at the end of a line — they'd be misread as task
  modifiers. Do not add `^id`s; jkb mints those on sync.
- Put the failure scenario / rationale on indented continuation lines under each task.
- If the review found nothing, write a single section `## Summary` with
  `- [x] No findings — clean review !p3`.

## 4. Report

Print `REVIEW_DIR/tasks.md` and a one-line summary: `N findings (H high, M medium, L low)`.
Mention the folder is under `.codereviews/` (git-ignored) and can be mounted with
`jkb mount codereviews .codereviews --serializer tasks --include '**/tasks.md'`.
