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
verifying, so a failing assertion about something else cannot disable it. Beside it, and for the
same ordering reason, it sweeps session transcripts by byte budget: without that, the sandbox's
deny list outgrows a single argv and **every** Bash call in **every** session fails at spawn. See
*Transcripts are swept by byte budget, not by age* at the end of this file for the measurement.

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
Code auto-updated `anthropic.claude-code` from the then-pinned 2.1.250 to 2.1.251 and fetched it
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

### Only what the container uses of `~/.jkb`, the credential read-only, and every repo's git config read-only (D52)

The whole of `~/.jkb` used to be bound in. It holds the operator's database, five whole-database
copies, and the daemon's **root token** — the credential that makes a caller the operator — so
anything in here could read it, or open the database directly (the no-raw-sqlite hook is advisory
and fails open). Measured from inside, 2026-09-25: `jkb.db`, its `-wal`/`-shm`, `backups/` and
`daemon/` were all visible. Now (design D52.8, `docs/task-lifecycle.md` D52):

| host | container | mode | for |
|---|---|---|---|
| `~/.jkb/claude-memory` | same | rw | auto-memory |
| `~/.jkb/logs` | same | rw | the hooks' logs |
| `~/.jkb-container/` | same | **ro** | the container credential — a coordinator grant, this container's ceiling |

`~/.jkb` itself is an image directory, so the database and the root token do not exist in here.
The container authenticates to `jkb serve` with its credential, written on the host by `setup.sh`
(`jkb role rotate-container --write --keep-live`, which keeps a live one rather than revoking every
worker minted from it). It is outside `~/.jkb` so it is not a read-only bind nested in a writable one.

**Every repository's `.git/config` and `.git/hooks` are bound read-only** by `run.sh`, generated
because this file cannot list repositories. Git on the HOST runs what those name (`core.fsmonitor`,
hooks, filters), so a container able to write them could run code as the operator (hole H5).
Measured on Docker Desktop 29.7.2: overwriting, renaming over (EBUSY), planting a hook and enabling
per-worktree config all fail, and the config is left unchanged. **A speed bump, not the boundary**
(corrected by review round 1): `.git/` is writable around the binds, so renaming `.git` itself away,
or planting `.git/commondir` — which redirects git's config and hooks anywhere (measured on git
2.51.1) — gets past them, and submodule configs are not bound. What holds is jkb's own audit before
every git call, which also refuses a redirected git directory — and jkb's git never enters a
submodule — and the reap service's scan, which also reports submodule configs, for your git
(`docs/task-lifecycle.md`, D52). What it costs in here: `git config
--local` and `git remote add` fail; `branch --set-upstream-to` and `push -u` print an error, exit
0 and record no upstream — set those on the host. Status, commit, branches, fetch and `git worktree
add`/`remove` (so `jkb task work`) are unaffected. A repository cloned after the container starts is
covered at its next start; until then jkb's own git refuses a planted one, and the host's reap
service posts a notification.

`verify.sh` asserts the database and token are absent, the credential is read-only, and every
generated git bind is read-only (a writable one fails the boundary check). `check-config.sh`
refuses the whole `~/.jkb` as a bind source and a writable credential mount; `mutate-config.sh`
and `mutate-verify.sh` carry a mutation for each.

**The harness's own rules are managed settings baked into the image**
(`/etc/claude-code/managed-settings.json`, from `managed-settings.json` here, root-owned): the
attestation hook (`jkb attest hook`, which tells `jkb serve` which agent made each `jkb` call),
the workflow Stop hook (which holds only a session launched with `JKB_DRIVE`), and deny rules keeping the model's in-process tools — which the Bash
sandbox does not confine — away from the credential, from session transcripts (a live tool call's
ticket is written there; `*.jsonl` only, so auto-memory stays readable, and workflow journals do
not), and from the files that configure the harness (a hook, agent definition or MCP server it
could add would run unsandboxed).

**The hooks run a pinned, root-owned `jkb`** — `/usr/local/lib/jkb-hook/jkb`, never `jkb` from
PATH. They run outside the sandbox with the credential readable, and `~/.cargo/bin` is writable from
inside it (measured: a sandboxed `touch ~/.cargo/bin/x` succeeds), so a PATH lookup ran whatever a
tool call last installed there. `setup.sh` pins its build with `sudo -n /usr/local/bin/pin-jkb-hook.sh`
(a root script, sudoers-pinned to no arguments), which a sandboxed command cannot run (measured:
`NoNewPrivs: 1`, `sudo -n` exits 1). After rebuilding jkb yourself, run it again. `check-config.sh`
refuses a managed hook naming anything else, and `verify.sh` that the pinned binary is missing or
writable. The residual: it pins whatever `~/.cargo/bin/jkb` is when it runs.

