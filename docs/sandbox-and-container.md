# Unattended Claude — the sandbox posture and the dev container

Running the IDE and CLI with no permission prompts behind a boundary that holds when the
model is wrong (D48), the container that nests inside it (D49), the egress firewall and
its verdict (D50/D51). Covers `scripts/auto-mode.sh`, `scripts/auto-mode-posture.json`
and the BOUNDARY questions about `.container/` — why there are two layers, what each one
is asked to hold, and what the egress verdict rests on.

**The container's own internals are recorded in [`.container/README.md`](../.container/README.md),
not here**, and that file is the one that grows when `.container/` changes: what each layer is
for, the measurements under them, the mount list, how to run and verify it. Said plainly because
the split is not obvious from either end — this file's title names the container, and a reader
who stops here would miss thirteen commits of decisions that landed over there (the PID-1
reaper, the namespace-identity discriminator, the mount list as the security boundary).

The `grep -q` refusal is NOT among them, though trunk's fix for it landed in `.container/`: it is
a repository-wide rule now, recorded in [git-hooks-installer.md](git-hooks-installer.md) and
enforced from `scripts/tests/dev-scripts.test.sh`. Both of these lists named it here first, which
is how a routing sentence written in the same commit as the move can still point backwards.

Part of the jkb documentation set; see [CLAUDE.md](../CLAUDE.md) for the
conventions every session is expected to know.

## Unattended Claude: the sandbox is the guarantee, the classifier is the ergonomics (D48)

Running the IDE and the CLI with no permission prompts, with a boundary that holds when the
model is wrong. `scripts/auto-mode.sh` + `scripts/auto-mode-posture.json`; design in
`openspec/changes/jkb-safe-auto-mode/`.

- **Claude Code already ships the boundary, so we do not build one.** 2.1.237 embeds
  `@anthropic-ai/sandbox-runtime`: every Bash command is re-executed under `sandbox-exec` with a
  generated seatbelt profile (macOS) or `bubblewrap` + seccomp (Linux), with a settings schema
  covering `filesystem.{allowWrite,denyWrite,denyRead,allowRead,disabled}`,
  `network.{allowedDomains,strictAllowlist,allowUnixSockets,…}` and `credentials.{files,envVars}`.
  That is strictly more targeted than a container — per-command OS confinement **plus** an egress
  allowlist **plus** credential denial, on the host, with the host toolchain.
- **Two layers, because they fail differently.** `--permission-mode auto` is a **classifier**
  (`claude auto-mode defaults` prints its rules as English prose): it decides what is worth
  asking about, it can be wrong, and it buys ergonomics. The sandbox bounds what happens when it
  is wrong, and it buys the guarantee. `autoAllowBashIfSandboxed` joins them — a sandboxed
  command is never shown to the classifier, so the OS boundary **is** the check.
- **`bypassPermissions` is the wrong mode, precisely.** The sandbox confines Bash and its
  children; it does **not** confine the in-process tools — the schema says so about
  `strictAllowlist` ("in-process tools such as WebFetch are not gated by this setting"). Skipping
  permissions leaves Read/Edit/Write/WebFetch unbounded, a hole the shape of the file-editing
  tools. `auto` keeps the classifier over exactly what the kernel does not cover, and the
  posture's `permissions.deny` rules close the named paths in **both** layers at once (the schema
  merges `Read(...)` deny rules into `filesystem.denyRead`, `Edit(...)` into `denyWrite`) — one
  list, two enforcers, rather than two lists that drift.
- **The posture is user-level, and Claude Code enforces that.** Several keys are honored only
  from user/managed/`--settings`, and the binary carries an **operator-posture guard** that
  refuses to run when a repo's `.claude/settings.json` negates `sandbox.enabled`,
  `sandbox.failIfUnavailable`, `sandbox.allowUnsandboxedCommands` or `disableAllHooks`
  ("operator posture belongs in the user-level settings.json"), and likewise refuses a project
  `env` block (`BASH_ENV`/`LD_PRELOAD`/`NODE_OPTIONS`/`GIT_*` are unsandboxed-exec inlets). So a
  cloned repo cannot switch it off — and installing the posture into a repo would silently drop
  half of it. It also has to hold in **every** repo under `~/repos`, which is why this is not jkb
  configuration.
- **Four settings, each closing one silent degradation.** `failIfUnavailable: true` is the hard
  gate: its default is `false`, under which a *warning* prints and commands run **unsandboxed** —
  the exact failure of believing you are protected. `allowUnsandboxedCommands: false` deletes the
  `dangerouslyDisableSandbox` parameter, or one argument steps outside and in auto mode nobody is
  asked. `network.strictAllowlist: true` **denies** rather than prompting, because a prompt in
  auto mode is a question nobody answers. `enabled: true` is the rest.
- **File access is an allowlist, both ways.** Writes were already default-deny (workspace only),
  so `filesystem.allowWrite` *is* the allowlist. Reads are default-deny too:
  `denyRead: ["~", "/Volumes"]` blankets the user's data and `allowRead` — which takes precedence
  over `denyRead` — re-opens the work roots and the toolchain. System paths (`/usr`, `/bin`,
  `/Library`) are deliberately **not** denied: a command that cannot read its own dynamic linker
  cannot run, so "allowlist everything" is not a posture but an inoperative machine; what is
  denied by default is *your data*, which is what a leak is about. The in-process `Read` tool
  **cannot** be made default-deny — Claude Code's rule model is deny-beats-allow with no
  re-allow, so a blanket `Read(~/**)` could never be punched through for `~/repos` — so there it
  is an enumerated deny list plus an empty `additionalDirectories`, and the stronger option
  (deny the `Read` tool outright, read through sandboxed `cat`/`rg`) is offered, not imposed.
- **What actually runs unsandboxed**, since that is the residual worth naming: `Read`/`Glob`/
  `Grep` (bounded by deny rules only), `Write`/`Edit`/`NotebookEdit` (deny rules + the permission
  scope), `WebFetch`/`WebSearch` (the schema states in-process tools are **not** gated by
  `strictAllowlist`), **MCP servers** (long-lived processes started at session start, never
  per-command wrapped — `jkb mcp` is one), **hooks** (not evidenced as sandboxed anywhere in the
  binary), and the `claude` process itself. Bash and everything it spawns — which is where the
  real capability lives — is the sandboxed part. Three keys answer what can be answered:
  `permissions.ask: ["WebFetch"]` (Read-anything ∘ WebFetch-anywhere is read-everything-send-
  anywhere outside the kernel boundary — the one composition that defeats the posture, so it is
  the single surviving prompt), `disableBypassPermissionsMode: "disable"` (the in-process layer
  is the *only* bound on those tools, so being able to switch it off is being able to remove
  them all), and `defaultMode: "auto"` (without it every IDE session starts prompting, and the
  ergonomic half of the ask is silently unmet).
- **The one place the obvious sketch inverts.** Granting `~/.claude` write access is the grant
  that must not be made: `~/.claude/settings.json` **is** the posture, so an agent that can write
  it can disable its own sandbox next session, and the guard above does not defend the file it
  lives in. The deny is kept narrow — `~/.claude/projects/**` stays writable (the auto-memory),
  and a repo's own `.claude/settings.json` stays writable because it cannot weaken the posture.
