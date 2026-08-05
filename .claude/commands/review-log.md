---
description: Run this repo's code reviewer and log each finding as a task in .codereviews/<datetime>-<branch>-<N>/tasks.md, mounted into jkb
argument-hint: "[range]  [low|high]  [-- anything to focus on]"
---

You are running a **logged code review**: run the reviewer, then persist every finding as an
actionable task in a per-run folder mounted into the KB. Do these steps in order.

Arguments given: `$ARGUMENTS`

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

The folder name is `<datetime>-<branch>-<reviewNumber>`, where `reviewNumber` is the count of
prior reviews for this branch plus one.

## 2. Run the reviewer

Resolve the range, effort and focus, then launch the workflow **exactly as `/review` steps 1–2
describe** — same resolution rules, same `scriptPath` lookup, same `args` object. The reviewer
is one workflow with two thin callers precisely so that "what counts as a finding" cannot differ
between them (design D37.7).

It returns `{findings, raw, refuted, reviewers, verified, features, context, note}`. Each finding
carries `severity` (`must-fix` / `concern` / `nit`), `file`, `line`, `summary`, `scenario`,
`fix`, `kind`, and possibly `unverified`.

At the default `low` effort nothing is skeptic-checked — findings are filed as found, because
only ~6% were ever refuted and discovering a false one while fixing it is cheaper than three
skeptics per finding up front. Note that in the doc's header line so a reader knows what they
are looking at; do not annotate every task with it.

## 3. Log the findings as tasks

Write `REVIEW_DIR/tasks.md` as a jkb **`tasks`-serializer** Markdown doc, so it can be mounted
and managed as tasks:

```
# Code review — <branch> @ <YYYY-MM-DD HH:MM>

Base: <range> · HEAD: <short SHA> · Effort: <effort> · Reviewers: <n> · Findings: <N> (of <raw> raw, <refuted> refuted)

## Must-fix

- [ ] <summary> — <file>:<line> !p1
  <scenario>
  Fix: <fix>

## Concern

- [ ] <summary> — <file>:<line> !p2
  <scenario>
  Fix: <fix>

## Nit

- [ ] <summary> — <file>:<line> !p3
  <scenario>
  Fix: <fix>
```

Rules for the doc — these are the serializer's, and getting them wrong corrupts the sync:

- One `- [ ]` checkbox per finding, grouped under its `## <Severity>` header. Omit a header
  that has no findings.
- The `!p1`/`!p2`/`!p3` must be the **last token on the checkbox line**. Keep the summary and
  `file:line` before it, never let the line wrap so the priority lands on a continuation line,
  and put no other `!p` / `#f=v` / `+ns` / `^id` token at the end of any line — they would be
  read as task modifiers.
- Scenario and fix go on **indented continuation lines** under the task.
- Do not write `^id`s; jkb mints them on sync.
- No findings → a single `## Summary` section with `- [x] No findings — clean review !p3`, and
  say in the prose line whether nothing was found or everything was refuted.

## 4. Mount the folder into the KB

Each review gets its **own per-folder mount**: the `tasks` serializer maps `##` headers to
namespaces under the mount, so one shared `.codereviews` mount would merge every review's
`## Must-fix` into a single namespace.

```sh
repo=$(basename "$(git rev-parse --show-toplevel)")
folder=$(basename "$REVIEW_DIR")
ns="repos/$repo/codereviews/$folder"
jkb mount create "$ns" "$REVIEW_DIR" --serializer tasks
jkb sync "$ns"
```

Findings home under `repos/<repo>/codereviews/<folder>/…` and auto-mirror to
`tasks/<repo>/codereviews/…` (D26/D32). A running `jkb sync --watch` will not see a
just-created mount until it restarts, so the one-shot `jkb sync` is what lands them now.
`mount create` is idempotent, so re-running a review is safe.

## 5. Report accuracy, then the findings

Before the findings, report how far this reviewer has earned trust here. Findings are tasks, so
their status says whether they were acted on (design D37.6):

```sh
jkb query --global --json 'kind:task ns:repos/'"$repo"'/codereviews/**' --limit 1000
```

Count `done` (accepted) against `cancelled` (dismissed as noise) among *settled* findings and
give the acceptance rate. Under roughly a 20% acting-rate means the sensitivity is wrong, and is
worth saying out loud. This is **reported only** — never suppress a class of finding because it
has been dismissed before. A class that keeps being dismissed may be a real problem the team
keeps deciding not to fix, and quietly ceasing to report it would turn that decision into an
invisible one.

Then print the findings (severity · `file:line` · summary, most severe first) and a one-line
summary: `N findings (H must-fix, M concern, L nit) from R raw, K refuted`. Say they are mounted
at `repos/<repo>/codereviews/<folder>` (git-ignored on disk) and browsable under
`tasks/<repo>/codereviews/…`.
