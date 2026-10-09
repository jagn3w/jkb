<!-- generated from jkb design design:git-hooks-installer-18dcdf3d55bff790709c5d, edit there (version 74.SoWY2sbO98sBAYanh7vMooUGAYeqy__8yccBAYmNpY7YooMPAYnS18u48qQPAYuCqpXMlq4BAYyVz97ojckKAYqD6KykgLAJAY6fk4DxzKAHAY-i_drdt6QFAZOT5YW6tu8BAZPKzujyybEBAZO40OTry_4CAZbLnt6Bl5EFAZepnZyCms8PAZa2447vzeYEAZrhtf7N3MsLAZymhamBqLsIAZ_dnYKN01cBoobK8fyDigIBo5vg3Z3K-gYBpNL6lJaAtQwBqNyNkufxowwBqa7Q6YCpqQkBrMq_-far3gYBr6Ci0ceQ4wcBr4blq_-jug0BsNeLivmbvg4Bsrm9j9Ku-QIBs4ymp5uJpAMBspDc5tf6pgYBuJPbpeTkqwcBuYb4qJubnAQBuIyCwrruzgoBu4XW3f-UtgoBvL-A2v3R2QQBu5PrxMK9ggcBwb6E07DGxAEBwrG54Z2D3wsBw9f50aqDtQ_Z3wPGhuvnkvXkAgHI2om55IE-Ac2fq8qZ7vQPAdDb7JqzkLgBAdHlwsuml7YKAdLAudGV2OIIAdLZ6NL144cKAdX077eU3a0FAdeC473Oj5IPAdipyvrF7vEDAdjVgLLb098PAdze0sSmiugGAd2Im6TXoMcEAd7-yPeKqa8CAd-ay-zonLsKAd_fgPnswOQKAd-l48j9qIEJAeHV4tyS7cIKAeCr6PP1j5gIAeTm39-ao8oGAeWuutHUt90FAebF9K7W9N4HAevW85zUoKMDAe64xsOCoOALAfDGkaizlqQKAfHg5PuKkJoLAfK7vbuX-cYIAfPCmKy57bcJAfPsusPG7aoKAfavuN31x-0OAffJvtfjr8ENAfjU9OWoueUGAf3R0bjVi88HAf6pzuHfsLsHAQ, blake3 0f39d35d8f1763899f8700cce4436bd2ce56d16bac5dfd14bfceee708ea5c323) -->
Migrated from: docs/git-hooks-installer.md (lines 1-1084)

# Git hooks installer

`scripts/setup.sh` installs a `post-merge` hook so a `git pull` keeps the `jkb` binary, the
extension, the services and the container kit current, and closes tasks whose branches merged.
Installing one hook sounds trivial. It became the longest-running defect cluster in this repo, at
least twenty-eight review rounds over `install_git_hooks`, the global chainer, `core.hooksPath`,
`.git/info/exclude`, the hook itself, and the scrub that keeps jkb's own git and `gh` calls out of
repositories it was not asked about. Nearly every lesson generalizes, which is why they are kept.

Read this before touching `scripts/lib.sh`, `scripts/setup.sh`, `scripts/hooks/post-merge`,
`scripts/tests/*.test.sh`, or the repository-selection scrub in `gitrepo.rs`, `pr.rs` and
`session.rs`. What `jkb task close-merged` does, and why every `jkb task work` session is a
worktree, belong to the task-lifecycle design. That jkb's own git runs hooks-off after auditing the
repository's config is in the agents-and-roles design; it is the same seam the scrub lives in, and
it does not affect this hook, which is your `git pull`'s hook, not jkb's.

Three rules recur, and most defects below are one of them broken:

- **A claim about a file is asked of the file.** A verdict is derived from the world, never from
  the outcome word of the step that tried to change it.
- **An unknown is not an answer.** A fact git refused to give, or that could not be measured, is a
  third value, never folded into "absent" or "not set", because the caller acts on absence.
- **A rule every call site must remember is the defect.** Put it in one function, one list or one
  structural position, and pin that.

## The post-merge hook

The hook runs unattended after every pull, and nobody reads its output until something is wrong.
Every sentence it prints must be true of the reader's layout, and it never fails the merge.

### `post-merge` runs `setup.sh` on build-affecting pulls, then `close-merged`

`scripts/hooks/post-merge` runs `setup.sh` when the pull touched `crates/`, `ui/`, `scripts/`,
`macos/`, `.container/` or `Cargo.*` (`.container/` so the container kit, which `setup.sh`
refreshes, follows a pull), then runs `jkb task close-merged`. A `core.hooksPath` set globally
*replaces* `.git/hooks`, which leaves a repo hook silently dead, so `setup.sh` also writes a global
**chainer** at the configured hooks path that dispatches to the repository's own hook.

### In the dev container, `setup.sh` rebuilds the binary and stops

With `JKB_REMOTE` set, `setup.sh` rebuilds the binary and exits. Once the container ran the host's
hooks, `post-merge` would run the host installer in there, and nearly everything after step 1
belongs to the machine serving the knowledge base: the scaffold and notification topic use
`ns mk` and `--db`, which remote mode refuses; services have no service manager; the chainer would
be written into the read-only mirror of the host's hooks directory. The rule sits in `setup.sh`,
not `post-merge`, so a hand-run gets the same answer. It is an early exit rather than a fourth
`skipped` state, because every `skipped` line names a flag (`--no-service`) and that would be false
here. Two steps are exceptions: `--link-memory` is honoured, and the exit names the VS Code
extension's container counterpart (`.container/install-extensions.sh`, run from the root-owned
mirror `/usr/local/lib/jkb-container/.container/`).

Pinned by `scripts/tests/container-hooks.test.sh`, which runs a **copy** of `setup.sh` in a scratch
repository with stub `cargo` and `jkb` and every service manager stubbed, and asserts `jkb` was
asked nothing but its version. A copy, because watching the case fail means deleting the guard
under test, and the unconfined version then ran the host installer against the developer's machine.

### Version skew is warned about only when something changed

`setup.sh` checksums the binary before and after `cargo install` and warns about client/daemon skew
only when they differ; it also compares the installed `<git-common-dir>/hooks/post-merge` with the
checkout's, the one host artifact a pull changes without changing the binary. Warning on every run
falsely called the host older after a `scripts/`-only pull, which teaches you to ignore it.

The skew is real both ways. A pull in the container rebuilds the container's `jkb` but not the
host's or its `jkb serve`, and the checkout is shared, so the host's later pull finds nothing to
merge and its hook never fires; the warning says to run `setup.sh` there by hand. A host pull
leaves the container's client at its create-time commit, still unwarned
(`task:jkb-client-and-jkb-serve-have-no-18d662f006f023a8`).

### The hook keeps the environment git hands it

`post-merge` and the emitted chainer keep bare `git`. Wherever `GIT_DIR` is set inside a hook it
names the repository the merge was about, and the hook has no other way to know. Measured on git
2.51.1: an ordinary checkout sets neither variable; a linked worktree sets `GIT_DIR` to
`<repo>/.git/worktrees/<name>`; a leaked `GIT_WORK_TREE` sets `GIT_DIR=<repo>/.git` and
`GIT_WORK_TREE=.`, and there it is the only pointer there is. Scrubbing could not help either:
`--show-toplevel` names the redirected tree with the variables set or unset, since git has already
chdir'd there (History: "Scrubbing `GIT_DIR` inside the hook").