- **`check` is two generic rules, not a list of assertions.** Claude Code enforces the boundary;
  re-checking its enforcement here would be a second model of the world. What is ours is that
  **the posture is a file and files drift** — Claude Code appends to `permissions.allow` on every
  "always allow", `/statusline` edits the same file, `claude auto-mode reset` rewrites a section.
  So the posture file has two halves and `check` asks two questions. **`require`** (what `install`
  merges): is it a **deep subset** of the effective settings? Arrays are subset-by-membership,
  never equality, so domains you add yourself are fine. **`forbid`**: is each named key empty or
  absent? That second rule exists because **a subset check cannot express emptiness** — a posture
  entry of `excludedCommands: []` would assert nothing, and `excludedCommands` is the sandbox's
  own bypass list ("all bash commands must run in the sandbox unless they are explicitly listed
  in excludedCommands"); `permissions.additionalDirectories` is the other, since it widens the
  only bound the unsandboxed tools have. Adding a key to either half extends the check *and* the
  **tests**, which generate their cases from the posture file (flip every boolean, drop every
  list entry, populate every forbid key).
- **The sandbox engages — established on the host, with a control** (D48.14). This was the last
  open question of D48/D49, and it is settled for the host: with the posture installed, a `$HOME`
  write is refused with **`EPERM`** while a control write inside `~/repos` succeeds. Neither TCC nor
  ordinary permissions explains that — `$HOME` is `drwxr-x---` owned by the user — and the read side
  tracks the posture exactly across three plain dotfiles of identical TCC status: `~/.gitconfig` and
  `~/.zshrc` readable (both `allowRead`), `~/.zsh_history` denied. The container case is separate
  and still open: it needs an authenticated session *inside* the container, because the sandbox
  wraps commands Claude Code runs and a plain `docker run` shell has no Claude Code in it.
  - **`CLAUDE_CODE_SANDBOXED` is not the test, and this file used to say it was.** It was **unset**
    throughout the measurement above. `auto-mode.sh sandboxed` asks the kernel instead — a control
    write inside an allowWrite root, a canary write to `$HOME` — and reports CONFINED / NOT CONFINED
    / **INCONCLUSIVE**. Three rounds reported CONFINED for refusals that were nothing to do with
    the sandbox — a directory squatting the canary path, an absent `$HOME`, a read-only `$HOME`,
    then a writable `allowWrite` subdirectory *beneath* an unwritable one — each fix adding another
    observation to establish the premise *a write to `$HOME` would otherwise have landed*. **That
    premise is not establishable from inside**: the sandbox intercepts `access(2)` too, so
    `[ -w $HOME ]` reports policy rather than permissions and every side channel is filtered by the
    thing being detected. The **errno answers it directly and subsumes all of them** — `EACCES` is
    the permission bits, `ENOENT` is no parent, `EISDIR` is something in the way, and only `EPERM`
    (seatbelt) or `EROFS` (a bubblewrap read-only bind) is policy. Compared numerically, so no
    locale or wording is involved. The verdict stays a **pure function** so the unconfined arm is
    testable from a confined machine, and the classifier is pinned against real kernel answers.
  - Costs nothing and needs no session, unlike `probe` — which remains the fuller check (egress and
    credential reads as well) and which correctly reported **INCONCLUSIVE** here rather than a pass,
    because a subprocess `claude` has no credentials in an agent session (`loggedIn: false` even
    with the sandbox explicitly overridden off, which is what attributes it to auth and not to the
    posture).
- **The posture makes Docker unreachable, and that is the right answer.** After installing it,
  `~/.docker/bin/docker` fails with `Operation not permitted` — the directory is under
  `denyRead: ["~"]` and in no `allowRead` entry. An unattended agent that can reach Docker can
  mount `/` into a container and is root on the host, so this is the boundary doing its job; the
  cost is that `.container/verify.sh` and `mutate-verify.sh` become **human-run** steps, which
  is now stated where they are documented rather than discovered when they stop working.
- **Installing it for real found two things no amount of checking could.** `install` ran clean
  (preflight green, 45 pre-existing allow rules and the theme preserved, `/tmp`, `$TMPDIR` and
  `mktemp` all still working — the three failures of the first attempt, absent), and then:
  - **Three `Write(...)` deny rules were inert, and Claude Code says so on every session start**:
    *"Write(path) is not matched by file permission checks — only Edit(path) rules are. Use
    Edit(path) instead (Edit rules cover all file-editing tools)."* The `Edit(...)` rules for the
    same three paths were already there, so nothing was unprotected — but an inert rule in a
    security posture reads as protection, and a warning printed at every start is how people learn
    to ignore warnings. **The `claude doctor` schema check could not catch it**: the rules are
    schema-valid, and what is wrong is their *semantics*. Only running it surfaced them.
  - **A subset merge cannot express removal**, which is the same shape as the reason `forbid`
    exists (a subset check cannot express emptiness). Deleting those three rules from `require`
    did nothing: the merge is add-only for arrays — deliberately, so your own `permissions.allow`
    survives a re-install — so an entry once installed stays for ever while `check` tolerates it
    as an extra. The posture gained a third half, **`retire`**: array members it has withdrawn,
    removed by `install` and reported as drift by `check`. Without it the only repair is editing
    `settings.json` by hand, which is the thing this script exists to stop people doing.
  - **And the agent locked itself out, exactly as designed.** The first `install` succeeded because
    the deny rule was not yet in force; the repairing `install` could not write, and said so:
    *"the posture denies writes to itself, so installing or repairing it is deliberately a human
    action."* That property had only ever been asserted in a comment. It is now demonstrated — and
    it means a posture repair is the operator's to run, which is the correct end state and worth
    knowing before you need it.
  - `jq` gotcha, pinned by a test: `false // x` is `x`, so the obvious spelling of "the value, or
    null if absent" turns every correct `false` into a failure — and the strongest setting here
    (`allowUnsandboxedCommands`) is exactly that shape. Use `has`, never `//`.
- **The posture is validated against Claude Code's own schema.** `claude doctor` reports settings
  violations for the directory it runs in, so the tests hand it the committed `require` block in
  a temp project and fail on `Invalid settings` — a typo'd key or an out-of-range enum installs
  cleanly, is ignored at runtime, and is indistinguishable from a posture in force. That check
  was **inert when first written**: the stub `claude` the `run` tests put on `PATH` shadowed the
  real binary, so it was validating against a stub that prints nothing, and only the mutation run
  found it. Resolve the real binary before the stub exists.
- **`probe` takes its verdict from the filesystem, not the transcript** — what a model narrates
  about its own confinement is not evidence. It needs a real billed session, so it is this
  change's `#[ignore]` test and is never in `check.sh`. **Two files, because one cannot tell the
  two failures apart**: "the canary is absent" is evidence the *sandbox* denied the write only if
  the session ran the command at all, and with the sandbox off — the state the probe exists to
  detect — Bash is no longer auto-allowed, so the **classifier** gets the out-of-bounds write and
  will very likely refuse it, and an absent canary would read as a clean pass. A control file
  written inside the workspace separates "denied at the boundary" from "never ran", and the
  second is reported **inconclusive**, never as a pass. The canary is deliberately not
  dot-prefixed: `~/.jkb-…` shares a prefix with the allowed `~/.jkb`, and that near-miss is how a
  probe comes to lie.
- **The container: measured, and the first answer here was wrong.** This section originally
  argued a container "buys nothing, because the sketch mounts `~/repos` and `~/.claude` and that
  *is* the blast radius". That is right about **Bash** and wrong about everything else — the
  generalisation was the error. For the in-process tools a container is not a second copy of the
  seatbelt: it puts the `claude` process itself in a mount namespace, so `Read`/`Glob`/`Grep`/
  `Edit` become default-deny **by the kernel**, which is exactly what the deny-beats-allow rule
  model cannot express. Genuine depth, and it closes the hole above.
  - **Whether the layers compose was measured** (Lima VM, Ubuntu 26.04 / kernel 7.0, Docker 29.7),
    with a no-container baseline first so a failure is attributable to the container profile and
    not the kernel. **Stock Docker cannot host it** — not root, not non-root, not with
    `--cap-add SYS_ADMIN`, not with AppArmor off; `bwrap` fails at namespace creation every time.
    The blocker is **seccomp**, and the fix is narrower than the folklore: neither `--privileged`
    nor `seccomp=unconfined` is required. Docker's *default* profile plus an unconditional allow
    for `clone, clone3, unshare, setns, mount, umount2, pivot_root, mount_setattr, open_tree,
    move_mount, fsopen, fsconfig, fsmount, fspick` suffices — and those are then usable only
    *inside* the user namespace `bwrap` creates, where the process holds no privilege over the
    host. It must also run **non-root**: with seccomp off, root in a container still cannot create
    a mount/net/pid namespace directly. Dev Containers already default to non-root.
  - **Two questions, and only one is about Docker.** Docker hosts limited mounts trivially — that
    is where the default-deny read property comes from, and it needs no seccomp work. The table is
    about the *different* question of running Claude Code's own sandbox **nested inside** such a
    container. "Stock Docker cannot host it" conflated them: false of the mounts, true only of the
    nesting. **Container-only is available today**; the seccomp profile is the price of keeping
    both layers, not of admission.
  - **Not established:** that Claude Code *itself* engages or refuses in a container. Two probes
    failed instructively. `claude -p` with an **invalid** key hangs with zero output — and hangs
    identically with no sandbox config, so the control proved it was the fake key; with **no** key
    it exits in a second (`Not logged in`), so Claude Code runs fine in a stock container. That
    suggested a credential-free discriminator, since `failIfUnavailable` is documented to error at
    startup and so should precede auth — **it does not**: in a stock container, where bwrap
    provably cannot create a namespace, it still printed `Not logged in`. So the sandbox is
    checked lazily, or auth precedes it; either way the probe cannot discriminate and the
    prediction behind it was wrong. Settling it needs a real session plus one `printenv
    CLAUDE_CODE_SANDBOXED` — i.e. credentials inside the container, which is the credential
    owner's call.
  - **What it does not buy:** `~/repos` mounted is still writable and push-able — the win is
    bounded to what you did not mount. And container egress is unrestricted by default, so if the
    inner sandbox ever fails to start you lose `strictAllowlist`; a container without its own
    iptables/ipset allowlist is a **downgrade** on egress. On macOS both container paths are a
    Linux VM, so the native loop (pinned rustup, `sqlite-vec` FFI, headless Chrome, launchd,
    worktrees under `~/repos`) has to be re-plumbed. That cost is unchanged; what changed is that
    the security argument now favours the container where it did not before.
- **Cross-platform, with the differences named rather than smoothed over.** The posture file is
  `~`-relative and carries both platforms' paths; macOS-only keys (`allowAppleEvents`,
  `enableWeakerNetworkIsolation`, `allowUnixSockets`) are inert on Linux and harmless. What
  actually differs: the mechanism is **bubblewrap + seccomp**, so `bubblewrap` and `socat` must be
  installed — `check` **warns** and `run` **refuses**, deliberately split, because "has the posture
  drifted" and "can this machine honour it" are different questions with different fixes and one
  exit code must not mean both. `denyRead` covers `/media`, `/mnt` and `/run/media` as well as
  `/Volumes` — `/mnt` being the most valuable entry on WSL, where the Windows filesystem lives —
  and `~/.cache` is in `allowRead`/`allowWrite` because without it a Linux build cannot read its
  own caches, and a posture too tight to work is one that gets switched off.
  - **`JKB_AUTO_MODE_SSH_AGENT` is macOS-only, and now says so.** `allowUnixSockets` is documented
    "Ignored on Linux (seccomp cannot filter by path)", so the overlay was a flag that reported
    success and did nothing — a guard that cannot fire. Linux's only lever is
    `allowAllUnixSockets`, all-or-nothing, which is not something to switch on behind a flag whose
    name promises a single socket. The test is branched per platform and each branch was run on
    its own platform, not inferred.
