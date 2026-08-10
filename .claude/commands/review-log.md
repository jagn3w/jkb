---
description: Run this repo's code reviewer and log each finding as a task in .codereviews/<datetime>-<branch>-<N>/tasks.md, mounted into jkb
argument-hint: "[range]  [low|high]  [-- anything to focus on]"
---

You are running a **logged code review**: run the reviewer, then persist every finding as an
actionable task in a per-run folder mounted into the KB. Do these steps in order.

Arguments given: `$ARGUMENTS`

## 0. Self-review first — the workflow is expensive

One run is roughly a dozen or more agents and millions of tokens. Spend it on what is hard to
spot: feature-level gaps, cross-file contradictions, reasoning that needs a whole subsystem in
view. Do not spend it on defects a careful reading catches — those cost a full extra pass to
find, plus another to review the fix.

**Reach high confidence in every part of the change before launching.** Naming a shaky area in
the report is not a substitute for resolving it — if you can already name the gap, the reviewer's
budget should not be spent rediscovering it. Whatever you are unsure of is precisely the thing to
test: write the test, run the path, or document the bound. Reasoning about a behaviour is not
evidence of it, and a test you have not seen fail is not evidence either.

So before launching the reviewer, read your own diff (`git diff <range>`) and check:

1. **Did every edit actually land?** Verify against a re-read of the file, not against what you
   believe you wrote. An edit that silently failed leaves a commit message describing behaviour
   the code does not have.
2. **Does each comment and doc match the code beside it?** Anything asserting a guarantee — trace
   the path and confirm it holds. Stale or aspirational comments mislead the next reader and the
   reviewer both.
3. **Is every new branch and check reachable?** Name a concrete input that exercises it. If you
   cannot, it is dead code wearing the costume of a safeguard.
4. **Who else implements this rule?** Before fixing at the site you are looking at, search for the
   other places that must obey the same rule. **A rule every call site has to remember is itself
   the defect** — prefer moving it into the callee, a type, or the schema, so no site can forget.
5. **Does any test exercise the path you changed?** A mode with no coverage is where defects live
   longest. If you added a regression test, revert the fix and confirm it fails on the assertion
   it is named for.
6. **Did you add a parameter, field, or variant?** Check every construction and call site supplies
   the right value — same-typed neighbours swap silently.
7. **Did you run it?** Execute the path you changed rather than reasoning about it.

Then run the repo's own verify command (its CI config or contributor docs name it). List each
part of the change with your confidence in it; anything short of high gets resolved before the
reviewer runs, not disclosed alongside its findings. Say in the report what this pass caught: a
review that surfaces only subtle things is the goal.

## 1. Resolve the repo, then create the review folder

**Resolve everything against the MAIN working copy**, not against wherever you are standing.
A `jkb task work` session is a git *worktree*, so inside one `git rev-parse --show-toplevel`
returns the session directory — which would put the findings under `repos/<session-name>/`, a
repo key nothing else uses, inside a directory `jkb task land` deletes when the session lands.
`--git-common-dir` is the same rule `gitrepo::main_root` uses, and it is correct in the main
copy too.

Run exactly this and use the printed values as `REVIEW_DIR`, `repo` and `branch`:

```sh
main=$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")
repo=$(basename "$main")
branch=$(git rev-parse --abbrev-ref HEAD)          # the branch under review — this checkout's
b=$(printf '%s' "$branch" | tr '/' '-')
d=$(date +%Y%m%d-%H%M%S)
n=$(( $(ls -d "$main"/.codereviews/*-"$b"-* 2>/dev/null | wc -l | tr -d ' ') + 1 ))
dir="$main/.codereviews/$d-$b-$n"
mkdir -p "$dir"
echo "REVIEW_DIR=$dir  repo=$repo  branch=$branch"
```

The folder name is `<datetime>-<branch>-<reviewNumber>`, where `reviewNumber` is the count of
prior reviews for this branch plus one. `branch` is this checkout's — the session branch when
you are in a session — and is what step 5 records the review against; `repo` and `REVIEW_DIR`
belong to the main copy, which outlives the session.

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

`repo` is the main copy's key from step 1 — never `basename $(git rev-parse --show-toplevel)`,
which in a session is the session's name.

```sh
folder=$(basename "$REVIEW_DIR")
ns="repos/$repo/codereviews/$folder"
jkb mount create "$ns" "$REVIEW_DIR" --serializer tasks
jkb sync "$ns"
```

Findings home under `repos/<repo>/codereviews/<folder>/…` and auto-mirror to
`tasks/<repo>/codereviews/…` (D26/D32). A running `jkb sync --watch` will not see a
just-created mount until it restarts, so the one-shot `jkb sync` is what lands them now.
`mount create` is idempotent, so re-running a review is safe. Pass every option you want
kept — it is the update command too, and an omitted flag no longer resets the stored value,
but restating `--serializer tasks` costs nothing and reads clearly.

## 5. Record the review against the branch

The findings now exist, so point the branch's tasks at them (design D38.4). This is what lets
`jkb task land` require a review instead of trusting that one happened:

```sh
jkb task review record --branch "$branch" --findings "$ns"
```

Pass `--branch` explicitly, using step 1's value. It defaults to the branch checked out where
it runs, which is right in a session and ambiguous anywhere else: run from the main copy that
default is the *staging* branch. `record` does now match a staging branch, via `onto=`, but only
for tasks whose own work is already **in** it — so relying on the default silently reviews a
different set from the one you are looking at. It tags every task carrying that `branch=` with
`reviewed=<sha>` and `review=<ns>`, and moves `in_progress` tasks to `needs_review`. A branch
no task claims — trunk, an ad-hoc range — matches nothing and says so; that is a note, not a
failure, because reviewing an arbitrary range is a legitimate thing to do.

## 6. Report accuracy, then the findings

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

Finally, say whether the branch can now land. You already know — the reader should not have to
discover it at `land` time:

- **no must-fix findings** → "this branch is landable (`jkb task land <uid>`)".
- **must-fix findings** → name them, and say landing is blocked until each is `done` or
  `cancelled`. `jkb staging ls` shows the same count against the task.
