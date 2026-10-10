<!-- generated from jkb design design:sandbox-and-container-18dcdf1cb383c738d9fcf2, edit there (version 150.lQGBxILJ67HaCwGBiPTuw_2eCAGC7L6l55ObDQGF4qvB7aP5CgGI9qLNgb3oBAGIxpOayp6bBgGN8q_q5rrRCQGP9PG1nterDwGT9MiXrtGuAQGUnrS1lq3iDAGWyPvL5_T7BgGWlKv16a73BwGXpuOvn_WFBQGZnJv575T6BwGW8s2L0Kq8DwGbwpLagd3DBAGcvMGb_omVAQGXwsagj9fWAQGhrvmpibqDCAGh6ovDpZAxAaHamdTYnrECAafG8IPXmK8OAamq79bt5OMNAaq4wuqL-fsEAa2mucfgtEoBrvyTs-i4ngwBr_Cx6am01gkBsMCv5enc6w0BsezKp46yqQ4BssblgK-U5gwBteKfv6L2pQEBtrCJw6nqsgEBt6aqgoOZ5g0Bt56mz4qj3gMBuvSonOS3-wwBuuSukNT1rgsBvPaI7cG2wQoBu4bRkrme7wEBu864taKv0wIBvqai1emCmwIBuu6RsKuWmgcBwaDIy-u2mggBxebSx9CWiAMBx7ru8_q72g8Bx5j3vY6-kwoBz_K0lcrmzAUB0KDLg_SknAIB0diSxpKxuA4B0pS6no3xKwHTpvbRu86gBAHQ7suh3bv4BwHVuKDZkuLzBQHe4ISxqaPtDgHj_vLu6NCKCwHlpuC78tptAebC7YbAnekJAenE0Jvw2PIJAeu-p4m02uYCAeyk34qe0eMKAe3sg6j7xPgHAe3go9-Q7_IIAe-CioHN8ZYOAfDOy_awkvsBAfCS2e7F4-wEAfP8r5aAsnkB857Es4el4AYB97Ti8Y6k2wQB-Pi2gcT9jwwB97rG9cH26g4B_L7uqceqXwH9_P-hoLvEAwH_yM3c8r6RBwGDi-W66ZZmAYSnuqn2lxkBh4mq27TznwEBidW-76jTuwMBiruwmZal1QkBirGFmPi8kwMBjMHW7P-NwgoBjdez65qB8gwBjv2J0PztbgGO1_-ChI6HCQGQr9Hzr-_9AgGRt-qA_NHXBAGSw8f3tYy1CgGTkYv61PufDQGUl4DgnOOGBgGU2dqq-sAPAZnFv97S6bQMAZnjm4zvsMQDAZqTpMDSjMcGAaPN6PX3veMCAaXL643AjfsNAaWp4ZGj4tQPAaj_x5fUk4gJAarz3u7l-MkPAauZhoCHocIGAayV9J3hlWHongetm--v_qzfCQGu2feK6ICJCQGq5Yvsv9ySAQGwt7eL8_u4BQGxuaSnmOCJAQGui8nTsZyBAwGu482vy_3yCQGy--bz4YGZCQG1397qi9z8DgG2_--j88X7BgG3n8rKkqOaDgG03_Pn9IeLAQG64e_q0pLcCAG8u8Pa3tgTAb2967XCoeAGAb6bvaaDyLcNAb_D_b3Tw8IBAcDRsfXy6asFAb7rsYrW6doLAb6rsJ_kvwIBvefvz8mKwgUBxIXAoqjJrw8Bv9ua_7vwmwMBxoHq0bKA9wgBv__51peWyAgByfvVn-CR8wIBy524geGnyw0Bzru5n4GKgQ0B0MXXvf-msAUB0cfI6MTerQ0B0YnfgrK-gAEB0aWd3ezMvgMB2s-suoyshQcB3IPEsLKw7gUB4Jej3MP9wgQB47_j2OWuiAEB55-i2daO_gwB5_2G9_j1ggYB7dOt88yL1QIB74-2mrCt7wIB8OHEh5z94gUB8a3m0Nuf5QYB89uZmN-WoQcB9bXRy6yo0gYB9fXX68TmtAIB97Xty4z00wMB-L_qs4vyzAMB-NPczcLtrgUB_fPflNCmggEB_emK9K2WjAMB_ZPkx-PiVAE, blake3 aafe114720cb13725371c56ccebf03149f49431da92d154c2977790097d7b0af) -->
# Sandbox and container

This design records the boundary that holds when the model is wrong. Claude Code runs in the
IDE and the CLI with no permission prompts, and what makes that acceptable is not the model's
judgement but two layers that fail for different reasons: Claude Code's own OS sandbox, under a
user-level posture (`scripts/auto-mode.sh`, `scripts/auto-mode-posture.json`), and a dev
container (`.container/`) with that sandbox nested inside it. Around them sit the container's
egress firewall and the verdict it boots on, the disposal of session worktrees that a sandboxed
session cannot delete, and the machinery that verifies the container: executed in CI, mutated,
and drift-checked against upstream.

The container's own internals live in [`.container/README.md`](../.container/README.md), which
stays hand-written: what each layer is for, the measurements under them, the mount list, the
PID-1 reaper, the namespace-identity discriminator, the kit, and how to run and verify it. That
file grows when `.container/` changes; this one records the boundary decisions. The `grep -q`
refusal is a repository-wide rule recorded in the git-hooks-installer design and enforced from
`scripts/tests/dev-scripts.test.sh`. Roles, the coordinator credential, the narrowed `~/.jkb`
mounts and harness attestation are the agents-and-roles design.

## The host posture: the sandbox is the guarantee, the classifier is the ergonomics

The posture is how an unattended agent runs on the host. It is installed and checked by
`scripts/auto-mode.sh` (`install`, `check`, `preflight`, `sandboxed`, `probe`, `run`) from the
one file `scripts/auto-mode-posture.json`, whose halves are `require`, `forbid` and `retire`.

### Claude Code already ships the boundary, so jkb does not build one

Claude Code 2.1.237 embeds `@anthropic-ai/sandbox-runtime`: every Bash command is re-executed
under `sandbox-exec` with a generated seatbelt profile (macOS) or `bubblewrap` + seccomp
(Linux), with a settings schema covering `filesystem.{allowWrite,denyWrite,denyRead,allowRead,
disabled}`, `network.{allowedDomains,strictAllowlist,allowUnixSockets,…}` and
`credentials.{files,envVars}`. That is per-command OS confinement plus an egress allowlist plus
credential denial, on the host, with the host toolchain.

### Two layers, because they fail differently

`--permission-mode auto` is a classifier (`claude auto-mode defaults` prints its rules as
English prose): it decides what is worth asking about, it can be wrong, and it buys ergonomics.
The sandbox bounds what happens when the classifier is wrong, and it buys the guarantee.
`autoAllowBashIfSandboxed` joins them: a sandboxed command is never shown to the classifier, so
the OS boundary is the check.

### `auto`, never `bypassPermissions`