- **`preflight` exists because every live breakage was knowable without installing.** The first
  real install denied its own settings file, `$TMPDIR` and `/tmp`, and all three are facts about
  the machine's *resolved* paths rather than about the settings file — so no amount of checking
  the posture could find them. `auto-mode.sh preflight` resolves what the machine actually needs
  (`$TMPDIR`, the real path of `/tmp`, `$PWD`, the settings file, the toolchain roots) and reports
  any that no `allowRead`/`allowWrite` entry covers; `install` runs it and **refuses** on a gap
  (`--force` overrides). Verified by reverting the posture to the version that broke the machine:
  it names all three, each with its fix.
  - **It compares against the entries AS WRITTEN, not only resolved.** Resolving both sides makes
    `/tmp` and `/private/tmp` agree, which would have hidden the exact symlink mismatch that
    denied `/tmp` — the sandbox matched the real path while the posture named the link. A path
    covered *only* after resolution is reported as a latent gap, not as covered.
  - **A passing preflight names what it cannot check.** `install` refuses on its verdict, which
    makes it read as authoritative, so "no gaps — this posture is workable" claimed far more than
    a filesystem-path check supports. It now says "no FILESYSTEM gaps" and prints the four blind
    spots every run: setuid-root exec (refused under any posture, not configurable, surfacing as
    an opaque exec denial — it cost a peer session a red gate), unix sockets, the unvalidated
    domain allowlist, and whether the sandbox engages at all. Same rule as everywhere else here —
    an unstated gap in a tool something gates on is indistinguishable from coverage.
  - **Deliberately not in `check.sh`**: whether the real posture covers the real paths depends on
    where the checkout lives (`~/repos` on a dev box, `/home/runner/work` in CI), so a passing
    assertion would be a test of the machine. The tests exercise the *logic* — a posture covering
    nothing is refused, one covering everything is not, a symlink listed only by its link name is
    flagged.
  - **It asked the deny side against `$HOME` while the posture declares five deny roots.** The
    posture also blankets `/Volumes`, `/media`, `/mnt` and `/run/media`, so a cargo home on an
    external volume — or, on WSL, anything under `/mnt/c`, which is where the Windows filesystem
    lives — was reported "outside denyRead", `install` was not refused, and every sandboxed build
    then failed to read its own registry. Exactly the two-readers-of-one-fact shape this file
    argues against everywhere else, in the tool whose whole job is to predict that breakage.
    `denyRead` is now read from the posture like its allow-side siblings.
  - **`cd ""` succeeds in bash, which quietly made an empty posture list mean `$PWD`.** jq prints
    nothing for an empty array, a here-string of nothing is still **one empty line**, and the
    resulting empty entry resolved to the current directory and entered the list as a prefix.
    **It never produced a false pass** — reproduced against a posture covering nothing, `$PWD`
    still reports a GAP, because the arrays feeding the *covered* branch are built without `cd`
    and `covered()` skips an empty prefix. What it produced was the wrong **remedy**: the checkout
    matched the resolved-only list, so the gap advised "covered only if the sandbox follows
    symlinks, list it literally" instead of "is in no allowWrite entry" — and on the deny side it
    would have been a false GAP, over-strict rather than under. (An earlier version of this bullet
    claimed the checkout read as *covered*. That was wrong, and wrong in the dangerous direction;
    a reviewer caught it and the paragraph now records what running it shows.) Fixing it exposed
    the other half: arrays that had always held at least one element could now be genuinely empty,
    and `"${arr[@]}"` under `set -u` on bash 3.2 aborts the script. Both were latent behind the
    same masking bug.
  - **`set -e` made the three-state check unreachable.** `settings_state` returns 0/1/2 and a bare
    call returning non-zero aborts the script before `case $?` runs, so the distinction existed
    and never fired. `|| st=$?` is what turns a return code into a value. Caught by the tests,
    and pinned by reverting it.
- **`~/Documents` is a useless sandbox canary on macOS.** TCC denies it to the terminal whether or
  not any sandbox is running, so a probe that reads it always looks confined — which is how a
  restored, sandbox-free machine was briefly misreported here as still sandboxed. Test
  confinement against a path the posture itself governs, never one the OS already protects.
- **The liveness probe stopped shelling out (D48.12), and the dilemma dissolved.** `ps` is
  setuid-root on macOS, a sandboxed process cannot exec setuid, so under this posture
  `owner::pid_exists` could never run and every `host:pid` owner read as `Fact::Unknown`. The only
  sandbox-level lever was `sandbox.excludedCommands`, which runs a command **wholly outside** the
  sandbox — and `forbid` requires that list empty precisely because `require` cannot bound it
  (subset semantics would let `["ps"]` become `["ps","bash"]`). Neither was needed: `ps` was
  chosen over `kill -0` because it reports processes it does not own (D27.2), and that reasoning
  is about the **shell builtin**, which collapses `EPERM` and `ESRCH` into one non-zero exit. The
  *syscall* separates them, and **`EPERM` is positive evidence of existence** — the kernel refuses
  because the process is there and is not ours. `rustix::process::test_kill_process` is a safe
  wrapper (no `unsafe`, and rustix was already in the tree), so the probe needs no subprocess, no
  `PATH`, and no setuid binary.
  - **Better with no sandbox in the picture at all**, which is the test that keeps it from being
    chosen for the wrong reason: no fork/exec per probe, no `PATH` dependency, identical on macOS
    and Linux. The mapping is a pure function, so the `Unknown` arm — the one that protects every
    claim — is an ordinary assertion instead of needing a deliberately-broken spawn (the previous
    version reached it by naming a nonexistent program, after an earlier one emptied `PATH` and
    reddened the shared gate one run in six).
  - **A pid outside `pid_t` is `No`, not `Unknown`**: no process can carry that id, so its absence
    is established rather than unobserved. That preserves the prior behaviour exactly.
  - **Untested:** whether a sandbox profile permits `kill(pid, 0)` against a *foreign-owned*
    process. It barely matters in practice — jkb's claimants are the same user's processes, so the
    live answers are `Ok`/`ESRCH`, neither of which needs privilege — and a denial would return
    `EPERM`, i.e. "alive", which is the safe direction (never reclaims live work).
- **A review round found six must-fix and eight concerns, and four of them were one shape**
  (D48.13): an assertion that matched text present on **both** the pass and fail paths, or read a
  file nothing had written. `mutate-verify.sh` grepped a label `verify.sh` prints identically
  either way, so 2 of 5 mutations reported CAUGHT with the guard deleted — under a summary line
  reading "every guard fired". `verify.sh`'s mount check filtered `mountinfo` by target prefix, so
  it *was* the list of absences this file claims it is not; `/var/run/docker.sock` passed. The
  seccomp assertion was satisfied by the generator's own trailing allow group, true by
  construction. And a Linux-only test grepped an argv file `run` never creates, passing having
  observed nothing. The fix is the same in all four: **assert on a discriminating signal** — a
  non-zero exit plus the FAIL-only rendering, the full mount set minus the runtime's own, the
  negative "no restricted entry still names these", the precondition that the file exists — and
  where a harness judges other guards, give it a **negative control**: an unmutated run must be
  reported MISSED, or the matcher is matching something present when nothing is wrong.
  - **The next round's must-fix was inside that round's fix, and it is the same shape one level
    up.** Removing the credential mount and renaming the cargo volume deleted **three** lines of
    `verify.sh`'s hand-written mount list where two were intended, dropping `.cargo/registry` — so
    a correctly-built container failed its own verifier, after the full toolchain build, because
    `setup.sh` ends by running it. Two lists that must agree **is** the defect: the list is now
    **derived** from `container.json` (both the string and object mount spellings), so there is
    one. A `CARGO_TARGET_DIR` guard added an hour earlier pinned a single string in that very file
    and could not see the list beside it — a guard aimed at the instance, not the class.
  - **The third round found the same class again, so the fix stopped being a fix and became a
    harness.** `verify.sh` had `mutate-verify.sh` watching it fail; `check-config.sh` had nothing,
    and rounds two and three each found an assertion in it that could not fail — a regex that
    could not cross a shell quote and so never caught the exact code it existed to prevent, and a
    rewrite that dropped the `type=volume` half of its own check while keeping the failure message
    about volumes. Hand-mutating after each round works until nobody does it.
    `.container/mutate-config.sh` breaks each config property in turn (18 of them) and requires
    a FAIL naming it, with the same negative control — and it needs no Docker, so unlike
    `mutate-verify.sh` it runs in `check.sh` and CI. **It found a live one on its first run**: the
    seccomp assertion grepped for the `seccomp=…` value anywhere in the file, so deleting the
    `--security-opt` flag and orphaning its value passed — Docker would apply its default profile,
    bubblewrap would fail, and the config still read as declaring one. It is asserted as a
    flag/value pair in `runArgs` now.
  - **A skip decided per-assertion is not a skip.** `run` refuses on a Linux host without
    bubblewrap, correctly — but the argv assertions ran unconditionally, so the shared gate went
    red for a fact about the machine; and the drift assertion three lines below asked only for a
    non-zero exit and no argv file, which is *exactly* what that dependency refusal produces. It
    would have reported a pass having never exercised the refusal it names. The group is skipped as
    a group, announced, and the drift assertion now matches the refusal **text**.
- **Two more were the posture not covering what the machine needs**, the D48.10 shape again in a
  new place: `CARGO_TARGET_DIR` was a *sibling* of the allowlisted `~/.cargo`, and `covered()`
  matches at component boundaries, so `denyRead: ["~"]` blanketed every sandboxed build in the
  container while both guards reported it healthy. Moved inside `~/.cargo/target`, and `preflight`
  now reads `CARGO_TARGET_DIR` so the class is checkable rather than latent.
- **`install`'s preflight gate turned CI red**, which is the coupling CLAUDE.md already says to
  avoid: preflight asks about the *machine*, and the hermetic suite drove `install` through it, so
  on a runner (`/home/runner/work/...`, under no allowWrite root) install wrote nothing and two
  assertions passed **vacuously** on the empty result. The suite now passes `--force`, and the gate
  gets one deliberate test of its own instead of being exercised incidentally by all of them.
- **The credential mount could not work on macOS.** `~/.claude/.credentials.json` is Linux/WSL
  only — macOS keeps credentials in the Keychain — and a bind mount with a missing source is a
  hard error, so the container never started on the host this repo is developed on. Removed
  entirely: authenticate *inside* the container (`claude auth login`), into the state volume,
  which also fixes the read-only mount that a login could not have written.
  - **Removing a mount left the plumbing shaped around it.** `setup.sh`'s symlink loop linked only
    the *directories* under `~/.claude`, because the credential file was the one thing bind-mounted
    and the loop had to go around it. With the mount gone, nothing linked it — so an in-container
    login sat in the writable layer and died with the next rebuild, while `container.json`
    promised the opposite. `.credentials.json` and `~/.claude.json` are now linked into the volume
    too, dangling until first write. The residual is stated rather than designed away: a writer
    that replaces a file by temp-and-rename would drop the link, which costs one re-login at the
    next create and can never reach the host.