**Not yet measured, because it needs this image running:** that managed hooks run beside the
project's and user's (they are not exclusive unless `allowManagedHooksOnly`, which is not set
because it would disable this repo's own hooks); that the managed deny rules merge with the
posture's; that returning `permissionDecision: "allow"` with `updatedInput` from the attestation
hook does not override a deny rule (it returns `allow` only for one plain `jkb` invocation, and
`ask` for any other command running `jkb`); and hook latency against the real daemon. `verify.sh` does not yet check these.

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
  - `apply-failed` **fails**: `run.sh` could not set the key (a leftover `config.lock` is the
    measured cause). It is its own word, because `-` means "the host sets none", and round 6 found
    a failed write reading as that and passing. An unreadable host config records `kept` and
    leaves the key as the last good start set it, rather than turning "unknown" into "no hooks".
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
been run. CI runs only its `--control` and `--ladder`. The record-driven verdicts (`not-applied`,
`apply-failed`, the notes) are driven by `verify.sh --self-test` from real record files through
`hooks_record_verdict`, since no test container starts through `run.sh`.

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

## Transcripts are swept by byte budget, not by age

Every Bash tool call in every container session failed at spawn with `E2BIG`. Not degraded —
total, from the first call, in a container that had worked the week before, with nothing in the
message naming the cause.

**What was measured, 2026-09-28, in `jkb-dev`:**

| | |
|---|---|
| transcript `.jsonl` files under `~/.claude/projects` | 1,182 |
| their path text | 224 KB (~194 bytes per path) |
| deny-list entries the sandbox profile built from them | ~2,396 |
| path text in the profile | 448 KB |
| what the harness reported | `command line 498.8KB across 3 args` |
| `MAX_ARG_STRLEN` (Linux, 32 pages) | 131,072 bytes |

Claude Code's Bash sandbox enumerates every transcript **individually** into its read-`denyOnly`
list and passes that profile to the shell as a **single argv string**, which the kernel caps at
one page-times-32. Two things multiplied it. Transcripts **nest** — `<slug>/<uuid>.jsonl` is depth
2, `<slug>/<uuid>/subagents/agent-*.jsonl` is depth 4, and
`<slug>/<uuid>/subagents/workflows/wf_*/agent-*.jsonl` is depth 6 — and the deep ones are the
bulk, one per task-swarm implementer, per reviewer and per Workflow agent. And
`~/.claude/projects` is a **symlink** to `~/.claude-state/projects`, so every file is listed under
both spellings: the 224 KB is doubled before the fixed security paths are added.

`.claude-state` is a Docker volume, so none of this resets on a rebuild. The count only goes up.

**Why the budget is bytes.** The failing quantity is bytes of argv, so that is what
`.container/sweep-transcripts.sh` counts: the path text of the `.jsonl` files under the root,
doubled for the two spellings, archived oldest-first until the projection is under 64 KB — half
the ceiling, leaving the other half for the security paths, the write-side lists and the
JSON quoting around every entry. **That half is not static** — it was described here as "~30 fixed"
paths, and the Round 8 note below records 89 measured, six of them per-task git worktrees, which
grow with exactly this workload and which no sweep can reclaim.

The two obvious alternatives are both things that already failed here:

- **Time-based** is what Claude Code itself does, and `cleanupPeriodDays` never binds — the byte
  budget is exhausted well inside any 30-day window. A container with retention configured is
  exactly the container that arrived at 1,182 files.
- **Count-based** is closer, but it drifts in the direction that breaks: the path text per file
  grows as agents nest deeper, so a count chosen against today's tree silently stops fitting
  without anything changing but the shape of the work.

**What it costs.** Archived transcripts move to `~/.claude-state/transcript-archive`, a sibling of
`projects/` in the same volume. It survives rebuilds, the move is a rename on one filesystem
rather than a copy, and being outside `projects/` is the only reason the sweep reduces anything.
Nothing is deleted. But Claude Code no longer lists an archived session, so `--resume` will not
offer it and `/resume` will not find it — recovering one is copying a file back, if you know it is
there. At ~194 bytes a path the budget keeps roughly the newest 169 files, and the newest 32 are
kept unconditionally whatever the arithmetic says, because the live session is writing one of them
right now.

`run.sh` runs it on every start, **before** `verify.sh` and for the same reason the deferred
worktree reap runs there (*Using it*, above): one failing assertion about something else must not
disable it. It is never fatal — what it could not archive it says, and a deny list slightly too
long is the state we were already in.

**These properties of one `find` carry the whole thing**, and each is held by both
`sweep-transcripts.sh --self-test` and a `check-config.sh` assertion, because every way of getting
it wrong is silent in both directions — a sweep that archives the wrong set reports success
exactly like one that archives the right set. No numeral stands in front of the list, in any of the
three files that carry it: the count lives in `PINNED_SWEEP_APPENDS`, which is derived from the
guard block itself, and a number written in prose beside a list is a second copy that goes stale on
the next condition — which is what happened here, "four" over five bullets, in a change whose whole
guard strategy is pinning counts so an unwatched branch cannot be added:

- **`-L`**, because the root is reachable through a symlink and `find` does not follow a symlinked
  *starting point* without it. Drop it and the sweep enumerates nothing and says so cheerfully.
- **No depth cap**, because the nested agent transcripts are the population that matters. Cap it
  and the container still dies at spawn with a sweep in the log saying it worked.
- **`-name '*.jsonl'`**, because `<slug>/memory/` holds auto-memory as `.md` files. The name is
  the guard; depth never was.
- **Nothing held back in the walk.** The enumeration feeds the *projection* as well as the plan,
  and the projection is the sizing of the argv that overflows: the kernel charges the same bytes
  for a path the sweep cannot reclaim as for one it can. The round that introduced the journal
  exclusion put it in this `find`, and the sweep then acted on 65,250 bytes where the real path
  text measured 96,612 — half again as much, all of it held-back journals — and printed
  `nothing to archive` while every Bash call went on dying at spawn. Journals are never archived and `.claude-state` is a volume, so the
  unreclaimable set only grows: at ~165 bytes each, about 198 of them exceed the whole budget on
  their own, and the sweep now says that out loud rather than reporting success it cannot deliver.
  The name is spared in `transcript_plan`; the walk counts everything.
- **`HELD_NAME`, spared in the plan**, and the sentence that used to stand here was wrong in a way
  that cost a real defect. It said the harness keeps `workflows/wf_*.json` run records; there are none —
  measured, zero anywhere under the root. What it writes is
  `<slug>/<uuid>/subagents/workflows/wf_*/journal.jsonl`, 23 of them in this container, which
  `*.jsonl` matches — and `swarm-status.sh` **discovers** every run by that exact name before
  requiring the file. So the sweep archived the harness's own run state oldest-first on every
  container start, and `swarm-status.sh <run>` answered `no swarm run found` for every past run.
  The `agent-*.jsonl` transcripts in those same directories are the bulk of what must be swept, so
  `workflows` cannot be pruned the way `memory` is: exactly one name is spared. The self-test
  written to prevent this asserted the survival of a `wf_*.json` fixture — a shape no real tree
  has — so it could not fail. A guard whose fixture models something that does not exist is not a
  guard, and this is the third place that same wrong rule was written down. The name itself is no
  longer spelled in `check-config.sh` either: it is **read out of `swarm-status.sh`'s discovery
  predicate** and required to agree, because the authority is an external harness and a pinned
  literal stays green through the one case that matters — that harness renaming its own file.
- **`memory/` pruned**, because under `-L` that per-repo symlink into the bind-mounted
  `~/.jkb/claude-memory` is followed like a real directory and the walk leaves the volume — into
  files the *host* owns.

And every project slug begins with `-` (the absolute path with non-alphanumerics replaced, so the
leading `/` becomes one): `-home-vscode-repos-jkb`. A bare `dirname "$rel"` reads that as the `-h`
option and dies, and it is every path in the tree rather than an edge case, so the relative
directory comes from `${rel%/*}` and every external command here is given `--`.

The archive must be **outside** the root, or the sweep grows the deny list it exists to shrink: the
next walk finds what this one moved, one directory deeper, for ever. That refusal was in place from
the start and was **inoperative in the only deployment it was written for**. It compared the
caller's *spellings* — a string prefix of the root as passed — while the walk is `-L` and
`~/.claude/projects` is a symlink into the state volume, so
`JKB_TRANSCRIPT_ARCHIVE=~/.claude-state/projects/.archive`, a path squarely inside the enumerated
tree, was not a prefix of `/home/vscode/.claude/projects` and was accepted. Reproduced against the
real script with nothing else touched: 530 → 602 → 674 deny bytes across two sweeps, files nesting
under `.archive/.archive/` — the same signature this document already records as the measured
defect. It now compares **resolved** paths (`pwd -P` on the root, and the archive's nearest
existing ancestor resolved the same way, which also collapses a `..` that climbs back in), while
the enumeration keeps the caller's unresolved spelling, because that is the spelling the deny list
is built from. The self-test exercises the refusal through *every* spelling, not just the one the
fixture happened to pass; the row that stood alone before was the row that passes either way.
Behind it the sweep now carries a post-condition on the two numbers it was already printing —
`before` and `after` sat side by side in the summary with nothing comparing them, which is how a
run that *grew* the deny list exited 0 with every count reading as success.

**What the self-test was really measuring.** Three rounds of review found the same shape here: an
assertion that passes for the wrong reason.

- The fixture's mtimes ascended in the same order as its `LC_ALL=C` paths, so *"archived the three
  oldest and kept the two newest"* was satisfied by **path order alone**. Measured: replacing the
  GNU `stat` format's `%Y` with a constant still printed `self-test passed`, that row included —
  with a non-numeric key `sort -k1,1n` ties every record and falls back to the path key for the
  identical set. So `%y` (a date string) or `%W` (0 on ext4) would have shipped green, and in the
  container `KEEP_NEWEST` would have protected the 32 lexicographically-*last* paths rather than
  the newest, archiving the live session's own transcript out from under it. The fixture now
  arranges mtime order to **disagree** with path order, so the two choose different sets and only
  the mtime one satisfies the rows; two further rows pin the record's first field as numeric and as
  ordered.
- None of the four budget constants was asserted anywhere — not here, not in `check-config.sh`, not
  in `mutate-config.sh` — although the file's own header says `--self-test` exists to *state* them.
  All four were mutated and every gate stayed green: `KEEP_NEWEST=0` (the harm `scripts/check.sh`
  names by name), `KEEP_NEWEST=3200`, a 4× budget and a 10× argv cap. They are stated now, before
  the first override, which is the only place the shipped values are readable.
- The *"running it again"* rows were vacuous: `KEEP_NEWEST=2` was still in force over a population
  of 2, so `n - keep` was zero and the plan was empty **by the floor** — they re-measured a floor
  asserted three rows earlier, under the heading written to catch the nesting defect. The second
  sweep now runs with the floor off and the budget set to exactly what the tree projects, so an
  empty plan is the sweep deciding it is under budget; a third runs with a budget that binds, and
  asserts it archives from the tree and not from its own archive.
- Nothing ran the file as a **program**. The root resolution and the argument dispatch were
  executed by no test, and `check-config.sh`'s `grep -qF CLAUDE_CONFIG_DIR` was satisfied by any
  mention anywhere — so one brace out of place (`"${CLAUDE_CONFIG_DIR:-$HOME}/.claude"`) made the
  sweep walk a tree that does not exist, print `does not exist`, exit 0, and leave every gate
  green: precisely the *"a sweep that cannot find its subject must not look successful"* failure the
  comment claims to have closed. There are now rows that invoke the script with `HOME`,
  `CLAUDE_CONFIG_DIR` and `JKB_TRANSCRIPT_ROOT` pointed at fixtures and read the root back out of
  its message, plus the `--dry-run`, unknown-argument and trailing-argument exits. That mention-form
  guard has been tightened to `CLAUDE_BASE=.*CLAUDE_CONFIG_DIR`: the self-test's own
  `env -u CLAUDE_CONFIG_DIR` was enough to satisfy the old one, which turned a CAUGHT mutation into
  a MISSED one the moment an unrelated row named the variable.

`|| true` on `run.sh`'s invocation is the entirety of the sweep's *never fatal* claim, and it was
the one pinned property whose only watcher was a mutation **anchor**: both wiring mutations carried
the text inside their anchor strings, so removing it from `run.sh` reported `NO-OP the mutation
changed nothing` and pointed at the mutation rather than at the lost non-fatality — whose natural
repair, relaxing the anchor, greens the gate. It has a condition and a mutation of its own now, and
the two wiring mutations find their line by its statement instead.

**Round 3 found the hole all of that guarding had been built around.** Nothing ever compared the
projection to the budget. `before`, `after` and `DENY_BUDGET_BYTES` were printed side by side on one
line with no pair of them ever tested — in a script whose entire subject is a budget. Both branches
reported success over budget, reproduced 2026-09-28 against a copy with only the budget lowered:

- **Empty plan.** 3 sessions + 3 run journals, 702 bytes projected against a budget of 500,
  `KEEP_NEWEST=32`. The archivable population is inside the floor, so `n - keep` is negative and the
  plan is empty *however far over the tree is*. Output: `702 deny bytes projected, budget 500 —
  nothing to archive`, rc 0.
- **Exhausted plan.** 43 sessions, budget 500: `archived 11 file(s) … (4086 -> 3076 deny bytes,
  budget 500)`, rc 0, with the residual six times the budget sitting in the same `printf` as the
  budget.

On the real container the second is reachable on the journals' own growth: the residual after a full
plan is `held_bytes` plus the newest `KEEP_NEWEST`, which passes 65,536 at roughly 161 journals,
while the unreclaimable warning added the round before only fires past about 199. In that window the
sweep prints success, `run.sh`'s `|| true` discards the code, and every Bash call still dies at
spawn — the exact failure this whole file exists to prevent, with a clean log. `transcript_over_budget`
now asks the question at all three exits, and the dry-run summary states the residual instead of
asserting `-> under` an outcome nothing checked.

Two smaller things the same round found, both of the *"half a guard"* shape this document keeps
recording. The containment comparison has two operands and only `phys_root` was pinned — replacing
`phys_archive="$(transcript_resolve "$archive")"` with `phys_archive="$archive"`, a plausible
simplification, left every gate green and re-admitted the nesting. And the projection multiplies the
*caller's* spelling of each path, which in the container is the shorter of the two: the real deny
list is larger by 6 bytes a file, about 7KB at 1,182 files, roughly 11% of the budget, in the
direction that overflows. The half-of-`MAX_ARG_STRLEN` margin absorbs it today; the constant now says
so, because whoever tightens that margin has to close this gap first.

`mv -n` is where the record has to be careful about platforms. On GNU coreutils 9.4 a collision
prints `mv: not replacing '…'` and exits 1; BSD and macOS skip silently and exit 0. The self-test
therefore asserts only what both agree on — the already-archived copy is unchanged and the source
stays in the tree — and asserts nothing about the return code or the message. The collision is not
hypothetical: a session whose `<slug>/<uuid>.jsonl` was archived and is then resumed by id recreates
that relative path, the next sweep plans it oldest-first, and `~/.claude-state` is a volume, so the
archived copy is the only one there is.

`JKB_DENY_BUDGET_BYTES` is a **self-test seam**, and it is the only way `--self-test` can drive this
file *as a program* against a tree it can build in a temp directory — without it every program-level
row has to point at an empty root, where `sweep_transcripts` returns at `no transcripts` before it
ever reads its third argument, so `--dry-run` through the CLI was exercised by nothing. Two things keep a seam from becoming a lever, and the second is narrower than it first shipped:

- `check-config.sh` refuses **any** seam appearing in any shipped file under `.container/` — the list
  of names read out of the script's own `SEAMS=` line and checked against every `${JKB_…:-}` it
  actually reads, the file list derived from the directory rather than retyped. Wired into the
  container, a seam silently disables the sweep while every start still reports success: the state
  this script exists to end, wearing a clean log. The first version named one of three seams and
  scanned four files by hand — missing `verify.sh`, which the same change had just made a *caller*
  and which runs **inside** the container, where a seam actually takes effect. (In `run.sh` it would
  not: `in_container` is plain `docker exec` with no `-e`, so a `VAR=… in_container …` prefix sets
  the variable for the docker CLI and never reaches the container.)
- `--self-test` refuses to run when **`JKB_DENY_BUDGET_BYTES`** is set in its own environment — that
  one, and deliberately not the others. Its value is what the constants rows read, so exported it
  gives a red gate for a correct script or a green one for rows that have stopped measuring the
  shipped number. The version that refused all three plus `CLAUDE_CONFIG_DIR` was backed out on a
  measurement: `./scripts/check.sh` went red on a *correct checkout* for anyone using the
  second-config-dir posture this very script cites approvingly, and `check.sh` stops at the first
  failing gate, so every step after it silently stopped running. A machine-dependent gate step
  degrades to a named skip in this repository; it never reddens. The other two cannot affect a row
  anyway — the function rows pass explicit paths and the program rows neutralise every seam with
  `env -u`. Do not re-add them.

One property has no executable test anywhere and says so in place: a transcript that vanishes
between the plan and the move must not be counted as a failure. Reaching that state needs a file to
disappear between two statements of one function — a live session, `cleanupPeriodDays` retention, or
a second `run.sh` all produce it in the field, and none of them can be staged. The static pin in
`check-config.sh` and its mutation are the whole of the coverage, which is worth writing down rather
than leaving a reader to infer that the fixture covers it.

**Round 4, and the two findings that were not about the guards.** The sweep's only trigger is a
container start. The origin story at the top of this section is exactly that path — a container that
had worked the week before reached 1,182 transcripts *without being recreated* — and the documented
workflow is `run.sh` once, then attach and keep working, where a second window (`code <path>` from an
attached terminal) never re-enters `run.sh`. The budget keeps roughly 169 files, and every swarm
implementer, reviewer and Workflow agent writes a transcript. Measured in this container on
2026-09-28, after the sweep had shipped: **80,092 deny bytes projected against a 65,536 budget**, in a
tree no sweep had reached since the start. A start-only trigger bounds the deny list at a rate that
has nothing to do with the rate transcripts are created, and that gap is not closed by anything in
this file.

What *is* closed is the reporting. `run.sh` discards the sweep's exit code with `|| true` — correctly,
since a deny list slightly too long must not abort a start — so the warning scrolled past several
steps before the verify anyone actually reads, and `verify.sh` said nothing about transcripts at all.
It now runs the sweep's own `--dry-run` (moves nothing; returns non-zero when the tree *as it
stands* is over budget — not when the plan would fail to reach it, which would be a verdict about a
tree that does not exist) and reports the answer where the operator is looking. The unreclaimable
floor is `accept_bad`, not `bad`: past roughly 199 run journals no sweep can bring the tree under, and
that is a condition to act on rather than a broken boundary — which is what this container's two exit
codes exist to distinguish.