The reason has been stated wrongly twice. It is **not** "in a linked worktree `GIT_DIR` is the only
way to reach the right repository": git chdirs to the worktree top before running a hook, and cwd
discovery answers the same there. Correcting a reason is not correcting a conclusion.

### A redirected checkout is detected, not fought

What a redirection costs is the checkout: `repo_root` may be an unrelated repository's, and running
its `setup.sh` is the harm the repository-selection rule exists to prevent. The hook asks whether
the checkout at `$repo_root` belongs to the repository the merge was about (`--git-common-dir` from
each side, relative answers anchored where they were asked from, compared with `pwd -P`), and stops
with a named reason when it does not. Verified by disabling the guard: an unrelated repository's
`setup.sh` executes.

### The checkout verdict is three-valued, and only `unestablished` may be promoted

The comparison yields `same`, `elsewhere` or `unestablished`, computed once. Measured across ten
layouts, **no legitimate layout lands in `elsewhere`, and no harmful one in `same`**; every
ambiguity the guard has had lives in `unestablished`. So:

> **The acceptance arm may promote `unestablished` to `same`. It must never see, let alone
> rewrite, an established answer.**

`unestablished` arises when the checkout cannot answer for itself: a `core.worktree` checkout has
no `.git` entry, so a scrubbed ask of `$repo_root` finds no repository. The merged repository does
know, since `core.worktree` is it declaring this tree, so a declaration naming `$repo_root`
promotes the verdict. The confinement is structural, and shrinks both failure modes: the worst a
future arm bug can do is accept a tree *no repository owns*, never build another repository's
checkout, and the worst an over-narrow arm can do is an honest, remediable skip.

It came from a design pass asking why three locally-correct, measured fixes each opened the
opposite defect (History: "The `core.worktree` acceptance arm"). The answer was one line,
`root_common="$(common_of "$repo_root")" || root_common=""`, which collapsed a three-valued fact
into two and made the arm an override of the whole comparison. `case10l` step 9 pins the
confinement by **removing** the arm's `GIT_WORK_TREE` predicate and requiring the refusal to
survive: testing the belt, not the braces. Because each fix here opened the opposite error, both
directions are re-measured after any change.

### The acceptance arm reads `core.worktree` only from where git honours it

Git honours `core.worktree` for work-tree resolution from exactly two files, and from nowhere else:

| declaration in | git honours it | bare `git config core.worktree` returns it |
|---|---|---|
| `$GIT_DIR/config` | yes | yes |
| `$GIT_DIR/config.worktree` (extension on locally) | yes | yes (via `--worktree`) |
| `$GIT_DIR/config` via `[include]` | **no** | yes |
| `--global` / `--system` / `GIT_CONFIG_GLOBAL` / `GIT_CONFIG_SYSTEM` | **no** | yes |
| `-c` / `GIT_CONFIG_COUNT` / `GIT_CONFIG_PARAMETERS` | **no** | yes |

Every divergent row is caller-reachable. Measured end-to-end against a prey directory holding a
marker `setup.sh`: `-c core.worktree=`, `GIT_CONFIG_COUNT`/`KEY_0`/`VALUE_0`,
`GIT_CONFIG_PARAMETERS`, a `--global` declaration and `GIT_CONFIG_GLOBAL` **all made the hook run
setup.sh in a directory no repository owns**, five for five. So the arm reads from git's honouring
set, defined as "wherever git would honour it" rather than a curated list, which also covers
channels not yet invented. The scoped read cannot be forged: all six vectors read back empty, and
the only handles that move the local scope (`GIT_DIR`, `GIT_COMMON_DIR`) move `hook_common` with
it. `-c core.worktree` is not a second redirect (git ignores it for resolution; the redirect is the
cwd), only forged testimony, which is why "read what git honours" closes it exactly.

`--worktree` is git 2.20, later than the 2.5 floor, and degrades correctly: against a git shimmed
to refuse it, the local-declaration layout still builds and the `config.worktree` layout skips,
which is what an old git honours too.

### Do not harmonize the `core.worktree` read with `_hooks_path_read`

`_hooks_path_read` walks system/global/local/worktree *with* `--includes`, rightly, because git
honours `core.hooksPath` from every stored scope that way. The shared rule is "read the key from
exactly where git honours it"; the keys differ in where that is, so the reads differ. `case10l`
step 17 pins it against the plausible future cleanup that re-opens the forge.

### `unestablished` gets its own sentence and an executed remedy

Inside `unestablished`, the legitimate bare-dotfiles layout and a leak into a directory no
repository owns are indistinguishable from the repository's records, so neither is built. The
refusal says so and prints a remedy: declare the tree with `core.worktree`.

**A remedy is a claim, so it is executed, not read.** It was first checked by a substring match and
was wrong twice. Step 11b now lifts every `jkb:   ` line out of the refusal, runs it, and requires
the next pull to build that tree. That found: on a bare repository, the exact layout it is printed
for, `git config core.worktree` fails `unable to set up work tree using invalid config` (measured
on 2.51.1; clearing `core.bare` first works, so the hook prints that line too, only when the
repository is bare); and the remedy did not parse with a space in the path, so it is shell-quoted
and the fixture tree is called `dot home`. The canonical alias `git --git-dir=D --work-tree=T` and
`export GIT_WORK_TREE=T` are byte-identical inside the hook (both become `GIT_WORK_TREE=.`,
measured on 2.51.1), so the post-remedy pull keeps the alias, and a flag-form step pins it.

### A remedy is written into the scope the value is honoured from

A remedy that writes the wrong scope is inert, and the refusal reprints for ever. Measured on
2.51.1: a `config.worktree` `core.worktree = /old` resolves to `/old`; `git --git-dir=D config
core.worktree /new` leaves it; `git --git-dir=D config --worktree core.worktree /new` moves it.
`core.bare` has the same hazard. Both keys go through **one** `honoured_read`, which returns the
value and the flag to write it back where it came from, so reading and writing cannot disagree
about scope.

Three follow-on lessons:

- A helper returning **two** facts must not be called in a command substitution: the scope was
  set in a subshell and lost, and every remedy printed the scope-less form. Caught by the step that
  **runs** the remedy.
- `case10l` step 9's mutation lost its anchor when the read moved into the helper; its premise
  check reported that rather than passing silently.
- The helper reads `extensions.worktreeConfig` itself, `local`. It first took it from a
  script-level variable assigned a hundred lines below, so the second caller worked only because
  the first had run: the rule-every-caller-remembers defect, inside the function written to remove
  it. `_hooks_path_read` had already learned this after a stray global `true` sent it into a
  `--worktree` read that exits 128 in any repo with more than one working tree.

### `close-merged` is skipped where the tree cannot describe itself

The hook's two chores are separate flags. `jkb task close-merged` is never *told* which repository
merged; it **discovers** it from its cwd with selection scrubbed (`repo_ctx` → `main_root` →
`rev-parse --show-toplevel`), which needs strictly more than the ask whose failure produced
`unestablished`. Measured against the real binary in the bare-dotfiles layout: `error: not inside a
git repo`, exit 1, then the hook's `close-merged failed (continuing)`, both false of that reader,
and the same in the `core.worktree` layout the arm accepts. So `$skip_close` is set wherever the
scrubbed ask failed, and chore 2 says it is skipping, **where there is a `jkb` to skip**: it asks
`command -v jkb` first, since an explanation printed before the check blamed jkb's discovery on
machines with no jkb. That branch is driven with a PATH of symlinks to the real `git`/`grep`/`bash`
and no `jkb`; both earlier steps had prepended a stub, so it never ran.