The sandbox confines Bash and its children; it does not confine the in-process tools. The
schema says so about `strictAllowlist` ("in-process tools such as WebFetch are not gated by this
setting"). Skipping permissions leaves Read/Edit/Write/WebFetch unbounded, a hole the shape of
the file-editing tools. `auto` keeps the classifier over exactly what the kernel does not cover,
and the posture's `permissions.deny` rules close the named paths in both layers at once: the
schema merges `Read(...)` deny rules into `filesystem.denyRead` and `Edit(...)` into
`denyWrite`. One list, two enforcers, rather than two lists that drift.

### The posture is user-level, and Claude Code enforces that

Several keys are honoured only from user, managed or `--settings` scope, and the binary carries
an operator-posture guard that refuses to run when a repo's `.claude/settings.json` negates
`sandbox.enabled`, `sandbox.failIfUnavailable`, `sandbox.allowUnsandboxedCommands` or
`disableAllHooks` ("operator posture belongs in the user-level settings.json"). It likewise
refuses a project `env` block (`BASH_ENV`/`LD_PRELOAD`/`NODE_OPTIONS`/`GIT_*` are
unsandboxed-exec inlets). So a cloned repo cannot switch the posture off, and installing it into
a repo would silently drop half of it. It also has to hold in every repo under `~/repos`, which
is why it is not jkb configuration.

### Four settings, each closing one silent degradation

`failIfUnavailable: true` is the hard gate: its default is `false`, under which a warning prints
and commands run unsandboxed, the exact failure of believing you are protected.
`allowUnsandboxedCommands: false` deletes the `dangerouslyDisableSandbox` parameter; otherwise
one argument steps outside, and in auto mode nobody is asked. `network.strictAllowlist: true`
denies rather than prompting, because a prompt in auto mode is a question nobody answers.
`enabled: true` is the rest.

### File access is an allowlist, both ways

Writes are default-deny (workspace only), so `filesystem.allowWrite` is the allowlist. Reads are
default-deny too: `denyRead: ["~", "/Volumes"]` blankets the user's data and `allowRead`, which
takes precedence over `denyRead`, re-opens the work roots and the toolchain. System paths
(`/usr`, `/bin`, `/Library`) are deliberately not denied: a command that cannot read its own
dynamic linker cannot run, so "allowlist everything" is not a posture but an inoperative
machine. What is denied by default is your data, which is what a leak is about.

The in-process `Read` tool cannot be made default-deny. Claude Code's rule model is
deny-beats-allow with no re-allow, so a blanket `Read(~/**)` could never be punched through for
`~/repos`. There it is an enumerated deny list plus an empty `additionalDirectories`; the
stronger option (deny the `Read` tool outright and read through sandboxed `cat`/`rg`) is
offered, not imposed.

### What runs unsandboxed, and the three keys that answer it

The residual worth naming: `Read`/`Glob`/`Grep` (bounded by deny rules only),
`Write`/`Edit`/`NotebookEdit` (deny rules plus the permission scope), `WebFetch`/`WebSearch`
(the schema states in-process tools are not gated by `strictAllowlist`), MCP servers (long-lived
processes started at session start, never per-command wrapped; `jkb mcp` is one), hooks (not
evidenced as sandboxed anywhere in the binary), and the `claude` process itself. Bash and
everything it spawns, which is where the real capability lives, is the sandboxed part.

Three keys answer what can be answered. `permissions.ask: ["WebFetch"]`: Read-anything composed
with WebFetch-anywhere is read-everything-send-anywhere outside the kernel boundary, the one
composition that defeats the posture, so it is the single surviving prompt.
`disableBypassPermissionsMode: "disable"`: the in-process layer is the only bound on those
tools, so being able to switch it off is being able to remove them all. `defaultMode: "auto"`:
without it every IDE session starts prompting, and the ergonomic half of the ask is silently
unmet.

### The agent may not write `~/.claude/settings.json`

Granting `~/.claude` write access is the grant that must not be made: `~/.claude/settings.json`
is the posture, so an agent that can write it can disable its own sandbox next session, and the
operator-posture guard does not defend the file it lives in. The deny is kept narrow:
`~/.claude/projects/**` stays writable (the auto-memory), and a repo's own
`.claude/settings.json` stays writable because it cannot weaken the posture.

Measured when it was first installed for real: the first `install` succeeded because the deny
rule was not yet in force, and the repairing `install` could not write and said so: "the
posture denies writes to itself, so installing or repairing it is deliberately a human action."
A posture repair is the operator's to run. That is the correct end state, and worth knowing
before you need it.

### `check` asks two generic questions, not a list of assertions

Claude Code enforces the boundary; re-checking its enforcement would be a second model of the
world. What is ours is that the posture is a file and files drift: Claude Code appends to
`permissions.allow` on every "always allow", `/statusline` edits the same file, and
`claude auto-mode reset` rewrites a section. So `check` asks two questions. `require` (what
`install` merges): is it a deep subset of the effective settings? Arrays are
subset-by-membership, never equality, so domains you add yourself are fine. `forbid`: is each
named key empty or absent?

`forbid` exists because a subset check cannot express emptiness. A posture entry of
`excludedCommands: []` would assert nothing, and `excludedCommands` is the sandbox's own bypass
list ("all bash commands must run in the sandbox unless they are explicitly listed in
excludedCommands"). `permissions.additionalDirectories` is the other forbid key, since it widens
the only bound the unsandboxed tools have. Adding a key to either half extends the check and
the tests, which generate their cases from the posture file: flip every boolean, drop every list
entry, populate every forbid key.

### `retire`: a subset merge cannot express removal

The `install` merge is add-only for arrays, deliberately, so your own `permissions.allow`
survives a re-install. That means an entry once installed stays for ever while `check` tolerates
it as an extra; deleting a rule from `require` does nothing. The posture's third half, `retire`,
lists array members it has withdrawn: `install` removes them and `check` reports them as drift.
Without it the only repair is editing `settings.json` by hand, which is the thing this script
exists to stop people doing. Found by retiring three inert `Write(...)` rules (see History).

### The posture is validated against Claude Code's own schema

`claude doctor` reports settings violations for the directory it runs in, so the tests hand it
the committed `require` block in a temp project and fail on `Invalid settings`. A typo'd key or
an out-of-range enum installs cleanly, is ignored at runtime, and is indistinguishable from a
posture in force. The check was inert when first written: the stub `claude` the `run` tests put
on `PATH` shadowed the real binary, so it validated against a stub that prints nothing, and only
the mutation run found it. Resolve the real binary before the stub exists.

The schema check cannot see semantics. Three schema-valid `Write(...)` deny rules were inert,
and only running the posture surfaced Claude Code's warning about them.

### `jq`: use `has`, never `//`

`false // x` is `x`, so the obvious spelling of "the value, or null if absent" turns every
correct `false` into a failure, and the strongest setting here (`allowUnsandboxedCommands`) is
exactly that shape. Pinned by a test.

### Cross-platform, with the differences named

The posture file is `~`-relative and carries both platforms' paths; macOS-only keys
(`allowAppleEvents`, `enableWeakerNetworkIsolation`, `allowUnixSockets`) are inert on Linux and
harmless. On Linux the mechanism is bubblewrap + seccomp, so `bubblewrap` and `socat` must be
installed: `check` warns and `run` refuses, deliberately split, because "has the posture
drifted" and "can this machine honour it" are different questions with different fixes and one
exit code must not mean both. `denyRead` covers `/media`, `/mnt` and `/run/media` as well as
`/Volumes`; `/mnt` is the most valuable entry on WSL, where the Windows filesystem lives.
`~/.cache` is in `allowRead`/`allowWrite` because without it a Linux build cannot read its own
caches, and a posture too tight to work is one that gets switched off.

### `JKB_AUTO_MODE_SSH_AGENT` is macOS-only, and says so

`~/.ssh` is unreadable, so SSH pushes fail inside the sandbox, which is correct for an
unattended agent. `JKB_AUTO_MODE_SSH_AGENT=1` allows the agent socket instead of the key, so it
can authenticate but never read the key. `allowUnixSockets` is documented "Ignored on Linux
(seccomp cannot filter by path)", so on Linux the overlay was a flag that reported success and
did nothing. Linux's only lever is `allowAllUnixSockets`, all-or-nothing, which is not something
to switch on behind a flag whose name promises a single socket. The test is branched per
platform and each branch was run on its own platform, not inferred.

### `preflight` predicts breakage from the machine's resolved paths

The first real install denied its own settings file, `$TMPDIR` and `/tmp`, and all three are
facts about the machine's resolved paths rather than about the settings file, so no amount of
checking the posture could find them. `auto-mode.sh preflight` resolves what the machine needs
(`$TMPDIR`, the real path of `/tmp`, `$PWD`, the settings file, the toolchain roots,
`CARGO_TARGET_DIR`) and reports any that no `allowRead`/`allowWrite` entry covers; `install`
runs it and refuses on a gap (`--force` overrides). Verified by reverting the posture to the
version that broke the machine: it names all three, each with its fix.

It compares against the entries as written, not only resolved. Resolving both sides makes `/tmp`
and `/private/tmp` agree, which would have hidden the exact symlink mismatch that denied `/tmp`:
the sandbox matched the real path while the posture named the link. A path covered only after
resolution is reported as a latent gap. The deny side reads every `denyRead` root from the
posture (`~`, `/Volumes`, `/media`, `/mnt`, `/run/media`), like its allow-side siblings; asking
only about `$HOME` let a cargo home on an external volume, or anything under `/mnt/c` on WSL,
pass preflight and then fail every sandboxed build.

### A passing preflight names what it cannot check

`install` refuses on preflight's verdict, which makes it read as authoritative, so "no gaps —
this posture is workable" claimed far more than a filesystem-path check supports. It says "no
FILESYSTEM gaps" and prints the four blind spots every run: setuid-root exec (refused under any
posture, not configurable, surfacing as an opaque exec denial; it cost a peer session a red
gate), unix sockets, the unvalidated domain allowlist, and whether the sandbox engages at all.
An unstated gap in a tool something gates on is indistinguishable from coverage.

### `preflight` is not in `check.sh`, and the suite passes `--force`

Whether the real posture covers the real paths depends on where the checkout lives (`~/repos` on
a dev box, `/home/runner/work` in CI), so a passing assertion would be a test of the machine. The
tests exercise the logic: a posture covering nothing is refused, one covering everything is not,
a symlink listed only by its link name is flagged. When the hermetic suite drove `install`
through the gate, CI went red: on a runner under no allowWrite root, install wrote nothing and
two assertions passed vacuously on the empty result. The suite passes `--force`, and the gate
has one deliberate test of its own instead of being exercised incidentally by all of them.

### Three shell defects preflight hit, each latent behind another

`cd ""` succeeds in bash, which made an empty posture list mean `$PWD`: jq prints nothing for an
empty array, a here-string of nothing is still one empty line, and the empty entry resolved to
the current directory. It never produced a false pass (reproduced against a posture covering
nothing, `$PWD` still reports a GAP, because the covered branch is built without `cd` and
`covered()` skips an empty prefix), but it produced the wrong remedy, and on the deny side a
false GAP. Fixing it exposed the next one: arrays that always held an element could now be
empty, and `"${arr[@]}"` under `set -u` on bash 3.2 aborts the script. And `set -e` made the
three-state `settings_state` (0/1/2) unreachable, because a bare call returning non-zero aborts
before `case $?` runs; `|| st=$?` is what turns a return code into a value. Each was pinned by
reverting it.

### Confinement is established by the errno, against a control

With the posture installed on the host, a `$HOME` write is refused with `EPERM` while a control
write inside `~/repos` succeeds. Neither TCC nor ordinary permissions explains that (`$HOME` is
`drwxr-x---` owned by the user), and the read side tracks the posture exactly across three plain
dotfiles of identical TCC status: `~/.gitconfig` and `~/.zshrc` readable (both `allowRead`),
`~/.zsh_history` denied.

`auto-mode.sh sandboxed` asks the kernel the same way: a control write inside an allowWrite root,
a canary write to `$HOME`, and a report of CONFINED, NOT CONFINED or INCONCLUSIVE. The premise "a
write to `$HOME` would otherwise have landed" is not establishable from inside, because the
sandbox intercepts `access(2)` too, so `[ -w $HOME ]` reports policy rather than permissions and
every side channel is filtered by the thing being detected. The errno answers it directly:
`EACCES` is the permission bits, `ENOENT` is no parent, `EISDIR` is something in the way, and
only `EPERM` (seatbelt) or `EROFS` (a bubblewrap read-only bind) is policy. Compared numerically,
so no locale or wording is involved. The verdict is a pure function, so the unconfined arm is
testable from a confined machine, and the classifier is pinned against real kernel answers. It
costs nothing and needs no session.

Test confinement against a path the posture itself governs, never one the OS already protects.
`~/Documents` is a useless canary on macOS: TCC denies it to the terminal whether or not any
sandbox runs, which is how a sandbox-free machine was briefly misreported as sandboxed.

### `probe` takes its verdict from the filesystem, and has two files

`auto-mode.sh probe` is the fuller check (egress and credential reads as well). What a model
narrates about its own confinement is not evidence, so the verdict comes from the filesystem. It
needs a real billed session, so it is an `#[ignore]`-class test and never in `check.sh`.

Two files, because one cannot tell the two failures apart. "The canary is absent" is evidence the
sandbox denied the write only if the session ran the command at all. With the sandbox off, the
state the probe exists to detect, Bash is no longer auto-allowed, so the classifier gets the
out-of-bounds write and will very likely refuse it, and an absent canary would read as a pass. A
control file written inside the workspace separates "denied at the boundary" from "never ran",
and the second is reported inconclusive, never as a pass. The canary is not dot-prefixed:
`~/.jkb-…` shares a prefix with the allowed `~/.jkb`, and that near-miss is how a probe comes to
lie. In an agent session `probe` correctly reported INCONCLUSIVE, because a subprocess `claude`
has no credentials there (`loggedIn: false` even with the sandbox overridden off, which
attributes it to auth and not to the posture).

### The posture makes Docker unreachable, and that is the right answer

After installing it, `~/.docker/bin/docker` fails with `Operation not permitted`: the directory
is under `denyRead: ["~"]` and in no `allowRead` entry. An unattended agent that can reach Docker
can mount `/` into a container and is root on the host, so this is the boundary doing its job.
The cost is that `.container/verify.sh` and `mutate-verify.sh` are human-run on the host, which
is stated where they are documented. CI, which is not under this posture, runs them (see
Verifying the container).

### Stated residuals, not guaranteed over

In-process tools are bounded by permission rules and not by the kernel, so a path nobody named
is Read-able (on the host; the container closes this). MCP servers and hooks are unsandboxed
processes with no posture key to reach them; use `--strict-mcp-config` in a repo that is not
yours. Any allowlisted host is an exfiltration channel: egress control bounds where, not what. A
posture too tight to work gets switched off, which is why `check` tolerates entries you add and
`probe` reports inconclusive rather than pass. `localhost` egress and the SSH-agent socket
overlay are unverified without a live session, and both fail in the safe direction.

## The dev container: both layers

`.container/` is the "both" configuration: a container and Claude Code's own sandbox running
inside it. `scripts/auto-mode.sh` alone is host-only; the container adds the one property the
host cannot express. Its harnesses are `check-config.sh` and `mutate-config.sh` (static, in the
gate) and `verify.sh` and `mutate-verify.sh` (need Docker; run in CI).

### The container's job is file access; the sandbox's job is everything else

An unmounted host path does not exist in the container, so the in-process tools
(`Read`/`Glob`/`Grep`/`Edit`) are bounded by the mount namespace rather than by permission rules:
default-deny by the kernel, which the deny-beats-allow rule model cannot express at all. That
puts the `claude` process itself in a mount namespace and closes the host posture's largest
residual. The nested sandbox still supplies per-command Bash confinement and the precise
hostname allowlist.

### Mounts and nesting are two questions, and only nesting needs seccomp work

Docker hosts limited mounts trivially; that is where the default-deny read property comes from,
and it needs no seccomp work. Running Claude Code's own sandbox nested inside such a container is
a different question. Measured in a Lima VM (Ubuntu 26.04, kernel 7.0, Docker 29.7) with a
no-container baseline first, so a failure is attributable to the container profile and not the
kernel: stock Docker cannot nest it, not as root, not as non-root, not with
`--cap-add SYS_ADMIN`, not with AppArmor off; `bwrap` fails at namespace creation every time.
Container-only was always available; the seccomp profile is the price of keeping both layers.

### Seccomp: the default profile plus fourteen syscalls, and non-root

The blocker in stock Docker is seccomp, and the fix is narrower than the folklore: neither
`--privileged` nor `seccomp=unconfined` is required. Docker's default profile plus an
unconditional allow for `clone, clone3, unshare, setns, mount, umount2, pivot_root,
mount_setattr, open_tree, move_mount, fsopen, fsconfig, fsmount, fspick` suffices, and those are
then usable only inside the user namespace `bwrap` creates, where the process holds no privilege
over the host. `.container/generate-seccomp.sh` produces `seccomp-bwrap.json` from upstream.
Non-root is load-bearing, not hygiene: with seccomp fully disabled, root in a container still
cannot create a mount/net/pid namespace directly.

### What the container does not buy

`~/repos` mounted is still writable and push-able; the win is bounded to what you did not mount.
Container egress is unrestricted by default, so if the inner sandbox fails to start you lose
`strictAllowlist`, and a container without its own IP allowlist is a downgrade on egress (hence
the firewall). On macOS every container path is a Linux VM, so the native loop (pinned rustup,
`sqlite-vec` FFI, headless Chrome, launchd, worktrees under `~/repos`) has to be re-plumbed.

### Where the `claude` process runs is what matters

The UI of VS Code is on the host; the server, extension host and terminals are in the container.
The Claude Code extension declares no `extensionKind` and has a Node `main`, so VS Code runs it
as a workspace extension, in the container, and `container.json` lists it so the Linux build is
installed inside (a host copy is platform-specific and separate).

### The mount list is the security boundary, asserted exhaustively

`verify.sh` reads `/proc/self/mountinfo` and fails if the mounted set is anything other than
what `container.json` declares, rather than listing paths that ought to be absent, which is the
"enumerate the secrets" shape the host posture is forced into and the container is not. The
expected set is derived from `container.json` (both the string and object mount spellings), not
written out a second time: a hand-written list once lost `.cargo/registry` while two other lines
were being removed, and a correctly built container failed its own verifier after the full
toolchain build. The comparison is on exact mount points with no prefix logic; prefix filtering
once let `$HOME` at `/host` and `/var/run/docker.sock` through. `~/.claude` is never mounted: it
holds `settings.json`, which is the posture, and a process the posture bounds must not read or
write the file deciding whether it is bounded. The current mount list, including the narrowed
`~/.jkb` binds, is in `.container/README.md` ("The mount list is the security boundary").

### `verify.sh` refuses to run outside the container, and never passes on a table it could not read

Run on the macOS host, it printed fourteen confident FAILs about a machine that was never the
subject, and two `ok` lines, because `/proc/self/mountinfo` does not exist there, so the mount
check compared an empty set and passed. The expected side had been guarded against emptiness
since it was derived; the actual side never was, in the one assertion the file exists for. The
read is kept separate from the result, and an unreadable table is a FAIL. Found by running the
script in the wrong place.

### A nested bind must be named, never inferred

`workspaceMount` binds the parent `~/repos`, so `mutate-verify.sh`'s own
`-v $REPO:/home/vscode/repos/jkb` is undeclared, and it cannot mount the parent instead, because
in a `jkb task work` session `$REPO`'s parent is `.jkb/work`. The tempting rule, "nested is
fine", is wrong: a mount point and a mount source are independent, and
`-v ~/.ssh:/home/vscode/repos/jkb/secrets` is inside the declared region and is still
exfiltration. The source cannot be checked from inside (Docker Desktop for macOS reports the
path inside the VM, which is why `dc_mount_sources` is host-side only). So
`verify.sh --declare <target>` adds to the derived set and is refused unless the value is a
strict descendant of something already declared as a bind (not a volume, which reaches no host
filesystem). `/host`, the docker socket and `~/.claude/settings.json` are all refused by
`verify.sh` itself, which is exactly the mutation set the harness exists to catch, and the count
of declared additions prints in the passing line (the task-lifecycle design's `--no-review`
lesson). A blanket rule would have weakened the shipped container; this weakens only the harness,
and only where a path is written down in a diff.

### `workspaceMount` binds all of `~/repos`

The argument is consistency, not convenience. `scripts/auto-mode-posture.json` grants `~/repos`
in both `allowRead` and `allowWrite`, so a container holding only jkb was tighter than the
boundary the same agent runs under on the host: a difference nothing had decided, which made a
cross-repo task impossible rather than deliberately refused. One container serves every repo
under `~/repos`.

### Open the repo root's mount, never only a worktree

`jkb task work` puts worktrees at `<repo>/.jkb/work/<session>`, and a linked worktree's `.git` is
a file pointing into `<repo>/.git/worktrees/…`. Mounting the parent puts both ends inside;
mounting only the worktree breaks git, because the gitdir it names is not there.

### No credential mount: authenticate inside, into the state volume

`~/.claude/.credentials.json` exists only on Linux/WSL; macOS keeps credentials in the Keychain,
and a bind mount with a missing source is a hard error, so a credential mount meant the container
never started on the host this repo is developed on. You authenticate inside the container
(`claude auth login`), into the state volume, which also fixes the read-only mount a login could
not have written. `.credentials.json` and `~/.claude.json` are linked into the volume by
`setup.sh`, dangling until first write. Claude Code replaces that link when it saves a login or a
refreshed token (measured on 2.1.276), so `dc_persist_login` moves the file back into the volume
at setup, at every `run.sh` start, and before `--stop`/`--rm`. The remaining residual and why a
volume at `~/.claude` was rejected are in `.container/README.md`.

### Volumes: every mount point exists in the image, and the target dir is under `~/.cargo`

A named volume whose path does not exist in the image is created root-owned, so cargo died with
`EACCES` minutes into the first build. Every volume mount point is created in the Dockerfile,
which is what makes Docker seed ownership from it. Cargo's `target/` lives in a volume, off the
bind mount: required by that uid mismatch, and independently right because it is the heaviest
write path and a macOS bind mount is a VM filesystem crossing. `CARGO_TARGET_DIR` is inside
`~/.cargo/target`, not a sibling of `~/.cargo`: `covered()` matches at component boundaries, so as
a sibling it fell under `denyRead: ["~"]` and blanketed every sandboxed build in the container
while both guards reported it healthy. `preflight` reads `CARGO_TARGET_DIR`, so the class is
checkable rather than latent.

### `jkb` is built into the container

`jkb` was not installed in the container at first, and the explorer extension spawns it and every
workflow verb needs it. `setup.sh` builds and installs it; `jkb 0.1.0` was confirmed running
inside. Found only by building and running it.

### An out-of-memory build does not say so

`rustc` is SIGKILLed and cargo reports a bare `(signal: 9, SIGKILL: kill)`. Diagnose from `dmesg`
rather than guessing. It was diagnosed twice: the first diagnosis was right about OOM and wrong
about the cause, since the test VM was holding a 2.9 GB copy of the repo on tmpfs.

### Root is not reachable from inside: the base image's grant is removed

`mcr.microsoft.com/devcontainers/base` ships `/etc/sudoers.d/vscode` containing
`vscode ALL=(root) NOPASSWD:ALL`, so the agent could flush the firewall, delete the allowlist
snapshot, rewrite a root-owned script, or simply `sudo -i`. Every root-ownership protection here
rests on the agent not being able to become root, and none of them checked. The grant is removed
in the Dockerfile, with `visudo -c` failing the build on a malformed result, since a broken
sudoers file locks out the grants the firewall needs. Found only by running `sudo -n -l` inside
the container: two review passes and every static check missed it, because nothing in the repo
says the base image grants it. A guard is worth what its weakest bypass is worth, and this bypass
was inherited rather than written.

The check asks sudo, not the file. `verify.sh` requires that every command `vscode` may run as
root is one of the intended grants, so a blanket grant re-added by any route (a rebuilt base
image, a feature, a helpful `RUN` line) fails it. `mutate-verify.sh` restores the grant in a
one-layer image built `FROM` the real one and requires that failure, since a sudoers entry cannot
be broken with a `docker run` flag. Measured in CI: the sudoers surface is exactly the two
intended grants (`init-firewall.sh` and `egress-status.sh`), and the root-owned scripts cannot be
replaced by the agent. The root-owned-path list is derived from the Dockerfile's `COPY` lines, so
a fourth installed script is covered without being remembered.

### Auto-memory is shared through `~/.jkb`, with no new mount

Claude Code keys memory by the project's absolute path, so one repo has two keys
(`-Users-…-repos-jkb` and `-home-vscode-repos-jkb`), and widening the workspace mount does not
change that. Binding the host's memory dir is the one mount forbidden everywhere here (`~/.claude`
holds the posture), it collides with `dc_link_state`'s symlink, and the slug is inexpressible in
`container.json`. So `scripts/link-claude-memory.sh` symlinks each side's `memory` dir at
`~/.jkb/claude-memory/<repo>/`, inside a bind that already exists. It migrates file by file and
never overwrites: a name on both sides is left alone and reported. It is opt-in on the host
(`setup.sh --link-memory`), because `post-merge` re-runs `setup.sh` and a `git pull` must not
rearrange somebody's `~/.claude`.

