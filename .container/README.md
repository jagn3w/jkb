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
- **`verify.sh` measures the wrong PID 1 inside a nested namespace, and now refuses.** Measured in
  the container, both topologies, one command apart. Under
  `bwrap --bind / / --dev /dev --unshare-pid --unshare-user --cap-drop ALL --proc /proc`, PID 1 is
  **bwrap itself**. The false pass was then **reproduced against the pre-refusal commit**, by
  running that version of the script in the same namespace — it printed

      ok  PID 1 reaps the orphans it adopts (PID 1 is: bwrap --bind / / ... --proc /proc -- ...)

  which asserts the container reaps in a sentence whose own parenthesis names bwrap. bwrap's init
  reaps unconditionally, so that `ok` was available for any container, including one that does
  not. A false pass in the single assertion the reaping work exists to add. The current script
  exits 2 there, while a plain attached terminal (PID 1 = `/usr/bin/tini -- sleep infinity`) runs
  every assertion and passes.
  - The same nesting makes `/proc/self/mountinfo` bwrap's too, which is why the refusal sits above
    every assertion rather than inside the reaping one.

### The discriminator is namespace identity, asked of the kernel — after two inferences were wrong

`entrypoint.sh` records the container's own `/proc/self/ns/pid` and `/proc/self/ns/mnt` — nsfs
inodes, which **are** those namespaces' identities — immediately before it hands over, and
`verify.sh` compares its own against them. Equality is identity by definition, so there is no
polarity to get backwards and no premise about who started whom. `bwrap --bind / /` is recursive,
so the marker stays readable from inside the sandbox while its `--proc /proc` means the ids do not
match: that asymmetry is exactly what discriminates.

Both earlier versions **inferred**, and each was wrong in a way its own author could not see:

- **A ppid walk** ("PID 1 is an ancestor ⇒ nested"). True for `docker exec`; **false for the
  container's own main command**, which is a direct child of PID 1. `mutate-verify.sh` runs
  `verify.sh` exactly that way, so the walk refused its control, all sixteen mutations and the CI
  Docker job — and `verify.sh`'s own self-test *pinned that shape as correct*. It also refuses on
  any ordinary Linux host, where systemd is an ancestor of every shell.
- **PID 1's starttime**, compared against a recorded one. Sound on the pid axis — starttime
  survives `exec`, so `entrypoint.sh`'s and tini's are the same — and **blind on the mount axis**:
  a sandbox mounting a fresh `/proc` *without* unsharing pid leaves `/proc/1` as the real tini, so
  the gate opens while the mount table is still the sandbox's. A pid-only test guarding a
  mount-table assertion is the same mistake one axis over. Recording **both** ids is the fix, and
  it is why the marker carries two.

**Delete-on-entry, write-before-handover**, which is the D51 shape rather than a nicety: `/run` is
the writable layer, so a marker outlives `docker stop`, and namespace ids are reused once a
namespace dies — a stale marker could be matched by an unrelated later namespace. Deleting first
and writing last makes that unrepresentable: a marker exists only for a boot that reached its
handover, and anything else leaves **absence**, which is refused. Pinned by a self-test that arms a
stale marker and boots into a refusing state.

The refusal prints a `FAIL` line rather than only writing to stderr, because `mutate-verify.sh`'s
`judge` needs one — a gate the harness cannot watch firing is the defect this directory keeps
meeting. Two mutations exist for it: the marker never written, and the marker naming somebody
else's namespaces.

**Measured, on a real daemon, after two discriminators that were not** (2026-09-11):

- `mutate-verify.sh --control` passes, **18/18** — and its `PID 1 is: /usr/bin/tini -- bash -c …`
  line is the topology that matters: the harness runs `verify.sh` as the container's OWN main
  command, a direct child of PID 1, which is exactly what the ppid walk refused. That refusal took
  the control, all sixteen mutations and four CI steps with it.
- The full harness reports **20/20 CAUGHT**, with the matcher shown to stay quiet about a healthy
  container. Three of those rows are new and none had ever been observed firing before: `PID 1
  never wait()s` — the assertion this whole branch exists to add — plus the marker never written
  and the marker naming somebody else's namespaces.
- Inside `bwrap $(bwrap-probe.sh --print-invocation proc)` it **refuses, naming both pairs**:

      recorded by the container's entrypoint:  pid=pid:[4026532556]  mnt=mnt:[4026532553]
      observed by this process:                pid=pid:[4026532691]  mnt=mnt:[4026532690]

  Both axes differ, which is the evidence that recording *two* ids was not belt-and-braces: a
  starttime comparison would have seen only the first, and a fresh mount namespace alone is a
  state it could not have refused.
- A plain attached terminal runs **19/19**, PID 1 `/usr/bin/tini -- sleep infinity`.

A refusal that does not print what it compared is the previous discriminator again — it refused
without ever showing its evidence, which is why nobody noticed it was answering a different
question. That is why the pairs are in the message and not only in the reasoning.

**Residual, stated:** the marker is `vscode`-owned, so a session inside can forge it. Forging can
only produce a *refusal* for anyone genuinely in the container's namespaces; the only false pass it
buys is for a run inside the same sandbox as the forger — an agent lying to its own verifier, which
no file ownership prevents.

- **PID 1 must reap, and `sleep` cannot.** `run.sh` keeps the container alive with `sleep
  infinity`, and `entrypoint.sh` used to end in a bare `exec "$@"` — which made that `sleep` the
  PID 1 of the namespace. PID 1 must `wait()` on the orphans it adopts, and `sleep` never does, so
  every orphan stayed a zombie for ever. Measured on a container 28 hours old: **3941 zombies of
  3968 tasks, and 4083 of `container.json`'s 4096 PIDs spent** — thirteen from a container that
  could not fork at all. It is one zombie per sandboxed Bash call, so it tracks agent activity and
  an unattended session reaches the limit unaided. `entrypoint.sh` execs tini now.
## Why `--init` is not how PID 1 is made to reap

Kept out of the measurements above on purpose: this one is **reasoned, not measured**, and a
section called "the measurements this is built on" must not carry an argument nobody ran.

Docker's `--init` wraps the entrypoint rather than being exec'd by it, so PID 1's argv would name
`entrypoint.sh` for the container's whole life. `run.sh`'s `settle()` reads a match of
`entrypoint.sh` in `ps -o args= -p 1` as "the entrypoint has not finished yet", so under `--init`
it would never settle, and every create and start would fail on its 120s budget — blaming a
black-holed resolver, since that is what usually holds a raise there. Exec'ing tini *from* the
entrypoint keeps PID 1's argv meaningful to that probe: `entrypoint.sh sleep infinity` while the
script runs, `tini -- sleep infinity` the moment it hands over.

What *is* pinned is the consequence rather than the premise: `run.sh --self-test` carries a
`settle_step` row for docker-init's argv asserting it reads as `waiting`. Whether `--init` really
produces that argv has not been run here. Nothing refuses `--init` in `runArgs` either, so if you
are reaching for it, this section is the whole of what stops you.

`run.sh`'s 120s-timeout message now names this as a second cause beside the black-holed resolver,
with `docker inspect -f '{{.HostConfig.Init}}'` to tell them apart: an entrypoint that `docker
logs` shows *completing* and a `settle()` that never returns is the wrapped case, not the stuck
one.

## The reaper path is single-sourced, and a guard over the duplication was the wrong answer

History, recorded here rather than in the files it is about, because the guard described never
existed in a shipped state — it was added and deleted inside one branch, and a static-check file
narrating its own branch's history reads as an inventory of checks the repository has.

The path was briefly written twice: the Dockerfile's `test -x` and `entrypoint.sh`'s `exec`
default each named `/usr/bin/tini`. `check-config.sh` grew 33 lines asserting the two agreed and
`mutate-config.sh` grew three mutations watching that fail. The guard worked and it was still the
wrong answer, for the reason this directory keeps arriving at: **delete the duplication rather
than police it** (D52.5, which removed the same shape for `/run/jkb-egress-verdict`). `ENV
JKB_REAPER` in the Dockerfile reaches the build-time `test -x` *and* the entrypoint process and
every `docker exec`, because ENV persists into the image config — one spelling, nothing to keep in
step, and 40 lines of guard and mutation deleted with it.

The clinching detail is what the guard did *not* cover: a **third** site, `mutate-verify.sh`'s
`gcc -o` target, which had already gone stale while the two-site guard reported agreement. That is
what a guard over duplication buys — agreement between the sites somebody remembered.

## Using it

