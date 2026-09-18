# The setup.sh git-hooks installer — the longest-running defect cluster in this repo

`install_git_hooks`, the global chainer, `core.hooksPath`, and `.git/info/exclude`.
This grew out of D34's post-merge hook and became its own subject: seventeen review
rounds, and nearly every lesson generalizes. **Read this before touching
`scripts/lib.sh`, `scripts/setup.sh` or `scripts/hooks/post-merge`.**

Part of the jkb documentation set; see [CLAUDE.md](../CLAUDE.md) for the
conventions every session is expected to know.

- **`scripts/hooks/post-merge`** (installed by `setup.sh`) runs `setup.sh` when the pull
  touched `crates/`/`ui/`/`scripts/`/`Cargo.*`, then `jkb task close-merged`. It never fails
  the merge. **Install wrinkle:** `core.hooksPath` set globally *replaces* `.git/hooks`, so
  `setup.sh` also writes a global chainer — without it the repo hook is silently dead.
- **In the dev container, `setup.sh` rebuilds the binary and stops** (`JKB_REMOTE` set). Once the
  container ran the host's hooks (`.container/README.md`, "Git runs the host's hooks"), `post-merge`
  would run this host installer in there after every pull that touches code. Nearly everything
  after step 1 belongs to the machine serving the knowledge base, and fails or misleads in the
  container (the two exceptions are below). The
  scaffold and the notification topic go through `ns mk` and `--db`, which remote mode refuses.
  Services have no service manager. The hooks install would try to write the chainer into the
  read-only mirror of the host's hooks directory. The rule sits in `setup.sh` rather than in
  `post-merge`, so a hand-run in the container gets the same answer. It is an early exit, not a
  fourth `skipped` state, because every `skipped` line in the summary names a flag
  (`--no-service`), and that would be false here. Two steps are exceptions: `--link-memory` is valid in the container and
  is honoured, and the VS Code extension has a container counterpart (the explorer
  `.container/install-extensions.sh` builds), which the exit names instead of calling it the host's. Pinned by
  `scripts/tests/container-hooks.test.sh`, which runs `setup.sh` with stub `cargo` and `jkb`, and
  asserts `jkb` was asked nothing but its version. That case runs a **copy** of `setup.sh` in a
  scratch repository, with every service manager stubbed. Watching it fail means deleting the very
  guard under test, and the unconfined version then ran the host installer against the developer's
  machine.
  - **It warns about version skew when the binary changed.** Every run was the first version, and a
    review pointed out that a pull touching only `scripts/` rebuilds byte-identical `jkb` and then
    falsely calls the host older, which teaches you to ignore it. `setup.sh` checksums the binary
    before and after `cargo install`, and says it only when they differ. A pull in the container rebuilds the container's
    `jkb` but not the host's, and not the host's `jkb serve`, and nothing checks client/daemon
    versions. The checkout is shared, so a later `git pull` on the host finds nothing to merge, and
    its `post-merge` never fires: the warning says to run `setup.sh` there by hand. The opposite
    skew is older than this change and still unwarned: a pull on the host rebuilds the host and
    leaves the container's client at its create-time commit. **Correction:** an earlier version of
    this bullet said that, before the container ran `post-merge`, "neither side rebuilt, so they
    always matched". That was false, and a review caught it: the host side always rebuilt. Both
    directions are what the version check is for
    (`task:jkb-client-and-jkb-serve-have-no-18d662f006f023a8`).
- **A hook goes in `--git-common-dir`, never `--git-dir`** (`scripts/lib.sh`'s `git_hooks_dir`).
  In a linked worktree the latter is `<repo>/.git/worktrees/<name>`, which holds no hooks — git
  resolves `hooks/` against the common dir. Since D36 puts *every* `jkb task work` session in a
  worktree, the old rule meant a `setup.sh` run from a session installed the hook where nothing
  would ever run it, printed success, and left the stale one in place; the chainer's dispatch
  line had the same bug, so a pull in a worktree found no repo hook either way. The oracle is
  `git rev-parse --git-path hooks/post-merge` — what git itself will execute — and
  `scripts/tests/git-hooks.test.sh` asserts against it rather than a hand-written path.
  Because `setup.sh` never clobbers a chainer it did not write, it now *refreshes* one it did:
  a chainer installed under the old rule would otherwise stay broken for ever.
- **Every git question in the installer is asked of `$repo_root`, never of the cwd**
  (`git_hooks_override`). A bare `git config --get core.hooksPath` answers for whatever
  repository the caller is standing in: run from another project it wrote jkb's chainer into
  *that* project, and run from outside jkb while jkb sets the key it wrote no chainer at all —
  the dead-repo-hook failure the chainer exists to prevent, under a success message. A relative
  value belongs to git's base too (githooks(5): git chdirs to the worktree top before running a
  hook), so leaving `mkdir -p` to resolve it put the chainer one directory per subdirectory you
  happened to run from. Same shape as the `--git-dir` bug one bullet up, which is why both rules
  now live in `lib.sh` rather than at the call site.
- **Every file `setup.sh` installs is written atomically** — `scripts/lib.sh`'s `install_exec`
  (temp file + `mv`), and `jkb_cli::atomic::write` for `jkb service install`'s units. The loop above
  is why: the hook runs `setup.sh`, and `setup.sh` installs *that hook*. `cp` rewrites the
  target inode in place, and bash reads a script lazily **by byte offset** — so a hook replaced
  mid-run by one of a different length resumes at an offset that no longer means what it meant
  and executes whatever fragment it lands on. It cost one fictional error (`post-merge: line
  35: i: command not found`, on a line blank in both versions) and could as easily have skipped
  `jkb task close-merged` silently. It fires only on pulls that change the hook's **length** —
  i.e. exactly the pulls that update the hook, when nobody is watching. `mv` is a rename: it
  swaps the directory entry and leaves the running process's inode alone. Pinned by
  `scripts/tests/install-exec.test.sh`, which `check.sh` and CI both run.
- **The chainer is replaced only when jkb wrote every byte of it** (`lib.sh`'s
  `install_chainer`, four outcomes: `installed`/`up-to-date`/`refreshed`/`foreign`). Ownership
  is byte equality against a body jkb actually emits — `chainer_body`, or a frozen `_vN` of a
  body it used to emit — never a marker inside the file. The first attempt grepped for the
  chainer's own comment line, which is a *proxy* for the claim rather than the claim: a user
  who adds a line to jkb's chainer still matches it, so the refresh arm replaced their file
  unattended on an ordinary `git pull`, with no backup and the same message as the intended
  upgrade. Byte equality cannot be satisfied by a file jkb did not write, so `foreign` is the
  safe default and nothing is overwritten to find out. Changing `chainer_body` means moving the
  outgoing body to the next `chainer_body_vN`; forgetting is not silent — the old chainer stops
  being recognised and is reported, never clobbered.
- **The body and the install arms live in `lib.sh`, not inline in `setup.sh`.** While they were
  a heredoc plus three inline arms nothing could execute them: reverting the chainer's dispatch
  to `--git-dir` left the whole gate green while every pull inside a worktree silently stopped
  running the repo hook — and `check.sh` and `ci.yml` both justify the shell-test stage on
  exactly the claim that these installs are unreachable from a Rust test. They were unreachable
  from everything. `scripts/tests/chainer.test.sh` drives all four outcomes and runs the
  installed chainer in both a plain checkout and a worktree.
- **A hooks path inside the working tree is excluded locally** (`reconcile_exclude`). A
  relative `core.hooksPath` resolves inside the tree, so the untracked chainer made every
  `jkb task work` session read dirty and `jkb task land` refuse it — and deleting it did not
  help, since the next pull recreates it. `.git/info/exclude` is the local, unpushed write D36
  already sanctions for `.jkb/`; editing someone's tracked `.gitignore` is not.
- **The exclude rule is reconciled on every run — every block of ours, not just the one for the
  path being installed — and ownership is byte identity again.** It was
  written once, on the run that installed a chainer, and never revisited — so a user who later
  replaced that chainer with their own hook had it git-ignored **for ever**: the next run said
  `chainer=foreign`, left the rule in place, and `git status` went silent with nothing
  attributing it to jkb. `reconcile_exclude <repo> <path> yes|no` now makes the file agree with
  the chainer outcome in both directions, which is one rule rather than two, because excluding
  a `foreign` chainer and refusing to touch it are opposite positions on the same question.
  jkb writes a **marked two-line block** and retracts only that exact adjacent pair — the
  `chainer_body` lesson applied a second time. A bare pattern may be a rule the user wrote, so
  it is reported (`unowned`) and never deleted: the residual harm becomes visible with a
  remedy instead of silent and permanent. Reconciling only the *installed* path was not
  reconciliation either — moving `core.hooksPath` left the old block for ever, and unsetting it
  returned before any reconciliation ran at all — so the desired state is *exactly the blocks
  jkb should own right now*, every other one is retracted whatever path it names. The file is
  rewritten through a temp file whose **write status is checked**: `printf` to a full disk fails
  after emitting part of its output, and dropping that status renamed a truncated file over
  every rule the user owns under the word `retracted`.
- **The desired state is a pure function of facts every worktree shares, never of the worktree
  that happens to be running** (`git_hooks_exclude_pattern`). `.git/info/exclude` lives in the
  common dir and applies to every worktree at once, so a desired state derived from
  `--show-toplevel` differs per run — and a sweep that *enforces* one turns that disagreement
  into a flip-flop: a main-checkout run added the block, the next run from a `jkb task work`
  session retracted it, and the main checkout read dirty in between, which is the state
  `jkb task land` refuses. D36 makes that the normal case, not a corner one. So the pattern
  comes from the **raw `core.hooksPath` string**, not from resolving a path and stripping a
  toplevel back off it: a relative value is resolved by git against each worktree's own top, so
  one anchored pattern is simultaneously right for all of them; an absolute one is hidden only
  when it is inside the **main** checkout, because an anchored rule would otherwise hide a
  same-named path in every other tree. The regression guard is an equality assertion — the same
  answer from the checkout and from a linked worktree — not a behaviour snapshot. Bare-ness is
  read from the porcelain's own `bare` attribute, not from `--is-bare-repository` of the
  directory this run was handed: in a bare-repo-plus-worktrees layout that answers `false` from
  the worktree, and the bare git dir then became the "main checkout".