### Sharing memory widens what sandboxed Bash can reach, deliberately

`~/.claude/projects` is under the posture's blanket `denyRead` and in no allow list; `~/.jkb` is
in `allowRead` and `allowWrite`, because the database lives there. Linking moves auto-memory from
a place sandboxed Bash cannot touch into one where a single auto-approved command rewrites it,
for this repo and, through the same grant, for every other repo's store. Memory is prose
re-injected into every later session, so the channel turns a one-shot injection into a durable
one, and the posture has no write-deny to carve `claude-memory` back out with (`filesystem`
offers `denyRead`/`allowRead`/`allowWrite` and nothing else). Weighed against a dedicated store
with its own declared bind, `~/.jkb` was chosen: both ends are the same person's agents, it is
prose rather than code, and the host side is opt-in.

The load-bearing fact, measured rather than assumed: file tools and Bash are bounded by different
mechanisms. The sandbox's `filesystem` block governs Bash; the `permissions` rules govern
`Read`/`Edit`/`Write`. An agent writes memory through the Write tool wherever the store lives,
so moving it somewhere the posture does not grant would only have stopped sandboxed Bash. Anyone
revisiting this should start there.

### The memory linker decides before moving, and `verify.sh` asks it live

The linker decides the whole migration before moving anything, and refuses a store holding
anything but plain files: a symlink planted by either side redirects the other's reads and
writes, including back into `~/.claude`. `verify.sh` asks the linker for the state rather than
inferring breakage from a missing link, because the linker leaves the link absent on purpose in
states it recognises. It asks live every time: reading `setup.sh`'s create-time record instead
made the store guard unable to fire after `postCreate` (a redirect planted the next day reported
`ok`), and an `exposed` record could not be cleared by the remedy the failure printed. The record
is an additional alarm, consumed once reported, and it records the outcome of the link, not the
state before it (recording the pre-state made every first-ever link record `unlinked`, which
`verify.sh` treats as fatal, so every new container failed `postCreate` once on a feature
working). The record still earns its place: the linker repairs, so a live question asked
afterwards sees the harmless state and not the one that was true at create.

## Starting the container: a script, attached to

`.container/run.sh` starts one long-lived container from `container.json`, and you attach to it.
Everything about mounts, seccomp, the firewall and the harnesses is independent of who starts
it. The kit (`~/.local/share/jkb-container-kit/kit/.container/run.sh`) is what runs it; the
checkout's own `run.sh` refuses, as `.container/README.md` records ("Everything unsandboxed runs
from the kit").

### Started by `run.sh` and attached to, not opened by Dev Containers

You attach ("Dev Containers: Attach to Running Container") and open any path inside, at any
depth; `code <path>` from an attached terminal opens more windows on the same container. Attaching
has no `workspaceFolder`, so the limitation of the Dev Containers path (it could not open a
folder nested inside the mount; see History) and the guard that policed it both stop existing. The
cost it removed was concrete: a session worktree carries its own `.container/`, so opening one is
how you exercise an edit, and under Dev Containers a change to the container could only be tested
after landing it. Two rebuilds in a row tested `main` instead, and read as the container being
broken.

### Rejected ways to keep Dev Containers

Three alternatives were worked out and rejected, each on a fact. A relative symlink
`~/repos/<name>` to the worktree gives `find_workspace_posture` a second directory carrying
`scripts/auto-mode-posture.json`, and the firewall fails closed on first raise; that refusal is
deliberate and must not learn an exception. A nested bind at `/home/vscode/repos/<basename>` has
the same effect for the same reason. A constant workspace path (`workspaceMount` from
`${localWorkspaceFolder}` to a fixed target) handles any depth and breaks session identity
instead: claims are `session:<pid>:<worktree>` and liveness is "does that worktree exist", so
every container would record one shared path and no claim would ever be reclaimable.

### The file is `container.json` and the folder is `.container/`

VS Code detects `devcontainer.json` and offers Reopen in Container, which would be a second way to
get a container, one built by Dev Containers and one by `run.sh`: two launch paths that start
identical and drift, in the mount list, which is the security boundary. Detection is file-based as
far as is known, so renaming the folder is belt-and-braces; it is done anyway because the name
asserted something no longer true.

### Every declared key is consumed

`run.sh` is the only thing that applies `container.json`, so a key nobody reads is possible and
looks exactly like configuration, and the key most likely to be added is another `mounts`-shaped
one. `run.sh --consumed-keys` names what it reads, and `check-config.sh` fails on any declared key
not in that list, so adding one forces the decision when it is added. The list must be true: it
once named `name` and `build` while `run.sh` read neither, so "every key is applied" was a true
sentence about two inert declarations, and the fix for the next inert key would have been to add
it to the list, which silences the check rather than satisfying it. Both keys are read now.
`run.sh --self-test` is in `./scripts/check.sh`.

### An unset `${localEnv:VAR}` is refused

Dev Containers substitutes the empty string, which is how `source=${localEnv:HOME}/repos` quietly
becomes `source=/repos`: a different host directory, mounted, with nothing to notice. A boundary
must not be able to move because a variable was not set.

### The firewall belongs to the container: it is the image's ENTRYPOINT

When the firewall was one caller's step, `docker start jkb-dev`, Docker Desktop's start button and
a daemon restart all brought the container up with no allowlist, and nothing checked, because
attaching runs nothing. `entrypoint.sh` is the image's `ENTRYPOINT`, so every route raises it, and
the raise is first on every start: iptables rules live in the container's network namespace and do
not survive a restart, and the rest of setup includes a toolchain download. `check-config.sh`
asserts the Dockerfile still wires `ENTRYPOINT` to `entrypoint.sh`; before that guard, deleting one
line left every check green while `docker start` came up unfiltered. `run.sh` does not raise a
second time concurrently: its re-raise is a wait on the raise's lock (see the firewall section).

### Replacing a lifecycle moves its guarantees onto whoever replaces it

`postCreateCommand` and `postStartCommand` were not just steps; they were the statements "this
happens on every start" and "this happens until it succeeds". Deleting the lifecycle deleted the
guarantees while the steps survived, and each defect the first review of `run.sh` found (fifteen
findings, four must-fix, every one introduced) was one guarantee that then had no owner. The
blocks below are those owners.

### Setup is chosen on a marker setup writes last, spelled once

"Did this invocation create the container" is not "did setup finish". `fresh=1` is true a minute
before setup completes, so an interrupted first run left `setup.sh` permanently unreachable:
every later run took the verify arm, and only `--rm` escaped. The arm is chosen on a marker
`setup.sh` writes as its last act. `run.sh` executes `setup.sh` from the same bind-mounted
checkout, so both source one constant for the marker's path; the agreement guard that once
policed two spellings was deleted with the second spelling (see Verifying the container).

### The staleness fingerprint normalises the checkout path and folds in the profile's content

`runArgs` names the seccomp profile by `${localWorkspaceFolder}`, so a session worktree, the case
`run.sh` exists to enable, computed a different hash from its main checkout, was told
`created from a different container.json` about a file that had not changed, and was advised
`--rm`, destroying the shared container plus `~/.vscode-server`, its extensions and
`~/.jkb-ui-build`, none of which are in a volume. Then the main checkout refused identically: two
checkouts ping-ponging over a declaration they agreed on. The workspace root is normalised out and
the profile's content is folded in, so a profile that really differs still forces a recreate.
`--build` rebuilds the image and recreates the container on it; leaving the container on the old
image meant a newly staged `.vsix` never arrived while `install-extensions.sh` said "rebuild the
container", advice just followed.