Two more assertions that passed for the wrong reason, both introduced by the round that was fixing
that very class:

- **Three of the four containment rows returned 1 for an unrelated reason.** They sweep `$root` while
  the budget around them was measured through `$work/projects-link`; the spellings differ by a byte
  per path, so the residual check returned 1 whatever containment decided, and `rc_of` sees only the
  code. Measured: deleting the entire containment block turned exactly **one** of the four red — the
  executable half of the guard against the `.archive/.archive/` nesting had come to rest on a single
  row balanced on an exact budget equality that any fixture edit breaks in silence. They assert the
  refusal's own words now, not the exit code.
- **`transcript_resolve` collapsed a `..` only when its left-hand component already existed.** On the
  *first* sweep — the only state the question is ever asked in, since the sweep is what creates the
  archive — the whole of `…/transcript-archive/../projects/.archive` is the unresolvable tail, the
  prefix test saw a path still containing `..`, and the archive was accepted inside the root (deny
  bytes 96 → 114 on the probe). Every later sweep then refused for ever, because `mkdir` had made the
  `..` collapsible. The self-test row written for this route passed for the same reason: it ran after
  a real sweep had already created the archive. The tail is collapsed lexically now, which is sound
  *here and only here* — a tail component that existed as a directory would have stopped the walk-up,
  so there is no symlink left in it for `..` to mean something else about.