`same`-by-declaration runs `setup.sh` and skips chore 2, genuinely two answers. `elsewhere` alone
keeps a blanket `exit`: there discovery succeeds against a different repository, and
`close-merged` would close tasks against the wrong repo key. Two verdicts must not share one `exit`
standing in for two decisions.

### What the `core.worktree` arm does not buy

In that layout `setup.sh` runs, but its hook section reports `error=not a git repo`, because `_git`
scrubs `GIT_DIR` and the tree has no `.git` to discover from. The message is false, and the
installer needs its own answer to "which repository is this?". It is filed rather than bolted on,
because the scrub it would have to relax is the one keeping jkb out of other people's
repositories; jkb's repository discovery cannot see a `core.worktree` checkout at all.

### `git worktree list --porcelain` is not the closed question it looks like

It reads the repository's records and ignores `GIT_WORK_TREE` and `GIT_COMMON_DIR`, so it was the
best candidate for a caller-proof answer. Measured, its main-worktree line names the **git
directory, not the working tree**, for every detached-gitdir layout: `core.worktree`,
`--separate-git-dir`, and submodules. Adopting it would have refused three legitimate layouts as a
"simplification". The latter two had no test until steps 12 and 13: a layout with no case is one
the next round is free to break.

### Hook tests drive a real merge, and cannot shim `git`

A fixture must not decide what environment git produces. The test that passed the broken scrub
built the inverse of git's layout (cwd in the right repo, `GIT_DIR` naming the foreign one), where
scrubbing can only look like a win. It drives a real merge now, in eight layouts: ordinary,
redirected, linked worktree, entered through a symlink, `core.worktree`, declared-vs-redirected,
relative `core.worktree`, and `GIT_COMMON_DIR`.

Git prepends its own `GIT_EXEC_PATH`, which holds a real `git`, to `PATH` before running a hook, so
a stub on `PATH` is never reached (measured: a hook printing `command -v git` answers
`/usr/local/libexec/git-core/git`). To test the hook against a git lacking an option, run the
script directly with the stub on `PATH`. Found by drawing the false conclusion first. Running it
directly also caught `env … command git`, which fails every call (`env` execs a binary; `command`
is a builtin) and exited the hook silently through its `|| exit 0`.

## Where the chainer is installed

### A hook goes in `--git-common-dir`, never `--git-dir`

`lib.sh`'s `git_hooks_dir` uses the common dir. In a linked worktree `--git-dir` is
`<repo>/.git/worktrees/<name>`, which holds no hooks. With every `jkb task work` session in a
worktree, the old rule installed the hook where nothing would run it, printed success, and left the
stale one; the chainer's dispatch line had the same bug. The oracle is
`git rev-parse --git-path hooks/post-merge`, what git itself will execute, and
`scripts/tests/git-hooks.test.sh` asserts against it rather than a hand-written path. `setup.sh`
*refreshes* a chainer it wrote, so one installed under the old rule does not stay broken for ever.

### Every git question in the installer is asked of `$repo_root`, never of the cwd

A bare `git config --get core.hooksPath` answers for whatever repository the caller stands in: run
from another project it wrote jkb's chainer into *that* project, and run from outside jkb it wrote
none, the dead-hook failure under a success message. A relative value belongs to git's base too
(githooks(5): git chdirs to the worktree top), so leaving `mkdir -p` to resolve it put a chainer in
whatever subdirectory you ran from. Both this and the `--git-dir` rule live in `lib.sh`
(`git_hooks_override`), not at the call site.

### A relative `core.hooksPath` with no working tree has no anchor, and is refused

With no working tree, git resolves a relative `core.hooksPath` against the *invoking process's
cwd* (measured from three directories on 2.51.1: `git --git-dir=B rev-parse --git-path
hooks/post-merge` answers `<cwd>/.githooks/post-merge`, and `git hook run` executes that copy).
There is nothing to resolve to, so jkb refuses (code 4, `unanchored`).

An earlier round claimed the git dir as "git's own rule" here, from a measurement taken with the
cwd **set to** the git dir, so jkb installed a chainer there and reported the good verdict while a
pull in a linked worktree ran a path that did not exist. **A measurement whose variable you did not
vary is not a measurement**, and every fixture had the same flaw: repo_root, git dir and cwd were
one directory, so a mutant ignoring git agreed with the oracle everywhere.

### An empty `core.hooksPath` is a dead hook, not "not set"

Git resolves `core.hooksPath=` to `/post-merge` and finds nothing. Folding it into "not set"
reported `dispatch=direct`, the verdict the renderer prints nothing for. It is code 3, and its
remedy asks for `--show-origin`, because `--get` prints one empty line for an empty value and
nothing for an unset one, so the operator's own check appeared to refute the warning.

### Every file `setup.sh` installs is written atomically, through one seam per language

`lib.sh`'s `install_exec` (temp file + `mv`) is the only function permitted to write an executable
destination; `jkb_cli::atomic::write` is the Rust seam, used by `service::install` and
`commands::write_all` (the `~/.claude/{workflows,commands}` assets). The hook runs `setup.sh`,
which installs *that hook*. `cp` rewrites the inode in place, and bash reads a script lazily **by
byte offset**, so a hook replaced mid-run by one of a different length resumes at an offset that
no longer means anything. It cost one fictional error (`post-merge: line 35: i: command not
found`, on a line blank in both versions) and could as easily have skipped `close-merged`. It
fires only on pulls that change the hook's length, exactly the ones nobody watches. `mv` swaps the
directory entry and leaves the running process's inode alone. The bundled assets had the same
loop (pull → hook → setup.sh → new binary reconciling the bundle while `/task-swarm` reads it), so
one rule covers three installers.

A call site is pinned by the destination's **inode**, not its content, which a rename and an
in-place rewrite leave identical: swapping `install_exec` for `cp && chmod 755` at
`install_git_hooks` or the three `install_chainer` arms left all four suites green. `harness.sh`'s
`inode_of` makes it one line; `scripts/tests/install-exec.test.sh` runs in `check.sh` and CI.
`service::install` derived destinations from `$HOME` and could not be tested, so
`install_units(manager, units)` takes them as an argument, as `commands::install_into` already did.

### The chainer is replaced only when jkb wrote every byte of it

`install_chainer` has four outcomes: `installed`, `up-to-date`, `refreshed`, `foreign`. Ownership
is byte equality against a body jkb emits (`chainer_body`, or a frozen `chainer_body_vN` it used
to emit), never a marker in the file. A file jkb did not write cannot satisfy it, so `foreign` is
the safe default and nothing is overwritten to find out. Changing `chainer_body` means moving the
old body to the next `_vN`; forgetting is not silent, since the old chainer is then reported, never
clobbered (History: "Chainer ownership by a marker line").

### The installer and its renderer both live in `lib.sh`