- **A fact that disqualifies one branch is asked on that branch.** Bare-ness was asked at the
  top of `git_hooks_exclude_pattern` and returned for the whole function — but it means only
  "there is no main checkout to anchor an ABSOLUTE path against". A *relative* `core.hooksPath`
  needs no main checkout at all: it resolves inside each linked worktree, so in a
  bare-repo-plus-worktrees layout the top-level gate dropped a working exclusion and made jkb
  **retract the block it had written itself**, leaving the worktree it had just written the
  chainer into reading dirty. The test for that layout drove only the absolute case, which is
  why it stayed green.
- **Declining to hide something is not the same as having nothing to hide, and only one of them
  is silent.** An absolute `core.hooksPath` inside a *linked* worktree cannot be excluded — an
  anchored rule applies to every tree at once — but the chainer really is an untracked file in a
  real tree, so it is `exposed` and the renderer warns, naming the tree and that `jkb task land`
  refuses a dirty target. Reported as `none`, which renders nothing, that tree read dirty for
  ever with nothing attributing the file to jkb: the very failure the exclusion exists to
  prevent, reached by the mechanism meant to prevent it. It is also a claim about **jkb's own**
  file, so only the caller — which knows the chainer outcome — may make it: derived from the
  path alone it warned, on every unattended pull, that jkb's chainer was dirtying a tree, about
  a file the user wrote and jkb had refused to touch three lines earlier.
- **setup.sh's closing summary is rendered from lib.sh, like the hook report before it.** It
  produced a finding in three consecutive review rounds — the watcher line claiming "running"
  after activation had failed and said so, the roots line asserting five roots the scaffold had
  just failed to create, the extension line telling you to reload for a build never made — and
  every one was invisible to a green gate, because nothing executes setup.sh. Each section now
  reports a **state word** (`created|untouched|skipped|failed`, …) rather than a boolean: a
  boolean could not tell "skipped by flag" from "an existing KB was left untouched", and the
  two arms that create nothing now name the command that repairs it instead of asserting the
  roots — "re-run setup.sh" was not a remedy, since the failure leaves the db file behind and
  the re-run takes the *left untouched* arm.
- **The test runner refuses a case name that is not a function.** `finish` asks only whether
  the FILE asserted anything, so with a hundred passing assertions beside them two deleted case
  bodies cost nothing: bash printed `case6g: command not found` on stderr and the suite exited
  0. Two regression pins — the cross-worktree agreement set and the whole CRLF exclude set, both
  for bugs shipped once already — went that way with the gate green. `run_cases` checks
  `declare -F` first, which is the harness's own stated failure mode closed one level down.
- **An answer git refused to give is `undecided`, never `none` — in EVERY arm that means it.**
  `none` is *proven absence* and the caller turns it into `want=no`, so a transient
  `worktree list` or `config --get` failure swept jkb's own block away and reported "jkb no
  longer stands behind hiding it": false, jkb could not check. Both run unattended from the
  post-merge hook. Fixing one arm and leaving its sibling — whose message already said "could
  not be listed" while its answer claimed proven absence — is the shape this area keeps
  producing, and is why the test enumerates the arms rather than checking one.
- **Three-valued means three values, and two different unknowns need two words.** This one
  function collapsed a `Fact` three rounds running. `ours` was a boolean, so a chainer install
  that FAILED counted as proven not-jkb's — the run asserted "the file at that path is not one
  jkb wrote" about a file jkb had written, and dropped the dirty-worktree warning with it,
  three lines below a comment saying that file may well be one jkb wrote. And `want=undecided`
  carried two unrelated unknowns: *nothing is decided about this pattern* (sweep the others)
  and *the derivation could not answer* (touch nothing). Sharing the word made a failed chainer
  install with a decidably-empty pattern skip the sweep and strand a stale block permanently.
  They are `undecided` and `unknown` now, and `ours` is `yes|no|unknown`.
- **A claim about a file is asked of the file.** `dispatch=` had that rule written down —
  derived from the world, not from the outcome word — and its sibling claim did not use it. An
  install that fails before creating anything leaves the path EMPTY, and `exposed` then said
  "the chainer there is not hidden … that working tree will read dirty" beside `dispatch=dead`
  ("nothing runnable is at …"), about an empty directory in a clean tree: two contradictory
  statements in one report. `exposed` now needs both ownership and existence.