Needs a container runtime on the host (Docker Desktop, OrbStack, colima, or Apple's `container`),
which macOS does not ship.

```sh
./.container/run.sh             # build if needed, start, firewall, setup, verify
```

### On an AppArmor host, the profile must be loaded — and a reboot unloads it

Docker's `docker-default` denies `mount`, so bubblewrap — and therefore Claude Code's nested
sandbox — cannot start under it. `.container/apparmor-jkb-dev` is `docker-default` with that one
rule relaxed and every other restriction kept, and it has to be in the kernel before the container
can use it:

```sh
sudo apparmor_parser -r -W .container/apparmor-jkb-dev
```

**Nothing installs it under `/etc/apparmor.d`, so this does not survive a reboot.** Run it again
after restarting the machine. `run.sh` asks whether Docker can apply the profile *before* it
creates or starts anything and prints this command with docker's own error beside it, so the
failure names itself — but it is worth knowing that a container which worked yesterday needs one
command today. It is deliberately not installed system-wide: jkb runs inside other people's
repositories and does not add root-owned policy to a machine the user did not ask it to change.

Then attach VS Code: **Command Palette → "Dev Containers: Attach to Running Container" → `jkb-dev`**,
and File → Open Folder to any path inside. From a terminal in an attached window, `code <path>`
opens another window on the same container. `run.sh --open [path]` does the attach for you, but the
Command Palette route is the documented one — the attached-container URI it builds is VS Code's
spelling and nothing here can test it.

On a **fresh** container, one more step: extensions are not installed yet. VS Code puts its server
into the container when you *attach*, which is after `run.sh` has finished — so setup finds nothing
to install into and says so. From a terminal in the attached window:

```sh
./.container/install-extensions.sh     # marketplace extensions from disk, then the jkb explorer
```

then *Developer: Reload Window*. `run.sh` cannot do it for you: it drives Docker from the host, and
the container deliberately has none. Automating it means a `postAttachCommand` in VS Code's
attached-container configuration (`imageConfigs/<image>.json` in its globalStorage), which is not
wired up yet.

The firewall is raised by the image's **entrypoint**, so it comes up on `docker run` *and* on
`docker start` — including Docker Desktop's start button and a daemon restart — rather than only
when `run.sh` is the one starting it. That matters because iptables rules live in the container's
network namespace and do not survive a stop: when the raise belonged to `run.sh` alone, every
other way of starting the container gave an unattended agent unrestricted egress, and nothing
checked. A boundary that depends on which caller you used is not one. `run.sh` re-raises it
synchronously as well, which is not a second rule — the raise is idempotent — but a way of knowing
it has finished before the next `docker exec` lands.

**The container boots on what the KERNEL holds, not on a record of what some raise established**
(D51). `sudo egress-status.sh` reads the live filter chains and prints one word; the entrypoint
decides on that, and `verify.sh` reports from it:

| state | means | the container |
|---|---|---|
| `allowlisted` | the allowlist rule is in the live chain and both IP families are bounded | starts |
| `denied` | no allowlist, but egress is provably denied — DNS and loopback only | starts, loudly: this is the state you attach to in order to repair it |
| `unfiltered` | denial could **not** be established on one or both families | **refuses to start** |
| *(no answer)* | the probe could not run, so nothing was established | **refuses to start** |

An unproven family counts as open. A container with **no way out over IPv6** is a different thing
and is fine: no off-link address and no default route means there is no path to deny, and that is
measured rather than assumed — a link-local `fe80::`, which every container on a default Docker
bridge has, is not a way out.

It used to read a verdict file, and that is the bug D51 fixed: a record is an *event* ("at some
moment a raise established X") while every reader is asking a *present-tense* question. `docker
stop` destroys every iptables rule and the file survives in the writable layer, so a restart whose
raise died before recording read the previous start's `allowlisted`, printed nothing, and started
an agent on an unrestricted network. The file is still written, and still carries the **reason** —
the kernel can say egress is unfiltered, it cannot say DNS failed — but nothing decides on it, and
`verify.sh` reports a record that disagrees with the live chain as drift.

`JKB_EGRESS_ACCEPT_UNFILTERED=1` in `containerEnv` is the one escape: it lets an unfiltered
container **boot**, so you can attach by hand and diagnose it. It does not make that container a
place to run an agent — `verify.sh` reports it as a failure on every run for as long as it is set,
and `run.sh --open` refuses to launch a window, because opening one *is* starting a session.

If your host genuinely cannot be given the guarantee, `JKB_EGRESS_ACCEPT_UNFILTERED` in
`container.json` boots it anyway — and `verify.sh` then reports a failure on every single run for
as long as it is set. It lives in `containerEnv` rather than as a `run.sh` flag so that
`docker start` honours the same decision and a session inside the container cannot grant it to
itself. Turning it on is a reviewable edit plus a recreate.

`run.sh` then runs `setup.sh` (posture, toolchain, `jkb`, extensions, `verify.sh`) if setup has
not completed in this container, and `verify.sh` alone if it has. That is decided by a marker
`setup.sh` writes as its last act, **not** by whether this invocation created the container: an
interrupted first run used to leave setup unreachable for the container's whole life. It also
sweeps deferred worktree archives — the container's job, because a session cannot archive its own
checkout and the host's reaper cannot see `/home/vscode/...` paths — and it does that *before*
verifying, so a failing assertion about something else cannot disable it.

```sh
./.container/run.sh --build     # rebuild the image (needed after a Dockerfile or extension change)
./.container/run.sh --stop      # stop it; volumes and image survive
./.container/run.sh --rm        # remove it, so the next run redoes first-run setup
./.container/run.sh --dry-run   # print the docker command instead of running it
```

### It is not a Dev Containers config, and the file is not called `devcontainer.json`

The declaration is `.container/container.json`. The name matters: VS Code detects
`devcontainer.json` and offers *Reopen in Container*, which would be a **second** way to get a
container — one built by Dev Containers, one by `run.sh` — and two launch paths that start
identical drift, in the mount list, which is the security boundary here.

Dev Containers was dropped because of one limitation with no workaround. Its `workspaceFolder` can
only be built from `${localWorkspaceFolderBasename}`; there is no variable for a folder's path
*relative* to the mount, and `initializeCommand` cannot supply one (it is a subprocess, and
substitution has already happened). So a folder nested inside the mount could not be opened —
`~/repos/jkb/.jkb/work/sess` resolved to `/home/vscode/repos/sess`, which does not exist — and the
near-miss was worse than the miss: a literal fallback silently started the agent in a **different
checkout**, with every guard passing, because the wrong repo is a perfectly good repo. A whole
host-side preflight (`check-workspace.sh`, now deleted) existed to refuse that case.

Attaching has no `workspaceFolder` at all. You open any path inside the container, at any depth, so
the limitation and the guard that policed it both stop existing — and **one** container serves every
repo under `~/repos` instead of one per opened folder.

What replaces the guard is a smaller, checkable claim. `run.sh` is now the only thing that applies
`container.json`, so a key nobody reads is possible and looks exactly like configuration — and the
key most likely to be added is another `mounts`-shaped one. `run.sh --consumed-keys` names what it
reads and `check-config.sh` fails on any declared key that is not in that list, so adding one forces
the decision at the moment it is added. `run.sh --self-test` (in `./scripts/check.sh`) exercises the
derivation itself, including that an unset `${localEnv:VAR}` is **refused** rather than substituted
empty — Dev Containers' own default, and the way `source=${localEnv:HOME}/repos` quietly becomes
`source=/repos`.

## Extensions are fetched at build time, and pinned

VS Code installs extensions when you **connect**, which is after `setup.sh` has raised the egress
firewall. Measured: both declared extensions failed with `ECONNREFUSED` to `*.gallery.vsassets.io`
and the container came up with neither installed — including the Claude Code extension, which is
most of what this container is for — as a non-fatal log line nothing gated on. Reordering cannot
fix it, because the firewall is re-raised on every start.

So `fetch-extensions.sh` downloads the `.vsix` files during `docker build`, where egress is
ordinary because the firewall only exists inside the running container, and `setup.sh` installs
them from disk. **This needs no widening of `allowedDomains`** — and the alternative is worse than
it sounds: the firewall pins names to IPs at raise time and cannot pin a wildcard, so
`*.vsassets.io` would not help and every extension *publisher* would need its own concrete host,
pinned to CDN addresses that rotate.

**Every entry must be version-pinned** (`publisher.name@version`); `check-config.sh` fails the
gate on one that is not, because unpinned means VS Code resolves "latest" over the network and we
are back to the download that cannot succeed. Adding or upgrading an extension is therefore an
edit plus a **rebuild**, the same ceremony the allowlist already asks for.

The list lives once, in `container.json`, and is read through `lib.sh`'s `dc_extensions` by all
four users of it — the build fetch, the install, the pinning gate, and `verify.sh`'s assertion
that they are actually present. `verify.sh` **skips** that last one where there is no VS Code
server, since `devcontainer up`, a plain `docker run` and `mutate-verify.sh` all build a correct
container with no VS Code in it; the skip is printed, and the judgement itself
(`missing_extensions`) is a pure function exercised by `verify.sh --self-test`, so its failure arm
is watched even though no container harness can reach it.

**The pin binds the download, not the installed version.** Measured on a container *restart*: VS
Code auto-updated `anthropic.claude-code` from the pinned 2.1.250 to 2.1.251 and fetched it
successfully, apparently through `code-server --use-host-proxy`, which tunnels via the host and so
does not meet the container's firewall at all. That path is not reliable — the same flag was
present in the create that failed — so it changes nothing about staging the `.vsix` at build time.
It does mean the assertion compares **ids, not versions**: an auto-update must not read as a
missing extension.

## One extension is built, not downloaded

The jkb explorer (`ui/vscode`) is not on the marketplace, so it is not in `container.json`'s
list and `fetch-extensions.sh` cannot stage it. Nothing installed it, nothing declared it, and
therefore nothing could assert it either — so **every container ever built came up without the
side panel**, silently. Two of this repo's recurring shapes at once: an absence nothing was
checking, and a rule (`code` vs `code-server`) that the host installer knew and the container did
not.

`install-extensions.sh` builds and installs it from the workspace, by calling
`scripts/install-extension.sh` — the **host's** installer, reused unchanged, so the container cannot
ship a different build of the extension from the one you install on the host for reasons nobody
decided. `setup.sh` calls that script too, but on a fresh container it finds no VS Code server and
correctly does nothing: the server arrives when you **attach**, which is after setup has run. So on
a new container this is the one step you run by hand, from a terminal in the attached window — see
*Using it* above. (Under Dev Containers the order was the reverse, which is why it was never a
separate step.) That script resolves
`code-server` and its `--server-data-dir` itself when there is no `code` CLI, which is the dev
container case. It builds from the checkout rather than from a snapshot baked into the image, so
the panel matches the code you are working in, and it needs only `registry.npmjs.org`, which the
posture already allowlists.

It builds into `~/.jkb-ui-build`, **not** into the workspace, and that is not tidiness.
`ui/node_modules` is inside the bind mount, so the container and the host share one copy — and it
is not portable: esbuild ships a native binary per platform and pnpm links only the current one.
A build in here would leave linux links there, and the host's `./scripts/check.sh` runs
`pnpm run build` with **no `pnpm install`** in front of it, so the next host gate would fail with
an esbuild platform error and nothing pointing at the container. One shared mutable directory,
two writers with incompatible requirements; the copy removes the sharing rather than adding a rule
both sides have to remember.

It is **fatal** on failure, like the `jkb` install, and for the same reason: the binary and the
panel are the two things that make this a jkb container rather than a generic one. `verify.sh`
appends its id to the declared list and asserts it like any other; `check-config.sh` pins the
derivation, because a rename dropping `publisher` from `ui/vscode/package.json` would otherwise
make that assertion silently check one fewer extension — the invisible-again failure. Both steps
are skipped where the repo builds no extension of its own, since this container is meant to serve
any repo under `~/repos`.

## The mount list is the security boundary

Everything absent from `container.json`'s `mounts` does not exist inside the container. Add to
it one path at a time and never mount all of `$HOME`. `verify.sh` asserts the mounted set is
**exactly** what is declared — exhaustively, from `/proc/self/mountinfo`, rather than by listing
paths that ought to be absent, because a list of absences can never be complete.

**Nothing** under `~/.claude` is mounted from the host — not `settings.json`, which **is** the
posture and which a process the posture bounds must not be able to read or write, and not the
credential file either. Authenticate inside the container (`claude auth login`). The credential
and account-state files are kept in the `.claude-state` volume, so a login survives a rebuild
without anything of the host's being visible.

**The link does not survive a login, so the login is moved, not only linked.** `setup.sh` links
both files into the volume while they still dangle, on the theory that Claude Code writes through
the link. It does for `~/.claude.json`, but not for the credential file. After an in-container
login, `verify.sh` found a regular file at `~/.claude/.credentials.json` (observed 2026-09-18, Claude
Code 2.1.276), so the login sat in the writable layer and a rebuild would lose it. The saver writes
a temporary file first, which makes a rename over the link the likely mechanism. That part is
inferred: triggering a real save needs a real OAuth exchange. Reading does follow the link, which
was measured: `claude auth status` reported `loggedIn: true` through a symlinked credential file
(dummy credentials, scratch `CLAUDE_CONFIG_DIR`, same day).

So `lib.sh`'s `dc_persist_login` moves a regular file found at either link site back into the
volume and links it again. **Which copy wins depends on whether this container has linked the file
before**, and a marker in the writable layer (`~/.claude/.jkb-login-linked`) records that:

- Once linked, a regular file can only be a Claude Code write that replaced the link, so the home
  copy is newer and moves into the volume.
- Before the first link (a fresh container at setup), a regular file came with the image or
  predates setup. The volume's copy is the login carried from the last container, so it wins, and
  the home file is set aside as `<file>.pre-link`, not deleted.

The first version of this rule said the home copy is always newer. A review caught that at setup
this would move an image-shipped `~/.claude.json` over the carried one on every rebuild. The plain
`ln -sfn` it replaced had kept the carried copy in that case.

It runs in `setup.sh`, on every `run.sh` start, and before `run.sh --stop` and `--rm`. For those two,
a **stopped container is started** first, because a container stopped by a reboot or by Docker
Desktop is exactly the one holding a refreshed token, and `--rm && run.sh` is the recreate this
script tells you to run. If the start is refused, it warns and carries on.

`verify.sh` reports a regular file there as a `note`, not a failure, because after any login or
token refresh it is the normal state. It fails when the last move failed
(`~/.claude/.jkb-login-carry-failed`, written by the mover and cleared by its next clean run), and
it fails a link that is missing, points elsewhere, or is a directory.
`scripts/tests/container-login.test.sh` covers the mover and `run.sh`'s `--stop`/`--rm` calls
against a stub `docker`. `verify.sh --self-test` covers the classification. The call on the
start path is not tested, because reaching it needs a whole container.

**Residual:** if the container is removed some other way (plain `docker rm`, Docker Desktop) after a
token refresh, the volume keeps the previous token. Refresh tokens rotate, so that can mean one
more login after the rebuild. It never exposes anything. A volume mounted at `~/.claude` would make
the file a plain file inside the volume and close this entirely, but `verify.sh` refuses any mount
at or under `~/.claude`, which is what keeps the host's `settings.json` away from the agent. That
guard is worth more than one login.

The expected set is **derived** from `container.json`, so adding a mount is a one-file change
and cannot drift out of step with the verifier. It used to be transcribed into `verify.sh` as
well, and the first time the mounts changed the copy went stale and a correctly-built container
failed its own verifier.

**`/vscode` is excluded, and it is the one exclusion the container runtime does not own.** The Dev
launcher mounts a named volume there to hold the VS Code server. It comes from the launcher's own
docker flags, so it can be declared neither in `container.json` — which would claim we create it,
when under `run.sh` or a plain `docker run` it is simply absent — nor with `--declare`, which is
refused for anything not nested inside a declared bind. Until it was excluded, opening the
container **failed outright** on `UNDECLARED mounts: /vscode`: the supported path was the one path
no harness drove, because `mutate-verify.sh` spells its own docker flags and never mounts it.
Anchored `^/vscode$`, so a bind at `/vscode/anything` still fails. It is kept now that we attach
rather than let Dev Containers create the container: whether a given VS Code version stages its
server through that volume is the launcher's business, an exclusion for a mount that never appears
costs nothing, and its absence costs a container that cannot start. The cost, stated: from inside
the container a volume and a bind are indistinguishable, so this one path is a spot where a host
bind would pass — which is not the threat this check is for, and a careless line in
`container.json` still is.

`verify.sh --self-test` exercises that exclusion list on a host with no Docker (it is in
`./scripts/check.sh`), because every entry is an *exclusion*: one that matches more than it names
drops a real mount from the set and the assertion still prints `ok`. That is this file's own
history — `^/dev` once matched `/devtools`.

All of **`~/repos`** is mounted, at `/home/vscode/repos`. The argument
for the width is consistency, not convenience: `scripts/auto-mode-posture.json` already grants
`~/repos` in both `allowRead` and `allowWrite`, so a container holding only jkb was *tighter* than
the boundary the same agent runs under on the host. That difference was nothing anyone had
decided, and it made a cross-repo task impossible in here rather than deliberately refused.
Everything the posture does not grant is still absent by the kernel: `~/.ssh`, `~/.aws`,
`~/Documents`, the rest of `$HOME`.

It is also what makes **one** container enough. You attach to it and open any path inside, so a
`jkb task work` session at `<repo>/.jkb/work/<session>` — inside the mount, but not
`~/repos/<name>` — is just another folder. Under Dev Containers it was not: `workspaceFolder` could
only name a basename, so opening a session started the agent in the **main checkout** instead,
silently, with every guard passing, and a host-side preflight had to refuse the case outright.
Keeping those two checkouts apart is the entire point of a session (D36), and it is now the
container's ordinary behaviour rather than something a guard protects.

The one path constraint left is about the checkout that *provides* the container, not about what
you may open: `run.sh` has to hand the container a path to its own `setup.sh`, so that checkout has
to be under `~/repos`. It says so and stops.

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
target `container.json` declares **as a bind**. Not a volume: a named volume reaches no host
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

`verify.sh` **asks the linker live** (`--status`) rather than inferring breakage from a missing
link — and reads setup.sh's create-time record as an *additional* alarm, never a substitute. Both
matter: the linker repairs (it removes a live link into a poisoned store), so a live question
asked afterwards sees the harmless `unsafe` rather than the `exposed` that was true at create;
but a record consulted *instead* of asking made the store guard unfirable after create, so a
redirect planted the next day reported `ok`.
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
repo's store. The posture has no write-deny to carve `claude-memory` back out with, so this was
**weighed and chosen**, not overlooked: both ends are the same person's agents, it is prose rather
than code, and the host side is opt-in (`setup.sh --link-memory`), never created by a `git pull`.

If you revisit it, start from the fact that decides what the alternatives buy — **file tools and
Bash are bounded by different mechanisms.** The sandbox's `filesystem` block governs Bash; the
`permissions` rules govern `Read`/`Edit`/`Write`. Measured: `Read` opens a memory file that Bash
cannot even `ls`. So an agent writes memory through the Write tool wherever the store lives, and
moving it to a path the posture does not grant would close the Bash channel **without** stopping
agents writing memory.

## Root is not reachable from inside

The mount boundary, the root-owned firewall, its allowlist snapshot and the pinned sudoers
argument are all protections against a process that cannot become root — so `vscode` may run
exactly one command as root, `init-firewall.sh`, with no arguments. The base image ships
`/etc/sudoers.d/vscode` granting `NOPASSWD:ALL`, which would make every one of those bypassable
with a single `sudo`; the Dockerfile removes it and `verify.sh` asks *sudo itself* what is
permitted, so a blanket grant re-added by any route fails.

The cost is real and intended: you cannot `sudo apt install` inside the container. Add packages to
the Dockerfile and rebuild, or `docker exec -u root` from the host.

## Git runs the host's hooks, from a read-only copy

**The defect.** VS Code copies the host's `~/.gitconfig` into the container every time you attach.
The copy includes its `core.hooksPath`, which on the Mac is `/Users/<you>/.config/git/hooks`, a path
that does not exist in here. Git treats a missing hooks directory as an empty one: no error, and no
hook runs. Observed 2026-09-18. Commits made in the container carried a `Co-Authored-By` trailer
that the host's hooks stop, and `post-merge` never ran on a pull. Nothing reported either, because
nothing looked.

**The fix is a copy, made on every start, and a config that points git at it.** `run.sh` reads
the hooks path git on the host uses, and copies that directory to the path the same raw value
means in here. An absolute path stays the same path, so `/Users/<you>/...` really exists in here.
`~/…` maps to `/home/vscode/…`, and trailing slashes are dropped. A relative value resolves inside
each repository, which is already mounted, so there is nothing to copy. Every start then sets
`core.hooksPath` in the container's `~/.config/git/config` to the copy's path. A relative value is
set as it is, since it means the same thing in here. The work is `lib.sh`'s `dc_mirror_host_hooks`,
and `run.sh`'s "git hooks" step calls it.

- **Why run.sh writes a config, and does not leave it to VS Code's copy.** Four review rounds found
  the same shape. VS Code's copy of `~/.gitconfig` arrives only on **attach**, which is after
  `run.sh` verifies. It never carries an `[include]`d file. And it goes stale when the host
  changes. Each round patched one edge and the next found another. Git reads
  `~/.config/git/config` whenever `~/.gitconfig` does not set the key, and lets `~/.gitconfig` win
  when it does, which with a current copy names the same directory. Measured on git 2.51.1,
  2026-09-18: with only that file setting it, the effective read and
  `rev-parse --git-path hooks` in a repository both answer its value, including when a
  `~/.gitconfig` without the key exists; with both set, `~/.gitconfig` wins. So the host's hooks
  run in here before the first attach, and when the host takes the value from an include.
- **One key, set as the container user, and never the file replaced as root.** The first version
  wrote the whole file root-owned. A review then measured what git does to it: with no
  `~/.gitconfig` (every container before its first attach), `git config --global user.email ...`
  writes *this* file through a lock file and a rename, so it came back user-owned, `run.sh` refused
  it on every later start, and the hooks path froze. Root ownership bought nothing here, because
  the threat is a sandboxed command redirecting the hooks path, and the posture already denies the
  sandbox writes to `~/.config` and `~/.gitconfig`. So `run.sh` sets the one key with
  `git config --file`, which keeps every other line in the file.
- **One reader, of what git actually uses.** `dc_global_hooks_path` serves both the host step and
  `verify.sh`. It is the value git uses outside any repository, includes followed. It is not
  `git config --global`, which, measured on the same git, stops reading `~/.config/git/config` as
  soon as `~/.gitconfig` exists. It tells *unset* apart from *set to the empty string*, which git
  reads as `/` and so runs nothing from.
- **What the host resolved is recorded, root-owned.** The record says the value, the file it came
  from, whether this start's mirror succeeded, and what `run.sh` applied. It lives at `/run/jkb-host/hookspath`, written
  by the root step in a root-owned directory, so nothing running as the container user can forge
  or delete it. Round 4 found the `vscode`-owned first version able to hide a failure. It counts
  only if it was written since this start: the entrypoint rewrites `/run/jkb/ns` on every start,
  and `run.sh` writes the record after that. `verify.sh` compares it with what git in here uses:
  - `not-applied` **fails**: `run.sh` applied a value and git in here uses none. Its remedy is
    re-running `run.sh`, which is always possible. 3e compares against what was *applied*, not
    against a value derived again. Round 5 found a relative host value applied as nothing while
    "nothing expected" read as agreement.
  - `stale` (the host dropped the setting) and `diverged` (the host changed it) are **notes**. Both
    come from a VS Code copy that predates the host's change. Each note gives the fix that does not
    wait for VS Code: `git config --global --unset core.hooksPath` in here, after which the value
    `run.sh` wrote applies. A failure would make `run.sh --open` refuse the window you fix it in,
    which rounds 3 and 4 found twice, with `awaiting` and `not-seen`, the states this replaced.
- **A copy, not a bind mount.** The mount list is the security boundary, and `verify.sh` asserts it
  exactly. A mount whose source is missing stops the container starting, and not every host has a
  global hooks directory. A copy needs neither, and cannot write back to the host. The cost is
  staleness. A hook edited on the host arrives at the next `run.sh` start. A **changed hooks path**
  needs one too: until then the copy of the new directory does not exist in here, and if VS Code
  reattaches first, its fresh `~/.gitconfig` points git at that missing directory, and git runs
  no hooks. `verify.sh` fails that as `missing`, with the remedy of re-running `run.sh`.
- **Not copied when it is already here.** A hooks path inside a bind (under `~/repos` or `~/.jkb`)
  already IS the host's own directory, live. The copy leaves it alone, and `verify.sh` reports it
  as shared, noting that it is as writable in here as that bind is. It never suggests moving it
  aside, since that would move the host's real hooks. A path inside one of the container's volumes
  is not the host's at all, and is reported.
- **Root-owned, and not writable from in here.** A hook runs whenever git does, including from the
  attached terminal, which is not sandboxed. A hooks directory the agent could write would let a
  sandboxed command plant code that runs outside the sandbox on your next commit. **It is only as
  strong as its parent directory**: anything that can write the parent, and it is not sticky, can
  rename the mirror away and put another directory in its place. For a `~/` path that is the
  container user. The sandbox can do it only where its posture grants writes, which `~/.config`
  (the usual place) does not, but `~/.cache` does. 3e notes a writable parent rather than failing
  it, since `run.sh` cannot change who owns your home.
- **Only its own directory is ever replaced**: a real directory, owned by root, carrying the marker
  `.jkb-host-mirror`. The marker alone proves nothing. A review found the forgery: any process that
  can write a directory can put a file of that name in it. Anything else at that path is refused
  and reported, never touched. `verify.sh` applies the same three tests.
- **The root step trusts nothing it can be handed.** The copy is assembled in a fresh root-only
  `mktemp -d`, never beside the target, where a symlink raced into the staging name could steer
  the extraction and the `chmod` elsewhere. A parent reached through a symlink is refused, and
  `mv -T` replaces the target name without following it.
- **Built whole, or not sent.** The archive is made on the host first, and a failed `tar` sends
  nothing, so a partial copy is never installed over a good one. Symlinks are dereferenced,
  because a hook linked to a host path would dangle in here.

`verify.sh` (3e) asks git where it will look. It fails the states in which no hooks, or the wrong
ones, run: a path with nothing there (the silent defect above), an empty value, an unreadable
config, a path git cannot expand, a directory that is not the mirror, a mirror writable from here,
and a host path that was not applied. When this start's mirror failed and an earlier copy is
still in place, it says so. `scripts/tests/container-hooks.test.sh` covers the copy against a stub `docker`, whose root
step emulates root's `chown`, `stat` and `tar`. Each guard was watched failing with its code
removed. The root step is GNU code, because the container is Ubuntu. The stub hands it GNU's tools
(Homebrew's `gmv`/`gstat`/`gtar` on a Mac), and without them those cases skip, naming what to
install, rather than fail on a flag the container never lacks. Linux CI always runs them. `verify.sh --self-test` covers the classification. `mutate-verify.sh` watches the missing
path and the forged mirror fail in a real container. That needs a Docker host, and has not yet
been run.

**What the hooks do in here.** `post-merge` runs `scripts/setup.sh` after a pull that touches
code. With `JKB_REMOTE` set, `setup.sh` rebuilds the `jkb` binary and stops. The scaffold, the
services, the git-hooks install and the notifier belong to the host
(`docs/git-hooks-installer.md` records that profile). `jkb task close-merged` then runs through
the daemon as usual. The host's own hooks, such as `commit-msg`, run unchanged, and they must work
on Linux: a hook that calls a macOS-only tool fails here, and a failing `commit-msg` refuses the
commit. Verified 2026-09-18: the host's `commit-msg` rejects a `Co-authored-by:` trailer in here,
and a plain commit goes through.

**Residual, stated rather than claimed away.** Keeping the hooks directory read-only stops a planted
*hook*. It does not stop sandbox-written code from running unsandboxed, because the hooks run
repository code. `post-merge` runs `scripts/setup.sh`, and `setup.sh` runs `cargo install`, which
executes build scripts and proc-macros. Both `scripts/` and `crates/` are writable from the
sandbox (`.git/hooks` and `.git/config` are not; measured by a review with `test -w` under bwrap).
So a sandboxed edit there runs unsandboxed the next time you pull, in an attached terminal, code
that touches those paths. The host has always had the same exposure: it runs the same hook over
the same checkout. What this change adds is that the path is now reachable from inside the
container too. The mitigation is the one that already applies on the host: review what an agent
changed before you pull over it.

## Sibling-repo toolchains: Ruby and PostgreSQL

One container serves every repo under `~/repos`, so when a sibling needs a toolchain, it goes in
the image. The first is `contextual-translate` (Rails 8 + Postgres). The Dockerfile builds a
**pinned** Ruby (`JKB_RUBY_VERSION`, with `ruby-build` at `JKB_RUBY_BUILD_TAG`) into a root-owned
`/opt/ruby`, and installs the distro's PostgreSQL with no `main` cluster. It links exactly
`ruby`, `gem`, `bundle`, `bundler`, `irb`, `pg_ctl`, `initdb` and `postgres` into `/usr/local/bin`.
The Dockerfile comment says why each is placed where it is. The layer is **on by default and off
in CI** (`JKB_WITH_RUBY=0`), because jkb's container job verifies nothing about it.

**Rails is not installed globally.** A project's `Gemfile.lock` owns its gem versions, including
the one that generates the app. To bootstrap a new app:

    bundle init && bundle add rails --version '~> 8.0' && bundle exec rails new . --force --database=postgresql

**Gems go in `~/.cache/bundle`, never the project.** The image sets `BUNDLE_PATH` there, and
`BUNDLE_USER_HOME` beside it. Do not `bundle config set --local path vendor/bundle`: `~/repos` is
the Mac's directory, and Linux-built native gems plus a `.bundle/config` there would be read by the
host's own `bundle`. That is the same reason `target/` is kept off the bind.

**The allowlist change reaches the Mac too.** `rubygems.org` and `index.rubygems.org` are in
`scripts/auto-mode-posture.json`, which is the host posture as well as the container's. They are
concrete names so that the firewall, which skips wildcards, allows them too. On the host, re-run
`./scripts/auto-mode.sh install` after pulling this, or `auto-mode.sh run` refuses on posture drift.
Once installed, host sessions can reach RubyGems as well. That is intended: it is a package
registry, like `registry.npmjs.org` and `crates.io` beside it.

**Postgres is per user, and the agent cannot reach yours.** Nothing here can start a system
service, so you run a cluster yourself with `initdb` + `pg_ctl`. The agent's Bash runs each command
in its **own network namespace**, whose only live interface is its own `lo`. Its seccomp filter
also refuses `socket(AF_UNIX)`. A server you start on the container's `127.0.0.1:5432` is therefore
invisible to it. Measured 2026-09-18 from a sandboxed Bash call:

- `/proc/net/dev` listed `lo` and the kernel's inert tunnel stubs (`tunl0`, `gre0`, …), and no `eth0`;
- `socket(AF_UNIX)` raised `PermissionError: [Errno 1] Operation not permitted`;
- a listener on `127.0.0.1:0` accepted a connection from the same Python process.

The `127.0.0.1` entry in `allowedDomains` does not help, because it governs only what the sandbox's
HTTP proxy will tunnel to. The sandbox exports the proxy as `HTTP(S)_PROXY`/`ALL_PROXY` and lists
`127.0.0.1` in `NO_PROXY`, and libpq reads none of them. `excludedCommands` is the usual way out,
and the posture requires it to be empty. So the agent starts a throwaway server **inside the same
command** as the tests, in a fresh data directory, and stops it on the way out:

    pg=$(mktemp -d) && initdb -D "$pg" --auth=trust >/dev/null \
      && pg_ctl -D "$pg" -l "$pg/log" -w \
           -o "-c listen_addresses=127.0.0.1 -c unix_socket_directories=''" start \
      || exit 1
    trap 'pg_ctl -D "$pg" -m immediate stop >/dev/null; rm -rf "$pg"' EXIT
    export PGHOST=127.0.0.1
    RAILS_ENV=test bin/rails db:prepare && bin/rails test

Every piece of it is there for a reason:

- **The fresh directory** means a run killed before its `trap` leaves nothing behind for the next
  run to trip on: no half-finished `initdb` and no stale `postmaster.pid`. Two agents at once also
  never share a cluster. Each command has its own network namespace, so two servers both on
  port 5432 do not collide either.
- **`PGHOST`** is needed because the server has no socket. A stock `database.yml` names no `host:`,
  so libpq would try `/var/run/postgresql` and fail with a message pointing away from the cause.
- **`|| exit 1`** is needed because tests run against no server fail with a connection error, not
  with the reason.

Verified 2026-09-18 on the rebuilt image (Ruby 3.4.10, PostgreSQL 16.15), all from a sandboxed
Bash call:

- `bundle install` of `pg` resolved through the allowlist and installed into `BUNDLE_PATH`, both as
  the prebuilt `aarch64-linux` gem and, with `BUNDLE_FORCE_RUBY_PLATFORM=true`, compiled against
  `libpq-dev`.
- The recipe above started a server. `PG.connect` created a database and read `select version()`
  back, and the `trap` stopped the server and removed the directory.
- With `PGHOST` unset, the same connect raised `PG::ConnectionBad` with an **empty message**, which
  is the misleading failure the `PGHOST` line exists to prevent.

**The dev server is a different recipe, not the same one with another directory.** The test
recipe always runs `initdb` and deletes its directory on exit, so reusing it with
`~/.cache/pg-dev` would throw the database away when the terminal closes. And leaving out only the
cleanup would fail the next time at `initdb` (the directory is not empty), with `|| exit 1` then
closing your terminal. So, from an attached VS Code terminal, which is not sandboxed:

    pg=~/.cache/pg-dev
    [ -f "$pg/PG_VERSION" ] || initdb -D "$pg" --auth=trust >/dev/null
    pg_ctl -D "$pg" -l "$pg/log" -w \
      -o "-c listen_addresses=127.0.0.1 -c unix_socket_directories=''" start
    export PGHOST=127.0.0.1
    bin/rails s          # and `pnpm dev` in another terminal, with the same PGHOST

Stop it with `pg_ctl -D ~/.cache/pg-dev stop`. The gate is `PG_VERSION`, not the directory. Either
way, a half-finished `initdb` then fails loudly, at `initdb` or at `start`, and never becomes a
database that is silently empty. The socket flag
is needed here too: the distro's socket directory, `/var/run/postgresql`, belongs to `postgres`.
`~/.cache` is in the container's writable layer, so a rebuild drops the dev database. VS Code
forwards the Vite port to the Mac. No port publishing and no permission change is involved.

## Verifying it

- `verify.sh` — inside the container: non-root, **PID 1 reaps what it adopts**, bwrap works, the
  mount set is exactly as declared, `~/.claude` is not a host mount, root is reachable only for the
  firewall, egress is denied *and* the allowlist still works, posture intact.
  - The reaping assertion **fails on every container created before it existed**, which is correct
    and is the point: the fix is an image change, and nothing else observes a running container —
    `run.sh` without `--build` finds the argument hash and the image id both matching and starts
    the old one. Recreate: `./.container/run.sh --rm && ./.container/run.sh --build`.
  - It **refuses to run inside Claude Code's own sandbox**, which wraps a Bash tool call in
    `bwrap --unshare-pid --proc /proc`. In there `/proc/1` and `/proc/self/mountinfo` are bwrap's,
    so both the reaping and mount-boundary assertions would describe the wrong subject — and the
    reaping one would *pass*, because bwrap's init reaps. Run it from a plain terminal in the
    attached container, or let `run.sh` run it for you.
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

One half of it **is** now established, negatively and then positively: the nested sandbox was not
running at all, because `bwrap` could not mount `/proc` — see the section below.

That was invisible for as long as it was because of a **probe that could not fail**, which is this
directory's recurring defect and not, as first written here, an absent one. `verify.sh` did run
`bwrap`, and printed `ok  bubblewrap can create its namespaces (nested sandbox can start)` — but
the invocation omitted `--proc /proc`, so it stopped one step short of the refusal. Namespace
creation and the proc mount are separate kernel checks with separate causes; passing the first
says nothing about the second, and the message claimed the second. The probe now mounts `proc`,
its `ok` line claims only what it establishes, and both flags the mechanism depends on
(`seccomp=…`, `systempaths=unconfined`) are watched failing by `mutate-verify.sh`.

## `/proc` has to be unmasked, and on Linux that is an unfinished trade

Docker masks a dozen `/proc` and `/sys` paths by mounting over them. Those masks are *submounts*,
which makes `/proc` not "fully visible", and the kernel then refuses a fresh `proc` mount inside a
non-initial user namespace (`mount_too_revealing()`). So `bwrap` fails with `Can't mount proc on
/newroot/proc`, and because the posture sets `failIfUnavailable: true`, Bash **errors** rather than
running unconfined. `container.json` therefore passes `--security-opt systempaths=unconfined`.
Measured with a negative control, one flag apart: with it the nested proc mount succeeds, without it
it is denied.

**Necessity was measured on both platforms, and the first measurement here was WRONG.** This
section previously said the flag was inert on macOS — that ten paths were unmasked and nothing was
bought. That came from a probe omitting `--unshare-pid`, which is the one flag that triggers the
refusal. Re-measured with Claude Code's actual invocation, in a container with the masks in place.

**The rows are NOT cumulative, and rows 1-3 share a base that is NOT Claude Code's full shape.**
Reading them as a ladder says the wrong flag is the trigger — row 3 would appear to cancel row 2's
refusal and row 4 to restore it. Rows 1-3 are `--bind / / --proc /proc --unshare-net` plus the flags
each row names; row 4 is the full invocation `bwrap-probe.sh` runs, which additionally carries
`--new-session --die-with-parent --dev /dev`.

| invocation | masks present | unmask applied |
|---|---|---|
| `--bind / / --proc /proc --unshare-net` — this is exactly what the old probe ran | OK | OK |
| that `+ --unshare-pid` | **`Can't mount proc on /newroot/proc`** | OK |
| that `+ --unshare-user --cap-drop ALL` | OK | OK |
| Claude Code's full shape (see `bwrap-probe.sh`) | **`Can't mount proc on /newroot/proc`** | OK |

So the flag is **load-bearing on macOS exactly as on Linux**, and the error in that table is the
one that motivated it. `--unshare-pid` is the trigger: the kernel refuses a fresh procfs for a NEW
pid namespace while the existing `/proc` is not fully visible, and docker's MaskedPaths are
submounts, so it is not. Do **not** make the flag host-conditional; a task proposing that was filed
on the wrong measurement and has been cancelled.

**How the wrong answer survived two review rounds, because the shape repeats.** The harness reported
`MISSED` for the mutation that drops this flag. That was a TRUE report — the guard could not fire —
and it was read as a fact about the host, because the weak probe agreed with the wrong reading. A
mechanism was then added to silence it, and when that was rejected the redesign was justified by the
same false premise. An alarm that is explained away twice is usually correct.

**This table is the only copy.** `.container/bwrap-probe.sh` runs Claude Code's invocation and
points here rather than restating the measurement; `verify.sh` and CI both call that script rather
than each keeping a probe. The retracted claim above survived for a while precisely because it was
written in three places and corrected in one — a measurement in a script comment is a claim nothing
re-establishes, so measurements live here, dated, once (D54.5). What the script's own header keeps
is the one fact that makes a simpler probe wrong: drop `--unshare-pid` and it passes in exactly the
state the flag exists to fix. `bwrap-probe.sh --self-test` asserts that flag is present and that the
two rungs differ by the proc mount alone, and `./scripts/check.sh` runs it.

**Why bubblewrap can or cannot start on a given host** is answered by
`./.container/mutate-verify.sh --ladder`, which re-measures with one security flag removed at a
time, starting from the container that ships. CI runs it as its diagnostic step. It replaced four
hand-written `docker run` arms in `ci.yml` that were a second copy of the shipped configuration and
drifted from it in every review round they survived.

**What the flag costs is unchanged by the correction above, and this section lost it once.** The
commit that fixed the measurement replaced this whole passage and deleted the enumeration, the host
table and the follow-ups with it — 92 lines removed against 26 added, and its message never said so.
Restored here. Correcting a claim is not licence to drop the analysis around it, and a security
document quietly losing its residual is a worse defect than the wrong sentence that prompted the
edit.

Docker has no selective unmask, and the flag is broader than its name: `systempaths=unconfined`
clears **both** `MaskedPaths` and `ReadonlyPaths`. Concretely it re-exposes

- **masked (were mounted over):** `/proc/acpi`, `/proc/asound`, `/proc/interrupts`, `/proc/kcore`,
  `/proc/keys`, `/proc/latency_stats`, `/proc/sched_debug`, `/proc/scsi`, `/proc/timer_list`,
  `/proc/timer_stats`, `/sys/devices/virtual/powercap`, `/sys/firmware`, and one
  `/sys/devices/system/cpu/cpuN/thermal_throttle` per CPU that has one
- **read-only (were mounted `ro`), now writable:** `/proc/bus`, `/proc/fs`, `/proc/irq`,
  `/proc/sys`, `/proc/sysrq-trigger`

**That list is a snapshot of somebody else's moving list, and it is deliberately not counted.** It
is `defaultLinuxMaskedPaths()` in moby's `daemon/pkg/oci/defaults.go`, read at `d5370d34d328`
(2026-04-27). `/proc/interrupts` and the `thermal_throttle` files were added by advisory
[GHSA-6fw5-f8r9-fgfm](https://github.com/moby/moby/security/advisories/GHSA-6fw5-f8r9-fgfm) and
powercap by [GHSA-jq35-85cj-fj4p](https://github.com/moby/moby/security/advisories/GHSA-jq35-85cj-fj4p),
so a daemon older than those masks *fewer* paths and your residual is correspondingly **smaller**
— the direction it is safe to be wrong in. This section used to state the length ("4 of the 11",
"the other 7", "all 11") and each of those was a second copy of the list: when `/proc/interrupts`
was dropped in an edit, three sentences of arithmetic silently agreed with the shorter list. Paths
are named here and never counted. Making it derived instead of copied is follow-up 6 below.

The read-only half is covered by DAC and not by the mask being gone: writing any of it is root-only
and the container is not root. So what is actually traded away is *information*, from the first
list. Every path on it is treated here as exposed: an earlier version subtracted the ones it
asserted were mode `0400` (`kcore`, `keys`, `timer_list`, `sched_debug`), which was a third copy of
a fact — this time a kernel fact, asserted from memory, varying by version, and subtracting in the
**unsafe** direction. Measuring it is follow-up 3. Three host classes:

| host | compensating layer | residual |
|---|---|---|
| macOS / Docker Desktop | none, but the exposed host is the LinuxKit VM, not your machine | small |
| Linux **with** AppArmor | `apparmor-jkb-dev` re-denies *read* on `/proc/kcore`, `/sys/firmware/**` and `/sys/devices/virtual/powercap/**`, and on `/proc/sysrq-trigger` from the read-only list | everything else on the masked list — `/proc/acpi`, `/proc/asound`, `/proc/interrupts`, `/proc/keys`, `/proc/latency_stats`, `/proc/sched_debug`, `/proc/scsi`, `/proc/timer_list`, `/proc/timer_stats`, `thermal_throttle`. The `deny /sys/[^f]*/** wklx` rule is **write-only**, so it does not cover `thermal_throttle` |
| Linux **without** AppArmor (SELinux, or no LSM) | **none** | the whole masked list, against the real host — `powercap` and the `interrupts`/`thermal_throttle` pair each carrying a published side channel (PLATYPUS, and GHSA-6fw5-f8r9-fgfm respectively) |

**The third row is the unfinished part, and it matters more as Linux becomes the main platform.**
How it should be fixed, in order of leverage:

1. **Upstream, which removes the trade entirely.** If `bwrap` bind-mounted `/proc` instead of
   fresh-mounting it, no flag would be needed. That invocation belongs to
   `@anthropic-ai/sandbox-runtime`, not to us, so this is a report to file rather than a patch.
2. **Fail closed on the uncompensated cell.** On Linux, masks off *and* no LSM profile mediating
   should refuse to start, the same shape as the egress boot gate — a container that cannot honour
   the boundary should say so rather than run and look identical to one that can. macOS is accepted
   explicitly, because the host there is the VM.
3. **Report which layer is in force.** When masks are off, `verify.sh` should say whether
   `apparmor-jkb-dev`, some other LSM, or nothing is compensating. An unstated residual is
   indistinguishable from coverage.
4. **Restore the discriminator the flag cost.** The mask is what made the AppArmor profile testable
   — docker-default's denials otherwise overlap what DAC already restricts to root, so a denial
   proves nothing. Half of the replacement has landed: `verify.sh`'s `bwrap` probe now mounts
   `proc`, and `docker-default` denies `mount`, so on an AppArmor host a pass separates this
   profile from the stock one. It does not separate *this* profile from `apparmor=unconfined`,
   which also passes. The other half is the `(enforce)` mode, which `verify.sh` still reads out of
   `/proc/self/attr/apparmor/current` and then discards with a `sed`.
5. **Check whether podman's `--security-opt unmask=` helps.** Reasoned dead — any surviving mask
   should still trip the kernel check — but unverified, and it needs a Linux box with podman. If it
   works it is strictly better than all-or-nothing.
6. **Vendor the masked-path list instead of copying it.** The enumeration above is a hand copy of a
   list upstream changes by security advisory, and it has already drifted once — losing
   `/proc/interrupts` in an edit, with three counted sentences agreeing with the shorter list. The
   structurally right home is the vendored-artifact framework this repo already has
   (`generate-*.sh` + `check-drift.sh`, which is what keeps the seccomp profile honest): a
   `generate-masked-paths.sh` extracting the slice from `defaults.go`, drift-checked in CI, with
   the README pointing at the artifact. Deliberately **not** done in the fix round that found the
   drift: a `sed` over Go source is exactly the extraction that silently truncates — the failure
   `check-config.sh` documents for the seccomp generator — so it needs its own emptiness and
   named-member pins and its own mutations. A must-fix in prose is fixed in prose.

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

## The container never opens the knowledge base — measured, not assumed

`~/.jkb` is still bind-mounted (auto-memory, worktree archives, logs, the daemon's token), but no
process in here opens `jkb.db`. Since the cutover (tasks S6.5) the container is in **remote mode**:
`JKB_REMOTE=host.docker.internal:7117` (`containerEnv`), so every `jkb` command reaches the host's
knowledge base through `jkb serve` (next section) or is refused, and there is no database of the
container's own. Before the cutover there was one — `JKB_DB` on a `jkb-kb-local` volume, empty at
first and never seeing the host's tasks. **Upgrading a container that had it:** there is no
export verb, so before rebuilding, look through it from the old container (`jkb query kind:task`,
`jkb ns ls`) for anything the host's knowledge base lacks — tasks filed in a checkout's `tasks.md`
are already there through the host's own sync — and recreate it on the host. Then remove the volume
with `docker volume rm jkb-kb-local` once the new container is up. The installed `jkb` must be from
the cutover or later (setup.sh reinstalls it on a rebuild): an older one reads the bare
`host:port` as a URL scheme and every command fails — `verify.sh` asks the installed binary through
the daemon for that reason. **Unmutated, stated:** that probe and the `--db` refusal probe beside it
have not been watched failing — `mutate-verify.sh`'s containers have no `jkb` installed and no
daemon, so both are skipped there.

The original decision was to share `~/.jkb/jkb.db` across the bind, and it was reversed because sharing it corrupts it.
SQLite's WAL mode needs every process to share two things: POSIX advisory locks on the database
and its `-shm` file, and a `MAP_SHARED` mapping of `-shm` (the wal-index). Across this bind mount
(virtiofs, Docker Desktop on macOS), `.container/sqlite-share-probe.py` measured both, with one process on
each kernel, on 2026-09-13:

| probe | same kernel | container → macOS |
|---|---|---|
| fcntl write lock held; other side asks `F_GETLK` and tries `F_SETLK` | seen, refused | **unlocked, acquired** |
| counter written 2,514 times through `MAP_SHARED`; other side samples its mapping | all seen | **2 distinct values, ended at 10** |
| 4 writers + a checkpointer per side, jkb's pragmas, 45 s | 100k+ rows, `integrity_check` ok | **malformed after 38 commits** |

A rollback journal does not help — it relies on the same locks. And a container *reader* is not
safe either: `jkb` opens read-write, and closing a WAL connection that believes it is the last one
checkpoints and truncates a WAL the host is still writing.

So **the host owns `jkb.db`** and the container reaches it through the host daemon over one allowed
TCP port, sending typed database operations (never whole CLI commands, which would run gates and git
on the host, outside this sandbox). What a command cannot do that way is refused in remote mode —
`docs/message-queue.md` lists the host-only commands.

**Enforced, not just configured.** Remote mode refuses `--db` and a non-empty `JKB_DB` before
anything opens, and `check-config.sh` / `verify.sh` fail on `JKB_DB` being set or the old volume
coming back. Behind that, the rule also lives where a database is created or opened: `jkb-core`'s `db::open` and `Db::backup` refuse any `file:` URI, and
any database that would touch a FUSE, 9p, NFS or SMB filesystem (`crates/jkb-core/src/shared_fs.rs`).
What is asked: the directory the files will be created in (symlinks followed, dangling ones
included, since SQLite creates the database at a dangling link's target; the nearest existing
ancestor for a path that does not exist yet), **and** the database file and its `-wal`/`-shm`/
`-journal` when they exist, because a single file bind-mounted into a local directory is invisible
to statfs of that directory. A statfs that cannot answer refuses — except for a file that is gone by
the time it is asked, which is skipped while its directory still judges: SQLite deletes `-wal`/`-shm`
when another process closes its last connection, and an open beside one that was just exiting was
refused as "cannot tell what filesystem" (met by a CLI test under a parallel run, 2026-09-15; both the
Rust and the shell copy skip it, each pinned by a test). Every script's database read goes
through `jkb_sqlite` in
`scripts/lib.sh`, which applies the same magic set — `scripts/tests/dev-scripts.test.sh` case11 fails
on a bare call, on the two sets drifting, and inside the container on the live bind not being
refused. So a process with remote mode switched off and `--db ~/.jkb/jkb.db` (or no `--db` at all), gets a
refusal instead of the host's database — **once the installed `jkb` carries the refusal**: a binary built before it opens
the host's database from in here (a review measured exactly that), so `setup.sh` must have rebuilt it,
and `verify.sh` asks the installed binary to open a probe on the bind and requires the refusal.
`db::open` also refuses any `file:` string: the bundled SQLite is compiled with `-DSQLITE_USE_URI`,
and `--db file:/home/vscode/.jkb/jkb.db` opened the host's database past a guard that judged a
relative path — measured, it listed the host's namespaces and touched its `-shm` before this fix. Measured in the container: `jkb --db ~/.jkb/refusal-probe/jkb.db ns ls` exits 1
naming the FUSE bind and creates no database file (the CLI's `create_dir_all` of the parent still runs
first, leaving an empty directory).

**Residual, stated.** The guard covers jkb and `jkb_sqlite`. Any *other* SQLite client run in the
container — `python3 -c 'import sqlite3; sqlite3.connect(".../.jkb/jkb.db")'`, a hand-typed database
shell — is not jkb and is not refused; `.claude/hooks/block-raw-sqlite.sh` matches only the shell,
only for agent tool calls, and fails open. What would close that for good is the container not
seeing the host's database file at all; the bind still carries it, because `~/.jkb` holds the token
and the other shared state (`openspec/changes/jkb-message-queue/design-r3.md`).

## The one opening to the host: `jkb serve` on port 7117

The host's `jkb serve` (`com.jkb.serve`, installed by `scripts/setup.sh` on the host) listens on the
host's `127.0.0.1:7117` and nowhere else. The container reaches it as `host.docker.internal:7117`.
Measured on the Mac, 2026-09-14, Docker Desktop 4.87.0:

| probe | result |
|---|---|
| plain `curlimages/curl` container → `host.docker.internal:7117/v1/hello`, no token | `401` — Docker Desktop forwards the alias to the host's **loopback**, so the daemon need not listen on anything wider |
| the same with `-4`, and with `--add-host=host.docker.internal:host-gateway` | `401`, `401` |
| `getent ahostsv4 host.docker.internal`, plain container and `jkb-dev` | `192.168.65.254` in both (v6 `fdc4:f303:9324::254` first) |
| `jkb-dev` before the rule, `curl -4` | "Connection refused" after 1 ms — this firewall's REJECT |
| inside the Claude Bash sandbox | the alias does not resolve (`getent` exit 2); its proxy resolves on the sandbox's behalf |
| the same sandbox, through its proxy, rebuilt container, alias in the installed posture | `/v1/hello` `401` without the token, the hello JSON with it; `:7118` `502`; `jkb --json mq topic ls` in remote mode `[]` |

**Port-only, never a posture domain's address.** Docker Desktop forwards that alias to the host's
loopback on *every* port, and the IP allowlist (`allowed`) is `hash:net` with no port. So the host's
address in `allowed` would open every service listening on the Mac's loopback to this container.
`init-firewall.sh` resolves the alias into its own set (`jkb-daemon`) first, installs
`RULE_DAEMON` (`-p tcp --dport 7117 -m set --match-set jkb-daemon dst -j ACCEPT`), and keeps the host
out of `allowed` two ways: the alias **by name** (it never reaches `allowed`, even when the daemon
lookup came back empty while the posture's lookup of the same name did not — the hole a review of
`bc0228a` found), and any address in the daemon set **by address**, so a second name for the host
cannot walk past a rule keyed on one spelling. **Residual:** a *different* name resolving to the host
during a raise whose daemon lookup failed still lands in `allowed`; `verify.sh` reports that as
`wide`, because the probe re-resolves the alias. The alias is still in the posture's `allowedDomains`:
that is what lets the nested sandbox's proxy tunnel to it, and the firewall is what stops the same
entry widening the coarse layer. `egress-status.sh` reports the opening as
`daemon=port|unresolved|absent|wide` and the `daemon_at` it is about, and `verify.sh` fails on
anything but `port`. "No other host port" means beyond DNS: the older rules accept 53 to any address,
the host's included.

**Everything in here uses it** (tasks S6.5): `JKB_REMOTE` (`containerEnv`,
`host.docker.internal:7117` — `host:port` with no scheme, which `jkb` reads as `http://`, because
`lib.sh`'s `dc_strip` cannot tell a URL's `//` from a comment) puts every `jkb` command in remote
mode, and `jkb notify hook` posts to the same address (design r3.2 N1). Before the cutover the hook
alone used it, through a `JKB_DAEMON_ADDR` that is gone. Without it the hook would look on the
container's own loopback — every notification lost with nothing to say so — and every other command
would look for a database. `check-config.sh` holds the value to `DAEMON_HOST`/`DAEMON_PORT` and the
variable's name to `remote.rs`'s `REMOTE_VAR`, and `mutate-config.sh` drifts and drops each. Changing
it needs a rebuild, like the rest of `containerEnv`. See [docs/notifications.md](../docs/notifications.md).

**What `verify.sh` asks.** The kernel's answer above, at the address the *image* names; then the
daemon's own answer (`/v1/hello` with the token from the `~/.jkb` bind — the path remote mode takes).
A missing token is a **failure**, since the container depends on the daemon for every command
(tasks S6.5) — unless `JKB_VERIFY_NO_DAEMON=1` says none is expected, which `mutate-verify.sh` sets
because its scratch `~/.jkb` has no daemon (a CI runner would too); it is a note then, and a mutation
drops the variable and watches the failure. A token that exists but cannot be read is a failure. With the egress override
armed and no firewall, the daemon failures are accepted ones, so `verify.sh` still exits 3.

**The other direction is the kernel's `wide` answer, not a curl.** A probe of `host.docker.internal:7118`
expecting a refusal was written and removed: on a Linux engine a closed host port refuses whether or
not the rule is wide, so it passed in exactly the state it existed for, and no mutation could show it
firing. The `wide` state is read from the live sets instead — `ipset test` against `allowed`, where a
read that fails for any reason but "is NOT in set" counts as wide.

**`--add-host=host.docker.internal:host-gateway` is pinned** although Docker Desktop does not need it
(measured, above): a Linux engine resolves the alias only with it, and CI raises this firewall on one.
On a Linux *host* the daemon would also have to listen where the bridge can reach it, which is not done.

**VS Code must not forward 7117.** Measured on the Mac, 2026-09-14: something in the container
listened on 7117, VS Code auto-forwarded it, and so held the **host's** `127.0.0.1:7117` ("Code
Helper" in `lsof`). `com.jkb.serve` crash-looped on `Address already in use` in `~/.jkb/serve.log`,
and connections to the port hung. The container therefore carries a `devcontainer.metadata` label
setting `portsAttributes."7117".onAutoForward` to `ignore`, which attaching reads (it reads nothing
from `container.json`) — **whether attaching honours it is not yet measured**. `jkb serve` names the
condition itself now: the refusal gives the `lsof` command and this cause.

**The probe must not trip a caller's ERR trap on the Mac.** `egress-lib.sh --self-test` (in `check.sh`)
runs every probe under `set -eE` with an ERR trap, on whatever machine runs the gate. On the Mac,
2026-09-16, `daemon_state` tripped it and the other probes did not. The Mac has neither `ipset` nor
`getent`, and its bash is 3.2. `daemon_state` was the only probe written `x="$(failing …)" || x=""`,
and that `||` guards only the assignment in the outer shell, not the failure inside the substitution's
subshell, to which `set -E` hands the trap. bash 5.2 in the container, with the tools present and
also with them hidden from `PATH`, did not fire it. So the mechanism is inferred, not reproduced.
Every fallback is now inside its substitution, and statuses are read by `ipset_rc`, which takes them
in an `&&`/`||` list inside the subshell.

Changing any of this takes a **rebuild** (`./.container/run.sh --rm && ./.container/run.sh --build`):
the firewall, its library and the posture snapshot are installed into the image and read at create.

## A session worktree is an ordinary folder in here

`jkb task work` puts worktrees at `<repo>/.jkb/work/<session>`, and a linked worktree's `.git` is a
*file* pointing into `<repo>/.git/worktrees/…`. All of `~/repos` is mounted, so both ends are
inside the container and sessions work normally — attach and open the worktree like any other
folder. (Mounting only the worktree would break git, because the gitdir it points at would not be
there; that is why the mount is the parent and not the folder you happen to be working in.)

This is what the move off Dev Containers bought. Its `workspaceFolder` could not express a nested
path, so a session could not be opened at all, and the fallback opened the main checkout instead —
which meant a change to this directory could only be tested after landing it.

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