The chainer body, the install arms and `render_git_hooks_report` live in `lib.sh`; setup.sh's hook
section is `render_git_hooks_report < <(install_git_hooks …)`. As a heredoc plus inline arms in
`setup.sh` nothing could execute them: reverting the dispatch to `--git-dir` kept the gate green
while every worktree pull stopped running the repo hook. `scripts/tests/chainer.test.sh` drives all
four outcomes and runs the chainer in a plain checkout and a worktree. Moving only the installer
drew the line one level too low: the renderer's arms were reachable from no test, and two findings
sat there with the gate green. Every renderer `case` has a default arm that **warns**, so a new key
with no arm surfaces instead of vanishing.

### `install_git_hooks` reports states, not actions, and ends in a verdict

While each key named something jkb *did*, every state arising from **not** acting had no key, arm
or test: a stale exclude rule, a failed exclusion, a hook git will never run. So `chainer=` and
`exclude=` carry a state word with evidence, and `dispatch=direct|chained|unknown|dead` is emitted
on every successful run, answering the question the feature exists for. `error=` means *nothing
was done* and is always the sole line (it used to follow `repo-hook=`, printing "repo hook: …"
then "skipping hook install").

### `dispatch=` is derived from the world, and every path question resolves the path first

`dispatch=` is `[ -f ] && [ -x ]` on the chainer, not the outcome word, because `foreign` and
`failed` each cover files that will and will not dispatch (a directory passes `-x` and is
unrunnable). It is three-valued because a foreign chainer may dispatch fine: `unknown`, never
`dead`. "Does `core.hooksPath` point at git's own hooks directory?" compares `pwd -P` results,
since a trailing slash, a symlink and a `..` spell one directory three ways. `git config`'s exit 1
(*not set*) is separated from any other failure (*set to something git will not resolve*, such as
`~someuser/` for an absent account), the verdict `unreadable`; folded into "not set" it reported
`direct` about a repo in which git runs no hooks at all.

### `exposed` is jkb's claim about its own file, so it needs ownership and existence

An absolute `core.hooksPath` inside a *linked* worktree cannot be excluded (an anchored rule
applies to every tree), but the chainer is a real untracked file there, so it is `exposed` and the
renderer warns, naming the tree and that `jkb task land` refuses a dirty target. **Declining to
hide something is not having nothing to hide, and only the second may be silent**: reported as
`none`, that tree read dirty for ever with nothing attributing it to jkb.

Only the caller, which knows the chainer outcome, may make the claim. Derived from the path alone,
it warned on every pull about a file the user wrote and jkb had refused to touch. Gated on `want`,
which collapses to `no` for every pattern-empty line, the state was unreachable the day it was
added, and its test, asserting only absence, passed anyway; the positive half catches it. After an
install that failed before creating anything, `exposed` warned about a dirty tree beside
`dispatch=dead`, about an empty directory. It now needs both ownership and existence.

## Reading `core.hooksPath`

### One read of the repository's own value: `_hooks_path_read`

`_hooks_path_read` is the single read. It prints the repository's own value, the last `--get-all`
entry not in `command` scope; `git_hooks_override` resolves it to a directory and
`git_hooks_exclude_pattern` uses the raw string. `--get` reports the winner, and `-c
core.hooksPath=X`, `GIT_CONFIG_COUNT` or `GIT_CONFIG_PARAMETERS` (which `git pull` exports into
the hook environment) beats everything stored. Measured: jkb installed its chainer at the injected
path, wrote an exclude rule for it, and reported `dispatch=chained` while the repository's own path
kept none, reachable unattended through `git -c core.hooksPath=X pull`. `--show-scope` reports such
a value as `command` (measured on 2.51.1), so git is asked rather than the variables stripped,
with no second model of git's precedence. Nothing stored reads as `direct`; a stored value gets its
chainer refreshed; no warning either way (History: "Refusing an injected `core.hooksPath`").

One reader, because two readers of one fact is how this file fails. The exclude derivation read
the key separately and derived a pattern from the injected value; an empty answer becomes
`want=no`, which sweeps, so **one environment variable retracted the exclude block for the
repository's own chainer**, leaving the tree dirty and `jkb task land` refusing it. Measured.
Fixing only the reader the defect surfaced in would have left the hole one call away.

### Before `--show-scope`, each stored scope is asked by name, and a 129 is counted

`--show-scope` is git 2.26; below it, falling back to `--get` reinstated the injected-value bug
(measured). So the fallback asks each **stored** scope by name (`--system`, `--global`, `--local`,
`--worktree`). An old enough git answers **129** for a scope flag it does not know, which is not
"not set in this scope": absorbed into it, every scope read empty and a stored value got
`dispatch=direct`. A 129 is counted, and all scopes refusing returns "unestablished". Likewise
`--show-scope`'s own 129 is not "cannot expand the value", or every repo on such a git would read
`unreadable` and get no chainer. The fallback is driven by a PATH shim that refuses
`--show-scope`, and the case asserts the shim really refuses first: **a branch no run takes is a
branch not known to work.**

### `--path` expands every value a scope returns, so a 128 re-asks raw

One broken line fails the whole `--path` read, including the split-config recipe `--includes`
exists for (`hooksPath = ~nosuchuser42/hooks` in a shared `~/.gitconfig`, corrected by an
`[include]`). Measured on 2.51.1, `rev-parse --git-path hooks/post-merge` answers the good path
while the scope read exited 128 and jkb reported `unreadable`. So a 128 re-asks the scope **raw**
and hands only its last value back to git for expansion.

A broken *effective* local value fails the raw read too: with `~nosuchuser42/hooks` winning in
`$GIT_DIR/config`, `git config --local --get-all core.hooksPath` exits 128, because git expands the
repository's own value during setup (measured on 2.51.1; `--global` returns it raw). That case lands
on "git will not expand this" one step early, which is right (`rev-parse --git-path` fatals too). A
losing broken local value is unaffected.

### The expansion probe is written by git, not printed into a file

A git config file is not plain text. The first probe used `printf '[core]\n\thooksPath = %s\n'`;
measured on 2.51.1, `/a/b#c` and `/a/b;c` came back **truncated at the comment character**, leading
whitespace was eaten, and `/a/back\slash` exited 128, unexpandable for a value git resolves fine.
`git config --file <f> <key> <value>` quotes on the way in and all six test values round-tripped,
tilde expansion included. **When the question is what git makes of a value, git writes the fixture
too.**

### Six refusal codes, and a probe we could not build is not a verdict about the value

The override read returns 1 unset, 2 the value, 3 empty, 4 unanchored, 5 this GIT, or 6 this
MACHINE. Failures to build the probe (`mktemp`, the `git config --file` write) first reported code
2, so a full disk, a read-only or `noexec` temp mount, or a scoped `TMPDIR` silently restored the
wrong answer: no chainer, and a remedy pointing at `--show-origin`, which prints an ordinary path.
They are code 6.

`broken` is one three-valued variable (0 fine, 1 the value, 2 the probe). As two flags,
`unprobeable` set at a low scope survived a higher scope failing for the ordinary reason, and the
operator was told to free disk space over a broken `~someuser/` path; `case10m` fails on that code
with that message. **Two flags that must be assigned together at five sites are one variable.**

The code-to-verdict mapping is lifted into `_override_verdict`/`_override_why`. Inline in
`install_git_hooks`, `*)` and `4)` behaved identically, so a mutation collapsing them stayed green
and nothing could tell an honest catch-all from one that would absorb a future code into a definite
`unanchored`. The functions can be called with a status that does not exist yet, and the test does.