### Extensions arrive on attach, so setup cannot install them

Under Dev Containers the VS Code server was unpacked before `postCreate`; now it arrives when you
attach, so setup installs nothing and the next `run.sh` finds a server with nothing in it. An
assertion whose two conditions used to be one was fatal there, with the remedy "rebuild the
container", which reproduces that exact state. The never-installed case reports and names
`install-extensions.sh`; a server that has extensions but is missing a declared one is still a
failure, because that is the original bug. Extensions are fetched at build time and pinned
(`fetch-extensions.sh`, whose `--self-test` is run by the gate and which refuses an empty list;
details in `.container/README.md`).

### A verify that aborts suppresses everything after it

The deferred-archive reap ended up downstream of a fatal `verify.sh`, so one unrelated assertion
disabled the only reaper that could finish container-side archive records. The reap runs before
the verify, and the verify's result is carried rather than exiting at the point of failure:
several assertions name a remedy you run from inside an attached window, and dying printed the
problem while withholding the way to fix it. The exit code is still verify's.

### `run.sh` handles the entrypoint's refusal

`docker run --detach` returns 0 even when the entrypoint refuses, and the next `docker exec` failed
with a bare "Container … is not running"; `set -e` killed `run.sh` there and the entrypoint's
explanation sat in `docker logs`, which nothing pointed at. `run.sh` asserts the container is
running after `run`/`start` and prints the tail of `docker logs` when it is not. The synchronous
re-raise's failure is a recorded condition, not an abort: under `set -e` it skipped the reap and
`verify.sh`, the very reporting this design assigns to verify.

### One exit code cannot carry the transport and the answer

`docker exec` exits 1 both when the container is gone and when the probed condition is false, and
125 when it could not run the command at all. `run.sh` sampled liveness once, before the
entrypoint had decided (`--detach` returns as PID 1 starts, while the raise is still in about
fifteen `getent` lookups), so a refusal seconds later was misread three ways: a setup-marker probe
recorded "setup did not complete" and started a ten-minute toolchain rebuild, verify's result took
the daemon's exit 1 for verify's verdict, and the run ended by printing "the container is running
and attachable" about a container that was not, from a verifier that never ran.

The fix is in the callee, above the dispatch (the rule the file-sync design states: a condition
that dominates every arm belongs above it). Every `docker exec` goes through one wrapper
(`in_container`); 125 is a transport failure and never the command's answer; on transport failure
it asks whether the container is alive and prints the tail of `docker logs`, with
`container_died` offering the entrypoint as a likely cause rather than asserting it. Probes whose
answer must be distinguishable from a transport failure print a word instead of leaning on an exit
code the daemon also uses. The first exec waits for the entrypoint to settle, and `settle` has a
third answer: past-the-entrypoint, could-not-read, or budget-exhausted, with the caller reporting
the last two rather than continuing. Those classifications are pure functions (`settle_step`,
over `(running, ps output)`) and are table-tested.

### Booting is not endorsing: `--open` only on a clean verify

`--open` launches a VS Code window, which is starting a session, so it opens only on a clean
verify; going on to launch a window into a container whose verifier just reported undeclared
mounts is not an action to take. The accepted-unfiltered override (below) buys a container you
can attach to by hand and diagnose, not a place to run an agent. `verify.sh` exits 3 when every
failure is one this container tolerates rather than a broken boundary, counting those failures
rather than assuming there is one, so the failure is still reported at full volume every run while
a caller can tell the two apart. `run.sh` names no cause for exit 3 and points at the FAIL lines,
because exit 3 has two producers with opposite remedies: the unfiltered-egress override, and the
Bash-sandbox transcript budget (recorded in `.container/README.md`).

### One way to ask whether the container is healthy

A hand-rolled `docker run` printed in a refusal message omitted the seccomp profile, `NET_ADMIN`,
the `~/.jkb` bind and the whole preamble that raises the firewall and installs the posture, so it
produced a dozen FAILs that read as a broken container instead of as a wrong command.
`mutate-verify.sh --control` reuses the exact flags and preamble every mutation runs against, so
"is my container ok" and "did that guard fire" cannot be answered about two different containers.

### A harness refuses a subject it cannot find, before reporting on one

A stray em dash pasted as the image name made every `docker run` exit 125, which `judge` read as a
non-zero `verify.sh` and reported as a guard that did not fire: nine MISSED lines and three
BUILD-FAILED blocks before the control called them unattributable. The control did its job, and
doing it that late is the defect. The image and the argument count are checked up front, the same
shape as the docker-on-PATH and daemon-reachable checks.

## The egress firewall

`init-firewall.sh` installs an IP-level allowlist in the container's network namespace;
`egress-status.sh` reports what the live chains enforce; `egress-lib.sh` holds the shared,
self-tested decisions and measurements; `entrypoint.sh` boots on the answer and `verify.sh`
reports it.

### The firewall exists because `strictAllowlist` lives inside the layer that might not start

A container's default egress is unrestricted, so a container whose nested sandbox failed silently
would be a downgrade on exfiltration versus the host. `init-firewall.sh` is coarse (IP-level) and
independent; the sandbox is precise (hostname at a proxy) and in-process. Coarse-but-independent
under precise-but-fragile is the point: they fail for different reasons.

### One allowlist, not two

The firewall reads `.require.sandbox.network.allowedDomains` out of
`scripts/auto-mode-posture.json`, the same file the sandbox posture comes from. Two egress lists
that can disagree is how the tighter one ends up decorative. A wildcard cannot be pinned to an IP,
so the firewall skips `*.rust-lang.org` and says so; the posture therefore also names
`static.rust-lang.org` concretely, or the toolchain download is blocked by the coarse layer while
looking allowlisted in the file.

### The allowlist is snapshotted at create, and the script takes no arguments

The script is installed into the image, root-owned, with sudoers granting exactly that path; a
script the agent can edit and also `sudo` is a root shell with extra steps. The caller once passed
the allowlist's path, and the path passed was the repo's own copy: bind-mounted, under
`allowWrite`, writable by the agent this layer exists to bound. Appending a domain and waiting for
the next start had root add it to the ipset, and a sudoers entry naming no argument accepts every
argument, so any readable JSON on the box was a valid allowlist. The workspace copy is read once,
at container create, before any agent session exists, and snapshotted root-owned and 0444; every
later raise reads the snapshot, the script refuses arguments, and sudoers pins it to none
(`… ""`). A rebuild is what re-reads the repo, the right ceremony for widening egress. Divergence
between the two is reported and never acted on: silence would let a legitimate edit look applied
when the coarse layer never saw it.

### `init-firewall.sh` discovers its workspace instead of naming it

A hard-coded `~/repos/jkb` became a statement about whichever checkout sat there once the mount
widened: it would snapshot another checkout's allowlist as the root-owned list every later start
runs on. The script cannot be told which (the sudoers grant forbids arguments, and an environment
variable is agent-settable), so one repo carrying `scripts/auto-mode-posture.json` is the answer
and two is a refusal, on the first raise only, because a later raise that exits non-zero leaves
the rules unapplied, and unapplied rules mean unrestricted egress.

### A refusal that installs no rule is not a refusal

Two guards in `init-firewall.sh` once exited before any iptables rule was applied, and rules do
not survive a restart, so a truncated snapshot (a state one of those guards' own comments calls
real) left unrestricted egress on every later start, permanently, since the snapshot is root-owned
and read-only. `fail_closed` is defined above every refusal, and `check-config.sh` fails the gate
on an `exit 1` in that file outside it. That guard's first version anchored its pattern to the
start of a line and walked past `|| exit 1`, the exact shape it existed for; the mutation caught
it.

### The ERR trap stays, and bare assignments get a fallback

An `ERR` trap (`set -E` into `fail_closed`) catches an abort no refusal wrote. Its first version
was worse than the hole it closed: `getent` exits 2 for a name with no A record, `pipefail` carries
that out, and a bare assignment is a simple command in no conditional context, so `errexit` fired
and one unresolvable domain took the whole raise to deny-all with no allowlist, blaming "an
unexpected failure at line 173" while the two arms written for that state became unreachable.
Measured which shapes trip it: a bare `x="$(cmd)"` does; `x="$(cmd)" || x=""` and
`if x="$(cmd)"` do not. `check-config.sh` rejects a bare command substitution in that file. The
same guard later caught a bare `v6="$(v6_state)"` as it was written.

### Serialise the raise in the callee

Two raises run concurrently on every fresh create (the entrypoint's and `run.sh`'s) over one
`allowed-new` ipset, one OUTPUT chain and one verdict record, and the interleavings are
destructive: `ipset flush` wiping the other's accumulation, `ipset add` failing silently into
`resolved=0` and a blanket deny on a machine with healthy DNS. `flock` on a fixed path at the top
of `init-firewall.sh` makes any two raises queue however they were started, `run.sh`'s re-raise is
a wait on that lock, and `iptables -w` closes the narrowest half the lock cannot cover. A rule
every caller must remember is the defect.

### Denial must be established, on both families

The one rule: a family that is not provably closed is open. `allowlisted` requires the allowlist
installed, the default-REJECT verified present, and v6 denial established; `denied` requires
blanket denial established on both families; anything else is `unfiltered`, including every case
where the answer could not be obtained. The rule applies to the success path too, which once
allowlisted IPv4, printed "IPv6 is UNFILTERED" and reported success, its own comment conceding
this was "safe only because the container has no IPv6 route, which is not checked here". No
`ip6tables` means v6 denial is not established, so the state is `unfiltered`, chosen by the
operator over a distinct non-blocking state: if denial cannot be established, be noisy.

### `verdict_state` states the rule once, and `VERDICT_STATES` the vocabulary

The fail-closed path and the success path each decided "both families provably closed" in their
own wording, and that is how the success path came to report success on unfiltered IPv6: one site
was fixed and the other kept its own reading. They differ only in what bounded is called there (a
blanket deny is `denied`, a raised allowlist is `allowlisted`), which is an argument, not a second
rule. `egress-status.sh` and the raise's success path answer through the same `verdict_state`.
`VERDICT_STATES` single-sources the words: a reader with no arm for a state the writer records
drops it into `*`, and unknown means a container that refuses to boot for ever, over a word.
`check-config.sh` requires every reader to carry an arm for every declared state, and the
self-test requires `verdict_state` to return nothing outside it.

### IPv6: measure a path, not an address

`v6_state` lives in `egress-lib.sh` beside the measurements it composes, and every caller uses
that one definition. `absent` requires no non-link-local address and no v6 default route
(`/proc/net/ipv6_route`); `absent` satisfies the establishment rule rather than weakening it,
because no source address and no route means no v6 egress. Without the clause the container would
refuse to boot on any kernel lacking `ip6tables` even where IPv6 is not in play. A link-local
`fe80::` is not an egress path: every container on a default Docker bridge gets one when the host
kernel has IPv6, and reading it as `open` refused ordinary containers and pushed their operators
onto the permanent override. A read that fails is `open`, distinguished from a table positively
read as holding only `::1`: `grep` exits 0/1/2 and only two of those are measurements. Both inputs
are injectable, and the self-test has rows for link-local-only, global-address, route-present and
unreadable-asserting-`open`.

### The boot decision is asked of the kernel

`egress-status.sh` is a second root-owned, argument-less, read-only script, granted by sudoers
exactly as the firewall is (`/usr/local/bin/egress-status.sh ""`). It reads the live v4/v6 filter
chains and prints `state=` plus `v4=`/`v6=`; it never installs, flushes or modifies a rule, so a
grant to run it is not a grant to change the boundary. `entrypoint.sh` boots on that answer and
`verify.sh` reports from it, which makes verify's claim to report "what the firewall DID, not what
egress happens to do" true. The probe establishes that a terminal REJECT is present and that the
allowlist rules precede it.

A verdict stored by the raise is an event, "at some moment a raise established X", while every
reader asks a present-tense question: is egress bounded now? `docker stop` destroys every iptables
rule while a file survives in the writable layer. Asking the kernel removes the failure class
instead of guarding it, covers `--entrypoint bash` (the escape the refusal itself recommends,
which runs no raise at all), and is immune to the concurrent-raise race, since the probe reads the
chain whichever raise wrote last. Residual: the probe runs as root via `sudo`, so a machine whose
`sudo` is broken gets no answer, which refuses, the same direction as every unobtainable
measurement.

### State from the kernel, the reason from the record

The kernel can say egress is unfiltered; it cannot say DNS failed at 09:14 or that the allowlist
snapshot was truncated. The raise records a verdict on every ending in `/run/jkb-egress-verdict`,
root-owned (`state=`, `v4=`, `v6=`, `reason=`), and its path is one constant in `egress-lib.sh`
sourced by the writer and both readers, with one `state=` parser beside it. A record whose state
disagrees with the probe is ignored for the decision and reported as drift, and a refusal still
prints a remedy. `record_verdict` flattens newlines, the one place it can be enforced, so a
multi-line remedy is not lost to a reader's `head -1`, and the round-trip test uses a reason the
writer actually emits.

### Unknown refuses; `unfiltered` refuses; `denied` boots loudly

`allowlisted` boots. `denied` boots loudly: it is safe, and it is the state you must attach to in
order to repair it. `unfiltered` refuses, and so does anything else: no output, an unreadable
answer, a word the reader does not know. A probe that cannot establish an answer says so; it never
degrades to the reassuring one. The asymmetry decides it: refusing costs a debugging session and
prints its own escape (`docker run --entrypoint bash`); booting costs the guarantee, silently, on
the `docker start` paths nothing else watches. A container in that state is nearly useless anyway
(no allowlist means no npm, no crates.io, no `api.anthropic.com`), so staying up buys
diagnosability, not work.