And a rule worth stating plainly, because this section keeps recording it: **a guard must pin the
whole of what its message claims.** `phys_root` was pinned and `phys_archive` was not; the seam
refusal named one of three seams and scanned four of the files that matter; the containment rows
asserted an exit code that two different things produce; the `verify.sh` guard required the sweep to
be *called* and not that any verdict came of it. Each shipped with a comment claiming the whole
property, and each was found by deleting the code underneath and watching the assertion stay green.

**Round 5 found the assertion added in round 4 reporting the state it was added to catch.**
`verify.sh` asks the sweep `--dry-run` and reads its exit code — and that code was a verdict on
`before - planned`, a tree that does not exist, because a dry run moves nothing. So whenever the real
sweep at container start had failed — the archive unusable, `mkdir` denied, ENOSPC on the state
volume, a containment refusal — `run.sh` discarded that failure by design, the dry run found a plan
that *would* have fitted, and `verify.sh` printed `ok  the transcript deny list fits in one argv` over
a tree in which not one file had moved and every Bash call still died at spawn. Reproduced: 60
transcripts, 7,200 deny bytes, budget 3,960, a regular file where the archive's parent must go — real
sweep `could not create … — nothing archived`, rc 1; dry run rc **0**. Both exit codes now answer one
question, *is the deny list as it stands on disk right now too long for one argv*, and `before -
planned` stays in the summary as information rather than as the verdict.

Three consequences of that block, all of the same family — a report is only as good as what it can
tell apart:

- **The unhelpable state was diagnosed too late.** `accept_bad` (exit 3, "a condition to act on";
  both codes refuse a window — `run.sh` tests `-ne 0` — and what exit 3 changes is which refusal
  the operator is given)
  was selected by a message that tested the run journals *alone*. But the residual after a full plan
  is those journals **plus the newest `KEEP_NEWEST`**, which crosses 65,536 at roughly 161 journals
  while a journals-only test only speaks past about 199 — and a container reaches the first on its
  way to the second. In that window the tree was equally beyond any sweep's help and was reported as
  a broken boundary: exit 1, `run.sh` refuses to open a window, and the only remedy sentence shown
  was the one that does not apply. `transcript_irreducible` now measures what no sweep can remove —
  held plus floor — so the verdict arrives when the state does.