### Every function in `lib.sh` behaves the same with `set -e` on or off

A reporter says `failed` in words, not in its exit status. The report reached setup.sh only because
the call was written `… || true`, which disables `set -e` for the whole body; without it the
subshell died in `install_chainer` and setup.sh printed the repo hook and nothing else while the
hook was dead. Both `core.hooksPath` readers died too: `x="$(cmd)"` aborts under `set -e`, and exit
1 there is the commonest case, the key being absent. They use `|| rc=$?`. The suites cannot be
sourced under `set -e`, so one case runs the installer in a `bash -euo pipefail` child, after
asserting its premise (a fixture that happened to set `core.hooksPath` would test nothing).

## The `.git/info/exclude` reconcile

A relative `core.hooksPath` puts the chainer inside the working tree as an untracked file. Hiding
it writes a file the user owns, so this section is about ownership, and about never sweeping on an
answer that was not established.

### A hooks path inside the working tree is excluded locally

`reconcile_exclude` writes `.git/info/exclude`. The untracked chainer made every `jkb task work`
session read dirty and `jkb task land` refuse it, and deleting it did not help, since the next pull
recreates it. `.git/info/exclude` is the local, unpushed write the task-lifecycle design already
sanctions for `.jkb/`; editing someone's tracked `.gitignore` is not.

### The exclude rule is reconciled on every run, over every block jkb owns

`reconcile_exclude <repo> <path> yes|no` makes the file agree with the chainer outcome in both
directions, on every run; excluding a `foreign` chainer and refusing to touch it are opposite
answers to one question. The desired state is *exactly the blocks jkb should own now*, and every
other jkb block is retracted, whatever path it names (History: "The exclude rule written once").

### A jkb block is a marked two-line pair, defined positively

jkb writes a **marked two-line block** and retracts only that exact pair, the `chainer_body`
lesson again. A bare pattern may be the user's, so it is reported `unowned` and never deleted: the
harm is visible with a remedy instead of silent. A block is a known marker line immediately
followed by a *pattern-shaped* line (non-empty, not a marker, not a comment). A marker heading
nothing is an **orphan**, removed and reported `tidied`. Without that definition the walk paired a
marker with the marker below it, retracted both and left the real pattern bare, which jkb then
called `unowned` and refused to touch for ever.

`session::ensure_excluded` writes its own block (`# jkb task sessions (git worktrees)` + `/.jkb/`)
from Rust. It survives the shell sweep because its marker is not in `exclude_known_markers`, which
a test pins from the shell side and a comment states at both ends.

### The desired state is a function of facts every worktree shares

`git_hooks_exclude_pattern` derives the pattern from the **raw `core.hooksPath` string**.
`.git/info/exclude` lives in the common dir and applies to every worktree, so a state derived from
`--show-toplevel` differs per run and the sweep turns that into a flip-flop: the main checkout
added the block, a session retracted it, and the main checkout read dirty in between. Git resolves
a relative value against each worktree's own top, so one anchored pattern is right for all; an
absolute one is hidden only inside the **main** checkout, since an anchored rule would hide a
same-named path in every tree. The guard is an equality assertion (same answer from the checkout
and a linked worktree), not a behaviour snapshot. Bare-ness comes from `git worktree list
--porcelain`'s `bare` attribute: `--is-bare-repository` answers `false` from a worktree of a bare
repo, which made the bare git dir the "main checkout".

A fact that disqualifies one branch is asked on that branch. Bare-ness was checked at the top of
the function, but it only means there is no main checkout to anchor an *absolute* path against. A
relative path needs none, so in a bare-repo-plus-worktrees layout jkb **retracted the block it had
just written** and left that worktree dirty. The layout's test drove only the absolute case.

### Three-valued means three values, and two different unknowns need two words

`ours` is `yes|no|unknown`, and `want` has both `undecided` and `unknown`:

- **An answer git refused to give is `undecided`, never `none`, in every arm.** `none` means proven
  absence and becomes `want=no`, so a transient `worktree list` or `config --get` failure swept
  jkb's block away saying "jkb no longer stands behind hiding it". One arm was fixed while its
  sibling's message said "could not be listed" and its answer claimed absence, so the test
  enumerates the arms.
- As a boolean, `ours` counted a **failed** chainer install as proven not-jkb's, and the run
  asserted that a file jkb had written was not jkb's.
- `want=undecided` carried two unknowns: *nothing is decided about this pattern* (sweep the
  others) and *the derivation could not answer* (touch nothing). Shared, they made a failed install
  with a decidably-empty pattern skip the sweep and strand a stale block permanently.

### A verdict that establishes nothing must not also sweep

`unreadable` left `want` at its `no` default, which retracts, so one run printed
`exclude=retracted` beside `dispatch=unreadable` and the next resolvable run put the block back:
the flip-flop through the destructive half. The verdict cannot decide it either, since `unreadable`
has three causes that differ on exactly that question. The branch says `undecided` and lets the
derivation decide: an unexpandable value establishes nothing, an empty one that nothing of ours is
anywhere, and a treeless relative one still yields a pattern right in every worktree.

### The reconcile sits below every arm, and refuses words it does not know

`install_git_hooks`' arms set `want` and a verdict and none returns, so no future arm can skip the
reconcile. The chainer-install-failed arm used to return early, and since its cause persists,
"the next run reconciles it" never came; its honest answer is `undecided`.

`reconcile_exclude`'s `want` had no default arm and fell through to the **sweep**, so a typo, or a
new word added at the caller and not the callee (the edit that introduced `unknown`), would retract
every jkb block and print "nothing was changed". The one consumer able to destroy the user's file
had no default. The guard must also sit where it can see the failure: placed in the callee, it
never ran, because the caller's default turned an unknown word into `pattern=""`, a recognised
`want=no`. Both ends now default to touching nothing.

### One writer for `.git/info/exclude`, whose write status is checked

Append and rewrite both go through `_exclude_write` (temp file and rename). The append was two
`>>` redirections, so a disk filling between them left a stray newline and half a marker. The temp
file's **write status is checked**: `printf` to a full disk fails after partial output, and
ignoring that renamed a truncated file over every rule the user owns, reported `retracted`.

A separator comes first: a file not ending in a newline had its last rule fused with ours
(`*.log/.githooks/post-merge`), destroying the user's rule while ours stayed inert.
`session::ensure_excluded` computes the same `sep`; the shell copy was written without consulting
it. Lines are read the way git reads them (`_exclude_line` trims one trailing CR), or on a CRLF
file jkb recognised neither its block nor the pattern and appended a fresh block every pull. What
is written back is the untrimmed original.

A redirection that fails on a `{ …; }` group is not reported to `if !`, only on a simple command
or function call: as a group the append printed `added` for a refused write. Found by running the
test, not reading it.

### `retracted` is split by what happened to the file, not by why

One word carried four causes and stated a reason false on three; a de-duplicated copy being *kept*
was reported `retracted` and then `kept`. Now `retracted` (jkb no longer stands behind hiding it),
`deduplicated`, and `tidied`.

### Whether the run changed the file is one measured line, below every arm