### The one escape is recorded, read and never inferred

`JKB_EGRESS_ACCEPT_UNFILTERED=1` in `container.json`'s `containerEnv` permits the boot. It is fixed
at create, so a session inside cannot grant it to itself, and `docker start` honours the same
decision, which a `run.sh` flag could not. This is the repo's answer to a posture too tight to
work: never a silent exemption, but a recorded one, as with `--no-review`. `verify.sh` reads the
variable itself (`docker exec` inherits `containerEnv`) and fails whenever it is `1`, independently
of the state: inferring it from an `unfiltered` state meant an operator who armed it and then fixed
the host got `allowlisted` and silence with the boot gate still disarmed, and an `unfiltered`
state reached any other way was blamed on a variable that might be `0`. The `unfiltered` arm is
worded from the evidence it has. An override nobody can see is indistinguishable from a rule that
does not exist. With the path measurement, an ordinary link-local container never needs the
override; what remains is a container with a real global IPv6 address and no `ip6tables`,
genuinely unfiltered.

### Rejected ways to answer the boot question

Re-probing egress from inside the entrypoint: a probe resolves a name, so a dead resolver is
indistinguishable from a deny-all; the raise and the chain are the only places that know what was
installed. Having `fail_closed` exit 0 when it established denial: the raise failed, there is no
allowlist and the container cannot work, so success and failure are the wrong axis; a reader
needs the state left behind. Recording the verdict in the container's environment or a label:
neither survives `docker start`. Making the stored record trustworthy with a `state=raising`
marker written first plus a token naming the network namespace (`/proc/self/ns/net`) plus a
staleness rule: three mechanisms reconstructing what the kernel will tell you, and none covers
`--entrypoint bash`. Recorded so they are not re-proposed.

### Not done, deliberately

Chain-shape assertions beyond the terminal REJECT: a chain rewritten by something with root inside
the container is out of scope, since that actor can flush the chain regardless, and the sudoers
grant is what bounds it. A `mutate-verify.sh` case for an `unfiltered` verdict: inducing one needs
a container with real IPv6 and no `ip6tables`, which no `docker run` flag produces, and a mutation
nobody has watched fire is the defect this directory keeps producing. The state is covered by
`entrypoint.sh --self-test` (the refusal and the override), `init-firewall.sh --self-test` (every
input that yields it) and `check-config.sh` (every reader carries an arm for it). Re-resolving the
allowlist on a timer is a pre-existing note in `init-firewall.sh`. Whether a routeless but
IPv6-up namespace carries a kernel `unreachable default` row that the route probe matches is a
`/proc/net/ipv6_route` fact, undecidable on macOS, and is to be settled from a real Linux container
or not at all.

## The nested sandbox on an AppArmor host

Running bubblewrap inside the container needs the seccomp profile above, the `/proc` unmask, and,
on a host running AppArmor, a profile that lets bwrap mount. Each was established by a run, not
argued.

### The nested sandbox starts: established in CI, three runs

Three CI runs, each failure naming the next: `docker-default (enforce)` failed with
`Failed to make / slave` (AppArmor denies `mount`); `jkb-dev (enforce)` failed with
`pivot_root: Permission denied` (a rule type of its own); `jkb-dev (enforce)` was green. This is
the central claim of the both-layers design established: Claude Code's own sandbox runs nested
inside the dev container, on a stock Ubuntu/Docker host, with the profile loaded.
`JKB_ACCEPT_NO_BWRAP` is not set, so a bubblewrap regression fails the build rather than being
narrated. Whether the nested sandbox engages for an actual tool call is asked in a live session
with `./scripts/auto-mode.sh sandboxed`, not with `printenv CLAUDE_CODE_SANDBOXED`.

### The AppArmor profile is docker-default with two rules changed

The remedy, chosen over `--security-opt apparmor=unconfined`, is `.container/apparmor-jkb-dev`:
docker-default with `deny mount,` replaced by `mount,` and `pivot_root,` added. Unconfined would
discard the `/proc` write denials, sysrq-trigger, kcore, the `/sys` restrictions, the network
denials and the ptrace confinement, none of which bubblewrap needs. It is not a new concession:
`seccomp-bwrap.json` already re-allows `mount`, `umount2` and `pivot_root` by name, for the reason
recorded there, and AppArmor was silently overriding a decision already taken and reviewed. An
explicit `deny` cannot be overridden later in AppArmor, so the rule is replaced, hence a whole
profile rather than an include plus an override. Loading is a host action needing root, so
`run.sh` refuses rather than falling back to docker-default, which would start a container whose
nested sandbox silently does not work. A reboot unloads it (`.container/README.md`).

### AppArmor's mount family is four rule types

`mount`, `remount`, `umount` and `pivot_root` are independent rule types. A bare `mount,` does not
cover `pivot_root`, and docker-default names no `pivot_root` rule, which in AppArmor means denied.
Upstream's own template is the proof: it grants `umount,` as a separate line while denying
`mount,`, which would be redundant if one rule covered the family. `remount` is a mount option and
is covered by the unqualified `mount,`; `umount,` is upstream's own. With these two the family is
complete, so no third line should become necessary: an expectation, stated as one. A profile
carrying only `mount,` reads exactly like one that works, so `check-config.sh` asserts the allow and
the absence of a deny for both `mount` and `pivot_root` in one loop, and `mutate-config.sh` deletes
the `pivot_root,` line and requires the failure.

### The profile is generated, because a check written from the same understanding agrees with it

The first profile was transcribed by hand from memory of moby's template. Fetched and diffed
against real upstream, it was missing `deny network alg,` (kernel crypto API),
`deny network vsock,` (host/guest channel), `deny /sys/devices/virtual/powercap/** rwklx,`
(PLATYPUS), the `abi <abi/3.0>,` declaration and the runc/crun signal peers `docker stop` needs; it
carried a stale 2017-era `@{PROC}` write-deny pattern and a duplicated signal rule. The ABI line is
the sharpest: in AppArmor ABI 4.0 `network,` no longer includes `network unix`, so omitting it
denies unix sockets on a 4.0 host, a functional break that would have surfaced as something
unrelated on exactly the modern distros this container targets. Every guard around the profile
passed, because each was written from the same memory as the file; none consulted the
authoritative source, and only reproducing the artifact from that source can.

