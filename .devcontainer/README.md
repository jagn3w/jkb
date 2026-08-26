# Dev container: Claude Code unattended, behind two boundaries

The "both layers" configuration of design D49 — a container **and** Claude Code's own sandbox
nested inside it. `../scripts/auto-mode.sh` alone is the host-only configuration; this adds the
one property the host cannot express.

## What each layer is for

| | Container | Nested sandbox (`sandbox.enabled`) |
|---|---|---|
| File access | **default-deny by the kernel** — an unmounted host path does not exist in here, so `Read`/`Glob`/`Grep`/`Edit` are bounded by the mount namespace, not by permission rules | default-deny for **Bash** via `denyRead`/`allowRead` |
| Network | coarse: an IP allowlist (`init-firewall.sh`) | precise: a hostname allowlist at a proxy (`strictAllowlist`) |
| Bash | not confined beyond the container | per-command confinement |
| Fails when | the mount list is edited carelessly | it cannot start (missing deps, wrong container flags) |

They fail for different reasons, which is the whole argument for running both. The container's
egress filter exists precisely because the sandbox's is *inside the layer that might not start*:
a container's default egress is unrestricted, so a container whose nested sandbox failed silently
would be a **downgrade** on exfiltration versus the host.

### Editing the allowlist takes a rebuild

Both layers read their domains from `scripts/auto-mode-posture.json`, but they read it at
different moments. The **sandbox** picks up an edit the next time the posture is installed. The
**firewall** reads it once, at container create, and snapshots it somewhere only root can write —
because that file lives in the bind-mounted workspace, where the agent this layer exists to bound
can edit it. Handing the workspace copy to a root-run script would let an agent widen its own
egress by appending a line and waiting for a restart.

So: **add a domain, then Rebuild Container.** A restart is not enough. The firewall says so on
every raise when the two differ, and refuses outright if the snapshot is empty or unparseable
rather than raising a firewall that blocks everything and reports success.

## The measurements this is built on

Taken in a Linux VM (Ubuntu 26.04, kernel 7.0, Docker 29.7), with a no-container baseline first so
a failure is attributable to the container profile and not the kernel.

- **Stock Docker cannot run the nested sandbox.** `bwrap` fails at namespace creation as root
  *and* as non-root, with `--cap-add SYS_ADMIN`, and with AppArmor disabled.
- **The blocker is seccomp, and the fix is narrow.** Neither `--privileged` nor
  `seccomp=unconfined` is needed: Docker's default profile plus an unconditional allow for 14
  namespace/mount syscalls is sufficient (`generate-seccomp.sh`). Those syscalls are only
  reachable *inside* the user namespace `bwrap` creates, where the process holds no privilege over
  the host.
- **Non-root is load-bearing, not hygiene.** With seccomp disabled entirely, *root* in a container
  still cannot create a mount/net/pid namespace directly — only the `--unshare-user` variants
  work. Non-root passes everything.

## Using it