`reconcile_exclude` wraps the decision, fingerprints the file either side, and emits
`exclude-file=changed|unchanged|unknown` **below every arm**, so no `return 0` can skip it. The
per-pattern arms carry no whole-file sentences, and `changed`/`unchanged` render **nothing**,
since the mutations are itemised already and silence is not a claim.

Three consecutive must-fixes were arms asserting a run-level fact they could not see, each fixed by
rewording. The third showed rewording was the wrong unit of repair: `exclude=undecided` had two
producers with opposite file semantics, and printed *"nothing in .git/info/exclude was changed"*
under *"X dropped from .git/info/exclude"*. The protocol had no scope axis: per-step events and
per-pattern verdicts exist, and *did this run change the file?* is neither. Rejected: a fourth
rewording; a stateful renderer (re-derives the fact from a proxy, output depends on line order);
splitting the word (fixes one word, not the class); folding the fact into each terminal detail
(moves the per-arm burden one seam over).

It is measured (`cksum`), not bookkept, because a write some future arm forgets to record is
exactly this seam's lie. The measurement then had the same defect: `_exclude_fingerprint` folded
*could not measure* into `absent`, and two failures compare equal, so with `cksum` unavailable jkb
reported `unchanged` **over a real write** (measured with a `cksum` exiting 127). `absent` stays an
established answer; a failure emits `unknown`, which the renderer warns about.

### setup.sh's closing summary uses a state word per section

Each section of the summary reports a **state word** (`created|untouched|skipped|failed`, …)
rendered from `lib.sh`. Three consecutive rounds found a false line there (the watcher "running"
after activation failed; five roots asserted after the scaffold failed; reload advised for a build
never made), each invisible to a green gate, because nothing executes setup.sh. A boolean could
not tell "skipped by flag" from "an existing KB left untouched", and arms that create nothing name
the repairing command: "re-run setup.sh" is not a remedy when the failure leaves the db file and
the re-run takes the *untouched* arm.

## The repository-selection scrub

jkb runs inside other people's professional repositories and must not decorate them, the same rule
that keeps it from writing a git ref (task-lifecycle design). Every git or `gh` call jkb makes is
built by one function per language that strips the variables naming a repository or a part of one.

### jkb's own git calls strip the six repository-selection variables

`lib.sh`'s `_git` and Rust's `gitrepo::scrub_repo_selection` strip `GIT_DIR`, `GIT_WORK_TREE`,
`GIT_COMMON_DIR`, `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY` and `GIT_ALTERNATE_OBJECT_DIRECTORIES`,
which outrank `-C`. With `GIT_WORK_TREE` exported (the standard bare-dotfiles recipe),
`install_git_hooks` **created `.githooks/` inside an unrelated repository** and reported
`dispatch=chained` while the repo it was asked about kept a dead hook. Measured. The Rust half has
the larger blast radius: a redirected `repo::main_root` makes `jkb task work` **create a worktree
inside somebody else's repository and rewrite its `.git/info/exclude`**. A review found the claim
stated as covered while only the shell half was.