- **Every non-zero exit was called "over budget".** The sweep returns non-zero for things that are
  not about the budget: a containment refusal (the only line is the refusal), a resolver failure (rc
  1 and *no output at all*), a syntax error (rc 2, bash's own message). Each printed "the transcript
  deny list is over budget — «unrelated text or nothing»" followed by a sentence asserting an
  archiving pass that never happened. The arms classify on the sweep's own wording now, and anything
  else is reported as *the sweep could not answer*, which is a different thing to act on.
- **Exit 3 gained a second producer and `run.sh`'s narration did not notice.** It named the
  unfiltered-egress override as the only thing exit 3 can mean and told the operator to unset a
  variable that is not set and recreate a container whose journals live in a volume. Two conditions
  with opposite remedies cannot share one hard-coded sentence, so the message now points at the FAIL
  lines, which each carry their own.

**The second trigger (2026-09-28).** The gap recorded above — the sweep firing only at container
start, while transcripts are created continuously — is closed by the **host's reaper**, not by a new
scheduler. `jkb task reap --watch` is already the one long-lived process on the host sweeping on a
timer for this project, and this is the same kind of job: something only an outside process is placed
to do. It now pokes the container on each tick
([`crates/jkb-cli/src/transcripts.rs`](../crates/jkb-cli/src/transcripts.rs)).

It has to go through Docker, and that is forced rather than chosen: `~/.claude-state` is a **named
volume** (`jkb-claude-state`), not a host bind, so there is no host path to walk — the work can only
happen inside. The reaper also knows a *database path*, never a checkout, so it cannot run a working
tree's copy of the script. It feeds the sweep to `docker exec -i … bash -s` on **stdin**, from a copy
`include_str!`'d into the `jkb` binary at compile time.

> **Superseded, and worth keeping.** This first installed the script *into the image* at
> `/usr/local/bin/`, the way `init-firewall.sh` and the egress scripts are, and exec'd it by path.
> What reversed it was a measurement: `ls /usr/local/bin/` in the **running** container showed only
> the four older scripts. Only a rebuilt image carries a new file, nothing forces a rebuild, so
> `bash` would have exited 127 — the trigger dead on every live container, reported once into
> `reap.log` and deduped for ever, with every gate green. Embedding removes the second copy instead
> of guarding it: no drift, no rebuild, no path for two files to agree about.

The container's **name** must agree across the two files that spell it, and it is **silent when
wrong**: a reaper poking a name nothing creates reports nothing for ever — the same end state as
having no second trigger at all, wearing a green log. So `check-config.sh` reads `DEV_CONTAINER_NAME`
and `run.sh`'s `${JKB_CONTAINER_NAME:-…}` default from the files that own them and requires them to
match, with an extraction that reads nothing a failure rather than a vacuous pass. (A second pair —
the in-image path — used to be here too, and is gone with the image copy: see below.)

The tick is **never fatal and usually silent**. No Docker, no such container, or a stopped one is the
ordinary case for anyone not using the dev container and says nothing at all; a sweep with nothing to
do says nothing, because this runs 96 times a day for ever and a log that reports a timer firing is a
log nobody reads the rest of; and an over-budget tree is said **once** while it stays the same, the
rule the reaper already applies to its own failures and the queue's compaction. Rejected: a loop
inside the container and a timer unit beside this one, both of which are a second scheduler to reason
about for one sweep.

The `docker` spawn is declared in `gitrepo.rs`'s `NOT_REPO_AWARE`, and that guard is what caught it:
`docker` is addressed by **container name**, never resolves a repository, and the caller's working
directory changes nothing about which container is poked.

**Round 6, and a guard whose three checks were really two.** The verdict-coverage loop added the
round before — requiring `verify.sh`'s deny-list block to reach `ok`, `bad` *and* `accept_bad` —
matched `bad "` as an unanchored substring, and `accept_bad "` **contains** `bad "`. So the
`accept_bad` arm alone satisfied the `bad` iteration: both plain arms could be demoted to notes, the
gate still printed 70/70, and the one mutation here (which demotes `accept_bad`) was still caught by
the survivors. Anchored now — and what it still *cannot* see is written beside it, because a static
read cannot do better: it establishes that the block reaches each **kind** of verdict, never that the
*budget* arms are the ones reaching them. The behavioural half — an over-budget tree really exiting
1, an unreclaimable one really exiting 3, which is what `run.sh` reads to decide whether to open a
window — needs a container, so it belongs in `mutate-verify.sh` and **is not covered yet**.

Two more of the same family, both in guards this series added:

- **The floor half of `transcript_irreducible` was watched by nothing.** Both rows exercising the
  "beyond any sweep's help" message held `KEEP_NEWEST=0`, so deleting the floor term left
  `--self-test`, `check-config.sh` and `mutate-config.sh` all green while `verify.sh` silently
  reclassified over-budget trees from `accept_bad` to `bad` — in exactly the 161-to-199-journal
  window this document claims the term closed. There is now a row whose budget sits *between* what
  the journals project alone and what they project plus the floor, so only counting both reaches it;
  and the static pin asks that each half is **recognised and counted**, after a mutant whose held arm
  was `{ next }` passed the version that asked only that the arm existed.
- **`bad_sites` counted lines while its neighbour counted occurrences**, rewritten in the same commit
  for exactly the reason lines undercount. Two failure paths on one line moved the pin by one, so one
  of them shipped unmutated under a printed coverage number. And that harness's success line claimed
  "each branch with a mutation", which a count cannot establish and which was false for two branches
  — both of them the *extraction read nothing* guards this file says must be watched failing. It now
  says only what the count establishes.

**Exit 3 stopped being about a choice.** Both files said every failure reported was "a condition this
container was configured to accept" — true while the unfiltered-egress override was the only
producer, and false the moment the transcript deny list joined it. Nobody *configures* a container to
accumulate 199 run journals; it is emergent, and it is the one exit-3 producer with a concrete
remedy. Telling an operator they chose a state they did not choose, about the only thing they can
act on, is worse than saying nothing. It now reads "one this container tolerates rather than a broken
boundary", and the enumeration of producers lives in the FAIL lines rather than in a second copy.

Relatedly, the over-budget FAIL arm had begun asserting an archiving pass it has no evidence of —
`verify.sh` only ever runs `--dry-run`. It names the causes it cannot tell apart instead: the floor
genuinely binding, a start sweep that could not write its archive (`could not create` in the
scroll-back), and an archive refusing colliding destinations (`mv: not replacing`).

**`docker exec` is the right mechanism; exec'ing a path was not.** Round 7 checked the live
container and found `/usr/local/bin/` carrying only the four older scripts — so the first version of
the second trigger, which baked the sweep into the image and ran it by absolute path, was **dead on
every already-running container**: only a rebuilt image has that file, nothing forces a rebuild,
`bash` would have exited 127, and the reaper would have reported that once into `reap.log` and
deduped it for ever with every gate green.

The fix removes the possibility rather than guarding it, which is this directory's own rule. The
script is embedded in the `jkb` binary with `include_str!` (the crate already reaches out of itself
this way for `.claude/commands/*.md`) and fed to `docker exec -i … bash -s` on **stdin**. There is
then no copy in the image to drift, no rebuild to require, and no path for two files to agree about —
the reaper runs exactly the sweep the `jkb` that `setup.sh` installed was built from. It is written
from a thread, because the script is 74KB against a 64KB pipe buffer and `bash -s` executes as it
reads: a blocking write from the main thread deadlocks the moment the child pauses to run a `find`.

Two things the same round caught about the tick itself. It had **no timeout**, and it runs *before*
`reap_once` — so a Docker daemon that is half-up and never answers (the mode that hangs; one that is
simply down errors fast) would stall the process this repository calls "the one that finishes every
deferred landing on the machine", silently, for ever. One minute now, generous because a loaded
daemon is slow before it is broken. And a daemon that will not answer is reported, where *absent* is
not: "there is no container here" and "there may be one over budget and I could not find out" are
different facts, and only the first is somebody working normally on a laptop or a cloud instance with
no Docker at all.

**A live session's transcript, once the sweep runs on a timer.** `KEEP_NEWEST`'s entire argument was
*"the live session is writing one of them right now"* — and it was written for a sweep that ran at
container **start**, when nothing is open. On the host reaper's timer it runs mid-flight, and during
a swarm more than 32 transcripts are touched inside one window: at that point the newest-32 floor
stops being a statement about live sessions at all, a running session's transcript can be archived
out from under it, and `/resume` cannot find it again.

Two guards, and they answer different halves:

- **The registry is the precise one.** `jkb` already knows which Claude sessions are live — every
  hook in the container posts to the host daemon, so container sessions are registered on the host —
  and a transcript is named for its session. The reaper reads that list *per tick* (never cached: a
  session started since the last sweep is the one most at risk, being also the most recently
  written) and hands the ids to the sweep in `JKB_KEEP_SESSIONS`, which never plans one. A database
  the binary cannot open yields an **empty** list, which is the safe direction — empty falls back to
  the other two protections rather than to none.
- **Recency is the belt to that brace**, for what the registry cannot see: a session predating it,
  one whose hooks are not reporting, a container not in remote mode. One hour, and it is deliberately
  generous because over-protecting is *visible* — the sweep already says when it cannot reach the
  budget — while under-protecting is a lost transcript.

Neither is the time-based **retention** this document rejects further up, and the distinction is the
whole point: what bounds the sweep is still bytes. These say only that a file written moments ago is
probably open, which is a different claim from "old files may go".

Both skips happen **before** the floor and, like the held name, their bytes are still *projected* —
a file the sweep may not archive was never a candidate, so letting one consume a `KEEP_NEWEST` slot
would reserve protection for something already protected, while the argv still has to count it.
`check-config.sh` holds `KEEP_SESSIONS_VAR` and the shell's `${JKB_KEEP_SESSIONS:-}` to each other:
a variable spelled differently at the two ends protects nothing while both files read correct.

**Round 8 — six findings, none must-fix, and the first round in single digits.** Three of them were
about the liveness guards added the round before, and two of those were the same shape.

The registry half protected a session's **own** `<slug>/<uuid>.jsonl` and nothing else, because it
matched the transcript's leaf name against a session id. But a Task-tool subagent opens no session,
so the registry never holds a row for `<slug>/<uuid>/subagents/agent-X.jsonl` — and those nested
transcripts are, as the walk's own comment says, *the bulk of the population*. They were left to the
recency window alone, which a subagent sitting an hour on one tool call or a pending permission
prompt walks straight out of. The match is on the **session directory** now: `/<uuid>.jsonl` or
`/<uuid>/`, so everything a live session wrote is covered by the row it does have.

`transcript_irreducible` did not know about either skip, so a container recreated after a heavy
swarm — the standard recovery from an E2BIG — had every transcript inside the recency window, no
plan could reach the budget, and the measure reported a number under it. `verify.sh` then fell past
`accept_bad` into plain `bad`: exit 1 instead of 3, `run.sh` refusing to open a window, and a FAIL
naming three causes none of which applied. Both functions ask **one predicate** now, held as a
single awk text rather than a rule each remembers — the day they disagree is the day a sweep
archives a live transcript while reporting itself unable to reclaim anything.

And the reaper drained neither output pipe until after its wait loop. An archive gone read-only in
an over-budget container makes `mkdir -p` fail for every planned file — ~100 bytes of stderr each,
~100KB across a thousand, against a 64KB buffer. The child blocks on write, `try_wait` never returns,
and at sixty seconds the tick reported `Unreachable`: *the daemon would not answer*, when the daemon
was fine and the disk was full, burning the whole timeout before `reap_once` every time. All three
pipes have a thread now, joined after the child is gone.

Two smaller ones worth keeping. `prog`'s `env -u` list named three of the four seams, so it is
derived from `$SEAMS` now and the next seam cannot repeat it. And the headroom comment called the
uncounted half "the ~30 fixed security paths" — measured in a session that day: **89 deny paths, six
of them registered git worktrees**. D36 gives every task its own worktree, so the uncounted half
grows with exactly the workload this sweep was written for, and no sweep can reclaim a worktree path.
A few hundred bytes against 65,536 today, but it runs in the direction that overflows and it
compounds with the spelling lean recorded above: both have to be closed before anyone tightens this
margin.

**Round 9 — fourteen findings and two must-fix, after I had told the user the curve had broken.**
It had not, and the reason is worth recording: both must-fixes were in the *newest* work, and the
first was the worst defect this branch produced.

**The test suite archived the developer's own transcripts.** `crates/jkb-cli/tests/cli.rs` spawns a
real `task reap --watch` child with the developer's own environment, to prove a compaction failure
does not end the service. That loop now sweeps the dev container on its first tick — so on any host
with `jkb-dev` running, which is the normal state while working on this repo, `cargo test`
`docker exec`'d the sweep into the live container and moved real transcripts out of
`~/.claude-state`, with `watch.kill()` at 1500ms able to orphan the exec mid-archive. The reaper now
resolves its target through `JKB_CONTAINER_NAME` exactly as `run.sh` does, the fixture points it at
a name nothing can create, and `the_cli_fixture_does_not_inherit_a_repository` asserts that on
the built command through `assert_jkb_isolated` — so deleting it reddens a test rather than somebody's `/resume`. The same seam
closes a real gap: an operator who sets `JKB_CONTAINER_NAME` had a trigger silently dead for ever.

**And the start trigger fired on the already-running path.** `run.sh` against a live container prints
"is already running" and fell straight through to the sweep, which passes no `JKB_KEEP_SESSIONS` —
so every protection rounds 7–9 added covered the reaper's tick and not this one. An agent blocked on
a permission prompt past the recency window, with the floor spent on subagent files, would lose its
transcript to somebody opening a window in the morning. The premise the start trigger rests on is
*nothing is open at container start*; it now only fires when that is true by construction, and the
reaper's tick — which does carry the ids — owns the running case.

Three more worth keeping. `verify.sh`'s rc-0 arm printed `ok the transcript deny list fits in one
argv` when the sweep exited 0 because it **found no tree at all**, which is the sweep's own stated
rule broken in the reporting layer: a sweep that cannot find its subject must not look successful.
`Sweep::Absent` — reported by saying nothing — absorbed *"docker could not be run or reached"* as
well as *"no such container"*, so a launchd agent whose PATH omits Docker Desktop gives a trigger
dead for ever and silent about it; the probe is `docker ps --filter` now, which exits 0 with empty
output when the daemon **answered**, so silence is earned rather than assumed, and both unit
templates carry a PATH. And the "no sweep can remove" message named two of its three terms, omitting
the one that usually dominates and is the only one that *lapses* — half an hour after a swarm the
operator was told to delete run journals worth a few hundred bytes when the answer was to wait.

**Round 10 — the first to review only fixes, and it found a must-fix anyway.** I had said the
previous round's size tracked how much *new code* a round saw. That was wrong, and the way it was
wrong matters: the must-fix here was a **consequence of round 9's own fix**.

Round 9 stopped `run.sh` sweeping when the container was already running, because that path passes
no live-session list and its only safety was the premise *nothing is open at container start*. True
as far as it went — and it traded a rare loss for a common one. A container up for days, or started
from Docker Desktop, was then swept by **nothing** on that path: the window stayed refused, and
re-running `run.sh` — the documented recovery for the very E2BIG this exists to prevent — stopped
recovering. Two messages also became false where they printed. `verify.sh` told the operator to look
for `could not create` in the scroll-back of a sweep that never ran, and the skip line asserted that
the reaper's tick owned the container, which `run.sh` never checked and which is false whenever
`setup.sh --no-service` was used or the unit is stopped.

The fix is to stop skipping and start passing: `run.sh` now reads the same registry the reaper does,
through the daemon that already holds it, and hands the ids over with `docker exec -e`. `in_container`
is a plain `docker exec`, so `-e` reaches it where a shell prefix would set the variable for the
Docker CLI and never enter the container. An empty list is the safe direction and the one the reaper
also takes when it cannot read: the recency window and the floor still stand.

That also settles what `JKB_KEEP_SESSIONS` **is**. It had been filed with the four switch-it-off test
seams and refused in any shipped file — with a message saying it silently disables the sweep, which
is false for this one variable and would have refused the thing `run.sh` is supposed to do. `SEAMS`
now names the four that must never appear in a shipped file; `INPUTS` names every `JKB_` the script
reads. The agreement check and the self-test's neutralisation cover `INPUTS`; only `SEAMS` earns the
blanket refusal.

Three more worth keeping:

- **A `PATH` that could only subtract.** The systemd reap unit got `Environment=PATH=` alongside the
  launchd one, and the two are not the same case: systemd *replaces* the inherited value, and its
  compiled default already carries `/usr/local/bin:/usr/bin:/bin`. So it could never make `docker`
  reachable where it was not — and on a host that had imported a richer `PATH` (rootless Docker in
  `~/bin`, a Nix profile) it removed `git`, which every worktree archive shells out to. Dropped from
  systemd, kept on launchd where the new value is a superset of launchd's own minimal default and
  can only add.
- **The container pin was on one of two fixtures.** `sessions.rs` builds its own `jkb` and did not
  get it, and the oracle both fixtures already run did not look — so the guard test read green over
  half the surface. It is harmless only because `sessions.rs` happens to call `task reap` one-shot
  today. The pin lives in `common/` now and `assert_jkb_isolated` asserts it, which is what makes a
  third fixture unable to arrive without it. Verified by deleting it from one fixture and watching
  that fixture's own isolation test go red.
- **The five-way budget classifier was executed by nothing.** It decides exit 1 against exit 3,
  and a one-token flip in any arm changed that with every gate green. What that code actually
  buys was overstated here and in three other places as "whether `run.sh` opens a window": it does
  not — `run.sh` refuses on ANY non-zero verify (run.sh's `verify_rc -ne 0` gate). Exit 3 changes which refusal the
  operator reads, which for the transcript floor is the difference between one remedy that applies
  and three that do not. Corrected in place rather than quietly, because the overstatement was the
  stated justification for the classifier's guards. It is a pure `sweep_verdict()` now, driven from nine literal rows in `verify.sh --self-test`
  (which the gate already runs and which needs no container); three arm mutations confirm the rows
  discriminate. Defining it *above* the self-test block was not incidental: the block exits before
  anything below it is read, and bash resolves a function at call time, so a use above its
  definition is an empty result rather than an error — the mistake `check-config.sh` records making
  three times with one helper, and which I made once here before moving it.

**Round 11 — and the protection added to prevent data loss could cause the E2BIG instead.**

`live_sessions` asked the registry for rows with `ended_at IS NULL` and called them live. But a
container session ends **without its `SessionEnd` hook** whenever the container is stopped — every
rebuild, and the documented E2BIG recovery, which is `run.sh` recreating the container. Those rows
can never be closed afterwards: the liveness probe needs the same instance, and a recreated container
is a different one, so an orphan stays open until the 90-day prune. For those 90 days the reaper
handed the dead ids to the sweep, which protects the whole subtree under each — and
`<slug>/<id>/subagents/**` is, in the walk's own words, the bulk of the population. Three or four
orphaned swarm sessions exceed the entire 65,536-byte budget on their own: the sweep reclaims
nothing, reports a floor it says "lapses" when this share never does, and every Bash call goes on
dying at spawn. **Reached by the recovery step this document tells the operator to run.**

The keep list is a liveness claim now, not an absence-of-an-end-record: a row must have been *seen*
within six of the registry's own refresh windows (`SEEN_REFRESH_MS * 6`, derived rather than picked —
a row refreshes at most hourly, so six hours of silence is not evidence of life). Over-keeping is
only the safe direction *while it lapses*. The decision is a pure `live_ids(rows, now)` so the rule
whose first version caused this is drivable from literals, including at the cutoff itself.

**And `verify.sh` measured a different tree from the one the sweep had just acted on.** It runs the
sweep's `--dry-run` to report the budget, and `run.sh` passed it no keep list — so a container held
down by live sessions came out `over` (exit 1) instead of `beyond` (exit 3), with a FAIL naming the
floor, an unwritable archive and colliding destinations, none of which applied, while the real cause
— a live session holding its whole subagent subtree, precisely what the keep list exists for — was
not among them. Reproduced at 120 subagent transcripts under one live session plus 40 archivable,
budget 30,000: `beyond` with the list, `over` without it. The verify exec carries the same `-e` now.

**A claim this record repeated four times was simply false.** "exit 1 refuses a window, exit 3 does
not" — `run.sh` refuses on **any** non-zero verify (run.sh's `verify_rc -ne 0` gate). Exit 3 changes *which refusal*
the operator reads, which for the transcript floor is the difference between one remedy that applies
and three that do not. That is still worth a classifier and its guards; it is not what was written
down, and the overstatement was the stated justification for them.

Two guards were pinning a spelling against nothing:

- The fixture's no-real-container pin named `JKB_CONTAINER_NAME` as a literal compared to nothing in
  `transcripts.rs`. Rename `CONTAINER_NAME_VAR` and `dev_container_name()`'s test follows the
  constant, `assert_jkb_isolated` compares the stale literal it set itself against the stale literal
  it expects, and `check-config.sh` compares only the default *value* — every guard green while
  `cargo test` goes back to archiving the developer's live transcripts. The fixture module is
  compiled into the bin's test build, so the two spellings are now one assertion.
- The live-session guard pinned only that `-e "JKB_KEEP_SESSIONS=` appears on the sweep line. Delete
  the six lines that *fill* it and the flag passes an empty string for ever, with the guard
  reporting a protection that no longer exists — reproduced, and check-config's output was
  byte-identical to the unmutated tree's. The derivation is pinned now, and so is the JSON field
  `jq` read, held to `ClaudeSession`'s serde name the way `HELD_NAME` is held to `swarm-status.sh`.
  **That guard lasted one commit** and went with the `jq` it was written for — the next change
  replaced the whole derivation with `jkb notify sessions --live-ids`, so there is no JSON field to
  agree about any more and what is pinned is that `run.sh` asks `jkb` at all. The diagnosis is worth
  keeping even though the guard is gone, because it is this branch's most repeated defect in
  miniature: `.[].session` matched `.[].session_id` as a substring, so the rename mutation went
  MISSED until the closing quote joined the needle.
  Writing that guard reproduced this branch's most-repeated bug in miniature: `.[].session` matched
  `.[].session_id` as a substring, so the rename mutation went MISSED until the closing quote joined
  the needle — the same shape as `accept_bad` satisfying a search for `bad`.

**The keep list had two implementations, and they diverged within one round.** The reaper read the
registry from the database; `run.sh` asked the daemon for `--json` and pulled `.[].session` out with
`jq`. Two answers to one question — *which sessions count* — in two languages, with nothing comparing
them. So when the reaper learned that an unclosable row is not a live session, `run.sh` went on
handing out ids from rows that had been open for months, which is precisely the state that makes the
sweep reclaim nothing. The fix above was made once and needed making twice, and that is the whole
argument against this shape.

`jkb notify sessions --live-ids` applies [`transcripts::live_ids`] and prints one id per line. Both
triggers call it; the rule lives once. It also deletes two couplings that existed only because the
rule was duplicated — `run.sh` no longer needs `jq`, and `check-config.sh` no longer has to hold a
JSON field name to `ClaudeSession`'s serde spelling. Access paths still differ (the reaper reads the
database, the host asks the daemon) and that is fine: what must not differ is the rule.

And the two empty lists are now distinguishable. *No live sessions* and *could not ask the daemon*
both produced an empty `sweep_keep`, so a sweep that ran with **no protection at all** looked in the
scroll-back exactly like one that had nothing to protect. `run.sh` says which happened, and
`check-config.sh` pins that it does.

Three smaller ones from the same round: the budget-seam refusal scanned `mutate-verify.sh` — the one
file this record designates for closing the known behavioural gap, which means staging a budget in it
— and refused it with a message that is false about that file; the header's `~30 fixed security
paths` was the figure the budget comment seventy lines below already re-measures at 89, left where a
reader meets it first; and `dev_container_name`'s test called `set_var` in a binary whose other tests
fork `git` concurrently, which is the one rule this crate wrote down for itself. The decision is a
pure `chosen_container_name(Option<String>)` now, driven by values.

Finally, `--json` got the shape its neighbour already had. The tick printed prose to stderr under
`--json` while the queue compaction beside it printed a document to stdout, so a machine consumer of
the reaper recorded a compaction and never a sweep, never an over-budget container, and never a
daemon it could not reach.

**Round 12 — the keep list was inverted, not merely truncated.** `live_sessions` took one page of
`claude_session::list` and dropped `next`. That listing is ordered **`seen_at` ascending** — least
recently seen first, which is what a liveness sweep wants to probe — so the first page is the
*oldest* rows, which is exactly the set the new recency filter discards, while the session running
right now has the largest `seen_at` and sits on the last page. Past `LIST_CAP` the keep list
therefore did not shrink; it emptied, and the tick swept a live container with no protection at all
and printed nothing, because `Quiet` is silent. Two other readers of this API already page, and one
carries a comment about this identical defect being found here before. It pages to exhaustion now.

The two fixes of round 11 combined to produce that: the recency filter is what turns "oldest first"
from a harmless ordering into an inversion. Neither was wrong alone.

**And the recovery path was made worse before it was made better.** Nothing closes a session's row
when its container is stopped or removed — no `SessionEnd` hook fires — so after `run.sh --rm`, every
session from the destroyed container still looks recently seen. `run.sh` passed those ids to the next
start, and each holds its whole `<slug>/<id>/subagents/**` subtree: three or four of them exceed the
entire budget, so the documented E2BIG recovery reclaimed nothing for up to six hours, where before
this trigger carried any list at all it recovered in one pass. The fix is not a heuristic — a
container this script just created, or just started from stopped, has **no sessions inside it**, so
the correct keep list there is empty and the registry is asked only when `$state` is `running`.

Three more of the same family — a rule applied to some of its instances:

- `NOTHING_TO_DO` listed two of the sweep's three quiet exits, omitting `does not exist — nothing to
  sweep`. `Said` is printed unconditionally (only failures are deduped), so a container whose
  transcript root is absent logged one identical line every quarter of an hour for ever — the noise
  the standing-key dedup exists to prevent, on the one path it did not cover. The count of the
  sweep's success exits is pinned now, so a fourth is a red gate rather than a new log line.
- Neither installed unit carried `JKB_CONTAINER_NAME`. A launchd agent gets only what
  `EnvironmentVariables` lists and a `systemd --user` unit gets the user manager's environment, so
  the operator override `dev_container_name`'s doc promises worked for `run.sh` and for the test
  fixture and **never for the reaper** — which resolved `jkb-dev` for ever and reported the miss by
  saying nothing. Captured at install time now, like the database path beside it.
- The `--live-ids` verb was a literal in `run.sh` and a separate literal in the clap derive, compared
  only by a grep over `run.sh`, and invoked by no test at all. Breaking it left every gate green while
  the next start took the "could not ask jkb" branch and swept a live container with an empty list —
  a benign-looking message over a dead protection. The existing daemon fixture exercises it now.

And `jkb notify sessions` printed "live" for rows `--live-ids` on the same command excludes, with
nothing in the output to tell them apart — so the listing an operator checks after a transcript is
archived contradicted the keep list. It shows `open, unseen Nh` past the threshold.