- **Stated residuals, not guaranteed over.** In-process tools are bounded by permission rules and
  not by the kernel, so a path nobody named is Read-able. MCP servers and hooks are unsandboxed
  processes with no posture key to reach them (use `--strict-mcp-config` in a repo that is not
  yours). Any allowlisted host is an exfiltration channel — egress control bounds *where*, not
  *what*. And a posture too tight to work gets switched off, which is why `check` tolerates
  entries you add and `probe` reports inconclusive rather than pass. `~/.ssh` being unreadable means SSH pushes
  fail inside the sandbox (correct for an unattended agent; `JKB_AUTO_MODE_SSH_AGENT=1` allows the
  agent **socket** instead of the key, so it can authenticate but never read it). `localhost`
  egress and that socket overlay are unverified without a live session, and both fail in the safe
  direction.

## Both layers: the dev container with the sandbox nested inside (D49)

`.container/` is the "both" configuration of D48 — a container **and** Claude Code's own
sandbox running inside it. `scripts/auto-mode.sh` alone is host-only; this adds the one property
the host cannot express. Design in `openspec/changes/jkb-safe-auto-mode/` (D48.7, D49).

- **The container's job is file access; the sandbox's job is everything else.** An unmounted host
  path does not exist in the container, so the in-process tools (`Read`/`Glob`/`Grep`/`Edit`) are
  bounded by the **mount namespace** rather than by permission rules — default-deny by the kernel,
  which the deny-beats-allow rule model cannot express at all. The nested sandbox still supplies
  per-command Bash confinement and the precise hostname allowlist.
- **The egress firewall exists because `strictAllowlist` lives inside the layer that might not
  start.** A container's default egress is unrestricted, so a container whose nested sandbox
  failed silently would be a **downgrade** on exfiltration versus the host. `init-firewall.sh` is
  coarse (IP-level) and independent; the sandbox is precise (hostname at a proxy) and in-process.
  Coarse-but-independent under precise-but-fragile is the point — they fail for different reasons.
  - **One allowlist, not two.** The firewall reads `.require.sandbox.network.allowedDomains` out
    of `scripts/auto-mode-posture.json` — the same file the sandbox posture comes from. Two egress
    lists that can disagree is how the tighter one ends up decorative.
  - **A wildcard cannot be pinned to an IP**, so the firewall skips `*.rust-lang.org` and says so;
    the posture therefore also names `static.rust-lang.org` concretely. Without that the toolchain
    download is blocked by the coarse layer while looking allowlisted in the file.
  - **It is installed into the image, root-owned, with sudoers granting exactly that one path.** A
    script the agent can edit, that the agent can also `sudo`, is a root shell with extra steps.
  - **…and all of that was decorative, because the base image grants blanket passwordless root.**
    `mcr.microsoft.com/devcontainers/base` ships `/etc/sudoers.d/vscode` containing
    `vscode ALL=(root) NOPASSWD:ALL`, so the agent could flush the firewall, delete the allowlist
    snapshot, rewrite the root-owned script, or simply `sudo -i`. Every root-ownership protection
    in this design rests on the agent not being able to become root, and none of them checked.
    Removed in the Dockerfile, with `visudo -c` failing the **build** on a malformed result — a
    broken sudoers file locks out the one grant the firewall needs. **Found only by running
    `sudo -n -l` inside the container**: two review passes and every static check missed it,
    because nothing in the repo *says* the base image grants it. The general lesson is the one
    this section keeps relearning one level up — a guard is worth what its weakest bypass is
    worth, and the bypass here was inherited rather than written.
  - **The check for it asks sudo, not the file.** `verify.sh` requires that every command `vscode`
    may run as root names the firewall, so a blanket grant re-added by *any* route — a rebuilt
    base image, a feature, a helpful `RUN` line — fails it. `mutate-verify.sh` restores the grant
    in a one-layer image built `FROM` the real one and requires that failure, since a sudoers
    entry cannot be broken with a `docker run` flag.
  - **…and that argument was the hole the script itself closed for the script.** The caller passed
    the allowlist's path, and the path passed was the repo's own copy — bind-mounted, under
    `allowWrite`, writable by the agent this layer exists to bound. Appending a domain and waiting
    for the next container start had root add it to the ipset. The sudoers entry made it worse
    rather than better: **a command naming no argument accepts every argument**, so any readable
    JSON on the box was a valid allowlist. Now the workspace copy is read exactly **once**, at
    container create, before any agent session exists, and snapshotted root-owned; every later run
    reads the snapshot, the script **refuses** arguments, and sudoers pins it to none (`… ""`). A
    rebuild is what re-reads the repo, which is the right ceremony for widening egress. Divergence
    between the two is **reported and never acted on** — silence would let a legitimate edit look
    applied when the coarse layer never saw it.
  - **Order:** the Dev Containers lifecycle is postCreate → postStart, so `setup.sh` raises the
    firewall as its *first* act. Leaving it to `postStartCommand` alone would run the whole of
    create — including a toolchain download — with open egress.
- **Measured, not assumed** (Ubuntu 26.04 / kernel 7.0 / Docker 29.7, baseline outside a container
  first): **stock Docker cannot run the nested sandbox** — `bwrap` fails at namespace creation as
  root and as non-root, with `--cap-add SYS_ADMIN`, and with AppArmor off. The blocker is
  **seccomp**, and neither `--privileged` nor `seccomp=unconfined` is required: the default profile
  plus 14 namespace/mount syscalls suffices. **Non-root is load-bearing, not hygiene** — with
  seccomp fully disabled, *root* in a container still cannot create a mount/net/pid namespace
  directly.
- **The mount list is the security boundary, and it is asserted exhaustively.** `verify.sh` reads
  `/proc/self/mountinfo` and fails if the mounted set is anything other than what
  `container.json` declares — rather than listing paths that ought to be absent, which is the
  "enumerate the secrets" shape the host posture is *forced* into and the container is not. Only
  `~/.claude/.credentials.json` is mounted, never `~/.claude`: that directory holds the posture,
  and a process the posture bounds must not read or write the file deciding whether it is bounded.
- **Every guard has been watched failing.** `mutate-verify.sh` breaks each property in turn — an
  undeclared mount, the host `~/.claude` mounted, stock seccomp, no `NET_ADMIN`, running as root —
  and requires `verify.sh` to fail naming it. It needs a Docker host, so it is an `#[ignore]`-class
  test; `check-config.sh` is the host-side half that runs in `check.sh`, and its real job is the
  **generated** seccomp profile: a patch that no-ops against a changed upstream yields a profile
  that parses, applies, and leaves the sandbox unable to start.
- **Three defects found only by building and running it**, none of which static review would have
  caught. (1) `jkb` was not installed in the container at all — the explorer extension spawns it
  and every workflow verb needs it; `setup.sh` now builds and installs it, and `jkb 0.1.0` was
  confirmed running inside. (2) **A named volume whose path does not exist in the image is created
  root-owned**, so cargo died with `EACCES` minutes into the first build; every volume mount point
  is now created in the Dockerfile, which is what makes Docker seed ownership from it. (3) Cargo's
  `target/` moved off the bind mount into a volume — required by (2)'s uid mismatch, and
  independently right because it is the heaviest write path and a macOS bind mount is a VM
  filesystem crossing.
  - **An out-of-memory build does not say so.** `rustc` is SIGKILLed and cargo reports a bare
    `(signal: 9, SIGKILL: kill)`. Diagnosed from `dmesg` rather than guessed at, twice — the first
    diagnosis was right about OOM and wrong about the cause, since the test VM was holding a 2.9 GB
    copy of the repo on tmpfs. Worth knowing because the symptom names neither memory nor the fix.
- **Where VS Code runs does not matter; where the `claude` process runs does.** Under Dev
  Containers the UI is on the host and the server, extension host and terminals are in the
  container. The Claude Code extension declares no `extensionKind` and has a Node `main`, so VS
  Code runs it as a **workspace** extension — in the container — and `container.json` lists it
  so the linux build is installed inside (a host copy is platform-specific and separate).
- **Open the repo root, not a session worktree.** `jkb task work` puts worktrees at
  `<repo>/.jkb/work/<session>` and a linked worktree's `.git` is a *file* pointing into
  `<repo>/.git/worktrees/…`. Mounting the root puts both ends inside; mounting only the worktree
  breaks git, because the gitdir it names is not there.
- **Still not established:** that the nested sandbox actually *engages* for a tool call. `bwrap`
  working is the mechanism, not the product, and the credential-free probe does not discriminate
  (see D48.7). Settle it in a live session with `./scripts/auto-mode.sh sandboxed`, **not** with
  `printenv CLAUDE_CODE_SANDBOXED` — see the measurement under D48 below.

## The dev container mounts ~/repos, and a session worktree is archived (D49)

The container follow-up bucket. Design in `.container/README.md`; the container harnesses are
`check-config.sh` + `mutate-config.sh` (static, in the gate) and `verify.sh` + `mutate-verify.sh`
(need Docker).

- **`workspaceMount` binds all of `~/repos`**, not just this repo. The argument is *consistency*,
  not convenience: `scripts/auto-mode-posture.json` already grants `~/repos` in both `allowRead`
  and `allowWrite`, so a container holding only jkb was **tighter** than the boundary the same
  agent runs under on the host — a difference nothing had decided, which made a cross-repo task
  impossible rather than deliberately refused. `workspaceFolder` FOLLOWS the folder you opened
  (`${localWorkspaceFolderBasename}`), and `initializeCommand` refuses one the mount cannot
  place — see the derived-folder note below.