The components are stripped because, with `GIT_INDEX_FILE=<victim>/.git/index` exported, `jkb task
work`'s `git -C <proj> worktree add` rewrote the victim's index and left its `git status` failing
`fatal: unable to read <sha>`. Measured on 2.51.1 by dumping `env | grep ^GIT_` from real hooks:
`post-merge` is handed no component selector, and `pre-commit`'s `GIT_INDEX_FILE=.git/index` is
relative, meaningless to `git -C <other dir>` (History: "Production held repository components
out").

`GIT_CONFIG_COUNT`/`GIT_CONFIG_PARAMETERS` are deliberately **not** stripped: the dev container
carries `safe.directory` grants in them, and without those git refuses the checkout. Pinned at both
ends, so the list cannot be "tidied" into a blanket sweep. Injected `core.hooksPath` and
`core.worktree` values are handled by asking git which scope a value came from.

### The scrub is ordered, and exactly one file is exempt from the ordering

`dev-scripts.test.sh`'s case6 requires every shell script that runs git to drop all six names
*above its first git call*; below it is `late`, as case1 already ruled for `cargo`. A file passes by
dropping them itself; by routing every call through a wrapper that is itself checked (`_git`'s
definition must carry all six, since crediting the name alone let a one-name `_git` pass); or by a
**`# case6-ambient:`** comment, which exempts the order only, never the six names. It exists for
`scripts/hooks/post-merge` and the set is pinned literally, so a second file acquiring one fails.
The hook earns it because reading the selection git hands it is the file's subject: the ambient ask
learns the tree, `common_of`'s scrubbed ask learns the repository. Before the marker it passed by
accident, credited for `common_of`'s `env -u`, which covered one call out of many.

The `_git` enforcement check derives every `_git -C` line from the file and requires a non-zero
count. It used to revert four *named* call sites; merging the two readers deleted one name, which
surfaced only because the probe asserts its premise (*"the revert did not apply, so nothing was
proven"*).

### Three repository-aware spawns, one rule, pinned at each of them

`gitrepo::scrub_repo_selection` is the rule; `git_cmd`, `gh_cmd` and `gate_cmd` are its callers.
Asking *who else implements this rule* found two non-git spawns resolving a repository from the
environment: `pr::gh` (with a leaked `GIT_WORK_TREE`, `gh` asks GitHub about **an unrelated
repository's pull requests**, and `close-merged` closes tasks on that) and `session::run_gate`,
whose verdict decides a landing. A test of the primitive is not the claim: with the scrub deleted
from `gh_cmd` and `gate_cmd`, a test of `scrub_repo_selection` stayed green. Each site builds its
`Command` in a named function with its own assertion; four mutations, all caught.

### Every spawn in the crate is gated by one allowlist, keyed on (file, function)

`no_spawn_in_the_crate_resolves_a_repository_unscrubbed` walks every `.rs` under the crate
(skipping `target/`) and requires each spawn to sit in a named scrubbing constructor or in
`NOT_REPO_AWARE` with a reason, so a new spawn forces the decision when written. What it learned,
each from something it missed:

- **Test code counts.** Cutting files at `mod tests` missed `gitrepo.rs`'s four fixtures, which
  with a leaked `GIT_WORK_TREE` commit into the other checkout, create branches `deep/er` and
  `mergecommit`, and move its HEAD (measured, and re-measured against the fix).
- **Exempt by location, never by name or line shape.** Keyed on the name `git_cmd`, a copied idiom
  was exempt on arrival; an earlier guard exempted any `let mut cmd = Command::new` line, the
  idiom all three constructors share. It asks which function encloses each spawn.
- **Every spawn is classified**, so "not repository-aware" is a decision, not an omission.
- **An unrecognized declaration is not the previous function.** Missing `pub(super) fn`, an
  unscrubbed `gh` spawn written after `gh_cmd` inherited its exemption (measured).
- **An exemption's named test must exist.** `src/archive.rs` cited a test nowhere in the crate; the
  comment is machine-checked now.
- **Scan code, not text** (`code_only` blanks comments and literals), since rustfmt split a
  `Command::new(` across lines.
- **"Spawn" has more than one spelling.** `Command::cargo_bin("jkb")` hid three fixtures: deleting
  `Fixture::jkb`'s isolation left everything green. The forms are a list, `SPAWN_FORMS`.
- **Assert identity, not arity.** `scrubbers.len() >= 6` on a const array of six could not fire.
  The `(file, fn)` pairs the scan reads must equal those the compiler saw, and it must find the one
  legitimate spawn, so an empty result cannot mean a broken walk.

The allowlist shrinks where it can: an exemption keyed on an enclosing function covers spawns not
yet written in it, so `help_advertises_the_mcp_subcommand` was routed through the file's fixture
(one line) instead.

### A test fixture's isolation is wider than production's scrub, and contains it

Fixtures drop `MUST_DROP` (`REPO_SELECTION_VARS` plus `GIT_TEMPLATE_DIR` and the two
`GIT_CONFIG_*` channels) and set `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM`. Production keeps the
config channels for `safe.directory` and never runs `git init`; a fixture must inherit neither. Two
rules, not drift. Env-injected config outranks the files, so `GIT_CONFIG_GLOBAL=/dev/null` alone is
not isolation: an exported `commit.gpgsign` failed `./scripts/check.sh` with `gpg: signing failed`.
The selection half is worse: `tests/sessions.rs` once scrubbed config but not selection, so with
`GIT_WORK_TREE` exported, `git -C <tmpdir> init` re-inits the other repo and the following commit
lands there. That is `check.sh`, the gate `jkb task land` and the merge queue trust, writing to a
repo the developer merely has configured.

The helper is `tests/common/mod.rs`, shared by every integration crate and compiled into the bin
crate's tests through `#[cfg(test)] #[path = "../tests/common/mod.rs"]`: `jkb-cli` is bin-only, so
tests cannot import `src/`, but they can share a file. One source text, three compilations. The
config half had been pinned by nothing (deleting the `GIT_CONFIG_GLOBAL`/`SYSTEM` lines left 137
integration tests green), so it is a list applied by one function and checked by an oracle.

The relation is **containment**: `MUST_DROP` ⊇ `REPO_SELECTION_VARS`, since a fixture inheriting
what production refuses writes into somebody's repository.
`the_fixtures_drop_everything_production_selects` asserts it from the compiled constants. It exists
because removing the fixtures' own `scrub_repo_selection` call removed the only coupling, and a
fourth production selector then left 118 tests green while fixtures kept it. The shell harness's
`isolate_git` had missed `GIT_CONFIG_PARAMETERS` and `GIT_COMMON_DIR` the same way; its list is
defined by a class, not a count, after the prose's "all five" went stale.

### Production iterates its list; the test holds its own literal; the comparison is equality

**Production is one artifact** (the list, iterated by the applier), **the test holds its own
literal**, and they are compared for **equality**. Iterating closes "the function does less than
the list"; the literal closes "the list shrinks and takes the guard with it"; equality closes "the
list grows". The literal lives beside the text naming the harm, and its failure message says *not*
to fix it by editing the list, because "the lists disagree" reads as an instruction to make the
harmful edit.

Two rounds erred in opposite directions. `isolate_git_env` restated the eleven names, so adding an
`.env_remove` left all four guards green; the fix made the appliers iterate the lists, which gave
the rule and its only guard one source: deleting `"GIT_WORK_TREE"` from both lists left 260 tests
passing while every `git` and `gh` spawn inherited it. Over-scrubbing costs nothing; an unscrubbed
selector points the tool at somebody else's repository. `pr.rs` had been right all along.

## Test-harness and gate lessons

From the installer's suites (`scripts/tests/harness.sh` and the `*.test.sh` beside it) and the gate
that runs them; each is a general rule about tests that pass while proving nothing.

### The runner refuses a case name that is not a function, and sees every name

`finish` asks only whether the file asserted anything, so two deleted case bodies cost nothing:
bash printed `case6g: command not found` and the suite exited 0, losing the cross-worktree and CRLF
pins with the gate green. `run_cases` checks `declare -F` first. It also derives orphaned cases,
and its `case[0-9][0-9a-z]*` missed `case_isolate`, so deleting that from a runner dropped a BSD-sed
pin silently; the pattern admits `_` now.

### A closed vocabulary is derived from its renderer, never from a hand-written table

`case14` compared the protocol to the render arms using a hand-written table, so a new state with
no row was undriven; code 6 `unprobeable` got no render arm at all and the case passed. Both sides
are derived now (`_override_statuses` from `_override_verdict`'s arms; render states from the
renderer's nested `case` arms), which found two uncovered arms on the first run. A verdict
rendering nothing fails as loudly as one hitting the catch-all, since `dispatch=direct` is the
silent verdict. The silence list is one array used by both assertions.

### An assertion satisfied by an absence is satisfied by everything

`case10h`'s last check failed on `dispatch=transient`, which the same commit deleted from every
producer, so its `*)` arm reported `ok` while the old-git path installed the chainer at an injected
value. The fallback was fixed, then asserted positively: the chainer lands at the repository's
`.githooks`, and nothing exists at the injected path.

### A mutation must land where it claims, and two routes need two assertions

A mutation replacing the first occurrence of each name in `isolate_git` hit the header comment and
reported MISSED for sound guards. Target the line, then assert the mutation applied. And
`case_isolate`'s one git-level check was satisfied by the `GIT_CONFIG_COUNT` unset above it, so it
could not see whether the `KEY_<n>` sweep happened. It is one assertion per injection route
(`count`, `PARAMETERS`); dropping `isolate_git`'s `HOME` redirect, a file leak no variable check
sees, fails both.

### `\|` in a BRE is a GNU extension

`isolate_git`'s sweep of `GIT_CONFIG_KEY_<n>` matched nothing under `sed --posix`, that is on
macOS, where this project is developed, and no case covered it. It is a `case` glob now, with a
harness case asserting git really sees no injected value.

### The shell-syntax gate is one function, selecting by shebang

`shell_sources` + `check_shell_syntax` live in `lib.sh`, called by `check.sh` and CI, because the
two hand-written file lists drifted by a `*.md` skip within a commit (green locally, red in CI). It
selects by **shebang** (`post-merge` has no `.sh`), and **finding no files is a failure**: an
unmatched glob had let a broken gate print "All checks passed".

### A pipe into a quiet `grep` is refused wherever `pipefail` is set

`grep -q` exits at its first match, a producer with more to write dies on EPIPE, and
`set -o pipefail` reports that, so a FOUND match reads as failure. Two instances, six days apart:
CI said "run.sh no longer runs verify.sh" about a `.container/run.sh` that did, and `post-merge`
said "no build-affecting changes pulled" about a pull that changed every crate. One rule in
`scripts/tests/dev-scripts.test.sh` covers `shell_sources` (41 files across all five script
directories) with one message (History: "Two half-guards for the quiet-`grep` race").

Measured, bash 5.2.21 on Linux, 30 trials per cell, match on the first line:

|             | 4 KB | 8 KB | 16 KB | 32 KB | 64 KB | 82 KB |
|---|---|---|---|---|---|---|
| `pipefail`    | 0/30 | 0/30 | 0/30 | 28/30 | 30/30 | 30/30 |
| no `pipefail` | 0/30 | 0/30 | 0/30 |  0/30 |  0/30 |  0/30 |

Each conclusion had first been asserted the other way here. It is **probabilistic**, a band not a
threshold ("800 files pass, 900 fail" was one sample; "our producer is small" describes today's
input). It is governed by the **64 KiB pipe buffer**, whether the producer must block, not write
sizes (`printf` writes 120-290 byte pieces). And **`pipefail` is the entire hazard**: without it
`grep -q`'s own status is reported, which refutes exempting `printf "$var" | grep -q` as "a single
write"; `post-merge` was that shape.

So the rule is conditioned, not blanket, and self-maintaining: the permitted sites are
`.claude/hooks` scripts that set no shell options, and the day one gains `pipefail` the case names
the line. A file is in scope if it sets `pipefail` itself (an anchored match, not prose), has no
shebang, or is sourced by name from a file in scope; the last clause exists because
`.container/lib.sh` and `egress-lib.sh` carry shebangs for `--self-test`. Membership names all four
libraries (`scripts/lib.sh`, `scripts/tests/harness.sh`, `.container/lib.sh`,
`.container/egress-lib.sh`), since a coverage floor is cleared by three dozen self-declaring files.
`egress-lib.sh` enters through the first clause by accident (a `set -euo pipefail` inside a quoted
`bash -c` body), over-inclusion and safe. That sentence was wrong twice, each time checked with the
detector under discussion; disabling the clause and re-running names exactly one file, and that is
the check to repeat.

The fix is a here-string. `<<<` is a pipe up to 65536 bytes and a temp file above, safe either way:
the shell finishes the write before the consumer runs, and one command leaves `pipefail` no second
status ("a here-string is a temp file" was a third wrong claim).

### `head -N` is the same race, and is deliberately not machine-checked

Of 25 `head -N` sites, 23 are safe (status discarded in a command substitution, `|| true`, or a
bounded producer), so a rule would be about 2/25 precise, the kind of guard that gets deleted. The
two real ones were fixed by hand: `scripts/swarm-status.sh`'s `find`/`sort` pair (20/20 aborts at
3000 lines, 0/20 with `sed -n 1p`) and `.container/check-drift.sh`'s stderr excerpt. "First line"
is spelled `sed -n 1p` by convention. Also found: `check-drift.sh`'s `diff -u … | head -60` runs
only when files differ, `diff` then exits 1, and under `set -euo pipefail` the first drifting
artifact ended the run, skipping every later generator (measured).

## History

Superseded and reversed decisions, with what replaced them and why. Most fixes here were locally
correct and globally wrong; the sections above are what survived.

### Chainer ownership by a marker line

Ownership first grepped for the chainer's own comment line, a proxy for the claim: a user who
added a line to jkb's chainer still matched, and the refresh arm replaced their file unattended
with no backup. Replaced by byte equality against `chainer_body` and its frozen predecessors.

### The exclude rule written once

The block was written on the run that installed a chainer and never revisited, then reconciled only
for the installed path. A replaced chainer stayed git-ignored for ever, a moved `core.hooksPath`
left the old block, and unsetting it skipped reconciliation. Replaced by reconciling exactly the
blocks jkb should own on every run. The pattern itself was first a resolved path with the toplevel
stripped, different per worktree; replaced by the raw `core.hooksPath` string.

### Refusing an injected `core.hooksPath`

The first fix refused an injected value as `dispatch=transient`, worse than the bug: a healthy repo
pulled with `-c` got three warnings advising it to store a value it stored, and a repo storing none
was advised to set one, which kills `.git/hooks` dispatch. Replaced by `--get-all` skipping
`command` scope; the verdict, its `why`, render arm and refusal code were deleted. The pre-2.26
fallback was then described as using flags that "predate `--show-scope` by a decade"; `--worktree`
does not, and an old git's 129 read as "not set" until it was counted.

### The version-skew "always matched" claim

A note said that before the container ran `post-merge`, "neither side rebuilt, so they always
matched". A review showed the host side always rebuilt; both skew directions are why the check
exists.

### The `core.worktree` acceptance arm

Five versions, each measured, each opening the opposite error:

| Round | The arm said | What it cost |
|---|---|---|
| 19 | declared and `-d`, resolved against the hook's cwd | a documented **relative** declaration was refused; git anchors it on the git dir |
| 20 | declared at all (comparison dropped) | `GIT_COMMON_DIR` makes git **ignore** `core.worktree` while `git config` reports it: a leak **built another checkout** |
| 21 | declared, resolved against `--git-dir`, equal to `$repo_root`, `GIT_WORK_TREE` unset | held both ways |
| 22 | **confined** to `unestablished` | the oscillation became impossible instead of patched |
| 23 | declared **in a file git honours**, equal to `$repo_root`; `GIT_WORK_TREE` condition deleted | the bare read had believed the caller (five forgery vectors) |

Round 22 is the one worth reading: the arm had been an assignment to `root_common`, an override
able to rewrite any verdict, so every fix chose which side of one line to widen. Round 23 deleted a
condition defended as load-bearing ("when the caller has overridden it the declaration says nothing
about the tree"). Measured false: every harmful case lands in `elsewhere`, out of the arm's reach,
and the condition made the remedy inert for every bare-dotfiles user, whose alias puts
`GIT_WORK_TREE=.` in the hook's environment. The test had agreed with the hook, dropping
`GIT_WORK_TREE` for the post-remedy pull on the strength of that paragraph.

### One `exit` for both chores, then `close-merged` on `unestablished`

The hook first ended every non-`same` verdict with one `exit`. Round 23 kept `close-merged`
running on `unestablished`, arguing it "needs only the repository the merge was ABOUT, which
`$hook_common` named". Reversed: `close-merged` discovers its repository from its cwd, which fails
in both affected layouts (measured against the real binary). The two flags stayed.

### Scrubbing `GIT_DIR` inside the hook

A round scrubbed selection inside `post-merge` after correcting the exemption's stated reason.
Measured end to end, one fixture: `ORIG_HEAD` stopped resolving, the `HEAD^..HEAD` fallback
answered about the wrong repository, and a pull touching `crates/` printed "no build-affecting
changes pulled", the failure the scrub was meant to prevent. Reverted; the passing test had built
the inverse of git's layout.

### Production held repository components out

`GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY` and `GIT_ALTERNATE_OBJECT_DIRECTORIES` were left out of
production on an unmeasured comment ("git exports `GIT_INDEX_FILE` to hook processes"), while an
exported `GIT_INDEX_FILE` let `jkb task work` corrupt another repository's index. The scrub grew
from three names to six; the headline said three for four rounds after the detail said six.

### Two isolation lists, one per side of the crate boundary

The fixture isolation lived first in `tests/sessions.rs` alone (so `tests/cli.rs` had none), then as
two lists, one per side of the crate boundary (`FIXTURE_CONFIG` was `#[cfg(test)]`, invisible to
integration tests), with a parity test parsing both from source. Replaced by the `#[path]` bridge:
the boundary forbade importing, not sharing a file. The parity test compared text, not
environments, and its message read as "sync the lists", measured as the edit that reopens the hole.

### Two half-guards for the quiet-`grep` race

The `run.sh` instance was fixed by a scan local to `.container/*.sh` matching one spelling, which
could not find the `post-merge` instance: the bug in the gap between two half-guards. The
repo-wide scope was wrong twice first: the `pipefail` clause `\<set\>[^#]*pipefail` matched a
comment (the only reason `.container/lib.sh` was in scope), and the sourced-library clause parsed
`. "$(dirname "$0")/harness.sh"` as the file `$(dirname`, leaving `harness.sh` out. Anchoring the
first clause without the third would have dropped `.container/lib.sh` entirely.