`.container/generate-apparmor.sh` is the sibling of `generate-seccomp.sh`: fetch `moby/profiles`'
`apparmor/template.go` (moby/moby's copy 404s), render the Go template with the values
`apparmor.go`'s `generate()` supplies, apply one patch, and vendor the result, so the policy is
reviewable in a diff and loading works offline. The patch refuses to no-op: exactly one
`deny mount,` must be found, or it errors, because a patch that no-ops against changed upstream
yields a profile that parses, applies, and leaves the sandbox unable to start (the same failure
`check-config.sh` names for the seccomp profile). The renderer implements only the template subset
present, raises on an unrecognised action, and afterwards requires every literal template line to
survive and no `{{` to remain. The generator's own refusals are exercised against fixture
templates with no network.

### Drift checks are one derived check over the generators

A vendored policy has two ways to become a lie and neither is visible by reading it: it was
hand-edited, or upstream moved. `.container/check-drift.sh` asks each `generate-*.sh` what it
writes (`--print-target`), snapshots that artifact, regenerates, compares, and restores; a third
generator joins by existing, where a hand-maintained artifact list beside the generators is the
two-lists-that-must-agree shape. Each artifact records `upstream-sha256: <hex>`, so there are
three outcomes: digest changed means upstream moved; digest the same means hand-edited; no digest
means cannot be attributed, said plainly rather than guessed. The first two are repaired by
looking at different diffs, which is why the messages differ, and both fail rather than warn: for a
security policy, "upstream has moved on and nobody looked" is worth a red build, and a warning
printed every run is how people learn to ignore warnings. `seccomp-bwrap.json` was already in sync;
there had simply been no way to know.

It needs the network, so it runs in CI and not in `scripts/check.sh`; a gate that only works
online is one that gets skipped. Its pure decision runs in the gate as `--self-test`, and
`check-config.sh` asserts the offline preconditions without executing anything (a generator is
executable, supports `--print-target`, declares `url=` and `out=`; its artifact exists, declares
itself generated, records a digest and that generator's URL). Artifacts are paired from the
generator's own `out=`, not by scanning for `GENERATED FILE -- DO NOT EDIT`, which matched the
checkers themselves; the scan survives only as a count invariant (excluding `*.sh`) that stops a
deleted generator silently orphaning its artifact.

### `/proc` is unmasked, and `verify.sh` asserts the flag's direct effect

bwrap's `--proc /proc` needs `--security-opt systempaths=unconfined`. Measured: with the flag the
container carries 0 submounts under `/proc`, without it 10; on macOS (LinuxKit)
`bwrap --bind / / --proc /proc` mounts proc in both cases, while on Ubuntu 26.04 removing the flag
denies it. Whether the kernel refuses a nested proc mount is a consequence, mediated by
`mount_too_revealing()`, and that is the step the two kernels disagree about. The flag's direct
effect, removing Docker's submounts over `/proc`, is observable from inside on every host. So
`verify.sh` asserts the direct effect, derived from the declaration like the mount boundary: if
`container.json`'s `runArgs` carry `systempaths=unconfined`, the running container must have zero
submounts under `/proc`; if not, it prints the count as a note and asserts nothing. That assertion
is the mutation's expectation, so removing the flag is caught on macOS, on Ubuntu, and on any
Docker that masks at all, with no host knowledge, no operator flag and no skip. The seccomp arm
gets no inert handling: that refusal is Docker's default profile, measured load-bearing on both
hosts, and a host where it reads MISSED is a Docker running without seccomp, which red reports
correctly. Whether to pass the flag only where it is load-bearing is an open question in
`.container/README.md` ("`/proc` has to be unmasked").

Rejected: an operator flag (`JKB_UNMASK_INERT=1`), a reflex override set once on every Mac;
keeping MISSED, since red for a fact about the machine trains people to read past red; and a
harness-owned oracle bwrap (a second invocation, or `srt` baked into the image), a second copy of
the probe that falls silent when both weaken together.

### One bwrap probe, and the diagnostic ladder is subtraction

`.container/bwrap-probe.sh` is the single probe: Claude Code's invocation, a namespace step (the
shape minus `--proc /proc`) then a proc step (the full shape, only if the first succeeded),
printing `BWRAP-NS=`, `BWRAP-PROC=` (`OK|FAILED|not-reached`) and `BWRAP-WHY=`. `verify.sh` calls
it. `mutate-verify.sh --ladder` runs it against the healthy flags and three `without` rungs (the
unmask; the unmask and AppArmor, skipped with a printed reason where no profile is passed; those
and seccomp), each row printing the flags of the array it actually ran. CI's discriminator step is
that one command. Subtraction keeps the hypothesis hand-named (three flag names) while making it
impossible for a rung to lose a flag by omission, makes the top rung the control by construction,
and is runnable by a human on Docker, which hand-spelled YAML arms never were. Deriving the arms in
YAML was rejected: it reimplements `without` in a `run:` block.

### The ladder, measured on macOS (2026-09-08)

Run by the operator on macOS / Docker Desktop, `Darwin 25.6.0`, arm64:

| rung | namespaces | proc-mount | why |
|---|---|---|---|
| `[shipped]` the container as it ships | OK | **OK** | |
| `[-unmask]` minus the /proc unmask | OK | **FAILED** | `bwrap: Can't mount proc on /newroot/proc: Operation not permitted` |
| `[-unmask -aa]`, `[-unmask -aa -seccomp]` | *skipped* | | no AppArmor on this host, so it is in no rung |
| `[-unmask -seccomp]` minus seccomp too | **FAILED** | not-reached | `bwrap: No permissions to create new namespace…` |

The top rung's printed flags are the full derived posture, the flip is attributable to the single
flag each rung names, `not-reached` distinguishes unestablished from proven no, and the AppArmor
rungs skipped with their reason. Removing `systempaths=unconfined` alone denies the nested proc
mount on macOS, which reverses an earlier claim that the unmask was inert there (see History).

### Probe strength is a separate, filed question

Dropping the unmask on Linux also drove the bwrap FAIL path, which records the shipped regression
of a probe without `--proc /proc` passing in the broken state; nothing on a Mac can drive it, so it
must not be dressed as covered there. The settled shape, scoped out and filed: run the probe's
payload as a mountinfo dump instead of `/bin/true`, count the children of the topmost `/proc` mount
by parent id (not by path; the shadowed masks still appear at `/proc/kcore`), and annotate from the
numbers, never from `rc`: FAILED is load-bearing here; passed with outside > 0 and inside = 0 is
inert here, measured; passed with inside = outside is MISSED, the probe is weaker than the
mechanism. CI should also check that the ladder and the annotation agree. Separately,
`bwrap-probe.sh` records the Claude Code version its invocation was transcribed from and nothing
enforces equality: the real builder emits `--ro-bind / /` when a write config is supplied (the
posture always supplies one) where the transcription says `--bind / /`, which does not change the
proc verdict. Extracting the argv builder from the installed `claude` bundle, `check-drift.sh`
style, is filed, not built.

## Verifying the container

Four harnesses and a CI job. `check-config.sh` makes static assertions about `.container/` and
runs in `./scripts/check.sh`; `mutate-config.sh` breaks each config property in turn and requires
a FAIL naming it, needs no Docker and runs in the gate and CI. `verify.sh` runs inside the
container and asserts the boundary; `mutate-verify.sh` breaks each property of a real container
and requires `verify.sh` to fail naming it. Both of the latter need Docker and run in the CI
`container` job, which fails the build.

### The container is executed, not simulated

Four review rounds on `.container/` produced 53 findings and about twelve must-fix, and round 4's
must-fixes were defects inside fixes made an hour earlier. Nothing in the directory had ever been
executed, so every check was a statement about the text of a script, and a statement about text
stops discriminating the moment the text is reworded. Sorted by cause, the findings are two
populations. Assertion rot (about fourteen): a self-test invoked by nothing, a guard grepping text
present on both paths, a guard with no emptiness pin, a self-test that defines its own copy of the
function under test. A wrong fact about the world (about nine, and every finding that mattered):
`fe80::` read as a way out, `grep` exit 2 read as "found nothing", `docker exec`'s 125 consumed as
the command's exit, `--detach` returning before the entrypoint decided, the base image's blanket
`NOPASSWD:ALL`, `getent` exiting 2 under an ERR trap. No harness catches the second population,
because the expected behaviour is written by the person holding the wrong model; a self-test of a
pure function validates the function against your model, and the defect was the model. What
answers it is contact with reality.

### CI has Docker, so the container runs on every push

Four rounds treated "the container cannot be run here" as a property of the project. It is a
property of the host: the posture blankets `~`, so Docker is unreachable there and `verify.sh`
and `mutate-verify.sh` were documented human-run steps nobody ran, a stable state precisely
because it is unrepeatable, which is why the static layer kept growing. GitHub's runners ship a
rootful daemon. The `container` job in `.github/workflows/ci.yml` builds the image, runs
`mutate-verify.sh --control` (`verify.sh` with the real flags) and `mutate-verify.sh`, and fails the
build. The first runs established, for the first time: the allowlist reaches the live chain
(`IPv4 bounded, IPv6 denied`), a non-allowlisted host is refused and an allowlisted one connects;
the sudoers surface is exactly the two grants; 13 of 14 mutations were CAUGHT on the first run.
They also found two defects no reading could: `mutate-verify.sh` had never worked for a host user
that is not uid 1000 (Docker Desktop maps ownership, a Linux runner at uid 1001 does not, so the
scratch store was unwritable and cleanup could not unlink container-owned directories), and the
`no NET_ADMIN` mutation could not be CAUGHT, because the entrypoint refuses to boot unbounded and
`verify.sh` never runs.

### Rejected: a fake `docker`

The first draft generalised `entrypoint.sh`'s stub (a fake `sudo` on `PATH`) to a fake `docker`,
`iptables`, `ipset` and `getent`. The `sudo` stub works because the contract with that
collaborator is one line of text this repo owns (`state=allowlisted`). `docker` is the opposite:
125 versus the command's exit, detach-versus-entrypoint timing, `.Image` versus the image's `.Id`,
exactly the facts this work kept getting wrong. A fake is a second implementation of the semantics
you misunderstand, written by whoever misunderstands them, and it converts a grep that matches
nothing into a behavioural test that validates the model against the model, which is more
dangerous because it reads as stronger evidence. A fake faithful enough to catch the staging-ipset
bug would have to model `ipset swap`. Also rejected: one human run and deleting the static layer
(a one-time run validates today's text, and the next edit regresses it with nothing re-running).

### Decisions are pure, measurements are injectable

`egress-lib.sh` states its own rule: everything that decides is pure and takes its inputs as
arguments; everything that measures is injectable (`JKB_INET6_PATH`, `JKB_EGRESS_VERDICT` and
friends). `init-firewall.sh --self-test` pins the establishment rule as a literal table, not a
re-derivation (writing the expectation as a second copy of the condition passes for any
condition), and round-trips `record_verdict` through the real `entrypoint.sh`, the one contract
spanning two files that no static check reaches. `run.sh`'s classifications (`settle_step`, the
125 handling) are pure functions over `(output, rc, liveness)` with tables. That catches
classification errors under the gate with no Docker; it cannot catch world-model errors, and CI is
what covers those.

### A fact written once takes its guard with it

Several guards existed only to police a duplication, and their comments asserted a false premise:
"the setup marker is spelled in two files and they cannot share a variable", "three different
processes" for the verdict path. `run.sh` and `setup.sh` run from one checkout, and the egress
readers can all source `egress-lib.sh`. So each fact collapsed to one copy (the setup marker, the
verdict path, the `state=` parser, `v6_state`, the root-owned-path list derived from the
Dockerfile's `COPY` lines), and each agreement guard was deleted in the same commit as the
duplication it guarded. You cannot regress a disagreement between two spellings when there is one
spelling. Where review findings asked for a new agreement guard, the duplication was removed
instead.

### The harness has one source of flags: `run.sh --print-args`

`run.sh --print-args <root>` prints the assembled docker arguments one per line (`docker_args` plus
the host-conditional AppArmor flag) and exits before resolving the container path, so CI can call
it. `mutate-verify.sh` derives its healthy flags from that output, removing `--name`, `--detach`,
`--workdir` and `--mount` with its `without` operator and appending its own base; it calls no
`dc_*` declaration reader and makes no AppArmor decision. Six review rounds of static comparison
between two assemblies preceded this, each adding a meta-guard the next round found broken. One
derivation has no agreement to guard. The failure direction is safe: a flag added to `run.sh`
flows into the control, and a flag dropped from `run.sh` is caught by `verify.sh`, which derives
from the declaration. Rejected: instrumenting `lib.sh`'s readers so the harness reports which it
called (an uninstrumented reader is absent from the observed set, and absent reads as not called).
This is the file-sync design's rule one level up: prefer an invariant the structure enforces over
one every caller must uphold.

### The declaration's effects are asserted from inside, per reader

`verify.sh` asserts each reader of `container.json` from inside the running container: the
declared `/proc` unmask is in force (`runArgs`), the running user equals `remoteUser`, and every
`containerEnv` line is present in the environment. Because `mutate-verify.sh --control` runs
`verify.sh` in CI on every push, a control that drifts from the declaration on any reader is red in
the harness's own control run, with no static guard. A static comparison kept as defence in depth
was rejected: it is a second model of one fact, and it is where the must-fixes were.

### Static checks pin known facts; they discover nothing

`check-config.sh`'s assertions and the `--self-test` blocks are regression pins: the `fe80`
fixture rows, the ERR-trap rows, `entrypoint.sh`'s stub matrix each pin a fact somebody learned the
hard way, and they are cheap and fast. They are not a discovery instrument, and treating dozens of
static assertions as one is the category error that produced rounds 2 to 4. A new static assertion
is legitimate when it pins a fact a real run taught us, and suspect when it reasons about what the
code probably does.

### A static guard is deleted where it is structurally dead, not merely duplicated at runtime

Deleted with the single-source changes, because their subject no longer exists: the control-set
block table, the reader pin and its reader list, the CI-probe extraction and its count pin, the
AppArmor-profile-name-in-CI guard, the control-flags marker pair, the single-definition and
call-shape guards over the AppArmor helpers, and each one's mutations. A wider deletion of about
twenty single-subject security guards (the `NET_ADMIN` declaration, the blanket-`NOPASSWD` removal,
`fail_closed` coverage, the `< <(` rule, the argument-refusal pins), on the argument that a runtime
observer reports each violation loudly, was declined: those guards are not copies, none produced a
finding, they are security guards trading every-machine static coverage for one CI job, and a
forty-assertion deletion is a large judgement surface on a pass meant to stop generating findings.
Filed, not done.

### Every guard is watched failing, against a negative control

`mutate-verify.sh` breaks each container property in turn (an undeclared mount, the host
`~/.claude` mounted, stock seccomp, no `NET_ADMIN`, running as root, the sudoers grant restored, the
`/proc` unmask removed) and requires `verify.sh` to fail naming it. `mutate-config.sh` does the same
for the config properties with no Docker. Each harness carries a negative control: an unmutated
run must be reported MISSED, or the matcher is matching something present when nothing is wrong.
The control must itself be able to fail: once it established the healthy exit code was 0 while
`judge` reported CAUGHT only on non-zero, so `MATCHER IS BROKEN` was unreachable. It asks the
discriminating half instead: the label must appear in a healthy container and must not be on a
`FAIL` line. `mutate-config.sh` found a live defect on its first run: the seccomp assertion grepped
for the `seccomp=…` value anywhere in the file, so deleting the `--security-opt` flag and orphaning
its value passed. It is asserted as a flag/value pair in `runArgs`.

### Assert on a discriminating signal

Four findings in one round were one shape: an assertion that matched text present on both the
pass and fail paths, or read a file nothing had written. `mutate-verify.sh` grepped a label
`verify.sh` prints identically either way, so 2 of 5 mutations reported CAUGHT with the guard
deleted, under a summary reading "every guard fired". The seccomp assertion was satisfied by the
generator's own trailing allow group, true by construction. A Linux-only test grepped an argv file
`run` never creates. The fix in each: assert on a discriminating signal (a non-zero exit plus the
FAIL-only rendering, the full mount set minus the runtime's own, the negative "no restricted entry
still names these", the precondition that the file exists).

### A guard and its mutation must both discriminate; a mutation changes exactly one thing

A guard that `run.sh` still runs `verify.sh` grepped the string `verify.sh`, which `run.sh` also
names in three failure messages, so deleting the invocation left it green; the mutation written to
watch it rewrote every occurrence, so it never established which one the guard reads. Two rules: a
guard asserting that something is invoked anchors on the invocation (a statement-level call, or
`sudo` for the firewall, which is the only way it can be invoked), never on a mention; and a
mutation changes exactly one thing, or deletes the single line. Deriving the firewall-caller guard
over the whole directory immediately produced three false positives (the name in a comment, an
error message, a `chmod` list), because "mentions it" was only ever a proxy for "calls it". The
self-test-list guards strip comments before extracting, since a commented-out self-test satisfied
them. A whitespace-exact mutation asserts its target exists before replacing it.

### A guard that qualifies a verdict must read an observation the verdict does not produce

Three guards written in one session could not fire for the case they named: a count pin sharing
its extractor's predicate (twice), and an inert-skip check comparing a `grep` to `rc`, both produced
by one `ok()` call. Write the guard's condition as a conjunction and, for each conjunct, name what
produces it in the healthy world; two conjuncts with the same producer are one conjunct. Provenance,
not truth, is the test: "is this condition true when the defect is present?" passes for an
entailed condition too. Mutation testing missed them because the mutations broke the guard's input
(deleted the marker text, respelled the assignment) while the case the guard names is a subject
defect. Mutate the defect the guard is named for, not the guard's own input. Where that world cannot
be built from where you sit, the guard is unverified, and the honest report says so rather than
writing "watched failing" about a different mutation.

### A guard's `ok` may claim only what it observed

A guard can read an independent observation and still fail on completeness: the reader pin read
source text against assembled flags and still certified a partial set. A guard's `ok` may claim
only what it observed, and its count comes from the observation, never from the expectation. An
emptiness pin is the degenerate `n > 0` case and catches only total failure; every finding of this
kind was partial. The rule is a fallback; the primary answer is to remove the second copy.

### A mutation must be attributable to the single change it names

A mutation that changes more than one thing is not evidence about the guard. `--user 4242` meant to
test the declared-user assertion also lacked a passwd entry, sudoers and write access to
`/home/vscode`, so it broke `sudo`, the firewall raise, the preamble and the memory linker, and
whichever failed first preempted the assertion. It was replaced by `--user root`: the same single
change the harness already models, root can still write, and root is not `vscode`. `judge` takes one
expectation, and running as root trips two assertions, so watching both costs one extra container,
which is cheaper than a verdict that is not attributable. A mutation can also pass on the symptom:
the `--dns 127.0.0.1` case reported CAUGHT whether or not `fail_closed` installed anything, because
a dead resolver breaks both egress probes by itself; `verify.sh` now observes what the firewall did
through the kernel probe.

### `mutate-config.sh`'s no-op guard checks presence and mode before content

A no-op mutation is reported as MISSED, a red gate blaming a guard that is fine. `mutated()` once
compared file contents only, so a mutation that removes a file simply stopped appearing (every
survivor matched), and a `chmod -x` changed nothing it could see. `seed()` records a manifest of
presence and executability, and `mutated()` checks that before content. Only the `x` bit: BSD and
GNU `stat` disagree on flags, and it is the only mode any mutation changes. `check-config.sh`
executes nothing, because it runs about sixty times inside `mutate-config.sh` and the first
version that executed generators timed out.

### Every expectation is a string `verify.sh` can print, until the CI run makes the check redundant

Rewording `verify.sh`'s `--declare` refusal left `mutate-verify.sh` grepping for text it never
prints, and that harness needed Docker, so the gate could not notice. `check-config.sh` statically
checks every `mutate-verify.sh` expectation is a string `verify.sh` can print, and
`mutate-config.sh` mutates that check. The layer exists because `mutate-verify.sh` never ran; with
it running in CI, staleness surfaces as a readable MISSED. It was kept until the CI job had been
observed green, because deleting a guard on the strength of an unproven replacement is the
unearned confidence being corrected; that condition is now met, and unwinding it is filed as its
own piece. `PINNED_BAD_SITES`, a hand-kept count of `bad(...)` sites in `check-config.sh` that fires
on every legitimate edit, is likewise still present, with its replacement filed: a derived check
that every `bad(...)` site has a mutation reaching it.

### Two lists that must agree are derived

`scripts/check.sh` and `.github/workflows/ci.yml` each enumerate the `.container/*.sh --self-test`
invocations; `check-config.sh` extracts both sets and requires them equal, plus a second guard that
every `--self-test` in `.container/` is run by the gate, since both lists could otherwise agree
about running nothing. It found a live one immediately: `link-claude-memory.sh --self-test` ran in
the gate and nowhere in CI. The same rule governs the consumed keys, the mount list and the drift
artifacts.

### A skip decided per assertion is not a skip

`run` refuses on a Linux host without bubblewrap, correctly, but the argv assertions ran
unconditionally, so the shared gate went red for a fact about the machine, and the drift assertion
three lines below asked only for a non-zero exit and no argv file, which is exactly what the
dependency refusal produces: it would have passed having never exercised the refusal it names. The
group is skipped as a group, announced, and the drift assertion matches the refusal text. A skip in
CI is skipped by name and printed; an unnamed skip makes a green tick mean less than it appears to.

### A green harness is evidence about the paths it runs

After the container harness went green, a claim that the container files were exercised was wrong:
the `getent` ERR-trap defect lives on the DNS-failure path, which the harness does not drive (it
covers the happy path and the no-`NET_ADMIN` path). Generalising a run into a property of a file is
the same shape as an unknown reported as a definite answer.

### A test fixture assumes nothing about the machine it runs on

Three sweep tests named `/home/vscode/repos/jkb` as "a repo this machine cannot reach", which is
precisely the bind target the container adds, so inside the container the path exists: the tests
took the opposite arm, the gate was red in the environment the change existed to introduce, and two
of them ran `git worktree prune` against the real checkout. Unreachability is a property of the
fixture (a tempdir path never created), not a claim about the world.

### Shell pitfalls the checking apparatus hit

Each produced a plausible value or a silent pass rather than an error. `( set -e; v="$(f)" ) && ok
|| aborted` cannot observe errexit, because bash suppresses it for any command in a `&&`/`||` list
and the suppression propagates into the subshell; observing errexit needs a separate process
(`bash -c` with `declare -f`, so it tests the real function and not a copy). A function ending in a
`grep` that exits 1 on no match, called as a bare assignment under `pipefail`, exits the script
having printed nothing. `$(printf '\n')` is the empty string, since command substitution strips
trailing newlines, so a `case` pattern meant to reject multi-line output became `**`. A backtick
inside double quotes after `\\` is unescaped, so bash ran `mount` and used its output as an
expectation string, and `bash -n` passed; use single quotes. Asking a generator that does not
implement `--print-target` does not get a refusal: it ignores the argument, fetches, rewrites its
artifact and returns its progress output as the target, so callers read the generator's text first.
The first CI drift step regenerated and then asked `git diff`, which was empty because regeneration
had already overwritten the edit; the check compares against a snapshot instead. `dc_apparmor_profile`
could not read upstream's quoted `profile "{{.Name}}"`, returning empty to
`--security-opt apparmor=`, and the hand-written profile was unquoted, so reader and file were
wrong together and agreed.

### Scripts carry specification; the README carries measurement, dated, once

The `.container` scripts were about half comment lines, many narrating a past defect or a
host-specific measurement, and one was provably false (a retracted macOS measurement surviving
seventy lines below the line reversing it). The test is: pinnable, or dated. A comment may state
what the code asserts, and the one fact that makes the obvious simpler version wrong. Anything
carrying a date, a host, a binary version, a commit id, a review round, or "used to" is a
measurement or a memory: it lives in `.container/README.md` or this design, once, and the script
points at it. A prose harness was rejected: prose has no failure path to watch, and what makes the
contradiction unrepresentable is the measurement existing in one place.

### The stopping rule for a harness pass

A pass over the harnesses lands only when, in order: the count went down (`check-config.sh` ok rows,
`mutate-config.sh` mutations and net lines all fall; a pass that adds mechanism is not this pass);
there is one assembly (no `dc_run_args`, `dc_remote_user`, `dc_container_env` or
`dc_require_apparmor_profile` in `mutate-verify.sh`); one probe (the bwrap invocation only in
`bwrap-probe.sh`); one statement per measurement; no guard reads a sibling harness's source except
the expectation strings and the self-test lists; every survivor is watched (`mutate-config.sh`
reports N of N, and its negative control still reports MISSED); one human run of `mutate-verify.sh`
and `--ladder` on Docker, pasted dated into the record; and one `low` review with zero must-fix and
no finding inside the lines the pass added. A must-fix inside the pass's own additions means the
design is wrong, and the answer is a design pass, not another round. The 2026-09-08 run met item 7
after the `--user` fix.

### What remains open or human

`container.json` still speaks the Dev Containers dialect (`//` comments, string-or-object mounts,
`${localEnv:}`, `${localWorkspaceFolderBasename}`) though one program, `run.sh`, reads it; plain JSON
with one mount spelling would delete parsing surface the guards keep re-checking. Filed as its own
change. A `run.sh` lifecycle scenario driving the real script against a container configured to
refuse (unfiltered egress, no override; using `JKB_CONTAINER_NAME`/`JKB_CONTAINER_IMAGE`) is open:
it would prove the exit-3 branch is reached, a dead container is not reported attachable, and a
slow entrypoint is waited for; until then `in_container`'s exit-on-death path and `container_died`
have not been executed. `check-config.sh`'s seccomp assertion does not inspect `includes`/`excludes`,
so a conditional allow would satisfy it (the vendored profile's allow is unconditional; the gap is
filed). The macOS-host end-to-end (`run.sh --open`, VS Code attaching) is not drivable by any
harness.

## Session worktrees, deferred disposal and the reap service

A sandboxed session cannot delete its own working directories, so disposal is an archive, the
landing that cannot archive records the worktree, and a reaper outside the session finishes it.
The code is `crates/jkb-cli/src/archive.rs`; the landing and claim rules are the task-lifecycle
design.

### A session worktree is archived, never deleted

`git worktree remove` unlinks recursively and stops at the first refusal; from inside a sandboxed
session that refusal is `<worktree>/.claude/settings.json` (Claude Code protects a project's policy
files from the agent whose policy they are), and 152 files were already gone, with the error naming
the directory and not the 62,421 lines. Disposal is one atomic `fs::rename` into
`<repo>/.jkb/archive/<session>-<stamp>`: partial destruction stops being representable rather than
being guarded against. `jkb task reap` deletes an archive once it is 30 days old, probing with
`remove_dir` first (`EPERM` versus `ENOTEMPTY`) so it never begins a walk it cannot finish.

### The refusal is scoped to the session's own directories, which is what makes deferral work

Measured across five live worktrees: only the session's own tree answers `EPERM`; every other
answers `ENOTEMPTY`. The deny is not ours (`auto-mode-posture.json` names only `~/.claude/*`), so
there is no knob, and there should not be: a session that could write its own `.claude/hooks/`
could run anything. So `land` never blocks: it grafts, records the worktree it could not move,
applies its plan, and any other process finishes it. `jkb service install` writes two units,
`com.jkb.sync` and `com.jkb.reap`, kept apart so a wedged file watcher does not also stop every
deferred landing. `jkb doctor` reports what is outstanding; `--fix` sweeps it.

### Every disposal goes through `archive::dispose`

`jkb task abandon` still called `git worktree remove`: the verb an operator reaches for to clear
the directory a deferred landing leaves behind was the one that gutted it. Both verbs go through
`archive::dispose`, the callee that remembers the rule instead of two call sites that must.
Whether to delete the branch is the caller's, because a landing's branch duplicates commits already
in the target while an abandoned branch holds the only copy.

### A record carries the decision that produced it, and can be cancelled

`dispose` once took `delete_branch` as an argument and threw it away, so the reaper applied land's
defaults to an `abandon` record and force-deleted the branch the verb had just printed "kept" for;
`--force`'s acceptance of a dirty tree was likewise unrecorded, so the sweep's dirty check held that
record for ever; and nothing could revoke a record, so `jkb task work` resuming a deferred session
got the directory back with a reaper still holding a claim on it. `archive::Plan` is part of the
record, `archive::revoke` is the cancel, and `task work` calls it before the worktree is handed
over. A refusal stops the verb: when a sweep holds the lock, `revoke` refuses, and a caller that
downgraded that refusal to a note licensed the sweep to archive the checkout it had just told the
operator to work in.

### A record establishes identity before acting, and one disposal is one record

A record names a path and a branch, and both are reusable names: remove a deferred worktree by
hand and `jkb task work` recreates a session at the same path on the same branch, and a sweep keyed
on those two would archive the live tree and force-delete its branch. The sweep checks that git
still registers the path as a worktree, that it is on the commit the landing recorded
(`Entry.head`; a commit id is not reused), and that it is clean. The record's own identity is not
the path either: a session name is reused (abandon, reopen, `task work` mints the same name), so a
marker named from the path let the next disposal overwrite the first. When several pending records
name one tree (`abandon --delete-branch`, then `abandon` again), the newest governs and the rest
are superseded; archived records are never superseded, because each names a distinct archive that
still has to be swept.

### The record store is untrusted input, so it gets a parser

The retention arm once passed whatever absolute path a record named to `remove_dir_all`. The
record store lived in `~/.jkb`, bind-mounted into the container and granted in the posture's
`allowWrite`, while the host's reaper is a launchd agent outside every sandbox, so an
agent-writable record steered an unsandboxed recursive delete at any directory; corruption reaches
the same place with no adversary. It survived two reviews. Both paths are constrained to
`<repo>/.jkb/{work,archive}`, checked once above both arms. The first containment guard did not
hold, because `Path::starts_with` compares components without interpreting them, so
`<repo>/.jkb/archive/../../../Documents` starts with the archive root while naming something else.
`Entry` is the wire form and is trusted for nothing; `archive::Record` is what the sweep sees; and
`Record::parse` is the only way between them, so no arm can skip it. `..` and `.` are refused rather
than resolved: nothing writes one, so a record containing one is corrupt or hostile.

### Reachability belongs above the dispatch, and unknown is not settled

The archived arm read "not visible from here" as "somebody removed it by hand" and dropped the
record, so each side of the container bind destroyed the other's archived records, and the
multi-gigabyte checkout each named became unreferenced and permanent. An absent directory is
evidence of removal only when the repo it lives under is reachable, and that is checked above both
arms. A repo root the sweep cannot reach leaves the record alone.

### One sweep at a time, under a lease that only proof of death breaks

Two concurrent sweeps lost each other's updates (the second finds the worktree gone and drops the
record the first just wrote), so a lock covers the reads as well as the writes, stale only when its
holder is proven gone. A lock nothing can break is a wedge: a container killed mid-sweep and rebuilt
takes its hostname with it, so its lock's holder is never provably dead. The default stays, since
breaking a live sweeper's lock is what the lock prevents, and the escape is a person's: the refusal
names the holder, and `jkb task reap --break-lock` exists.

### The records and the lock live in the database

A file beside the database was no lock across the two kernels that share `~/.jkb`, and a dev
container reaching the knowledge base through `jkb serve` has no path to it. The records are rows
of `worktree_removals`, their paths written `~/repos/…` where they lie there, so the host's reap
service resolves a record the container wrote to the same checkout, which gives the container's
deferrals a finisher (a session cannot archive its own checkout, so every `land` in the container
defers, and the container has no init system to run a service). The lock is the `removal-sweep`
row of `leases`, holder `<owner> <nonce>`, takeover and release as compare-and-sets on the exact
holder. The old file store is only listed for the operator; nothing reads or acts on its files. The
op rules are the daemon-and-messaging design. `jkb task reap` opens the database each pass and also
compacts the message queue, so a database a newer jkb migrated fails that pass, reported once while
unchanged, and never stops the service (pinned by `task_reap_compacts_the_message_queue`).

### `gitrepo::deletions_only` tells a part-way removal from work in progress

The second land attempt over a half-deleted worktree refused with "it has uncommitted changes —
commit them in the session first", which over 152 deletions means committing the wreckage.
`deletions_only` asks four whitespace-free git questions rather than parsing
`status --porcelain`, whose leading status column is exactly what the trimming capture helper eats.

## Liveness across the container boundary

Host and container share `~/.jkb` and the knowledge base's claims, so a liveness fact has to say
which side it was observed from.

### The liveness probe is a syscall, not `ps`

`ps` is setuid-root on macOS and a sandboxed process cannot exec setuid, so under the posture
`owner::pid_exists` could never run and every `host:pid` owner read as `Fact::Unknown`. `ps` was
chosen over `kill -0` because it reports processes it does not own (the agents-and-roles design's
claim model), and that reasoning is about the shell builtin, which collapses `EPERM` and `ESRCH`
into one non-zero exit. The syscall separates them, and `EPERM` is positive evidence of existence:
the kernel refuses because the process is there and is not ours.
`rustix::process::test_kill_process` is a safe wrapper (no `unsafe`; rustix was already in the
tree), so the probe needs no subprocess, no `PATH` and no setuid binary. It is better with no
sandbox in the picture at all: no fork/exec per probe, identical on macOS and Linux, and the
mapping is a pure function, so the `Unknown` arm that protects every claim is an ordinary
assertion. A pid outside `pid_t` is `No`, not `Unknown`: no process can carry that id. Untested:
whether a sandbox profile permits `kill(pid, 0)` against a foreign-owned process. jkb's claimants
are the same user's processes, so the live answers are `Ok`/`ESRCH`, and a denial would return
`EPERM`, "alive", the safe direction.

### A pid is meaningless without the host that issued it

`Liveness::Process` carried only the pid, so a claim or a sweep lock written inside the container
was probed against the host's process table: a live owner reported dead and freed, or a dead one
reported alive. It carries the host, and a foreign host is `Unknown`, which frees nothing.
`hostname()` no longer falls back to the literal `"localhost"`, which both sides of the boundary
answered, so the rule's two sides gave the same name and the rule was not one.

### An absence is only proof where the place it would be is visible

`Liveness::Worktree` was not host-qualified. A host session claims as `session:<pid>:/Users/…`; in
the container that path is absent, `try_exists` said `false`, and the reclaim freed the claim of a
session running on the host. The session id carries no host, so the question is asked of the
filesystem: the parent directory must exist for an absence to count. That is the archive sweep's
reachability rule one level down, and it needs no change to an id format already in databases.
When several members of a set could answer, name which one is authoritative instead of letting
whichever is reached answer.

## History

Superseded and reversed decisions, with what replaced them and why. Review-round narratives are
compressed to the lesson.

### A container buys nothing

The posture's first design argued a container "buys nothing, because the sketch mounts `~/repos`
and `~/.claude` and that is the blast radius". Right about Bash, wrong about everything else: for
the in-process tools a container puts the `claude` process in a mount namespace, making file access
default-deny by the kernel, which the permission rule model cannot express. Replaced by the
both-layers dev container. A related conflation, "stock Docker cannot host it", was true only of
nesting the sandbox, never of limited mounts.

### `CLAUDE_CODE_SANDBOXED` as the confinement test

The record once said to settle whether the sandbox engages with `printenv CLAUDE_CODE_SANDBOXED`.
It was unset throughout the host measurement in which the sandbox demonstrably refused writes.
Replaced by `auto-mode.sh sandboxed`, which asks the kernel for the errno. Before the errno rule,
three rounds of `sandboxed` reported CONFINED for refusals unrelated to the sandbox (a directory
squatting the canary path, an absent `$HOME`, a read-only `$HOME`, an allowWrite subdirectory
beneath an unwritable one), each fix adding another premise check; the errno subsumes them all.
Two credential-free discriminators were also tried in a stock container and failed: an invalid key
hangs identically with and without sandbox config, and `failIfUnavailable` did not error before
`Not logged in` even where bwrap provably could not start.

### Three inert `Write(...)` deny rules

The posture shipped `Write(...)` deny rules beside `Edit(...)` rules for the same paths. Claude
Code printed on every session start that `Write(path)` is not matched by file permission checks,
only `Edit(path)` is. Nothing was unprotected, but an inert rule in a security posture reads as
protection, and a warning printed at every start trains people to ignore warnings. `claude doctor`
could not catch it (schema-valid, semantically wrong). Removing them from `require` did nothing,
which is what introduced `retire`.

### `sandbox.excludedCommands: ["ps"]` for the liveness probe

Under the posture `ps` could not exec, and the only sandbox-level lever was `excludedCommands`,
which runs a command wholly outside the sandbox and which `forbid` requires empty, since `require`
could not bound it (`["ps"]` could become `["ps","bash"]`). Neither was needed: replaced by the
`kill(pid, 0)` syscall through rustix. Earlier versions of the probe reached the `Unknown` arm by
naming a nonexistent program, and before that by emptying `PATH`, which reddened the shared gate one
run in six.

### The credential bind mount, and the symlink residual

`.container/` first bind-mounted `~/.claude/.credentials.json` read-only. It could not work on
macOS (the file does not exist there and a missing bind source is a hard error) and a login could
not write it. Removed; authentication moved inside, into the state volume. Removing the mount left
`setup.sh`'s symlink loop shaped around it (linking only directories), so an in-container login sat
in the writable layer and died with the next rebuild; the file links were added. The residual was
then stated as "a temp-and-rename writer costs one re-login". Superseded 2026-09-18: Claude Code
2.1.276 replaces the link on every login and token refresh, so `verify.sh` failed from then on;
`dc_persist_login` replaced the residual.

### Opening the container through Dev Containers

The container was first a Dev Containers config (`devcontainer.json`), with `workspaceFolder`
following the opened folder (`${localWorkspaceFolderBasename}`), an `initializeCommand` and
`check-workspace.sh` refusing a folder the mount could not place, and the firewall raised as the
first act of `setup.sh` because the lifecycle is postCreate then postStart. `workspaceFolder` can
only be built from the basename, with no variable for a path relative to the mount, and
`initializeCommand` cannot supply one (a subprocess cannot set its parent's environment, and
substitution has already happened), so `~/repos/jkb/.jkb/work/sess` resolved to
`/home/vscode/repos/sess`, and a literal fallback opened a different checkout silently with every
guard passing. `check-workspace.sh` also advised setting `JKB_REPOS_DIR`, read by nothing, so
following the advice switched the preflight off without moving the mount: a remedy the machine does
not accept is worse than none. Replaced by `run.sh` and attaching, the ENTRYPOINT raise, and the
`.container/` rename.

### `verify.sh`'s workspace assertion, three wordings

The assertion that the workspace is mounted went through a hard-coded path (true of whichever
checkout sits there), the script's own directory containing a `Cargo.toml` (true by construction
wherever it can run), and the declared target in `mountinfo` (which the harness's own bind layout
never produces, so every mutation reported CAUGHT and then the control failed and judged nothing).
Replaced by asking whether this checkout is inside a mount point that is both mounted and declared,
with `--declare` folded in.

### The egress-failed marker

`init-firewall.sh` computed whether egress was denied, printed it, and threw it away; it recorded a
cause string in `/run/jkb-egress-failed`, written by `fail_closed` before any `iptables` call, with
every call `|| true`. So the marker meant "`fail_closed` ran", not "egress is denied", and the
raise's four endings (success; deny-all installed; deny-all failed; aborted) collapsed into two,
with deny-all installed and deny-all failed writing identical text. `entrypoint.sh` read presence
as proof, printed "egress is DENIED" and booted, which on a failed deny-all gave an unattended agent
an open network on the `docker start` routes. It is the house defect: an unestablished answer
spelled as a definite one, like `Fact::Unknown` collapsed to `false`, `ahead_count` returning `0`,
or `has_own_commits` answering no when `rev-list` failed. Replaced by a verdict written on every
ending, with absence its own answer (`unknown`, never `denied`), which made writing it late safe.

### The stored verdict as the boot decision

The verdict file then decided the boot. Nothing said when an older verdict stops counting, so it
counted for ever: `docker stop` destroys every rule while the file survives in the writable layer,
and a raise that died before recording left the previous start's `allowlisted` standing, so the
entrypoint exec'd the agent onto an empty OUTPUT chain silently. The task-lifecycle design already
had the lesson ("evidence of a landing is spent once the task is put back to work"): turning a
history into a present-tense answer needs a rule for when an older row stops counting, and here it
was written nowhere. Replaced by `egress-status.sh` asking the kernel; the file survives for the
reason only.

### `v6_state` as "a non-loopback address exists"

Named "is IPv6 egress provably closed" while asking "does a non-loopback address exist", it was
wrong both ways: a link-local `fe80::` read as `open` and refused ordinary containers, and an
unreadable table returned `absent`, read as provably closed, with grep's exit 2 hidden behind
`2>/dev/null`. The self-test row that pinned the unreadable case as `absent` passed: verifying a test
fails for the right reason is not the same check as verifying it asserts the right thing. Replaced
by the path measurement. An earlier copy of `v6_state` inside the ERR-trap self-test meant that row
tested a copy; the copy was deleted when the function moved into `egress-lib.sh`.

### The exit-3 wording "configured to accept"

`verify.sh`'s exit 3 was described as "a condition this container was configured to accept", true
while the unfiltered-egress override was its only producer. The transcript-budget check made it
false: nobody configures a container to accumulate workflow journals past what any sweep reclaims,
and the old message sent an operator to unset a variable that was not set and recreate a container
whose journals live in a volume. Replaced by a message that names no cause and points at the FAIL
lines.

### Agreement guards over duplicated constants

`check-config.sh` asserted that the setup marker's two spellings agreed, that the verdict path's
three spellings agreed, and that the two `state=` parsers agreed, each guarded by mutations, on the
false premise that the files could not share a variable. Replaced by one constant or function each,
with the guard deleted in the same commit.

### Assembly comparisons between `mutate-verify.sh` and `run.sh`

`mutate-verify.sh` assembled its own healthy flags from `dc_*` declaration readers, and
`check-config.sh` compared them: first `--print-flags` against `dc_run_args` alone, which left all
57 checks green and all 83 mutations caught when the user or env expansion was deleted; then three
contiguous blocks per reader from a table, plus a pin grepping the assembly for `dc_[a-z_]*(` calls,
rewritten three times, each rewrite finding a new way to read nothing. Across six rounds the
finding counts rose (6/1, 8/1, 7/0, 3/2, 7/2, 10/3 findings/must-fix), because each round added a
meta-guard the next reviewed. The per-reader property was right; the mechanism was the defect.
Replaced by `run.sh --print-args` and per-reader assertions inside `verify.sh`.

### CI's four hand-spelled bwrap arms

CI first discriminated the bwrap failure with a probe of its own beside `verify.sh`'s, in four
hand-written arms. Its first version granted no `NET_ADMIN`, so every arm hit the entrypoint's
refusal and printed identical output, an experiment that discriminated nothing and read as "all
three fail, so it is not AppArmor"; each arm then classified its output as ran or `DID NOT RUN`.
The arms kept losing flags by omission, and the probe body was a second copy. Replaced by
`bwrap-probe.sh` and `mutate-verify.sh --ladder`.

### The bubblewrap failure attributed to AppArmor's userns restriction

When bwrap first failed in CI with `Failed to make / slave` (an `EPERM` on
`mount(MS_SLAVE|MS_REC)` after its namespaces were created), a note called it a correction to the
recorded "fails at namespace creation" measurement; that measurement was about stock Docker, and
`generate-seccomp.sh` already records that re-allowing only the namespace calls moves the failure to
`mount`. With seccomp verifiably allowing `mount` unconditionally, the leading hypothesis became
`apparmor_restrict_unprivileged_userns=1`. Run 2 ruled it out: with `jkb-dev` loaded the MS_SLAVE
mount succeeded inside the namespace, which a transitioned process could not do. `apparmor=unconfined`
was never adopted; the profile replaced it. The `stock seccomp` mutation was meanwhile CAUGHT for the
wrong reason, since the healthy container failed the same bubblewrap assertion, and became
discriminating once the control was green. `JKB_ACCEPT_NO_BWRAP` was set while the failure was
undiagnosed and removed once a remedy existed to verify.

### The hand-written AppArmor profile and its bespoke CI check

The first `apparmor-jkb-dev` was transcribed from memory and wrong in five ways no guard could see.
Replaced by `generate-apparmor.sh`. Its first CI check was a bespoke step regenerating the profile
and asking `git diff`, which passed a gutted profile because regeneration overwrote the edit.
Replaced by `check-drift.sh`, derived over every generator.

### The unmask mutation judged by its consequence, and `RUN_INERT_IF`

`mutate-verify.sh`'s `/proc` unmask arm asserted the consequence (bwrap's proc mount fails) and
reported MISSED on macOS, where LinuxKit permits the nested mount anyway. The conclusion drawn, that
the mutation was unjudgeable there, was false. A `RUN_INERT_IF` mechanism with a `run()` branch, two
`check-config.sh` guards and three `mutate-config.sh` cases was invented to survive it, and a script
comment recorded the unmask as inert on macOS. Replaced by asserting the flag's direct effect;
`RUN_INERT_IF` and its guards were deleted, and the 2026-09-08 ladder run measured the unmask
load-bearing on macOS too.

### The hand-written expected mount list, and prefix filtering

`verify.sh` once filtered `mountinfo` by target prefix, which made it a list of absences
(`/var/run/docker.sock` passed), and later kept a hand-written mount list beside `container.json`
that lost `.cargo/registry` in an edit, guarded by a `CARGO_TARGET_DIR` check aimed at one string in
that file that could not see the list beside it: a guard aimed at the instance, not the class.
Replaced by exact mount points derived from the declaration.

### Archive records and the sweep lock as files

Records and the sweep lock first lived as files under `~/.jkb`, the lock with the land lock's
proven-gone rule. Across the host and container kernels a file lock was no lock, and the container
reaching the knowledge base through `jkb serve` had no path to the files. The container compensated
by sweeping once per start in `postStartCommand`, best-effort behind the firewall raise. Replaced by
`worktree_removals` rows and the `removal-sweep` lease, finished by the host's reap service. In
between, `jkb task reap` was changed to not open the database at all, because migrating first turned
the shared-`jkb.db` divergence into a launchd restart loop; that was superseded when the reaper
moved to the database and gained queue compaction, which opens it per pass and reports a failure
without stopping.