- **A nested bind must be NAMED, never inferred.** `verify.sh` compares exact mount points with no
  prefix logic (prefix filtering is what once let `$HOME` at `/host` through), so once the declared
  target is the parent, `mutate-verify.sh`'s own `-v $REPO:/home/vscode/repos/jkb` is undeclared —
  and it cannot mount the parent instead, because in a `jkb task work` session `$REPO`'s parent is
  `.jkb/work`. The tempting rule — *nested is fine* — is wrong: a mount point and a mount SOURCE
  are independent, `-v ~/.ssh:/home/vscode/repos/jkb/secrets` is inside the declared region and is
  still exfiltration, and the source cannot be checked from inside (Docker Desktop for macOS
  reports the path inside the VM, which is why `dc_mount_sources` is host-side only). So
  `verify.sh --declare <target>` **adds** to the derived set and is **refused** unless the value is
  a strict descendant of something already declared **as a bind** (not a volume, which reaches no
  host filesystem, so nothing nested under one is reviewable this way) — `/host`, the docker socket and
  `~/.claude/settings.json` are all refused by verify.sh itself, which is exactly the mutation set
  the harness exists to catch. The count prints in the passing line (D38's `--no-review` lesson).
  A blanket rule would have weakened the *shipped* container; this weakens only the harness, and
  only where a path is written down in a diff.
- **Auto-memory is shared through `~/.jkb`, with no new mount.** Claude Code keys memory by the
  project's absolute path, so one repo has two keys (`-Users-…-repos-jkb` /
  `-home-vscode-repos-jkb`) and widening the workspace mount does not change that. Binding the
  host's memory dir is the one mount forbidden everywhere in this design (`~/.claude` holds
  `settings.json`, which IS the posture), it collides with `dc_link_state`'s symlink, and the slug
  is inexpressible in `container.json`. So `scripts/link-claude-memory.sh` symlinks each side's
  `memory` dir at `~/.jkb/claude-memory/<repo>/` — inside the bind that already exists. It migrates
  file by file and **never overwrites**: a name on both sides is left alone and reported. Opt-in on
  the host (`setup.sh --link-memory`), because `post-merge` re-runs `setup.sh` and a `git pull`
  must not rearrange somebody's `~/.claude`. Stated plainly: agent-writable prose flowing from a
  bounded context to a less bounded one is a channel, argued for rather than added by reflex.
- **A session worktree is ARCHIVED, never deleted** (`jkb-cli/src/archive.rs`). `git worktree
  remove` unlinks recursively and stops at the first refusal; from inside a sandboxed session that
  refusal is `<worktree>/.claude/settings.json` — Claude Code protects a project's policy files
  from the agent whose policy they are — and 152 files were already gone, with the error naming the
  *directory* and not the 62,421 lines. Disposal is now one atomic `fs::rename` into
  `<repo>/.jkb/archive/<session>-<stamp>`: partial destruction stops being representable rather
  than being guarded against. `jkb task reap` deletes an archive once it is 30 days old, probing
  with `remove_dir` first (`EPERM` vs `ENOTEMPTY`) so it never begins a walk it cannot finish.