- **jkb's own git calls strip every variable naming a repository or a part of one** — six since
  round 28: `GIT_DIR`/`GIT_WORK_TREE`/`GIT_COMMON_DIR` and
  `GIT_INDEX_FILE`/`GIT_OBJECT_DIRECTORY`/`GIT_ALTERNATE_OBJECT_DIRECTORIES` (`_git`, and
  `gitrepo::scrub_repo_selection`). The headline said three for four rounds after the nested
  bullet below it said six. Those
  outrank `-C`, so with `GIT_WORK_TREE` exported — the standard bare-dotfiles shell recipe —
  `--show-toplevel` answered somebody else's tree and `install_git_hooks` **created `.githooks/`
  inside that unrelated repository**, reporting `dispatch=chained`, while the repo it was asked
  about kept a dead hook. Measured. jkb runs inside other people's professional repositories and
  must not decorate them — the same rule that keeps it from writing a git ref (D46) — and this
  is a wrapper rather than a note at each of the six call sites.
  - **Both halves, because the Rust one has the larger blast radius.** `gitrepo::git_cmd` is the
    same seam for the CLI: the shell installer decorates a repo with a `.githooks/` directory,
    but `repo::main_root` feeding a redirected answer to `jkb task work` **creates a git worktree
    inside somebody else's repository and rewrites its `.git/info/exclude`**. Every production
    spawn in the crate is built there; a review found the claim stated as covered while only the
    shell half was.
  - **Everything that names a repository or a part of one — six, not three since round 28.** `GIT_CONFIG_COUNT`/`GIT_CONFIG_PARAMETERS`
    inject configuration and are deliberately left alone — this project's own dev container
    carries `safe.directory` grants in them, and stripping those makes git refuse the checkout
    outright. Pinned at both ends, so the list cannot be "tidied" into a blanket sweep.
  - **The rule is ORDERED, and exactly one file is exempt from the ordering.**
    `dev-scripts.test.sh`'s case6 requires every shell script that runs git to have dropped all
    six names *above its first git call* — a scrub below the first call is `late`, which is the
    same correction case1 had already made for `cargo` ("a source line BELOW the first invocation
    reads as compliant and is not"). A file satisfies it three ways: it drops the names itself
    above the call; it routes every call through a wrapper that is ITSELF checked (`lib.sh`'s
    `_git`, whose definition is read and required to carry all six — granting `wrapper` on the
    name alone let a `_git` stripped to one name pass everything); or it carries a
    **`# case6-ambient:`** comment.
    That marker exempts a file from the ORDER and from nothing else — the six names must still
    all be dropped somewhere in it. It exists for `scripts/hooks/post-merge` and the set is
    pinned literally in the suite, so a second file acquiring one fails the gate rather than
    incrementing a number in a message. The hook earns it because git invokes it with the
    selection already set and reading it is the file's subject: an ambient ask learns what tree
    git handed it, `common_of`'s scrubbed ask learns which repository that tree belongs to, and
    telling those apart is what the file is for. Before the marker it passed by accident, on the
    strength of `common_of`'s `env -u` list — credited for a scrub covering one call site out of
    many.
  - **It stops at hooks on purpose**, and the REASON has been stated wrongly twice, so it is
    written out here once. Not *"in a linked worktree `GIT_DIR` is the only way to reach the
    right repository"* — that is false, git chdirs to the working tree top before running a hook
    and cwd discovery answers the same. It is that **wherever `GIT_DIR` is set, it names which
    repository the merge was about**, and the hook has no other way to know. Measured on 2.51.1,
    all three layouts: an ordinary checkout leaves it unset, a linked worktree sets it to
    `<repo>/.git/worktrees/<name>`, and a leaked `GIT_WORK_TREE` sets it to `<repo>/.git` with
    `GIT_WORK_TREE=.` — and in that last one it is the only pointer there is. A round scrubbed
    it anyway and MEASURED the result: `ORIG_HEAD` stopped resolving, and a pull touching
    `crates/` printed *"no build-affecting changes pulled"*, the exact failure the scrub was
    written to prevent. So `scripts/hooks/post-merge` and the emitted chainer keep bare `git`,
    and the hook detects a redirected working tree instead of fighting it — it cannot win, since
    `--show-toplevel` names the redirected tree either way.
  - **The test harness needed the same sweep and had missed two.** `isolate_git` unset five
    variables and left `GIT_CONFIG_PARAMETERS` and `GIT_COMMON_DIR`, both of which outrank the
    empty configuration it builds — and the dev container exports the first. Its list is now
    defined by a class rather than a count, after the prose's "all five" had already gone stale.
- **A relative `core.hooksPath` is anchored at the working tree top — and with NO working tree
  it has no anchor at all.** Measured from three directories on git 2.51.1: git resolves it
  against the *invoking process's cwd*, so `git --git-dir=B rev-parse --git-path
  hooks/post-merge` answers `<cwd>/.githooks/post-merge` and `git hook run` executes whatever
  copy is under that cwd. A round claimed the git dir as "git's own rule" for that case — on a
  measurement taken with the cwd **set to** the git dir, which cannot tell the two apart — and
  jkb then installed a chainer there and reported the good verdict, while a `git pull` in a
  linked worktree ran a path that did not exist. There is nothing to resolve to, so jkb refuses
  and says so; running setup.sh against a worktree resolves normally. **A measurement whose
  variable you did not vary is not a measurement**, and the test that would have caught it had
  the same flaw: every fixture made repo_root, the git dir and the cwd one directory, so a
  mutant that ignored git entirely agreed with the oracle everywhere.
- **`core.hooksPath` set to the empty string** is the opposite error: git resolves it to
  `/post-merge` and finds nothing, so the repo hook is dead — and folding it into "not set"
  reported `dispatch=direct`, the verdict the renderer prints nothing for. Its remedy line asks
  for `--show-origin`, because `--get` prints one empty line for an empty value and nothing for
  an unset one: the operator's own check appeared to refute the warning.
- **A verdict that admits nothing was established must not also sweep.** The `unreadable` branch
  left `want` at its `no` default, and `no` retracts — so one run printed `exclude=retracted …`
  beside `dispatch=unreadable`, two lines making opposite epistemic claims about one path, after
  which the chainer read untracked and the next resolvable run put the block back. That is the
  flip-flop the worktree-invariant derivation exists to prevent, reintroduced through the
  destructive half. The verdict must not decide it either: `unreadable` covers three causes that
  differ on exactly the question the sweep asks, so the branch says `undecided` (no chainer was
  attempted, so nothing is known about *this* pattern) and lets the derivation have the last
  word — an unexpandable value establishes nothing, an empty one establishes that nothing of
  ours is anywhere, and a treeless relative one still yields a pattern that is right inside
  every worktree.
- **One writer for `.git/info/exclude`.** The append was two `>>` redirections with no
  rollback, so a disk filling between the separator and the block left a stray newline and half
  a marker line under a report saying the step did not land. Both paths go through
  `_exclude_write` now, which also retires the separator special case — a rebuilt file cannot
  fuse the user's last rule with our marker.
- **A guard belongs where it can see the failure it was written for.** The refusal for an
  unrecognised intent went into `reconcile_exclude` — and the caller's own default turned an
  unknown derivation word into `pattern=""`, which the funnel collapses to a perfectly
  recognised `want=no`, so the callee's guard never ran: the sweep retracted jkb's block and
  the renderer warned about the unknown word *after* the file had changed. Both ends now
  default to touching nothing.
- **The one consumer that can destroy the user's file refuses an input it does not recognise.**
  `reconcile_exclude`'s `want` had no default arm and fell through to the branch that SWEEPS,
  so a typo — or a fifth word added at the caller and forgotten at the callee, which is
  precisely the edit that introduced `unknown` — would retract every block jkb owns and then
  print a line the renderer reads as "nothing was changed". Every *renderer* in that file
  already had a warning default; the destructive consumer had none.
- **A claim about jkb's own file is gated on ownership, not on a variable that means something
  else.** The `exposed` downgrade read `want`, which the pattern-empty rule collapses to `no`
  two lines later — and an exposed line is by definition pattern-empty, so the downgrade was
  unconditional and the state unreachable the day it was added. Its test asserted only that
  `exposed` was *absent*, which passes just as well when it can never appear; the positive half
  is what catches it.
- **A per-step line may not describe the file; exactly one line does, and it is measured.**
  Three consecutive must-fixes in `render_git_hooks_report` were one shape — an arm asserting a
  run-level fact it could not see — and each was fixed by rewording that arm. The third made it
  plain that rewording is the wrong unit of repair: `exclude=undecided` has **two producers with
  opposite file semantics** (`want=unknown` returns early and touches nothing; `want=undecided`
  runs the sweep first), so no wording of that arm could ever have been right. It printed
  *"nothing in .git/info/exclude was changed"* directly beneath *"X dropped from
  .git/info/exclude"* — two lines about one file stating opposite facts, unattended, from the
  post-merge hook. The cause is that the wire protocol had **no scope axis**: some states are
  per-step events, some are per-pattern verdicts, and *did this run change the file?* is neither.
  `reconcile_exclude` is now a wrapper that fingerprints the file either side of the decision and
  emits `exclude-file=changed|unchanged` **below every arm**, so the dozen `return 0`s in the
  decision body cannot skip it — structural, like `install_git_hooks`' own funnel. It is measured
  rather than bookkept (`cksum`, with `absent` as a distinct value) because a write some future
  arm forgets to record is exactly the class of lie this seam keeps producing: *a claim about a
  file is asked of the file*. Every whole-file sentence is gone from the per-pattern arms, and
  both real values of the new key render **nothing** — the mutations are already itemised by
  their own lines, and silence is not a claim. Rejected: a fourth rewording; a stateful renderer
  (re-derives the producer's fact from a proxy, and makes output depend on line order); splitting
  the report word (fixes one word, leaves the class); folding the fact into each terminal
  detail (moves the per-arm memory burden one seam over).
- **An environment-injected `core.hooksPath` is refused, and git is asked rather than stripped.**
  `_git` deliberately does not strip `GIT_CONFIG_COUNT`/`GIT_CONFIG_PARAMETERS`: they carry the
  `safe.directory` grants this project's own dev container needs, and stripping them makes git
  refuse the checkout outright. But an injected value is the **calling process's**, not the
  repository's — and measured, jkb installed its chainer at the injected path, added an exclude
  rule for it, and reported `dispatch=chained`, the good verdict, while the repository's own
  hooksPath kept no chainer at all, so no later pull ran one. Reachable unattended, since
  `git -c core.hooksPath=X pull` exports the setting into the hook environment and the hook runs
  setup.sh. `git config --show-scope` reports such a value as scope `command` (measured on
  2.51.1), so the case is **detected by asking git** — no stripping, no second model of git's
  precedence. The first fix REFUSED it (`dispatch=transient`) and that was worse than the bug for
  the common case: a healthy repo pulled with `-c` printed three warnings and advised storing a
  value it had already stored, and a repo storing none was advised to set one, which kills
  `.git/hooks` dispatch outright. **`--get-all`, skipping `command` scope**, answers both with no
  warning at all — nothing stored reads as `direct`, a stored value gets its chainer refreshed —
  and the verdict word, its `why`, its render arm and its refusal code all stopped existing.
  Below git 2.26 there is no `--show-scope`, and falling back to `--get` reinstated the whole bug
  there (measured: chainer installed at the injected path, exclude rule written for it,
  `dispatch=chained` reported). So the fallback asks each STORED scope by name —
  `--system`/`--global`/`--local`/`--worktree` — rather than asking for the winner.
  **Superseded in part, and the correction matters more than the claim.** That bullet used to end
  "which predate `--show-scope` by a decade. Same answer on every git, not a degraded one on an
  old git." `--worktree` is not in the decade-old group, and an old enough git answers **129** to
  a scope flag it does not know — which was then absorbed into "not set in this scope", so every
  scope read empty and a repo with a stored `core.hooksPath` got `dispatch=direct`, the one
  verdict the renderer prints nothing for. A 129 is counted now, and all scopes refusing returns
  "unestablished" rather than "stores none". Left as written, this paragraph told the next reader
  a 129 could not happen — from the file `CLAUDE.md` sends them to *before* touching `lib.sh`.
- **`--path` expands EVERY value a scope returns, not just the winner.** So one broken line
  anywhere in a file failed the whole scope read — including the split-config recipe
  `--includes` exists for: a shared `~/.gitconfig` carrying `hooksPath = ~nosuchuser42/hooks`
  plus an `[include]` whose file corrects it. Measured on 2.51.1, `rev-parse --git-path
  hooks/post-merge` answers the good path — git resolves it and will run hooks there — while
  the scope read exited 128 and jkb reported `dispatch=unreadable` and wrote no chainer. The
  round-21 fix named this exact harm one scope *out* and left it standing one scope *in*. So a
  128 now re-asks the scope **raw** and puts only its last value — the one git would use from
  here — back to git for expansion.
- **That probe must be written by git, not printed into a file.** A git config file is not plain
  text, and the first version of the probe used `printf '[core]\n\thooksPath = %s\n'`.
  Measured on 2.51.1: `/a/b#c` and `/a/b;c` came back **truncated at the comment character** —
  silently, handing the caller a shorter path that exists nowhere — leading whitespace was
  eaten, and `/a/back\slash` exited **128**, reporting as unexpandable a value git resolves
  perfectly well, which is the very defect this arm was added to remove. `git config --file <f>
  <key> <value>` quotes on the way in (`hooksPath = "/a/b#c"`) and all six test values
  round-tripped, tilde expansion included. The general form: **when the question is "what does
  git make of this value", git writes the fixture too.**
- **A probe we could not BUILD is not a verdict about the value** — rc 6, not rc 2. Both failure
  paths (`mktemp`, and the `git config --file` write) reported "core.hooksPath cannot be expanded
  on this machine", so a full disk, a read-only or `noexec` temp mount, or a sandbox with a
  scoped `TMPDIR` silently reverted the whole arm to the wrong answer it was added to remove —
  no chainer written, and a remedy pointing at `--show-origin`, which prints a perfectly ordinary
  path. `setup.sh` runs unattended from `post-merge`, so the warning is all anyone sees. Six
  refusal codes now: 1 unset, 2 the value, 3 empty, 4 unanchored, 5 this GIT, 6 this MACHINE.
- **…and it must not OUTLIVE the scope that raised it.** Round 24, found in this file's own
  self-review. Codes 2 and 6 were carried in two flags — `broken` and `unprobeable` — which have
  a fourth state neither arm means: set at a low scope, `unprobeable` survived a *higher* scope
  breaking for the ordinary reason, and the read answered "jkb could not create a temporary file"
  about a value that had been tested and had failed. Wrong fact, wrong remedy, and the operator
  is told to free disk space over a broken `~someuser/` path. Reachable in three scopes at once,
  and pinned by `case10m`, which fails on the pre-fix code with exactly that message. **Two
  flags that must be assigned together at five sites are one variable**, so the fix is a
  three-valued `broken` (0 fine / 1 the value / 2 the probe): each arm becomes a single write and
  the stale combination cannot be spelled. The same shape as the `root_common` sentinel in
  `post-merge` — a state space with one more state than the code has names for.
- **A remedy must be written into the scope the value is HONOURED from.** Round 24's must-fix,
  and the third time this refusal's one instruction has been inert for the layout it is printed
  for. Reading a `config.worktree` declaration is what made that layout reachable; the line
  printed beside it wrote the LOCAL scope, which a `config.worktree` value outranks. Measured on
  2.51.1 — `config.worktree: core.worktree = /old` resolves to `/old`; `git --git-dir=D config
  core.worktree /new` leaves it `/old`; `git --git-dir=D config --worktree core.worktree /new`
  moves it. So the user ran the command, nothing changed, and the refusal reprinted for ever
  beneath a new sentence claiming it "replaces that declaration". `core.bare` has the identical
  hazard: a `config.worktree` `bare = true` survives a local `bare = false`. Both keys now go
  through **one** `honoured_read`, which returns the value AND the flag needed to write it back
  where it came from — reading and writing cannot disagree about SCOPE because the flag comes from
  the same function. Precisely the scope, and not the git directory: the read is made through the
  ambient `GIT_DIR` while the remedy is printed against `--git-dir=$hook_common`, and with the
  extension on those are two different `config.worktree` files whenever `GIT_DIR` names a LINKED
  worktree (measured on 2.51.1: `GIT_DIR=<repo>/.git/worktrees/W git config --worktree` reads
  `…/worktrees/W/config.worktree`, while `git --git-dir=<repo>/.git config --worktree` writes
  `<repo>/.git/config.worktree`). It is not reachable from this arm — a linked worktree has a
  `.git` FILE, so `common_of "$repo_root"` succeeds and the verdict is never `unestablished` — but
  the unqualified sentence was the kind this record's own rule says the next round builds on. Three follow-on lessons, all cheap and all already paid for elsewhere in this file:
  a helper returning **two** facts must not be called in a command substitution (the scope was
  assigned in a subshell and discarded, and every remedy printed the scope-less form again —
  caught by the step that RUNS the remedy, one commit after it was written); and the mutation in
  `case10l` step 9 lost its anchor for the second time when the declaration read moved into that
  helper, which its own premise check reported rather than passing silently. And the third,
  caught by round 24's own self-review: the helper took the flag that DECIDES which scope to ask
  — `extensions.worktreeConfig` — as an implicit input, a script-level variable assigned a
  hundred lines below the definition and read nowhere else, so the second caller worked only
  because the first had run. A caller that forgot it would get the LOCAL value silently while
  git honoured the worktree one: this helper's own defect, reintroduced as a rule every call
  site has to remember, inside the function written to remove that rule. It reads the flag
  itself now, `local`, which is what `_hooks_path_read` in `lib.sh` had already settled on after
  a stray global `true` sent it into a `--worktree` read that exits 128 in any repo with more
  than one working tree.
- **`jkb task close-merged` cannot run in either layout where the tree is not self-describing —
  REVERSING the round-23 split.** That round ended the `unestablished` verdict with a flag
  instead of `exit 0`, on the argument that chore 2 "needs only the repository the merge was
  ABOUT, which `$hook_common` named". It does not need to be *told* that repository — it
  DISCOVERS it, from its cwd, with `GIT_DIR`/`GIT_WORK_TREE`/`GIT_COMMON_DIR` scrubbed
  (`repo_ctx` → `main_root` → `rev-parse --show-toplevel`), which is strictly more than the
  `--git-common-dir` ask whose failure produced the verdict. Measured against the real binary in
  the bare-dotfiles layout: `error: not inside a git repo — a task session is a git worktree, so
  run this from the repo`, exit 1, and the hook then adds `close-merged failed (continuing)` —
  two lines, both false of that reader, printed under a refusal whose whole worth is that every
  sentence in it is true. It fails the same way in the layout the arm ACCEPTS, which nobody had
  measured: a `core.worktree` checkout has no `.git`, so scrubbed discovery cannot see it. So
  the skip is set on the branch where the scrubbed ask failed — covering both — and chore 2 says
  it is skipping rather than doing it silently, **where there is a `jkb` to skip**. It asks
  `command -v jkb` FIRST and the explanation lives inside that arm: printed before the check, the
  sentence blamed jkb's repository discovery on a machine with no jkb, for a chore that could not
  have run either way, on every pull. Round 25 found that reorder pinned by nothing — both chore-2
  steps prepend a stub, so the jkb-absent branch never ran — and it is now driven with a PATH of
  symlinks to the real `git`/`grep`/`bash` and no `jkb`, built from the real binaries so it holds
  on a developer machine, where `jkb` normally IS on PATH. **The part that was right is that the flags stay
  two**: `same`-by-declaration runs setup.sh and skips chore 2, which is genuinely two answers,
  and only `elsewhere` still shares a blanket `exit` (there discovery SUCCEEDS and close-merged
  would close tasks against the wrong repo key). The underlying gap is the filed one — jkb's
  repository discovery cannot see a `core.worktree` checkout at all, which is also why
  `install_git_hooks` reports `error=not a git repo` there.
- **A repository whose EFFECTIVE `core.hooksPath` is unexpandable fails the RAW scope read too.**
  Measured on 2.51.1 while building that test: with `hooksPath = ~nosuchuser42/hooks` winning in
  `$GIT_DIR/config`, `git config --local --get-all core.hooksPath` — no `--path` — exits 128,
  because git expands the repository's own value during setup. The same read against `--global`
  returns the value raw. So the re-ask-raw arm can never reach the probe for a broken *local*
  winner; it lands on "git will not expand this" one line earlier, which is the right answer for
  it (`rev-parse --git-path` fatals too) and worth knowing before writing a fixture that assumes
  otherwise. A losing broken value inside the local scope is unaffected — the winner is good, so
  setup succeeds and the raw read returns both lines.
- **A mapping with an unreachable arm is lifted out so it can be called.** The refusal-code
  `case` sat inline in `install_git_hooks`, where `*)` and `4)` were behaviourally identical —
  there is no fifth code today — so a mutation collapsing them stayed green and nothing could
  tell an honest catch-all from one absorbing a future code into a **definite** `unanchored`
  verdict whose remedy would be false of it. `_override_verdict`/`_override_why` can be called
  with a status that does not exist yet, and the test does.
- **Both readers of `core.hooksPath` died under `set -e`.** A bare `x="$(cmd)"` is a simple
  command, so a non-zero substitution aborts the shell — and exit 1 there is the **commonest**
  case, the setting not being present at all. `lib.sh`'s header promises every function behaves
  the same with `set -e` on or off; that promise was being kept only by how the one production
  caller happens to spell the call (`… || override_rc=$?`, which disables errexit for the whole
  invocation). `|| rc=$?` at both, pinned by a case that asserts its own premise first — a
  fixture that happened to have a `core.hooksPath` would pass having tested nothing.
- **One read of `core.hooksPath`, because two readers of one fact is how this file fails.**
  `git_hooks_override` refuses an environment-injected value; `git_hooks_exclude_pattern` read
  it separately and happily derived a pattern from it. The derivation has the last word by
  design, and an empty answer becomes `want=no`, which sweeps — so **one environment variable
  retracted the exclude block for the repository's own chainer**, leaving that file untracked,
  the tree dirty and `jkb task land` refusing it, unattended from the post-merge hook. Measured.
  `_hooks_path_read` is now the single read: it prints the repository's own value — the last
  entry that is not `command` scope — and both consumers take what they need from it — the override resolves it to a directory, the derivation wants
  the raw string — so the transient refusal cannot reach one and miss the other. Fixing only
  the reader the defect surfaced in would have left the identical hole one call away, which is
  this area's whole history.
- **The repository's own `core.hooksPath` is read, not the winning one.** `--get` reports
  whichever scope wins, and `-c core.hooksPath=X` — or `GIT_CONFIG_COUNT`/`GIT_CONFIG_PARAMETERS`,
  which `git pull` exports into the hook environment — beats everything stored. Measured: jkb
  installed its chainer at the injected path, wrote an exclude rule for it, and reported
  `dispatch=chained` while the repository's own path kept none. The first fix REFUSED, and was
  worse than the bug for the common case: a healthy repo pulled with `-c` printed three warnings
  and advised storing a value it had already stored, and a repo storing none was advised to set
  one — which kills `.git/hooks` dispatch outright. `--get-all` plus skipping `command` scope
  answers both with no warning at all: nothing stored reads as `direct`, a stored value gets its
  chainer refreshed. A verdict word, a `why`, a render arm and a refusal code all stopped
  existing.
- **The hook keeps the environment git hands it — and the round that took it away MEASURED a
  fix that measurement then contradicted.** The exemption's stated reason was wrong (*"in a
  linked worktree `GIT_DIR` is the only way to reach the right repository"* — it is not; git
  chdirs to the working tree top first, so cwd discovery answers the same), and correcting a
  reason was mistaken for correcting a conclusion. Scrubbed, the hook was measurably WORSE:
  git runs a hook with cwd at the working tree it resolved, and what it puts in the environment
  depends on the layout (measured, all three: ordinary checkout sets neither variable; a linked
  worktree sets `GIT_DIR` to `<repo>/.git/worktrees/<name>`; a leaked `GIT_WORK_TREE` sets
  `GIT_DIR=<repo>/.git` and `GIT_WORK_TREE=.`). Wherever it IS set it names the repository the
  merge was about, and in the third layout it is the only pointer there is, so stripping it
  discards the merged history —
  `ORIG_HEAD` stopped resolving, the `HEAD^..HEAD` fallback answered about the wrong
  repository, and a pull touching `crates/` printed *"no build-affecting changes pulled"*, the
  exact sentence the strip was written to prevent. Measured end to end, both arms, one fixture.
  - **It could never have helped either.** `--show-toplevel` answers the redirected tree with
    the variables set OR unset: with `GIT_DIR` set and `GIT_WORK_TREE` unset git regards the
    **cwd** as the work-tree top, and git has already chdir'd to the redirected one. So there
    is nothing to win by stripping, only the subject to lose.
  - **What the redirection really costs is the CHECKOUT, and that is detected, not fought.**
    `repo_root` may be an unrelated repository's, and running its `setup.sh` is the harm the
    whole repository-selection rule exists to prevent. The hook asks git whether the checkout
    at `$repo_root` belongs to the repository the merge was about (`--git-common-dir` from
    each side, relative answers anchored at the directory they were ASKED FROM, compared with
    `pwd -P`) and stops with a named reason when it does not. Verified by disabling the guard:
    an unrelated repository's `setup.sh` executes.
  - **And a checkout that CANNOT answer is the hard half — three rounds live here.** A
    `core.worktree` checkout has no `.git` entry of its own, so asking `$repo_root` with the
    environment scrubbed finds no repository at all, `root_common` comes back empty, and the
    comparison above read that as foreign: a perfectly ordinary checkout refused, with the D34
    automation permanently off there. The repository being merged knows the answer, because
    `core.worktree` is *it declaring this working tree* — so a declaration naming `$repo_root`
    restores it. Each round narrowed what "naming" means, and every narrowing was a measurement:

    | Round | The arm said | What it cost |
    |---|---|---|
    | 19 | declared and `-d`, resolved against the hook's cwd | a git-documented **relative** declaration was refused — git anchors it on the **git directory**, not the cwd |
    | 20 | declared at all (the comparison dropped) | `GIT_COMMON_DIR` makes git **ignore** `core.worktree` while `git config` still reports it, so a leak at one repository **built another checkout** — the whole harm, through the arm above it |
    | 21 | declared, resolved against `--git-dir`, and equal to `$repo_root`; and `GIT_WORK_TREE` unset | holds both ways: the relative form is accepted, the ignored declaration is not |
    | 22 | *nothing about the arm's predicates changed* — the arm was **confined** | the oscillation stopped being possible, rather than being patched again |
    | 23 | declared **in one of the two files git honours**, and equal to `$repo_root`; the `GIT_WORK_TREE` condition **deleted** | the bare read believed the caller (five forgery vectors built a foreign tree), and the deleted condition made the printed remedy inert for every user of the layout it is printed for |

    **Round 22 is the one worth reading, because it is not another predicate.** A design pass
    asked why three locally-correct, individually-measured fixes each opened the opposite
    defect, and found the answer in one line — `root_common="$(common_of "$repo_root")" ||
    root_common=""`. That collapsed a THREE-valued fact into two: "could not establish" became a
    sentinel the comparison read as "belongs elsewhere". The arm existed to repair the collapse,
    and because it was spelled as an assignment to `root_common` it was an **override of the
    comparison** — able to rewrite any verdict, not just the absent one. With no third place to
    stand, every fix had to choose which side of one accept/refuse line to widen. Round 20 is
    exactly that: the arm reaching a verdict that was never in doubt.

    Measured across ten layouts: **no legitimate layout lands in `elsewhere`, and no harmful one
    lands in `same`.** Every ambiguity this guard has ever had lives inside `unestablished`. So
    the verdict is computed once, three-valued, and:

    > **The acceptance arm may promote `unestablished` to `same`. It must never see, let alone
    > rewrite, an established answer.**

    Structural, not remembered — this record's own rule turned on the guard enforcing it. It
    shrinks both failure modes at once: the worst a future arm bug can do is accept a tree *no
    repository owns* rather than build a different repository's checkout, and the worst an
    over-narrow arm can do is an honest, remediable skip rather than a false permanent refusal.
    `case10l` step 9 pins the confinement by **removing** the arm's `GIT_WORK_TREE` predicate and
    requiring the refusal to survive anyway — the difference between testing the belt and testing
    the braces. It passes, which makes that predicate measured belt-and-braces; it is kept one
    more round anyway, because the round-20 scar is precisely a confident argument that an arm
    could not misfire.

    `unestablished` now gets its own sentence and a remedy, which resolves the bare-dotfiles
    false refusal without pretending to decide the undecidable: inside `unestablished`, the
    legitimate dotfiles layout and a leak into a directory no repository owns are
    indistinguishable from the repository's own records, so neither is built.

    **A remedy is a claim, so it is executed and not read.** Round 22 shipped that remedy checked
    only by a substring match for `config core.worktree`, and it was wrong twice over. Step 11b
    now lifts every `jkb:   ` line out of the refusal, **runs** it, and requires the next pull to
    reach the ordinary case and build that very tree:

    - **It did not work on a bare repository** — the exact layout it is printed for. `git config
      core.worktree` on a repo with `core.bare = true` warns "core.bare and core.worktree do not
      make sense" and then fails `unable to set up work tree using invalid config`. Measured on
      2.51.1: with `core.bare` cleared first, the declaration takes and the next pull is the
      ordinary case, so the hook now prints that line too — and only when the repository *is*
      bare, because a remedy cannot afford a line most readers should ignore.
    - **It did not parse when the path had a space.** The remedy is a command and is now shell-
      quoted as one; the step's fixture tree is called `dot home` for that reason.

    The step drops `GIT_WORK_TREE` when it runs the remedy, which measures the other half of the
    message: the arm requires the caller not to have overridden the work tree, so the
    parenthetical "prefer that declaration to an exported `GIT_WORK_TREE`" is not advice but a
    precondition. That is also the one user-visible cost of keeping the `GIT_WORK_TREE`
    predicate, and it is named where the user reads it.

    **A closed question was looked for and does not exist.** `git worktree list --porcelain` was
    the best candidate — it answers from the repository's records and ignores `GIT_WORK_TREE` and
    `GIT_COMMON_DIR` entirely — but measured, its main-worktree line names the **git directory,
    not the working tree**, for every layout whose gitdir is detached: `core.worktree`,
    `--separate-git-dir`, and submodules. Adopting it would have refused the layout the arm
    exists for plus two the comparison already handles, as a simplification. Those last two had
    no test until round 22 added steps 12 and 13; a layout with no case is a layout the next
    round is free to break.

    The shape of the mistake was the same twice: a fix for a **false refusal** opened a **false
    acceptance**, and vice versa. Both directions have to be re-measured after any change here.

    **Round 23 — SUPERSEDED, and this is the paragraph that was wrong.** It used to end: "The
    `GIT_WORK_TREE` condition is separate and also load-bearing — `core.worktree` is the
    *repository's* answer and `GIT_WORK_TREE` the *caller's*, and when the caller has overridden
    it the declaration says nothing about the tree we are standing in." Measured false in both
    directions. The declaration says exactly the needed thing about the tree we stand in — it
    equals `$repo_root`, which is the whole test — and removing the condition changes no harmful
    outcome, because every harmful case lands in `elsewhere`, which the arm cannot reach.

    What the condition actually did was make the printed remedy **inert for every real user of
    the layout the refusal is written for**. The canonical alias is `git --git-dir=D
    --work-tree=T`, and measured on 2.51.1 the flag form and `export GIT_WORK_TREE=T` are
    **byte-identical inside the hook** — git rewrites both to `GIT_WORK_TREE=.` after chdir'ing
    to the tree. So the user read a refusal, ran the remedy it named, and got the same refusal on
    the next pull, for ever, with its sentence "the repository does not declare it" now false.
    Nobody sets up bare dotfiles and then stops using the alias.

    The test agreed with the hook instead of testing it: step 11b **dropped `GIT_WORK_TREE`** for
    the post-remedy pull, on the strength of that same paragraph. It keeps the alias now, and so
    does the new flag-form step.

    **The rule that replaced it is one fact, not a predicate.** Git honours `core.worktree` for
    work-tree resolution from exactly two files — `$GIT_DIR/config`, and `$GIT_DIR/config.worktree`
    when `$GIT_DIR/config` itself enables `extensions.worktreeConfig` — and from nowhere else:

    | declaration in | git honours it | bare `git config core.worktree` returns it |
    |---|---|---|
    | `$GIT_DIR/config` | yes | yes |
    | `$GIT_DIR/config.worktree` (extension on locally) | yes | yes (via `--worktree`) |
    | `$GIT_DIR/config` via `[include]` | **no** | yes |
    | `--global` / `--system` / `GIT_CONFIG_GLOBAL` / `GIT_CONFIG_SYSTEM` | **no** | yes |
    | `-c` / `GIT_CONFIG_COUNT` / `GIT_CONFIG_PARAMETERS` | **no** | yes |

    Four rows where a bare read diverges from git's own honouring, and **every one of them is
    caller-reachable**. Measured end-to-end against a prey directory holding a marker `setup.sh`:
    `-c core.worktree=`, `GIT_CONFIG_COUNT`/`KEY_0`/`VALUE_0`, `GIT_CONFIG_PARAMETERS`, a
    `--global` declaration and `GIT_CONFIG_GLOBAL` **all made the hook run setup.sh in a
    directory no repository owns**. Five for five, against the arm whose comment read "so it is
    that repository's config and not the caller's".

    So the arm reads from git's honouring set, which is not a curated list — it is defined as
    "wherever git would honour it", and measured to coincide. The caller-reachable channels are
    exactly the channels git's own setup ignores, which is one fact covering all of them,
    including channels not yet invented at command scope. The scoped read cannot be forged:
    all six vectors read back empty, and the only handles that move the local scope — `GIT_DIR`,
    `GIT_COMMON_DIR` — move `hook_common` *with* it, so the declaration always comes from the
    repository the verdict is about. The caller cannot split the two.

    **`-c core.worktree` is not a second redirect, and saying so would mis-explain the fix.**
    Measured: git ignores a command-scope `core.worktree` for resolution, so the toplevel is
    never redirected by it. The redirect is the cwd; the `-c` only supplies the forged
    *testimony* that makes the hook accept it. Which is precisely why "read only what git
    honours" closes it exactly, rather than approximately.

    **Do not harmonize this read with `_hooks_path_read`.** That function walks
    system/global/local/worktree *with* `--includes`, and it is right to, because git honours
    `core.hooksPath` from every stored scope that way. The shared rule is "read the key from
    exactly the places git will honour it"; the two keys differ in where that is, so the two
    reads must differ. `case10l` step 17 pins it — the one plausible future edit that re-opens
    the forge while looking like consistency.

    `--worktree` is git 2.20, later than this file's 2.5 floor, and unlike `--absolute-git-dir`
    it **degrades correctly**: measured against a git shimmed to refuse it, the local-declaration
    layout still builds and the `config.worktree` layout skips — which is what that old git
    honours too, since a git predating the extension ignores `config.worktree` itself. The read
    mirrors the running git at every age.

    **One `exit` served two chores — SUPERSEDED in the half that mattered.** This paragraph
    used to end: "close-merged needs only the repository the merge was ABOUT, and that was never
    in doubt: `$hook_common` named it." What reversed it: `close-merged` is never *told* that
    repository, it DISCOVERS it — `repo_ctx` → `main_root` → `rev-parse --show-toplevel` with
    the caller's selection scrubbed, which is strictly more than the `--git-common-dir` ask whose
    failure produced this verdict. Measured against the real binary in the bare-dotfiles layout:
    `error: not inside a git repo — a task session is a git worktree, so run this from the repo`,
    exit 1, and the hook then adds `close-merged failed (continuing)` — two lines, both false of
    that reader, under a refusal whose whole worth is that every sentence in it is true. It fails
    the same way in the layout this arm ACCEPTS, which nobody had measured. So `$skip_close` is
    set on the branch where the scrubbed ask failed, covering both, and chore 2 says it is
    skipping rather than going quiet. See the bullet above for the full account.

    **The half that stands** is that the flags are two, not one: `same`-by-declaration runs
    `setup.sh` and skips chore 2, which is genuinely two answers. `elsewhere` alone keeps a
    blanket `exit`, because there the cwd belongs to a different repository, discovery SUCCEEDS,
    and `close-merged` would scope itself to *that* one, closing tasks against the wrong repo
    key. What two verdicts must not share is one `exit` standing in for two decisions.

    Two things this arm does NOT buy, stated because half of it is missing: `setup.sh` runs, so
    the binary, extension and service refresh, but its **hook** section does not —
    `install_git_hooks` reports `error=not a git repo` in this layout, because `_git` scrubs
    `GIT_DIR` and the tree has no `.git` to discover from. That message is false and the
    installer needs its own answer to "which repository is this?"; it is filed rather than
    bolted on, since the scrub it would have to relax is the one keeping jkb out of other
    people's repositories.
  - **A fixture must not decide for itself what environment git produces.** The test that
    passed the broken version built the INVERSE of git's own layout — cwd in the right repo,
    `GIT_DIR` naming the foreign one — under which scrubbing can only look like a win. It
    drives a real merge now and reads what the hook prints, in eight layouts: ordinary,
    redirected, linked worktree, entered through a symlink, `core.worktree`, declared-vs-
    redirected, relative `core.worktree`, and `GIT_COMMON_DIR`.
  - **You cannot shim `git` inside a hook, and a test that tries measures nothing.** git
    prepends its own `GIT_EXEC_PATH` — which contains a real `git` binary — to `PATH` before
    running a hook, so a stub earlier on `PATH` is never reached and the case silently exercises
    the real git and passes whatever the hook does. Measured: a hook printing `command -v git`
    answers `/usr/local/libexec/git-core/git`. To test this hook against a git that lacks an
    option, run the script DIRECTLY with the stub on `PATH`; a `git merge` run will not do it.
    This was found by drawing the false conclusion first.
  - **And running it caught a bug in the fix it has since replaced**: `env` execs a binary
    while `command` is a shell builtin, so `env … command git` failed every call and the hook
    exited at its first one, silently, because the next token is `|| exit 0`.