Needs a container runtime on the host (Docker Desktop, OrbStack, colima, or Apple's `container`),
which macOS does not ship. Then "Reopen in Container" in VS Code, or:

```sh
docker build -t jkb-dev .devcontainer
./.devcontainer/verify.sh          # inside the container: assert it is what it claims
```

`setup.sh` runs on create: firewall first (the lifecycle is postCreate → postStart, so leaving it
to `postStartCommand` would run the whole of create with open egress), then the posture, then
`verify.sh`.

## The mount list is the security boundary

Everything absent from `devcontainer.json`'s `mounts` does not exist inside the container. Add to
it one path at a time and never mount all of `$HOME`. `verify.sh` asserts the mounted set is
**exactly** what is declared — exhaustively, from `/proc/self/mountinfo`, rather than by listing
paths that ought to be absent, because a list of absences can never be complete.

**Nothing** under `~/.claude` is mounted from the host — not `settings.json`, which **is** the
posture and which a process the posture bounds must not be able to read or write, and not the
credential file either. Authenticate inside the container (`claude auth login`); `setup.sh` links
the credential and account-state files into the `.claude-state` volume, so a login survives a
rebuild without anything of the host's being visible.

The expected set is **derived** from `devcontainer.json`, so adding a mount is a one-file change
and cannot drift out of step with the verifier. It used to be transcribed into `verify.sh` as
well, and the first time the mounts changed the copy went stale and a correctly-built container
failed its own verifier.

All of **`~/repos`** is mounted, at `/home/vscode/repos`, and `workspaceFolder` follows the folder
you opened — `/home/vscode/repos/${localWorkspaceFolderBasename}`. The argument
for the width is consistency, not convenience: `scripts/auto-mode-posture.json` already grants
`~/repos` in both `allowRead` and `allowWrite`, so a container holding only jkb was *tighter* than
the boundary the same agent runs under on the host. That difference was nothing anyone had
decided, and it made a cross-repo task impossible in here rather than deliberately refused.
Everything the posture does not grant is still absent by the kernel: `~/.ssh`, `~/.aws`,
`~/Documents`, the rest of `$HOME`.

**Only a folder directly inside `~/repos` can be opened**, and `initializeCommand`
(`check-workspace.sh`) refuses anything else before the container is created. A literal
`workspaceFolder` would be worse than a refusal: a `jkb task work` session lives at
`<repo>/.jkb/work/<session>`, which is inside the mount but is not `~/repos/<name>`, so the
container would start the agent in the **main checkout** instead — silently, with every guard
still passing, because the wrong repo is a perfectly good repo. Keeping those two apart is the
entire point of a session (D36). Sessions are worked on the host; the container is for the main
checkout.

### A nested bind must be named

`verify.sh` compares exact mount points, with no prefix logic — filtering by prefix is what once
let `$HOME` at `/host` through. So a bind *inside* a declared target is undeclared, and
`mutate-verify.sh` needs exactly one: it spells its own docker flags and must mount the repo at
`/home/vscode/repos/jkb`, because in a `jkb task work` session the repo's parent directory is
`.jkb/work` and mounting it would put the checkout at `/home/vscode/repos/<session>`.

Nesting is **not** granted automatically. A mount point and a mount source are independent —
`-v ~/.ssh:/home/vscode/repos/jkb/secrets` is inside the declared region and is still
exfiltration — and the source cannot be checked from inside the container, because on Docker
Desktop for macOS `/proc/self/mountinfo` reports the path inside the VM rather than the host path.
So the exception is named instead: `verify.sh --declare <mount-point>` **adds** to the derived set
(it can never switch a check off) and is **refused** unless the value is a strict descendant of a
target `devcontainer.json` declares **as a bind**. Not a volume: a named volume reaches no host
filesystem, which is exactly why `check-config.sh` reviews bind sources and waves volumes through
— so a bind nested under `~/.cargo/target` would be a host mount somewhere nobody reviewed. `--declare /host`, `--declare /var/run/docker.sock`
and `--declare /home/vscode/.claude/settings.json` are therefore all refused by `verify.sh`
itself, and `mutate-verify.sh` watches that refusal fire. The count appears in the passing line,
because an override nobody can see is indistinguishable from a rule that does not exist.

### Auto-memory is shared through `~/.jkb`, not through a mount

Claude Code keys auto-memory by the project's **absolute path** —
`~/.claude/projects/<slug>/memory/`, where `<slug>` is that path with every character outside
`[A-Za-z0-9-]` replaced by `-`. So one repo has two keys, `-Users-you-repos-jkb` on the host and
`-home-vscode-repos-jkb` in here, and widening the workspace mount does not change that: the key
comes from where the repo *is*, not from what is in it.

The obvious fix — bind the host's memory directory in — is the one mount this design forbids, for
the reason above. So the store lives at **`~/.jkb/claude-memory/<repo>/`** instead, inside the
bind that already exists and is already reviewed, and each side symlinks its own slug's `memory`
directory at it. `scripts/link-claude-memory.sh` does both sides; `setup.sh` runs it on every
container create, and on the host it is opt-in (`./scripts/setup.sh --link-memory`) because it
writes under `~/.claude` and the `post-merge` hook re-runs `setup.sh` after every pull. Nothing it
does overwrites, and it decides **before it moves anything**: if any name exists on both sides,
nothing is moved, no link is made, and the collision is reported. Migrating what it could and then
declining the link left that side holding only the colliding file, with its `MEMORY.md` naming
notes it could no longer read — worse than never having run. A symlink pointing elsewhere is never
retargeted, and a store holding anything but plain files is refused rather than followed: it is
written by agents on both sides of the boundary, and a symlink planted in it would redirect the
other side's reads and writes wherever it points.

`verify.sh` **asks the linker** (`--status`) rather than inferring breakage from a missing link.
The linker leaves the link absent in states it recognises — a collision, an unsafe store — so
reading "no link" as "broken" failed `postCreate` for a state the design calls normal. Those
states report and pass; only an unexplained absence fails.

Stated plainly, because it is a hole in "the boundary is what you did not mount" — and there are
**two** channels here, not the one this was designed around.

The first is container → host: memory is agent-**writable** prose injected into context, so a
shared store carries text from container sessions into the less-confined host ones. Same person's
agents at both ends, prose rather than code, through a directory that was already shared.

The second was measured rather than predicted. `~/.claude/projects` sits under the posture's
blanket `denyRead` of `~` and in no allow list, so **sandboxed Bash cannot touch auto-memory at
all**; `~/.jkb` is in both `allowRead` and `allowWrite`, because jkb's database lives there. So
linking moves memory from a place sandboxed Bash cannot reach into one where a single
auto-approved command can rewrite it — for this repo and, through the same grant, for every other
repo's store. The posture has no write-deny to carve `claude-memory` back out with, so this is
**accepted, not mitigated**, and it is why the host side is opt-in (`setup.sh --link-memory`) and
never created by a `git pull`. If the trade is not wanted, the store belongs at a path the posture
does not grant, with its own declared bind into the container.

## Root is not reachable from inside

The mount boundary, the root-owned firewall, its allowlist snapshot and the pinned sudoers
argument are all protections against a process that cannot become root — so `vscode` may run
exactly one command as root, `init-firewall.sh`, with no arguments. The base image ships
`/etc/sudoers.d/vscode` granting `NOPASSWD:ALL`, which would make every one of those bypassable
with a single `sudo`; the Dockerfile removes it and `verify.sh` asks *sudo itself* what is
permitted, so a blanket grant re-added by any route fails.

The cost is real and intended: you cannot `sudo apt install` inside the container. Add packages to
the Dockerfile and rebuild, or `docker exec -u root` from the host.

## Verifying it

- `verify.sh` — inside the container: non-root, bwrap works, the mount set is exactly as declared,
  `~/.claude` is not a host mount, root is reachable only for the firewall, egress is denied *and*
  the allowlist still works, posture intact.
- **Run these from your own terminal, not from an agent session.** Once the host posture is
  installed the Docker CLI is unreachable — `~/.docker/bin` is under `denyRead: ["~"]` and in no
  `allowRead` entry, so it fails with `Operation not permitted`. That is the posture working: an
  unattended agent that can talk to Docker can mount `/` into a container and is root on the host.
  Allowlisting it to make the harness runnable would trade the boundary for convenience.
- `mutate-verify.sh` — needs a Docker host. Breaks each property in turn and asserts `verify.sh`
  fails naming it. A guard nobody has watched fail is not a guard.
- `mutate-verify.sh --control` — **the one way to ask "is this container healthy" from outside**.
  One healthy run, printed verbatim, using the same flags and the same preamble every mutation
  runs against. Do not hand-roll the `docker run`: it needs the seccomp profile, `NET_ADMIN`, both
  binds, and a preamble that raises the firewall, links the state and the memory store, and
  installs the posture — and a command missing any of those prints a dozen FAILs that read as a
  broken container rather than as a wrong invocation. (`verify.sh` itself is for use *inside* the
  container, where the lifecycle has already done all of that; it refuses to run anywhere else.)
- `check-config.sh` — host-side, no Docker, part of `./scripts/check.sh`. Its real job is the
  seccomp profile: it is **generated**, and a generator whose patch no-ops against a changed
  upstream yields a profile that parses, applies, and leaves the nested sandbox unable to start.

## What is still not established

That the **nested** sandbox engages for a tool call *in here*. `bwrap` working is the mechanism,
not the product, and the obvious credential-free probe does not discriminate: with
`failIfUnavailable: true` in a stock container — where `bwrap` provably cannot run — Claude Code
still reached the auth check rather than erroring at startup. So the sandbox is checked lazily, or
auth precedes it.

Settling it needs a live, authenticated session **inside** the container, then
`../scripts/auto-mode.sh sandboxed`. Running that from a plain `docker run` shell answers a
different question and will say NOT CONFINED, correctly: the sandbox wraps commands *Claude Code*
runs, and there is no Claude Code in that shell.

**On the host this is now established**, which is the useful precedent: with the posture installed,
a `$HOME` write was refused with `EPERM` (not `EACCES`, and `$HOME` is `drwxr-x---` owned by the
user, so ordinary permissions allowed it), while a control write inside `~/repos` succeeded — and
`~/.zsh_history` was unreadable while the allowlisted `~/.gitconfig` and `~/.zshrc` were fine, three
plain dotfiles with identical TCC status differing only in the posture.

Use `auto-mode.sh sandboxed` for this, **never** `printenv CLAUDE_CODE_SANDBOXED`: that variable was
**unset** throughout the measurement above. It had been this repo's recommended test.

## On a Linux host

Better, mostly: no VM, so bind mounts are native and the IO penalty above disappears. Two things
to know. **UID mapping** — a bind mount carries the host's uids, and Dev Containers' default
`updateRemoteUserUID` remaps the container user to yours, which is what makes the workspace
writable when your host uid is not 1000; the cargo `target/` volume sidesteps the question
entirely. **Rootless Docker is untested here**: it already runs the container inside a user
namespace, so nesting bubblewrap within it may behave differently from the rootful case measured
above. Run `verify.sh` and believe it over this paragraph.

## Give it enough memory

Building this workspace needs a few GB — `headless_chrome`, the image/AV1 crates and the ONNX
graph are each large — and **an out-of-memory build does not say so**: `rustc` is SIGKILLed and
cargo reports a bare `(signal: 9, SIGKILL: kill)` with no mention of memory. If you see that, it
is the VM's memory limit, not a broken toolchain. Raise the runtime's memory (Docker Desktop's
Resources pane, `colima start --memory`), or cap parallelism with `CARGO_BUILD_JOBS=2`, which
lowers peak usage far more than it costs in wall-clock.

## `~/.jkb` is shared with the host, deliberately

The knowledge base is bind-mounted, so the container and the host see the same database — which is
the point, and every write goes through the audited writer-actor, so damage is undoable. The
consequence to know about is the usual one: the DB migrates in place, so a container `jkb` built
from a branch with a newer migration will lock the host binary out until it is rebuilt. Point
`JKB_DB` at a container-local path if you would rather they were independent.

## Open the repo root, not a session worktree

`jkb task work` puts worktrees at `<repo>/.jkb/work/<session>`, and a linked worktree's `.git` is a
*file* pointing into `<repo>/.git/worktrees/…`. Mount the repo root and both ends are inside the
container, so sessions work normally. Mount only the worktree and git breaks, because the gitdir
it points at is not there.

Costs, stated: on macOS this is a Linux VM, so bind-mount IO is slower and the toolchain is the
container's, not your host's. `~/repos` mounted is still writable and push-able — the container's
win is bounded to what you did **not** mount.

## A session worktree is archived, not deleted

`jkb task land` used to finish with `git worktree remove`, which unlinks the tree recursively
and stops at the first refusal. Run from inside a sandboxed agent session that refusal comes at
`<worktree>/.claude/settings.json` — Claude Code protects a project's policy files from the agent
whose policy they are — by which point 152 files were gone. The verb reported an error about the
*directory* and said nothing about the 62,421 lines it had already removed.

Disposal is a **rename** now: the whole tree moves to `<repo>/.jkb/archive/<session>-<stamp>` in
one atomic operation, so there is no partial state for a failure to leave behind, and a worktree
disposed of by mistake is still there to move back. Deleting it is a separate, later decision —
`jkb task reap` removes archives older than 30 days, and probes each with `remove_dir` first so it
never begins a walk it cannot finish.

The refusal is scoped to the session's **own** working directories: measured across five live
worktrees, only the session's own tree answers `EPERM`, every other one answers `ENOTEMPTY`. So
`land` never blocks on it — it grafts, applies its plan, records what it could not move, and any
other process finishes the job. `jkb service install` installs that reaper beside the sync
watcher (`com.jkb.reap`), as a second unit rather than another job for the watcher: a wedged file
watcher must not also stop every deferred landing on the machine from completing. `jkb doctor`
reports what is outstanding and `jkb doctor --fix` sweeps it.

Three rules keep the sweep from being the destructive thing it replaced. It **holds** rather than
acts whenever it cannot establish something: a repo root it cannot reach settles nothing (the
ordinary case once host and container share `~/.jkb` at different paths), and a tree that is not
still a registered worktree sitting on the commit the landing recorded is a different session
reusing the name, not this record's business. One sweep runs at a time, because two both reading a
pending record both act on it — the second finding the worktree gone and deleting the record the
first had just written. And the **cost is reported**: a landed session's checkout carries the
repo's build output, so `jkb doctor` and `jkb task reap` print what the archives occupy. It is
deliberately not pruned — `git clean -X` deletes exactly the regenerable files and also deletes a
gitignored `.env`, and unrequested deletion is what this whole mechanism exists to avoid. Shorten
`--retain-days` if size matters more than the safety net.