- **The refusal is scoped to the session's OWN working directories, and that is what makes
  deferral work.** Measured across five live worktrees: only the session's own tree answers
  `EPERM`; every other one answers `ENOTEMPTY`. And the deny is **not** ours — `auto-mode-posture
  .json` names only `~/.claude/*` — so there is no knob, and there should not be: a session that
  could write its own `.claude/hooks/` could run anything. So `land` never blocks: it grafts,
  records the worktree it could not move, applies its plan (D48's ordering intact), and any other
  process finishes it. `jkb service install` now writes **two** units — `com.jkb.sync` and
  `com.jkb.reap` — kept apart so a wedged file watcher does not also stop every deferred landing.
  `jkb doctor` reports what is outstanding; `--fix` sweeps it.
- **The reviewer found the second disposal route, and it is the shape this repo keeps meeting.**
  `jkb task abandon` still called `git worktree remove` — the verb an operator reaches for to clear
  the directory a deferred landing leaves behind was the one that gutted it. Both verbs now go
  through `archive::dispose`, which is the callee that remembers the rule instead of two call sites
  that must. Its `delete_branch` is the caller's, because a landing's branch is a duplicate of
  commits already in the target while an abandoned branch holds the only copy.
- **A record names a path and a branch, and both are reusable names**, so the sweep establishes
  identity before acting: git still registers that path as a worktree, it is still on the commit
  the landing recorded (`Entry.head`), and it is clean. Remove a deferred worktree by hand and
  `jkb task work` recreates a session at the same path on the same branch; a sweep keyed on those
  two would archive the live tree and force-delete its branch. A commit id is not reused.
- **Unknown is not settled, and one sweep runs at a time.** A repo root the sweep cannot reach used
  to clear the record — ordinary once host and container share `~/.jkb` at different paths, and it
  deleted the only record of a live worktree. Two concurrent sweeps did lose each other's updates
  (the second finds the worktree gone and drops the record the first just wrote), so a `SweepLock`
  covers the reads as well as the writes, with `LandLock`'s rule that a lock is stale only when its
  holder is **proven** gone.
- **`workspaceFolder` follows the folder you opened, and `initializeCommand` refuses one the mount
  cannot place.** A literal path under the target opens whichever repo sits there — for a session
  worktree, the main checkout — silently, with every guard passing, because the wrong repo is a
  perfectly good repo. `check-config.sh` asserts both halves and `mutate-config.sh` watches each
  fail.
- **The memory linker decides the whole migration before moving anything**, and refuses a store
  holding anything but plain files (a symlink planted by either side redirects the other's reads
  and writes, including back into `~/.claude`). `verify.sh` **asks** it for the state rather than
  inferring breakage from a missing link — the linker leaves the link absent on purpose in states
  it recognises, so the inference failed `postCreate` for a state the design calls normal.
- **The record carries the decision that produced it (`archive::Plan`), and can be cancelled.**
  Three findings in review 2 were one cause: `dispose` took `delete_branch` as an argument and
  threw it away, so the reaper applied *land's* defaults to an `abandon` record and force-deleted
  the branch the verb had just printed "kept" for; `--force`'s acceptance of a dirty tree was
  likewise unrecorded, so the sweep's own dirty check held that record for ever; and nothing could
  revoke a record, so `jkb task work` resuming a deferred session got the directory back with a
  reaper still holding a claim on it — which then either archived the checkout the operator was
  sitting in or, once they committed, refused for ever as "a different session reusing the name".
  `Plan` is part of the `Entry`, `archive::revoke` is the cancel, and `task work` calls it.
- **A guard whose expectation no longer matches its subject is a guard nobody has seen fire.**
  Rewording verify.sh's `--declare` refusal left `mutate-verify.sh` grepping for text it never
  prints — and that harness needs Docker, so the gate could not notice. `check-config.sh` now
  checks statically that every expectation is a string verify.sh can print. Same round: an
  assertion that the workspace is mounted had been rewritten to say the directory containing the
  running script contains a `Cargo.toml`, true by construction (its third wording, below, is the
  one the harness accepted); and a self-test assertion could
  not fail because `note` wrote to the stdout `run` was capturing, so every clean link run exited
  1 with its own report eaten.
- **`init-firewall.sh` discovers its workspace instead of naming it.** The hard-coded
  `~/repos/jkb` was a statement about whichever checkout sat there once the mount widened — it
  would snapshot another checkout's egress allowlist as the root-owned list every later start
  runs on. It cannot be told which (the sudoers grant forbids arguments, and an environment
  variable is agent-settable), so one repo carrying `scripts/auto-mode-posture.json` is the
  answer and two is a refusal — on the **first raise only**, because a later raise that exits
  non-zero leaves the rules unapplied, and unapplied rules mean unrestricted egress.
- **A remedy the machine does not accept is worse than no remedy.** `check-workspace.sh` advised
  setting `JKB_REPOS_DIR`, read by nothing else in the tree: following the advice switched the
  preflight off without moving the mount, producing the exact silent wrong-checkout open the file
  exists to prevent. One statement of where repos live, and it is `container.json`'s.
- **The sweep may only delete inside the tree it owns.** The worst defect on this branch, and it
  survived two reviews: the retention arm passed whatever absolute path a record's `archive` field
  named to `remove_dir_all`. The record store lives in `~/.jkb`, which is bind-mounted into the
  container and granted in the posture's `allowWrite`, while the host's reaper is a launchd agent
  outside every sandbox — so an agent-writable JSON file steered an unsandboxed recursive delete
  at any directory, past a probe that answers "permitted" for any ordinary one. Corruption reaches
  the same place with no adversary. Both paths are constrained to `<repo>/.jkb/{work,archive}`,
  checked **once above both arms** (D45.5's rule: a condition that dominates every arm belongs
  above the dispatch) — the pending arm had a guard and the archived arm had nothing.
- **A refusal that installs no rule is not a refusal.** Two guards in `init-firewall.sh` exited
  before any iptables rule was applied, and rules do not survive a container restart — so a
  truncated snapshot, the state one of those guards' own comments calls real, left unrestricted
  egress on every later start, permanently (the snapshot is root-owned and 0444). `fail_closed` is
  defined above every refusal now and `check-config.sh` fails the gate on an `exit 1` in that file
  outside it. That guard's own first version anchored its pattern to the start of a line and
  walked past `|| exit 1` — the exact shape it existed for; the mutation caught it.
- **A pid is meaningless without the host that issued it.** `Liveness::Process` carried only the
  pid, so a claim (or a sweep lock) written inside the container was probed against this machine's
  process table: a live owner reported dead and freed, or a dead one reported alive. It carries
  the host, and a foreign one is `Unknown`, which frees nothing. `hostname()` also stopped falling
  back to the literal `"localhost"` — which both sides of the boundary answered, so the rule's two
  sides gave the same name and the rule was not one.
- **The container is where deferral is normal, and it had no finisher.** A session cannot archive
  its own checkout, so every `land` in there records one — and the host's reaper correctly holds
  those records, because it cannot see `/home/vscode/...`. There is no init system in the
  container to run a service, so `postStartCommand` sweeps once per start, best-effort behind the
  firewall raise.
- **The container harness settled two findings no amount of reading would have.** `verify.sh`'s
  workspace assertion went through three wordings — a hard-coded path (describes whichever
  checkout sits there), the script's own directory (true wherever it can run), and the declared
  target in `mountinfo` (which the harness's own bind layout never produces, so every mutation
  reported CAUGHT and then the control failed and the run judged nothing). It asks whether this
  checkout is inside a mount point that is both mounted and declared, `--declare` folded in. And
  the harness's negative control could not fail: the health check establishes the control's exit
  code is 0 and `judge` reports CAUGHT only on non-zero, so `MATCHER IS BROKEN` was unreachable —
  in the file whose whole job is finding guards that cannot fire. It asks the discriminating half
  instead: the label must appear in a healthy container and must not be on a `FAIL` line.
- **The record store is untrusted input, so it gets a parser.** The containment guard that closed
  round 3's arbitrary-delete finding did not hold: `Path::starts_with` compares components without
  interpreting them, so `<repo>/.jkb/archive/../../../Documents` "starts with" the archive root
  while naming something else. A check the sweep remembers to call, over paths nobody normalized,
  is two mistakes. `Entry` is now the wire form and is trusted for nothing; `archive::Record` is
  what the sweep sees; `Record::parse` is the only way between them, so no arm can be written that
  skips it. `..` and `.` are **refused** rather than resolved — nothing here writes one, so a
  record containing one is corrupt or hostile and neither deserves a best-effort reading.
- **Reachability belongs above the dispatch, like containment.** The pending arm held an
  unreachable `repo_root`; the archived arm read "not visible from here" as "somebody removed it
  by hand" and dropped the record — so each side of the container bind destroyed the other's
  archived records, and the multi-gigabyte checkout each named became unreferenced and permanent.
  An absent directory is evidence of removal only when the repo it lives under is reachable.
- **One disposal, one record.** The marker's name was a pure function of the worktree path, and a
  session name is reused: abandon, reopen, `task work` mints the same name at the same path, and
  the next disposal wrote over the first record. `Entry.head` exists because a path and a branch
  are reusable names; the record's own identity was still the path.
- **A lock that nothing can break is a wedge.** Making a foreign host `Unknown` was right, and it
  made the sweep lock permanent for a container killed mid-sweep and then rebuilt — its hostname
  gone with it, so every sweep on both sides no-ops for ever. The default stays (breaking a live
  sweeper's lock is what the lock prevents); what was missing is an escape a person can take, so
  the refusal names the lock file and its holder and `jkb task reap --break-lock` exists.
- **Sharing memory through `~/.jkb` widens what sandboxed Bash can reach, and that is a decision,
  not an oversight.** `~/.claude/projects` is under the posture's blanket `denyRead` and in no
  allow list; `~/.jkb` is in `allowRead` **and** `allowWrite`, because the database lives there.
  Linking therefore moves auto-memory from a place sandboxed Bash cannot touch into one where a
  single auto-approved command rewrites it — for this repo and, through the same grant, for every
  other repo's store. Memory is prose re-injected into every later session, so the channel turns a
  one-shot injection into a durable one. The posture has no write-deny to carve `claude-memory`
  back out with (`filesystem` offers `denyRead`/`allowRead`/`allowWrite` and nothing else).
  **Weighed against a dedicated store with its own declared bind, and `~/.jkb` was chosen**: both
  ends are the same person's agents, it is prose rather than code, and the host side is opt-in
  (`setup.sh --link-memory`), never created by a `git pull`.
- **The load-bearing fact underneath it, measured rather than assumed:** file tools and Bash are
  bounded by *different* mechanisms. The sandbox's `filesystem` block governs Bash; the
  `permissions` rules govern `Read`/`Edit`/`Write`. So an agent writes memory through the Write
  tool wherever the store lives — moving it somewhere the posture does not grant would not have
  stopped agents writing memory, only sandboxed Bash. Anyone revisiting this should start there,
  because it is the fact that decides what the alternatives actually buy.
- **`verify.sh` refuses to run outside the container, and never passes on a table it could not
  read.** Run on the macOS host it printed fourteen confident FAILs about a machine that was never
  the subject — and two `ok` lines, because `/proc/self/mountinfo` does not exist there, so the
  mount-boundary check compared an EMPTY set and passed. `EXPECTED` had been guarded against
  emptiness since the day it was derived; `actual` never was, in the one assertion the file exists
  for. The read is now kept separate from the result, and an unreadable table is a FAIL. Found by
  running the script in the wrong place, which is the sort of thing no amount of reading finds.
- **There is one way to ask whether the container is healthy, and it is not a `docker run` you
  write yourself.** A hand-rolled one printed in a refusal message omitted the seccomp profile,
  `NET_ADMIN`, the `~/.jkb` bind and the whole preamble that raises the firewall and installs the
  posture — so it produced a dozen FAILs that read as a broken container instead of as a wrong
  command. `mutate-verify.sh --control` reuses the exact flags and preamble every mutation runs
  against, so "is my container ok" and "did that guard fire" cannot be answered about two
  different containers.
- **A harness must refuse a subject it cannot find, before it reports on one.** A stray em dash
  pasted as the image name — copied out of prose where one followed the command — made every
  `docker run` exit 125 ("could not start the container"), which `judge` reads as a non-zero
  `verify.sh` and reports as a guard that did not fire: nine MISSED lines and three BUILD-FAILED
  blocks before the control finally called them unattributable. Thirteen alarming lines for a fact
  about the command. The control did its job, and doing its job that late is the defect; the image
  and the argument count are checked up front now, the same shape as the docker-on-PATH and
  daemon-reachable checks already there for exactly this reason.
- **One idea closes three of round 5's findings: when several members of a set could answer, name
  which one is authoritative instead of letting whichever is reached answer.**
  - *Which variant carries the host question.* `Liveness::Process` was host-qualified in round 3
    and `Liveness::Worktree` was not — same enum, same boundary. A host session claims as
    `session:<pid>:/Users/…`; in the container that path is absent, `try_exists` said `false`, and
    `reclaim_dead` freed the claim of a session running on the host. The session id carries no
    host, so the question is asked of the filesystem: **an absence is only proof where the place
    it would be is visible** — the parent directory must exist. That is the archive sweep's
    reachability rule, one level down, and it needs no change to an id format already in databases.
  - *Which observation is the state.* `--status-file` recorded `status_of`'s answer from BEFORE
    the linker ran, to preserve an alarm the repair clears — and that made a successful
    FIRST-EVER link record `unlinked`, which `verify.sh` treats as fatal. Every new container
    failed `postCreate` exactly once, on a feature working. `link_one` already returns `exposed`
    for the case the pre-state existed to keep, so recording the OUTCOME loses nothing.
  - *Which record governs.* Giving each disposal its own marker (round 4) opened a second way to
    the regression `Plan` was added to prevent: `abandon --delete-branch`, change your mind,
    `abandon` again — two pending records for one tree, and the older one still force-deletes the
    branch the later run printed "kept" for. The newest pending record per worktree governs and
    the rest are superseded; **archived** records are never superseded, because each names a
    distinct archive that still has to be swept.
- **The `ERR` trap added in round 4 was worse than the hole it closed, and only measurement showed
  it.** `getent` exits 2 for a name with no A record, `pipefail` carries that out, and a BARE
  assignment is a simple command in no conditional context — so `errexit` fired, `set -E` sent it
  to `fail_closed`, and **one unresolvable domain took the whole raise to deny-all with no
  allowlist**, blaming "an unexpected failure at line 173" while the two arms written for exactly
  that state became unreachable. Measured which shapes trip it: a bare `x="$(cmd)"` does; `x="$(cmd)"
  || x=""` and `if x="$(cmd)"` do not. The trap stays — it is what catches an abort no refusal
  wrote — and the one bare assignment gained a fallback.
- **A green harness is evidence about the paths it runs, not about a file.** After the container
  harness went green I said the container files were no longer unexercised. The firewall defect
  above lives on the DNS-failure path, which the harness does not drive: it covers the happy path
  and the no-`NET_ADMIN` path. Generalising a run into a property of a file is the same shape as
  every "unknown reported as a definite answer" this branch has been correcting.
- **A test fixture must not assume anything about the machine it runs on.** Three sweep tests
  named `/home/vscode/repos/jkb` as "a repo this machine cannot reach" — which is precisely the
  bind target this change adds, so inside the container the path EXISTS: the tests took the
  opposite arm, **the gate was red in the environment the change exists to introduce**, and two of
  them ran `git worktree prune` against the real checkout. Unreachability is a property of the
  fixture now (a tempdir path never created), not a claim about the world.
- **A record consulted INSTEAD of asking makes a guard unfirable.** `verify.sh` read setup.sh's
  create-time memory record in place of asking the linker, so the store guard could never fire
  after `postCreate` — a redirect planted the next day reported `ok` — and an `exposed` record
  could not be cleared by the remedy the failure printed. It asks live every time, and the record
  is an additional alarm, consumed once reported. The record still earns its place: the linker
  *repairs*, so a live question asked afterwards sees the harmless state and not the one that was
  true at create.
- **A caller that downgrades a refusal to a note undoes it.** `revoke` refuses when a sweep holds
  the lock, and its doc says the honest outcome is for the operator to re-run — but `task work`
  printed a note and handed the session back, licensing that sweep to archive the checkout it had
  just told the operator to work in. The cancellation now happens **before** the worktree is
  handed over, and a refusal stops the verb.
- **A mutation can pass on the symptom rather than the behaviour.** The `--dns 127.0.0.1` case
  reported CAUGHT whether or not `fail_closed` installed anything, because both egress probes
  resolve a name and a dead resolver breaks them by itself. `fail_closed` now writes
  `/run/jkb-egress-failed`, cleared only by a successful raise, so `verify.sh` — which runs as
  `vscode` and cannot ask iptables anything — observes what the firewall DID rather than what
  egress happens to do.
- **`jkb task reap` no longer opens the database.** It touches no rows, but ran through the
  dispatch that migrates first — so the shared-`jkb.db` divergence this project documents as
  routine turned the one process that finishes every deferred landing into a launchd restart-loop,
  with the only symptom in `reap.log`.
- **`gitrepo::deletions_only`** tells a part-way removal from work in progress. The second land
  attempt refused with *"it has uncommitted changes — commit them in the session first"*, which
  over 152 deletions means committing the wreckage. Asked as four whitespace-free git questions,
  not by parsing `status --porcelain` — whose leading status column is exactly what the trimming
  capture helper eats.

## The container is started by a script and attached to, not opened by Dev Containers (D49)

Everything above about mounts, seccomp, the firewall and the harnesses is unchanged. What changed
is who starts the container, and it removes a limitation rather than working around one.

- **Dev Containers could not open a folder nested inside the mount.** `workspaceFolder` can only be
  built from `${localWorkspaceFolderBasename}` — there is no variable for a path *relative* to the
  mount, and `initializeCommand` cannot supply one (a subprocess cannot set its parent's
  environment, and substitution has already happened). So `~/repos/jkb/.jkb/work/sess` resolved to
  `/home/vscode/repos/sess`, which does not exist, and the near-miss was worse than the miss: a
  literal fallback opened a **different checkout** silently, with every guard passing, because the
  wrong repo is a perfectly good repo. **This is what supersedes the `workspaceFolder`/
  `initializeCommand`/`check-workspace.sh` bullets in the section above.**
- **The cost was concrete: a change to the container could only be tested after landing it.** A
  session worktree carries its own `.container/`, so opening one is exactly how you would exercise
  an edit — and that was the one thing Dev Containers could not do. Two rebuilds in a row tested
  `main` instead, and read as the container being broken.
- **Attaching has no `workspaceFolder`.** `.container/run.sh` starts one long-lived container from
  `container.json`; you attach ("Dev Containers: Attach to Running Container") and open any path
  inside, at any depth, with `code <path>` from an attached terminal opening more windows on the
  same container. So the limitation and the guard that policed it both stop existing — and **one**
  container serves every repo under `~/repos` rather than one per opened folder, which is what the
  widened mount was for in the first place.
- **Three alternatives were worked out and rejected, each on a fact rather than a preference.** A
  relative symlink `~/repos/<name>` → the worktree gives `find_workspace_posture` a second
  directory carrying `scripts/auto-mode-posture.json`, and it `fail_closed`s on first raise — that
  refusal is deliberate and must not learn an exception. A nested bind at
  `/home/vscode/repos/<basename>` has the same effect for the same reason. A **constant** workspace
  path (`workspaceMount` from `${localWorkspaceFolder}` to a fixed target) handles any depth and
  breaks session identity instead: claims are `session:<pid>:<worktree>` and liveness is *does that
  worktree exist*, so every container would record one shared path and no claim would ever be
  reclaimable.
- **The file is `container.json` and the folder is `.container/`, and the names are the point.**
  VS Code detects `devcontainer.json` and offers *Reopen in Container*, which would be a second way
  to get a container — one built by Dev Containers, one by `run.sh` — and two launch paths that
  start identical drift, in the mount list, which is the security boundary. Detection is
  file-based as far as is known, so renaming the folder is belt-and-braces; it is done anyway
  because the name asserted something no longer true, which is the same argument that renamed the
  file.
- **What replaces the deleted guard is a smaller, checkable claim.** `run.sh` is now the only thing
  that applies `container.json`, so a key nobody reads is possible and looks exactly like
  configuration — and the key most likely to be added is another `mounts`-shaped one.
  `run.sh --consumed-keys` names what it reads and `check-config.sh` fails on any declared key not
  in that list, so adding one forces the decision at the moment it is added rather than at the
  moment somebody notices it never applied. `run.sh --self-test` is in `./scripts/check.sh`.
- **An unset `${localEnv:VAR}` is REFUSED, where Dev Containers substitutes the empty string.**
  That default is how `source=${localEnv:HOME}/repos` quietly becomes `source=/repos` — a different
  host directory, mounted, with nothing to notice. A boundary must not be able to move because a
  variable was not set.
- **The lifecycle rule survives its mechanism.** The firewall is raised **first**, on every start,
  because iptables rules live in the container's network namespace and do not survive a restart and
  because the rest of setup includes a toolchain download.

### Egress is asked of the kernel, not of a record (D51)

Design: `openspec/changes/jkb-egress-liveness/`. Supersedes the **decision** half of D50 below,
whose diagnosis and reason-record stand. Review round 3 (`low`, 3 reviewers, 17 raw → 10 findings,
4 must-fix, none pre-existing) is the input; the findings cluster into seven causes, not ten sites.

- **D50 made the raise record what it established. It never made the record say what it is about.**
  A verdict is an *event* — "at some moment, a raise established X" — and every reader asks a
  *present-tense* question: is egress bounded **now**? Nothing said when an older verdict stops
  counting, so it counted for ever, and `docker stop` destroys every iptables rule while the file
  survives in the writable layer. A raise that died before recording left the **previous start's**
  `allowlisted` standing; the entrypoint read it, printed nothing, and exec'd the agent onto an
  empty OUTPUT chain — unrestricted egress, silently, on exactly the `docker start` path the
  entrypoint was added to cover.
- **This repo already had the lesson, one subsystem over**: *"evidence of a landing is spent once
  the task is put back to work."* Turning a history into a present-tense answer needs a rule for
  when an older row stops counting; there it was written separately in each reader and they
  disagreed, here it was written nowhere.
- **So ask the kernel.** `egress-status.sh` is a second root-owned, argument-less, **read-only**
  script granted by sudoers exactly as the firewall is; it reads the live filter chains and prints
  one word. `entrypoint.sh` boots on that and `verify.sh` reports from it — which is what finally
  makes verify's own comment true, since it claimed to report *"what the firewall DID"* while
  reading a file that can outlive what it describes.
- **Chosen over making the record trustworthy**, which is the obvious repair: a `state=raising`
  marker plus a token naming the network namespace plus a staleness rule — three mechanisms
  reconstructing what the kernel will simply tell you. Asking directly removes the failure class
  instead of guarding it, and covers a case none of them do: `--entrypoint bash`, the escape D50's
  own refusal recommends, runs **no raise at all**, so no marker is ever written.
- **The record still supplies the REASON**, which the kernel cannot — that DNS failed, that the
  snapshot was truncated. State from the probe, explanation from the record, and a record
  disagreeing with the probe is reported as drift rather than obeyed.
- **A measurement must measure its own claim.** `v6_state` was named *"is IPv6 egress provably
  closed"* and asked *"does a non-loopback address exist"*, which was wrong in **both** directions.
  Every container on a default Docker bridge gets a link-local `fe80::/64` when the host kernel has
  IPv6 — nothing can leave — and that read as `open`, so ordinary containers refused to boot and
  their operators were pushed onto the permanent override. And an *unreadable* table returned
  `absent`, which the rule reads as provably closed: `grep` exits 0/1/**2** and only two of those
  are measurements, with the third hidden behind `2>/dev/null`. It measures a **path** now — no
  off-link address **and** no default route — and a failed read is `open`.
- **A fact that can be read is never inferred.** `verify.sh` deduced the override from an
  `unfiltered` state. So an operator who armed it and then fixed the host got `allowlisted` and
  silence, with the boot gate still disarmed — the exact condition the override's own justification
  says must never be invisible — and an `unfiltered` state reached any other way was *blamed* on a
  variable that may be `0`. `docker exec` inherits `containerEnv`; it is read directly.
- **One exit code cannot carry the transport and the answer.** `docker exec` exits 1 both when the
  container is gone and when the probed condition is false, and `run.sh` sampled liveness **once**,
  before the entrypoint had decided — `--detach` returns as PID 1 starts. So a refusal seconds later
  was misread three ways, ending with *"the container is running and attachable"* about a container
  that was not, from a verifier that never ran. There is one `docker exec` wrapper now, it waits for
  the entrypoint to settle, and the setup probe **prints a word** instead of leaning on an exit code
  the daemon also uses.
- **Booting is not endorsing.** `--open` launches a VS Code window, which *is* starting a session, so
  it opens only on a clean verify. The override buys a container you can attach to **by hand** and
  diagnose. `verify.sh` gained a distinct exit code for "every failure is a condition this container
  was configured to accept", so the failure is still reported at full volume every run while a caller
  can tell it from a broken boundary — and the message stops advising you to fix a condition the
  design requires to keep failing.
- **Serialise in the callee.** Two raises run concurrently on every fresh create (the entrypoint's
  and `run.sh`'s re-raise) over one ipset, one chain and one record; the interleavings install a
  blanket deny on a machine with healthy DNS. `flock` at the top of `init-firewall.sh`, so a third
  way to start a raise is covered without being told.
- **A guard and its mutation must both discriminate.** The round-2 guard grepped the string
  `verify.sh`, which `run.sh` also names in three failure **messages** — text on the pass path and
  the fail path both — so deleting the invocation left it green; and the mutation written to watch it
  fail rewrote **every** occurrence, so it never established which one the guard reads. Two rules
  now: anchor on the **invocation**, and **a mutation changes exactly one thing.** The same cause
  produced an assertion named *"the reason reaches the reader intact"* that passed only because it
  was fed a single-line reason the writer never emits, while every real reason is multi-line and the
  readers' `head -1` stripped the remedy. `record_verdict` flattens newlines — the one place it can
  be enforced.
- **Two lists that must agree, derived.** `scripts/check.sh` and `ci.yml` each enumerate the
  container self-tests; `check-config.sh` compares them, plus a second guard that every
  `--self-test` in `.container/` is run by the gate, since both lists could otherwise agree about
  running nothing. **It found a live one immediately**: `link-claude-memory.sh --self-test` ran in
  the gate and nowhere in CI.

### The raise records a verdict, and the container boots on it (D50)

Design: `openspec/changes/jkb-egress-verdict/`. Two review rounds produced a must-fix in this one
subsystem, **the second caused by the first's fix**, which is the signal to model rather than
patch again.

- **`init-firewall.sh` computed whether egress was denied, printed it, and threw it away.** What it
  recorded was a *cause string*, written by `fail_closed` **before** any `iptables` call with every
  one of those calls `|| true` — so the marker meant *"`fail_closed` ran"*, which is a different
  fact from *"egress is denied"*. Its four endings collapsed into two distinguishable ones, and the
  two that mattered most — deny-all installed, and deny-all **failed** — wrote the identical thing.
  `entrypoint.sh` read presence as proof of denial, printed `egress is DENIED`, and booted. On the
  second one that sentence is false and an unattended agent gets an open network, on precisely the
  `docker start` routes the entrypoint was added to cover.
- **It is the house defect — an unestablished answer spelled as a definite one** — reproduced while
  fixing a finding about a boundary that depended on its caller. `Fact::Unknown` collapsed to
  `false`, `ahead_count` returning `0`, `has_own_commits` answering *no* when `rev-list` failed.
- **The raise records a verdict** (`/run/jkb-egress-verdict`, root-owned, written on every ending):
  `state=allowlisted|denied|unfiltered` plus the per-family detail and the reason. **Absence is its
  own answer** — dying before the record leaves `unknown`, never `denied` — which is what makes
  writing it *late* safe, where writing the old marker early was what made it uninformative.
- **Denial must be ESTABLISHED, on both families.** A family that is not provably closed is open.
  That rule now reaches the **success** path too: it used to allowlist IPv4, print *"IPv6 is
  UNFILTERED"*, and report success anyway, its own comment conceding this was *"safe only because
  the container has no IPv6 route, which is not checked here."*
- **`unfiltered` refuses to boot.** The asymmetry decides it: refusing costs a debugging session and
  prints its own escape (`docker run --entrypoint bash`); booting costs the guarantee, silently, on
  the paths nothing else watches. A container in that state is nearly useless anyway — no allowlist
  means no npm, no crates.io, no `api.anthropic.com` — so staying up buys diagnosability, not work.
  `denied` still boots, loudly: it is safe, and it is the state you need to attach to in order to
  repair it.
- **…but absence of the path establishes denial too.** No `/proc/net/if_inet6`, or no non-loopback
  v6 address, means no source address and no route: `v6=absent` satisfies the rule rather than
  weakening it. Without that clause the container refuses to boot on any kernel lacking `ip6tables`
  *even where IPv6 is not in play at all*. It is a measurement, and an unobtainable measurement is
  `open` — which is also, exactly, what the existing `set -E`/ERR-trap guard demanded of the
  assignment. **That guard caught this change as it was written**: a bare `v6="$(v6_state)"` would
  have aborted the whole raise into `fail_closed`.
- **The rule is stated once (`verdict_state`), because it was already spelled twice.** The
  fail-closed path and the success path each decided "both families provably closed" in their own
  wording, and that is *how* the success path came to report success on unfiltered IPv6: one site
  was fixed and the other kept its own reading. They differ only in what bounded is called there —
  a blanket deny is `denied`, a raised allowlist is `allowlisted` — which is an argument, not a
  second rule. Found by this file's own question, *who else implements this rule*.
- **The writer's half now has a self-test, and it was the half that had none.** The reader had
  fourteen assertions and the measurement behind its verdict had zero — the wrong way round, since a
  verdict is only as good as what established it. `init-firewall.sh --self-test` is pure and
  path-injected (`JKB_INET6_PATH`, `JKB_EGRESS_VERDICT`): no iptables, no root, no `/proc`. It pins
  the rule as a **literal table** rather than a re-derivation — writing the expectation as a second
  copy of the condition passes for any condition, including the wrong one this replaced — and it
  round-trips `record_verdict` through the **real** `entrypoint.sh`, which is the one contract
  spanning two files that no static check can reach. All three groups were watched failing.
- **The vocabulary is single-sourced too** (`VERDICT_STATES`). A reader with no arm for a state the
  writer records drops it into `*`, and unknown means a container that refuses to boot **for ever**,
  over a word. `check-config.sh` requires both readers to carry an arm for every declared state;
  the self-test requires `verdict_state` to return nothing outside it. Neither half is decorative.
- **Two existing guards fired on this change, and exempting them needed a guard of its own.** The
  self-test block sits above `set -E`, so the stray-exit and bare-substitution rules do not apply to
  it — but that is only true while it *exits* before the trap is installed. So the exemption is
  paired with a check that the block ends in an exit: a refactor that let it fall through would run
  during a real raise while still exempt from the two rules that make one survivable.
- **The one escape is recorded.** `JKB_EGRESS_ACCEPT_UNFILTERED=1` in `containerEnv` — fixed at
  create, so a session inside cannot grant it to itself, and `docker start` honours the same
  decision, which a `run.sh` flag could not. `verify.sh` reports it as a **failure** every run for
  as long as it is set: an override nobody can see is indistinguishable from a rule that does not
  exist. Without it, a host with real IPv6 and no `ip6tables` could not start the container at all.
- **`run.sh` learned that the entrypoint can refuse.** `docker run --detach` still returns 0, so
  the next `docker exec` failed with a bare *"Container … is not running"* and `set -e` killed the
  script, leaving the explanation in `docker logs` which nothing pointed at. And the synchronous
  re-raise's failure is now recorded rather than fatal — under `set -e` it skipped the reap and
  `verify.sh`, i.e. the very reporting the design assigns to verify.
- **`--open` is an action, not a note.** Carrying verify's exit code so the attach instructions
  still print is right; going on to launch a window into a container whose verifier just reported
  undeclared mounts is not. Before the result was carried, `set -e` made that unreachable.
- **Four guards, because a guard that cannot fire is this directory's recurring defect.** Nothing
  asserted the Dockerfile still wires the `ENTRYPOINT` — delete one line and every check stays
  green while `docker start` comes up unfiltered. The round-1 "one verifier" guard checked only
  that `setup.sh` does *not* verify while its passing line claimed `run.sh` does. The verdict path
  is spelled by one writer and two readers in three processes that cannot share a variable, so
  their agreement is asserted like the setup marker's. **And the mutation harness caught the
  drift-detection guard being unable to fire**: grepping for the literal path it already knew could
  only ever find one distinct value, so it now extracts each file's own spelling and compares those.

### What review round 1 found: replacing a lifecycle moves its guarantees onto whoever replaces it

Fifteen findings, four must-fix, **every one `introduced`** — and three of the four are one
sentence: `postCreateCommand`/`postStartCommand` were not just *steps*, they were the statements
*this happens on every start* and *this happens until it succeeds*. Deleting the lifecycle deleted
the guarantees while the steps survived, and each defect is one guarantee that then had no owner.

- **The firewall belongs to the CONTAINER, not to `run.sh`.** As one caller's step, `docker start
  jkb-dev`, Docker Desktop's start button and a daemon restart all brought the container up with no
  allowlist, and nothing checked, because attaching runs nothing. It is the image's **ENTRYPOINT**
  now, so every route raises it. `run.sh` re-raises it synchronously — not a second rule, the raise
  is idempotent, but `docker run` returns before the entrypoint finishes and the next `docker exec`
  would race it. Two failure modes, answered differently: `init-firewall.sh`'s own `fail_closed`
  has already installed deny-all and recorded why, so the container stays **up** (egress denied,
  `verify.sh` reports the marker, a person can repair it); a failure that left no marker installed
  no rules and **refuses to run**.
- **"Did this invocation create the container" is not "did setup finish".** `fresh=1` is true a
  minute before setup completes, so an interrupted first run left `setup.sh` permanently
  unreachable — every later run took the verify arm, and only `--rm` escaped. The arm is chosen on
  a marker `setup.sh` writes as its **last** act. It is spelled in two files that cannot share a
  variable (one runs on the host, one in the container), so `check-config.sh` asserts they agree:
  drift there re-runs the whole of setup for ever while reporting success, which reads as slowness
  rather than as a bug.
- **A fingerprint of the derived arguments included the host checkout path.** `runArgs` names the
  seccomp profile by `${localWorkspaceFolder}`, so a session worktree — *the case this change
  exists to enable* — computed a different hash from its main checkout, was told `created from a
  different container.json` about a file that had not changed, and was advised `--rm`, destroying
  the shared container plus `~/.vscode-server`, its extensions and `~/.jkb-ui-build`, none of which
  are in a volume. Then the main checkout refused identically: two checkouts ping-ponging over a
  declaration they agreed on. The workspace root is normalised out and the profile's **content** is
  folded in, so a profile that really differs still forces a recreate.
- **An assertion whose two conditions used to be one.** `verify.sh` checked the declared extensions
  under the same condition `setup.sh` installs them under — true under Dev Containers, where the
  server was unpacked before `postCreate`. Now the server arrives when you **attach**, so setup
  installs nothing and the next `run.sh` finds a server with nothing in it: fatal, under `set -e`,
  with the remedy "rebuild the container" — which reproduces that exact state. The never-installed
  case reports and names `install-extensions.sh`; a server that has extensions but is missing a
  declared one is still a failure, because that is the original bug.
- **A verify that aborts is a verify that suppresses everything after it.** The deferred-archive
  reap was unconditional in `postStartCommand` and ended up downstream of a fatal `verify.sh`, so
  one unrelated assertion disabled the only reaper that can finish container-side archive records.
  It runs **before** the verify now, and the verify's result is *carried* rather than exiting at
  the point of failure — several assertions name a remedy you run from inside an attached window,
  and dying printed the problem while withholding the way to fix it. The exit code is still
  verify's.
- **Two guards could not fire and one was inert.** `fetch-extensions.sh --self-test` was called by
  nothing, so the marketplace URL derivation was exercised by no automated run; a zero-length
  extension list staged nothing and exited 0; and `consumed_keys()` listed `name` and `build` while
  `run.sh` read neither — so "every key is applied" was a true sentence about two inert
  declarations, and the fix for the *next* inert key would have been to add it to the list, which
  silences the check rather than satisfying it. Both keys are read now.
- **Deriving a list forces a distinction that enumerating one hides.** The firewall-argument guard
  named `setup.sh` and `run.sh` by hand, in a directory this change gave a third caller. Derived
  over the directory, it immediately produced three false positives — the file name in a comment,
  in an error message, and in a list of paths to `chmod` — because "mentions it" was only ever a
  good enough proxy for "calls it" across two hand-picked files. It is anchored on `sudo` now,
  which is the only way it *can* be invoked.
- **`--build` rebuilt the image and left the container on the old one**, so a newly staged `.vsix`
  never arrived while `install-extensions.sh` said "rebuild the container" — advice just followed.
  The staleness check's own argument (`docker start` reuses what the container was built with)
  applies to the image and was applied only to the arguments.