- **An assertion whose only discriminator is a token no producer can emit is not an assertion.**
  `case10h`'s last check failed on `dispatch=transient` — deleted from every producer by the same
  commit — so its `*)` arm ran unconditionally and reported `ok` while the old-git path was
  measurably installing the chainer at an injected value. The repair was not to reword it: the
  fallback was fixed so the property is *true*, and then asserted positively (the chainer lands at
  the repository's `.githooks`, and nothing exists at the injected path). Same shape as the
  `--no-review` lesson: a check satisfied by the absence of something is satisfied by everything.
- **A guard's exemption is by LOCATION, never by line shape.** `no_production_git_spawn_bypasses_
  git_cmd` exempted any line spelling `let mut cmd = Command::new` — which is the module's own
  idiom, shared verbatim by `git_cmd`, `gh_cmd` and `gate_cmd` — so the next helper written that
  way was exempt the moment it was added, which is the edit the guard exists to catch. It asks
  which function encloses each spawn now, and asserts it still finds the one legitimate spawn, so
  an empty result cannot mean the walk is broken.
- **"Both directions" must have no names it cannot see.** `run_cases` derives orphaned cases with
  `case[0-9][0-9a-z]*` — `case` then a DIGIT — and `case_isolate` was the one name in four suites
  outside it. Deleting it from a runner's argument list, precisely the edit that check exists to
  catch, left the gate green with a BSD-sed portability pin gone. Widened to admit `_`, so a case
  cannot fall outside it by being spelled reasonably.
- **A test fixture's isolation is WIDER than production's scrub, and that is two rules, not
  drift.** `gitrepo::scrub_repo_selection` strips repository selection and deliberately leaves
  `GIT_CONFIG_COUNT`/`GIT_CONFIG_PARAMETERS` alone (the dev container's `safe.directory` grants
  live there). A fixture must not inherit configuration it did not choose: env-injected config
  OUTRANKS the files, so `GIT_CONFIG_GLOBAL=/dev/null` is not isolation on its own, and an
  exported `commit.gpgsign` reddened `./scripts/check.sh` with `gpg: signing failed`. Measured
  both ways. One helper, because the file had two spawn sites and one of them had already
  remembered only half the list.

  **Round 22 corrects where that helper lives and what pins it.** `isolate_git_env` was in
  `tests/sessions.rs`, and each `tests/*.rs` is its own crate — so `tests/cli.rs` could not
  share it and had NO isolation at all, invisible to the crate-wide guard because that guard
  keyed on the literal `Command::new(` while these fixtures build their process with
  `Command::cargo_bin("jkb")`. The helper now lives in `tests/common/mod.rs`, which both
  include, and the guard keys on a `SPAWN_FORMS` list rather than one spelling.

  The CONFIGURATION half then turned out to be pinned by nothing at all. Measured: deleting the
  two `GIT_CONFIG_GLOBAL`/`GIT_CONFIG_SYSTEM` lines from `isolate_git_env` left all 137 tests in
  the two integration crates green — the three written to pin that very function included,
  because they checked only `env_remove` — and deleting the whole block from BOTH `src/`
  fixtures left 118 green, because `assert_scrubbed` names only the selection variables. The
  state that produces is exactly the harm all three comment blocks describe. So the block is a
  list now, applied by one function and checked by an oracle, rather than three copies held up by
  prose. The asymmetry is now exactly three names: `MUST_DROP` is `REPO_SELECTION_VARS` plus
  `GIT_TEMPLATE_DIR` and the two `GIT_CONFIG_*` injection channels. Production keeps the config
  channels because it must not discard a `safe.directory` grant it needs, and leaves
  `GIT_TEMPLATE_DIR` alone because it never runs `git init`; a fixture must not inherit either.
  Two rules, not drift — and the relation between the lists is CONTAINMENT, asserted by
  `the_fixtures_drop_everything_production_selects`, because the coupling that used to be
  structural (the fixtures calling `scrub_repo_selection` themselves) was removed in round 26 and
  nothing replaced it for a round: adding a fourth selector to production left 118 tests passing
  while the fixtures stopped dropping it.

  **Production scrubs repository COMPONENTS too, as of round 28**, and the reason it did not is
  worth keeping. The comment that held them out said production "must not discard a component a
  caller legitimately handed it — git exports `GIT_INDEX_FILE` to hook processes". Measured on git
  2.51.1 by dumping `env | grep ^GIT_` from real hooks: `post-merge`, the only hook jkb installs
  and the one that runs `close-merged`, is handed NO component selector at all; `pre-commit` is
  handed `GIT_INDEX_FILE=.git/index`, relative, which is meaningless to a `git -C <other dir>`
  call. Meanwhile the harm was reachable from a user-facing command — with
  `GIT_INDEX_FILE=<victim>/.git/index` exported, `jkb task work`'s `git -C <proj> worktree add`
  rewrote the victim's index and left `git status` there failing with `fatal: unable to read
  <sha>`. A justification nobody had measured was holding open a corruption path.

  **SUPERSEDED — the two-lists half.** This paragraph used to continue: "There are two such lists,
  one per side of the crate boundary — `FIXTURE_CONFIG` is `#[cfg(test)]`, and an integration test
  compiles with `cfg(test)` OFF — and each is asserted against the function beside it." Both named
  identifiers are gone. What reversed it: round 26 bridged the boundary with
  `#[cfg(test)] #[path = "../tests/common/mod.rs"]`, compiling ONE source text into the bin
  crate's test build and both integration crates, so the second list was never necessary — the
  boundary forbade importing, not sharing a file. The parity test that compared the two lists went
  with the duplicate; it compared text rather than environments, and its failure message read as
  an instruction to sync them, which round 25 measured as the edit that reopens the scrub hole.
  The live account is the seam bullet near the end of this file.
- **A hand-written coverage table is the same defect one level up.** `case14`'s row list caught
  `unaskable` only because somebody remembered to write its row; when code 6 `unprobeable` was
  added with no render arm AT ALL, the case still passed. The set is derived from
  `_override_statuses` now — which derives itself from `_override_verdict`'s own arms — and a
  verdict that renders nothing fails as loudly as one that renders the catch-all, because
  `dispatch=direct` is the silent verdict and silence would read as "the hook will run".
- **A closed vocabulary is derived from its renderer, or a new word gets no coverage silently.**
  `case14` asserted "the protocol and the render arms are the same set" from a HAND-WRITTEN table
  — so a state added to `render_git_hooks_report` with no row was an arm nothing drove, invisible
  because a shorter table passes just as happily. Derived from the renderer's own nested `case`
  arms, it found two uncovered arms on its first run (`exclude-file=changed`/`unchanged`, silent
  by design and in neither list). The silence list is now one array used by both assertions:
  written twice, a state added to one copy is "covered" while nothing asserts its silence.
- **A mutation that lands in a comment proves nothing, and looks exactly like a passing guard.**
  Checking `isolate_git`'s two unsets, the mutation replaced the first occurrence of each name in
  the file — which was the header comment — so both runs reported MISSED for guards that were
  fine. Target the line, then assert the mutation applied. (The reviewer's own rule, one level
  down: *a mutation changes exactly one thing*.)
- **Two assertions are not redundant if one sees a route the other cannot.** `case_isolate`'s
  single git-level check was satisfied by the `GIT_CONFIG_COUNT` unset above it — git reads no
  `KEY_<n>` without the count — so it could not tell whether the sweep it named had happened. It
  is one assertion per injection ROUTE now (`count`, and `PARAMETERS`, which nothing gates), and
  the pair earns its place against the variable check: dropping `isolate_git`'s `HOME` redirect —
  a FILE-based leak no variable check can see — leaves `isolate: vars` green and fails both.
- **A test fixture that mutates the developer's other repository.** `crates/jkb-cli/tests/
  sessions.rs` scrubbed ambient git CONFIG and not the three variables that select a
  REPOSITORY, so with `GIT_WORK_TREE` exported `git -C <tmpdir> init` re-inits the other repo,
  creates nothing in the tmpdir, and the `add`/`commit` that follow land a commit in it. That is
  `./scripts/check.sh` — the gate `jkb task land` and the merge queue trust — writing to a repo
  the developer merely happens to have configured.
- **`\|` in a BRE is a GNU extension.** `isolate_git`'s sweep of `GIT_CONFIG_KEY_<n>` matched
  nothing under `sed --posix`, i.e. on macOS, which is where this project is developed — so the
  isolation silently did not happen on one of its two platforms, and no case covered it either
  way. A `case` glob now, with a harness case that asserts git really sees no injected value.
- **Every spawn in the crate is now gated by ONE allowlist, keyed on (file, function).**
  Per-site pinning was the previous state of the art and it was not enough: it says nothing about
  the next file. `no_spawn_in_the_crate_resolves_a_repository_unscrubbed` walks every `.rs` under
  the crate root (skipping `target/`) and requires each spawn to sit in a named scrubbing
  constructor or in an explicit `NOT_REPO_AWARE` list with its reason — so a new spawn forces the decision when it is
  written. Three rules it had to learn, each from something it missed:
  **test code counts** (its first version cut each file at `mod tests`, which is exactly where
  the live damage was — `gitrepo.rs`'s own four fixtures scrubbed nothing, and with a dirty
  checkout at the other end of a leaked `GIT_WORK_TREE` they commit into it, create branches
  `deep/er` and `mergecommit`, and move its HEAD; measured, then measured again against the fix);
  **exempt by location, not by name** (keyed on the bare name `git_cmd`, a new module copying the
  idiom was exempt on arrival, and two files already spell it that way); and **every spawn is
  classified**, so "not repository-aware" is a recorded decision rather than an omission.
  Two more it learned the round after: **an unrecognized declaration is not the previous
  function** — a three-prefix list missed `pub(super) fn`, so `enclosing` kept the name above it
  and an unscrubbed `gh` spawn written immediately after `gh_cmd` inherited its exemption
  (measured; that is exempt-on-arrival reproduced inside the mechanism against it, and an unknown
  spelled as a definite answer inside the rule that forbids it) — and **an exemption's named test
  must exist**, since `src/archive.rs` was exempted on a comment naming a test that was nowhere
  in the crate, so deleting that fixture's scrub left everything green. The comment is
  machine-checked now. It also scans CODE rather than text (`code_only` blanks comments and
  literals), because a `Command::new(` that rustfmt had split across lines was invisible to it. Its own
  allowlist error — keying `tests/sessions.rs` on the delegate rather than the spawn site — was
  caught by the guard on first run.

  **And one more, which cost a whole round: it keyed on ONE SPELLING of "spawn".** `Command::new(`
  is not how a test builds the binary under test — `Command::cargo_bin("jkb")` is — so the three
  `jkb` fixtures were invisible to a guard whose doc claims NO SPAWN IN THE CRATE goes unscrubbed.
  Measured: deleting the isolation from `Fixture::jkb` left the guard and the whole suite green.
  The forms are a list (`SPAWN_FORMS`) now. The premise moved too: `scrubbers.len() >= 6` was
  `6 >= 6` on a const array of six — a guard that cannot fire, inside the guard against guards
  that cannot fire — and counting what the scan parsed instead fixed the arity while still
  saying nothing about WHICH six lines were read. It asserts IDENTITY now: the `(file, fn)` pairs
  the scan reads must equal the pairs the compiler saw. The allowlist also shrank by one, which
  is the right direction: `help_advertises_the_mcp_subcommand` was exempted for "runs `jkb
  --help`, which never resolves a repository" — true today, and an exemption keyed on the
  ENCLOSING FUNCTION covers spawns not yet written in that body. Routing it through the file's
  own fixture cost one line and removed the entry.
- **Three repository-aware spawns, one rule, pinned at each of them.** Asking *who else
  implements this rule* found two production spawns that were not git and resolved a repository
  from the environment anyway: `pr::gh` — `gh` finds the repo through git, so a leaked
  `GIT_WORK_TREE` has it asking GitHub about **an unrelated repository's pull requests**, and
  `close-merged` then closes tasks on that answer — and `session::run_gate`, whose verdict
  decides a landing and which would be verifying a different checkout. `gitrepo::
  scrub_repo_selection` is the rule; `git_cmd`, `gh_cmd` and `gate_cmd` are its three callers.
  **A test of the primitive is not the claim** (the `install_exec` lesson again): with the
  scrub deleted from `gh_cmd` and from `gate_cmd`, a test of `scrub_repo_selection` alone was
  perfectly green, so each call site builds its `Command` in a named function and each has its
  own assertion. Four mutations, one per site plus the blanket-strip guard, all caught.
- **The measurement itself had the defect it was added to prevent.** `_exclude_fingerprint`
  folded *could not measure* into `absent`, and two failures compare equal — so with `cksum`
  unavailable jkb reported `exclude-file=unchanged` **over a real write**. Measured with a
  `cksum` that exits 127 on PATH. It is three-valued now: `absent` stays an established answer
  (creating or removing the file must register), an unreadable path or an unrunnable `cksum`
  returns non-zero, and the wrapper emits `unknown`, which the renderer warns about rather than
  passing over in silence. Silence there would say *nothing to report about the file*, which is
  exactly what could not be established.
- **A derived list, because a hand-written one goes stale silently.** The `_git` enforcement
  check reverted four *named* call sites to prove it fires; merging the two readers deleted one
  of those names, and a third of the coverage would have stopped being exercised. It only
  surfaced because the probe asserts its own premise — *"the revert did not apply, so nothing
  was proven"* — instead of counting a no-op edit as a pass. It now derives every `_git -C`
  line from the file itself and requires a non-zero count, so it cannot quietly check nothing.
- **A branch no run takes is a branch that is not known to work.** `--show-scope` is git >= 2.26
  and an older one exits **129** for the unknown option — which is not *"cannot expand the
  value"*, and folded together every repo on such a git would report `dispatch=unreadable` and
  jkb would stop installing chainers entirely. The fallback is exercised by a PATH shim that
  refuses `--show-scope`, and the case asserts the shim really refuses first: a shim that
  quietly worked would pass having tested the ordinary path twice.
- **The shell-syntax gate is one function, not two copies of a file list.** `shell_sources` +
  `check_shell_syntax` live in lib.sh and CI calls them, because the hand-written copy in
  `check.sh` and the one in `ci.yml` drifted by a `*.md` skip within a commit of each other —
  green locally, red in CI, on one tree. It selects by **shebang**, not by a two-extension
  denylist (`scripts/hooks/post-merge` has no `.sh`, and the next `.txt` beside a hook would
  have been fed to `bash -n`), and **finding no files is a failure**: every unmatched glob was
  swallowed, so a broken gate printed its header and then "All checks passed".
- **A jkb block is defined positively, so the bad shapes follow instead of being remembered**: a
  known marker line immediately followed by a *pattern-shaped* line (non-empty, not itself a
  marker, not a comment). A marker that heads nothing is an **orphan** — jkb's own line, inert
  to git, removed and reported `tidied`. Without that definition the walk paired a marker with
  the marker below it, computed the block's "pattern" as the marker's own text, retracted
  **both** marker lines and left the real pattern bare — which jkb then reported `unowned` and
  refused to touch for ever, the exact silent-and-permanent harm the ownership rule exists to
  end, caused by the parser.
- **jkb is not the only writer of marked blocks in that file, and each sweeps only its own.**
  `session::ensure_excluded` writes `# jkb task sessions (git worktrees)` + `/.jkb/` from Rust;
  it survives because its marker is not in `exclude_known_markers`, which was a fact enforced by
  nobody until a test pinned it from the shell side and a comment stated it at both.
- **`retracted` split by what happened to the file, not by why.** One word carried four causes
  and the renderer stated a reason false on three of them; a de-duplicated copy of the block
  being *kept* was reported `retracted` and then `kept`, two contradictory lines about one
  pattern. Now `retracted` (jkb no longer stands behind hiding it — true of all three of its
  causes), `deduplicated`, and `tidied`.
- **The reconcile is below every arm, not inside one.** `install_git_hooks`'s arms set `want`
  and a verdict and none of them returns, so a fifth arm added later cannot skip the
  reconciliation — the property is structural rather than remembered. It had to be: the
  chainer-install-failed arm returned early, and because that precondition is itself
  persistent, "the next successful run reconciles it" never came. That arm's honest answer is
  the third value `undecided`: nothing is known about *this* pattern, and nothing needed to be
  known about the others.
- **An exclude line is read the way git reads it** (`_exclude_line`): git trims one trailing CR,
  so a CRLF file is functional to git and our comparisons must agree. They did not, so on such a
  file jkb recognised neither its own block nor the pattern and appended a fresh one on every
  qualifying pull. What is written back is the untrimmed original — agreeing about what a line
  *means* is not licence to rewrite how it is spelled.
- **A verdict is derived from the world, and every question about a path resolves it first.**
  `dispatch=` is `[ -f ] && [ -x ]` on the chainer (a directory is executable to `test` and
  unrunnable to git), and "does `core.hooksPath` point at git's own hooks directory?" compares
  `pwd -P` results, because a trailing slash, a symlink and a `..` are three spellings of one
  directory and literal equality put back the alarming message the guard was added to remove.
  `git_hooks_override` now separates `git config --get`'s exit 1 (*not set*) from anything else
  (*set to something git will not resolve* — a `~someuser/` for an absent account), which is the
  fourth verdict `unreadable`: folding it into "not set" reported `direct`, the verdict the
  renderer prints nothing for, about a repo in which git runs no hooks at all.
- **`install_git_hooks` reports STATES, not actions, and ends in a verdict.** Three findings
  were one shape: while each key named something jkb *did*, every state that arises from **not**
  acting had no key, no render arm and no test — a stale exclude rule, an exclusion attempted
  and failed, a hook installed where git will never run it. So `chainer=`/`exclude=` carry a
  state word with its evidence, and `dispatch=direct|chained|unknown|dead` is emitted on every
  successful run, answering the one question the feature exists for. It is derived from the
  world (`[ -x "$chainer" ]`), not from the outcome word, because `foreign` and `failed` each
  cover a file that will dispatch and one that will not — and it is three-valued because a
  foreign chainer may dispatch perfectly well and we cannot know, so it is `unknown`, never
  `dead`. `error=` now means *nothing was done* and is always the sole line: it used to follow
  `repo-hook=`, so setup.sh printed "repo hook: …" and then "skipping hook install".
- **Both halves of the seam live in `lib.sh` — the installer AND `render_git_hooks_report`.**
  Moving only the installer drew the boundary one level too low: nothing runs setup.sh, so its
  `case` arms were reachable from no test, and two findings sat in them with the gate green
  while the tests re-parsed the protocol themselves. Every `case` in the renderer has a default
  arm that **warns**, so a key added to the producer with no arm surfaces instead of vanishing.
  setup.sh is one line: `render_git_hooks_report < <(install_git_hooks …)`.
- **Every function in `lib.sh` behaves the same with `set -e` on or off**, and a reporter says
  `failed` in words rather than in its exit status. The report used to reach setup.sh only
  because the call happened to be written `… || true`, which disables `set -e` for the whole
  function body; without it the subshell died inside `install_chainer` and setup.sh printed the
  repo hook and **nothing else**, while `core.hooksPath` was set and that hook was dead. The
  suites cannot be sourced under `set -e` (`fail` increments and continues by design), so one
  case runs the installer in a `bash -euo pipefail` child instead.
- **A redirection that fails on a `{ …; }` group is not reported to `if !`** — only on a simple
  command or a function call, where bash returns non-zero as expected. Written as a group, the
  exclude append printed `added` for a write that had just been refused: the very defect the
  `failed` state exists to report, reintroduced inside its own fix. Found by running the new
  test, not by reading it.
- **An append to `.git/info/exclude` writes its separator first.** A file not ending in a
  newline — a hand-edited one usually does not — had its last rule fused with ours (`*.log` +
  `/.githooks/post-merge`), destroying a rule the user owns while our own pattern stayed inert,
  under a success message. `session::ensure_excluded` computes the same `sep`: one rule, an
  implementation in each language, and the shell copy was written without consulting the Rust
  one.
- **A caller of the atomic write is pinned by the destination's INODE, not by its content.**
  The primitive having a test is not the claim; the mutation that matters is at a call site,
  and a rename and an in-place rewrite leave identical content. Replacing `install_exec` at
  `install_git_hooks`' own call with `cp && chmod 755` — exactly the code this change exists to
  remove — left all four suites green, as did the same swap at the three `install_chainer`
  arms. `harness.sh`'s `inode_of` makes the assertion one line, and `lib.sh`'s header states
  that `install_exec` is the only function permitted to write an executable destination.
  `service::install` was the same gap in Rust and had no way to be tested at all: it derived
  its destinations from `$HOME`, so `install_units(manager, units)` now takes them as an
  argument — the split `commands::install_into` already made, for the same reason.
- **The atomic write is one seam, `jkb_cli::atomic::write`**, used by `service::install` and by
  `commands::write_all`. The shell half was fixed first, the service unit second, and the
  `~/.claude/{workflows,commands}` assets were still truncating in place — on a path setup.sh
  itself triggers, since a pull runs the hook, which runs setup.sh, which reinstalls the binary,
  whose next invocation reconciles that bundle, and a running `/task-swarm` or `/review` reads
  those files. Three installers, one rule, so it is not a rule each new installer must remember.
- **A pipe into a quiet `grep` reports a FOUND match as a failure — refused for the whole
  repository, conditioned on `pipefail`.** `grep -q` exits at its first match; a producer with
  more to write dies on EPIPE; `set -o pipefail` reports that, so the test comes back inverted.
  Two real instances, in two directories, six days apart: `.container/run.sh` had CI announce
  "run.sh no longer runs verify.sh" about a file that did (the invocation at byte 23501 of
  25810), and `scripts/hooks/post-merge` announced "no build-affecting changes pulled — skipping
  setup.sh" on a pull that changed every crate. The first was fixed with a scan local to
  `.container/*.sh` matching only the `dc_strip_comments | grep -q` spelling, which could not
  have found the second: different spelling, different directory. **Two half-guards with the bug
  in the gap between them** — so there is now one home (`scripts/tests/dev-scripts.test.sh`), one
  glob (`shell_sources`, 41 files across all five script directories, the same list `check.sh`
  and `ci.yml` use) and one message, and the container-local copy is a pointer.

  Measured, bash 5.2.21 on Linux, 30 trials per cell, match on the first line:

  |             | 4 KB | 8 KB | 16 KB | 32 KB | 64 KB | 82 KB |
  |---|---|---|---|---|---|---|
  | `pipefail`    | 0/30 | 0/30 | 0/30 | 28/30 | 30/30 | 30/30 |
  | no `pipefail` | 0/30 | 0/30 | 0/30 |  0/30 |  0/30 |  0/30 |

  Three corrections come out of that table, and each one had been asserted the other way in this
  repository first. It is **probabilistic** — a band, not a threshold — so the "800 files pass,
  900 fail" pair originally recorded for `post-merge` was one sample reported as an edge, and
  "our producer is small" is a claim about today's input rather than a property. It is governed
  by the **64 KiB pipe buffer**, not by write sizes: `printf` writes in 120-290 byte pieces, and
  what decides it is whether the producer must BLOCK — the container fix's "buffer size is
  irrelevant" and this file's earlier "bash writes in 4 KB stdio chunks" were both wrong, in
  opposite directions. And **`pipefail` is the entire hazard**: without it the producer's death
  changes nothing, because `grep -q`'s own status is what the shell reports. That last one also
  refutes the container fix's exemption of `printf "$var" | grep -q` as "a single write, safe" —
  `post-merge` was exactly that shape.

  So the rule is conditioned rather than blanket, and that is what makes it self-maintaining: the
  three sites it permits today are both `.claude/hooks` scripts, safe only because those files
  set no shell options, and the day one of them gains a `set -o pipefail` the case fails and
  names the line. A blanket rule would have had to exempt them by directory — the same fact
  recorded where nothing checks it.

  **The scope is three clauses, and each one was wrong first.** A file is in scope if it sets
  `pipefail` itself, OR has no shebang, OR its basename is sourced from a file already in scope.
  The first was written as `\<set\>[^#]*pipefail` and matched PROSE — a comment reading "because
  every caller happens to set `pipefail`" satisfied it, which is the only reason
  `.container/lib.sh` was in scope at all. The second replaced a first attempt that parsed the
  `.`/`source` lines of every script for library names and, on `. "$(dirname "$0")/harness.sh"` —
  how all five suites source the harness — captured `$(dirname` as the filename, leaving
  `harness.sh` outside the guard that names it while a planted line in `lib.sh` (sourced by an
  absolute path) made it look verified. The third exists because "no shebang" is not the whole
  set: `.container/lib.sh` and `egress-lib.sh` carry one, being `--self-test`-able, and are
  sourced by scripts that set `pipefail`. Anchoring the first clause without adding the third
  would have dropped `.container/lib.sh` out entirely — the two bugs were holding each other up.
  `egress-lib.sh` survives anchoring, but for a reason nobody wrote on purpose: its only match is
  a `set -euo pipefail` inside a single-quoted `bash -c '…'` body, script TEXT rather than a
  command that shell runs, so the first clause admits it and the clause that would cover it
  properly is never consulted. Over-inclusion, therefore safe. This sentence has been wrong twice
  in opposite directions — first claiming that file sets `pipefail` itself, then that both
  libraries depended on the third clause — because each time it was checked with the detector
  under discussion. Disabling the clause and re-running names exactly one file, and that is the
  check worth repeating rather than the prose worth trusting.
  Membership now names all four libraries (`scripts/lib.sh`, `scripts/tests/harness.sh`,
  `.container/lib.sh`, `.container/egress-lib.sh`), because a coverage FLOOR is cleared by the
  three dozen files that declare `pipefail` themselves and can never notice a missing member.

  The fix is always a here-string. `<<<` is a pipe at or below 65536 bytes and a temp file above
  — the switch landing exactly on the pipe buffer — and it is safe either way for a reason that
  has nothing to do with which: the shell finishes the write before the consumer is exec'd, and
  there is ONE command in the pipeline, so `pipefail` has no second status to take. "A here-string
  is a temp file" was the third wrong claim, corrected here.

  **`head -N` is the same race and is deliberately NOT machine-checked.** Of 25 sites in this
  tree, 23 are safe — status discarded inside a command substitution, `|| true`-guarded, or a
  producer bounded to a line or two by construction — so a rule would be ~2/25 precise, which is
  the shape of guard that gets deleted rather than obeyed. The two real ones were fixed by hand
  (`scripts/swarm-status.sh`'s `find`/`sort` pair, measured 20/20 aborts at 3000 lines against
  0/20 for `sed -n 1p`; `.container/check-drift.sh`'s stderr excerpt). Spelling "first line" as
  `sed -n 1p` is a convention, stated here, not a gate. Found alongside them and fixed with them:
  `check-drift.sh`'s `diff -u … | head -60` sits in the arm reached only when the files DIFFER,
  and `diff` exits 1 on every difference, so under `set -euo pipefail` the first drifting
  artifact ended the run and skipped every later generator — measured, the next line does not
  execute.

- **A test's expected value is written down; production iterates a list; the two are never the
  same source.** Two rounds got this seam wrong in opposite directions, and the second was caused
  by the fix for the first. Round 24: `isolate_git_env` RESTATED the eleven names its constants
  spelled, so the function could drift from both — measured, adding an `.env_remove` left all four
  isolation guards green. I made the appliers iterate the lists and dropped the `#[cfg(test)]` on
  `REPO_SELECTION_VARS` so `scrub_repo_selection` could iterate it too. Round 25: the rule and its
  only guard now had ONE source — measured, deleting `"GIT_WORK_TREE"` from that list and from
  `MUST_DROP`, two lines, left 260 tests passing while every `git` and `gh` spawn inherited an
  exported one. The harmless defect had been traded for the harmful one: over-scrubbing costs
  nothing, an unscrubbed selector points the tool at somebody else's repository.

  The arrangement with neither hole has **production as one artifact** (the list, iterated by the
  applier), **the test holding its own literal**, and the comparison being **equality**. Iterating
  closes "the function does less than the list says". The literal closes "the list shrinks and
  takes the guard with it". Equality closes "the list grows" — which is what a separate parity
  test used to be for. Two conditions on the literal, both learned here: it lives beside the text
  naming the harm, and its failure message says outright *not* to reconcile it by editing the
  list, because "the lists disagree" reads as an instruction to make exactly the measured edit.

  `pr.rs` had been right all along and its comment argued for it — a comment round 24 silently
  falsified by making `scrub_repo_selection` iterate the constant it called "a SEPARATE
  expectation list". The parity test that parsed both lists out of their source files is deleted
  with the duplicate it existed for: it compared two pieces of TEXT rather than two environments,
  and the crate boundary that forced the duplication (`jkb-cli` is bin-only, so an integration
  test cannot import from `src/`) is bridged by `#[cfg(test)] #[path = "../tests/common/mod.rs"]`
  — one source text, three compilations, no parity to check. The one relation that remains is
  CONTAINMENT in the other direction — `MUST_DROP` must contain `REPO_SELECTION_VARS`, since a
  fixture that inherits what production refuses is a fixture writing into somebody's repository —
  and that is asserted from the constants the compiler saw, one-directional because wider is the
  safe side. It exists because removing the redundant call removed the only coupling, which
  nothing noticed for a round.
