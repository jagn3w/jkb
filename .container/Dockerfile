# Dev container for jkb with Claude Code running unattended (design D49).
#
# TWO LAYERS, NESTED. The container gives kernel-enforced default-deny FILE ACCESS — a host path
# that is not mounted does not exist in here, so Claude Code's in-process tools (Read/Glob/Grep/
# Edit) are bounded by the mount namespace rather than by permission rules, which is the one
# property the rule model cannot express. Claude Code's OWN sandbox then runs nested inside it,
# giving per-command confinement and the egress allowlist. .container/README.md has the
# measurements behind each choice.
#
# NON-ROOT IS LOAD-BEARING, not hygiene. Measured: a root process inside a container cannot create
# a mount/net/pid namespace directly even with seccomp disabled, so bubblewrap — and therefore the
# nested sandbox — works only as a normal user.
FROM mcr.microsoft.com/devcontainers/base:ubuntu-24.04

# --- root: everything that needs privilege, before we drop it -------------------------------
# bubblewrap + socat are what Claude Code's Linux sandbox shells out to; its own dependency error
# names exactly those two. iptables/ipset/dnsutils are for the egress firewall, which is the layer
# that survives if the nested sandbox ever fails to start.
# THE REAPER, SPELLED ONCE. entrypoint.sh execs this to become a PID 1 that reaps; without it
# `sleep infinity` is PID 1, never wait()s, and every orphan reparented to it is a zombie for ever
# (README.md, "The measurements this is built on"). An ENV rather than a literal in two files
# because it is visible BOTH to the `test -x` below, which fails the build if the package stops
# putting it here, AND to the entrypoint process and every `docker exec`, since ENV persists into
# the image config. Two spellings needed a guard to keep them in step; one needs nothing, which is
# what this directory did for /run/jkb-egress-verdict once egress-lib.sh single-sourced it.
ENV JKB_REAPER=/usr/bin/tini

# LINK WITH LLD, NOT GNU ld. Measured 2026-10-01 in this container on arm64, touching
# crates/jkb-cli/tests/sessions.rs and rebuilding `cargo test -p jkb-cli --test sessions --no-run`,
# four alternating runs per linker, best of each: GNU ld 21.2s wall / 7.8s CPU, lld 14.9s wall /
# 6.4s CPU. Taken with two other Claude sessions running, so CPU time is the more reliable figure.
# The lld measured was the toolchain's rust-lld 22.1.2 through its gcc-ld shim; what ships is
# Ubuntu's `lld`, because the shim's path names the toolchain version and an ENV pointing at it
# would break silently at the next rust-toolchain.toml bump. Both triples, because this image is
# built for amd64 too and a flag for the other triple applies to nothing, silently (on x86_64,
# rustc already defaults to its own lld; this makes the container say so rather than rely on it).
# The flag and the package are here together, beside the build-time check below, because a flag
# whose linker is absent fails every link in the container. verify.sh asserts the RESULT — a binary
# cargo links in here carries LLD's .comment stamp — because RUSTFLAGS in a session overrides
# these per-target flags and quietly reverts to GNU ld.
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-fuse-ld=lld" \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-fuse-ld=lld"

RUN apt-get update -qq \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
        bubblewrap socat ripgrep jq tini lld \
        build-essential pkg-config libssl-dev ca-certificates curl git \
        iptables ipset dnsutils \
    && curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq nodejs \
    && corepack enable \
    # If the package ever stops putting it where JKB_REAPER says, the container would fail to
    # start at all, with the cause one exec deep -- so it fails the BUILD instead, the way the
    # sudoers `visudo -c` check does.
    && test -x "$JKB_REAPER" \
    # ...and the same for the linker the CARGO_TARGET_*_RUSTFLAGS above name: gcc's -fuse-ld=lld
    # looks for `ld.lld` on PATH, so a package that stops providing it fails the build here rather
    # than every link in the container.
    && ld.lld --version \
    && rm -rf /var/lib/apt/lists/*

# --- sibling-repo toolchain: Ruby + PostgreSQL (for ~/repos/contextual-translate) ----------------
# This container serves every repo under ~/repos, and nothing can be installed into it at runtime:
# root is unreachable and egress is allowlisted. So a sibling's toolchain is baked in HERE, where
# the build has ordinary egress. README.md, "Sibling-repo toolchains", has how to use it.
#
# ON BY DEFAULT, OFF IN CI. `run.sh --build` takes the default, so the container you attach to has
# it. CI passes JKB_WITH_RUBY=0: a from-source Ruby compile and two more build-time hosts
# (github.com/rbenv, cache.ruby-lang.org) are a cost and an outage surface that jkb's own container
# job gains nothing from. Nothing the CI job verifies is touched by this layer: it adds root-owned
# files under /opt and root-owned links in /usr/local/bin, and changes no directory's ownership.
#
# PINNED, not "latest in the series": a cached layer's RUN text never changes, so a floating
# version would give a Mac that built last month a different Ruby from a fresh build today, and
# nothing would record which. Bundler is Ruby's own default gem, so the Ruby pin pins it too. Rails
# is deliberately NOT installed globally: a project's Gemfile.lock owns its gem versions, including
# the one `rails new` runs from (README.md has the bootstrap).
#
# Three placement choices, one reason each:
#  - ROOT-OWNED under /opt: the agent can run the interpreter but not replace it. Gems go in
#    BUNDLE_PATH (below), never in /opt.
#  - OUTSIDE $HOME: the nested sandbox denies reads of ~ except an allowlist, so a Ruby under
#    ~/.local/share/mise (the usual mise layout) could not even be executed from the agent's Bash.
#  - LINKED INTO /usr/local/bin rather than put on ENV PATH: a Claude Code session can inherit a
#    PATH that never saw this image's ENV (observed 2026-09-18: a session whose PATH was the Mac
#    host's, while every other image ENV, JKB_NS_MARKER included, did arrive), and /usr/local/bin
#    is on every PATH, that one included. Only the named executables are linked; a project's own
#    come from `bundle exec` or its binstubs.
#    CAUSE FOUND 2026-10-02, corrected here rather than rewritten: this was recorded as unexplained
#    harness behaviour, and it was not. The repo's .claude/settings.local.json carried an
#    `"env": {"PATH": "/Users/<user>/.cargo/bin:..."}` block written for the Mac. Claude Code puts
#    settings `env` into every session, and that file is in the checkout the host shares, so the
#    Mac's PATH replaced this image's in container sessions. Only PATH was replaced because only PATH
#    was in that block, which is why every other ENV arrived. Deleting the block restored the
#    image's PATH (measured by another session). verify.sh and check-config.sh now refuse a settings
#    `env` that names any variable this image or containerEnv sets. The linking still stands: it
#    costs nothing and survives a PATH replaced by any other route.
#
# The distro's `postgresql` would create a root-managed `main` cluster that nothing here can start
# (starting a service needs root), so it is suppressed and each user runs their own with pg_ctl.
# `install -d`, not `mkdir -p`: check-config.sh reads every `RUN mkdir -p` as a volume pre-create.
ARG JKB_WITH_RUBY=1
ARG JKB_RUBY_VERSION=3.4.10
ARG JKB_RUBY_BUILD_TAG=v20260917
RUN if [ "$JKB_WITH_RUBY" != 1 ]; then echo "JKB_WITH_RUBY=$JKB_WITH_RUBY: no Ruby/Postgres layer"; exit 0; fi \
    && install -d /etc/postgresql-common/createcluster.d \
    && printf 'create_main_cluster = false\n' > /etc/postgresql-common/createcluster.d/no-main.conf \
    && apt-get update -qq \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
        autoconf bison libyaml-dev libpq-dev libffi-dev zlib1g-dev libreadline-dev libgmp-dev \
        libjemalloc2 postgresql postgresql-client \
    && git clone --quiet --depth 1 --branch "$JKB_RUBY_BUILD_TAG" \
         https://github.com/rbenv/ruby-build.git /tmp/ruby-build \
    && /tmp/ruby-build/bin/ruby-build "$JKB_RUBY_VERSION" "/opt/ruby-$JKB_RUBY_VERSION" \
    && ln -s "/opt/ruby-$JKB_RUBY_VERSION" /opt/ruby \
    # `ln -s` without -f: a name that already exists in /usr/local/bin fails the build rather than
    # silently shadowing whatever was there.
    && for exe in ruby gem bundle bundler irb; do ln -s "/opt/ruby/bin/$exe" /usr/local/bin/; done \
    && pg_bin="$(ls -d /usr/lib/postgresql/*/bin | sort -V | tail -n 1)" \
    && ln -s "$pg_bin/pg_ctl" "$pg_bin/initdb" "$pg_bin/postgres" /usr/local/bin/ \
    && /usr/local/bin/ruby -v && /usr/local/bin/bundle -v && /usr/local/bin/pg_ctl --version \
    && rm -rf /tmp/ruby-build /var/lib/apt/lists/*

# WHERE GEMS GO: container-local, never the project directory. ~/repos is a bind mount from the
# Mac, so a `vendor/bundle` there would put Linux-built native extensions (pg, bootsnap) and a
# `.bundle/config` where the host's own `bundle` reads them -- the same reason CARGO_TARGET_DIR
# keeps target/ off the bind. ~/.cache is in the sandbox posture's allowRead AND allowWrite, so the
# agent's Bash can install there. BUNDLE_USER_HOME moves Bundler's own cache and config off ~, which
# the sandbox cannot read. Image ENV does reach sessions (only PATH was seen replaced). Not a volume:
# a rebuild re-runs `bundle install`, which is minutes, against a lockfile, so nothing is lost.
ENV BUNDLE_PATH=/home/vscode/.cache/bundle \
    BUNDLE_USER_HOME=/home/vscode/.cache/bundle-home

# The firewall script is installed INTO THE IMAGE, root-owned, and sudoers grants exactly that one
# path. It must never be run from the mounted workspace: a script the agent can edit, that the
# agent can also sudo, is a root shell with extra steps.
COPY --chown=root:root init-firewall.sh /usr/local/bin/init-firewall.sh
# The entrypoint is root-owned for the same reason and by the same argument: it is what raises the
# firewall on every start, so an agent able to rewrite it could simply not raise it.
COPY --chown=root:root entrypoint.sh /usr/local/bin/entrypoint.sh
# The live-chain probe and the library both scripts source. Root-owned by the same argument: the
# probe is what the boot decision is made on, and the library is what the raise sources as root —
# either one writable by `vscode` would be a root shell with extra steps. The library is sourced
# from `$(dirname "$0")`, which is why it sits in /usr/local/bin beside its two callers rather than
# in a lib directory: one install location, resolving the same way here and in the repo.
COPY --chown=root:root egress-status.sh /usr/local/bin/egress-status.sh
COPY --chown=root:root egress-lib.sh    /usr/local/bin/egress-lib.sh
RUN chown root:root /usr/local /usr/local/bin /usr/local/share /usr/local/bin/init-firewall.sh \
                    /usr/local/bin/entrypoint.sh /usr/local/bin/egress-status.sh \
                    /usr/local/bin/egress-lib.sh \
    # Owned explicitly, not inherited. Everything here rests on `vscode` being unable to replace
    # the script sudo runs as root, or the allowlist snapshot beside it — and unlink/replace is
    # governed by the DIRECTORY, not the file mode. Debian-derived images have historically
    # shipped /usr/local as `root:staff` group-writable, the base tag floats, and a feature could
    # add the user to a group at any time. Same shape as the blanket-sudo grant: an inherited
    # property nothing in this repo stated. verify.sh asserts the result at runtime.
    && chmod 0755 /usr/local /usr/local/bin /usr/local/share \
    && chmod 0755 /usr/local/bin/init-firewall.sh /usr/local/bin/entrypoint.sh \
                  /usr/local/bin/egress-status.sh \
    # IN THE IMAGE it is only ever sourced, so it needs no exec bit, and not having one is a small
    # statement that it is never a command anybody runs as root by name. (In the repo it does carry
    # one, because ./scripts/check.sh runs its --self-test directly.)
    && chmod 0644 /usr/local/bin/egress-lib.sh \
    && printf 'vscode ALL=(root) NOPASSWD: /usr/local/bin/init-firewall.sh ""\n' > /etc/sudoers.d/init-firewall \
    && chmod 0440 /etc/sudoers.d/init-firewall \
    # THE SECOND GRANT, and it is read-only (D51.1). The boot decision asks the kernel what the
    # filter chains hold, and only root can read them — so the entrypoint (which runs as `vscode`)
    # needs this. egress-status.sh only inspects and prints: it must never install, flush or modify
    # a rule, so a grant to run it is not a grant to change the boundary. Pinned to NO ARGUMENTS by
    # the same rule as the firewall's, because a command naming no argument accepts every argument.
    && printf 'vscode ALL=(root) NOPASSWD: /usr/local/bin/egress-status.sh ""\n' > /etc/sudoers.d/egress-status \
    && chmod 0440 /etc/sudoers.d/egress-status \
    # ...and the grant above means nothing while THIS file exists. The devcontainers base image
    # ships /etc/sudoers.d/vscode containing `vscode ALL=(root) NOPASSWD:ALL`, so the agent could
    # already run anything as root: flush the firewall, delete the allowlist snapshot, rewrite the
    # script above, or just `sudo -i`. Every root-ownership protection in this design — and the
    # sentence three lines up about a root shell with extra steps — was decorative until this line.
    # Found by running `sudo -n -l` inside the container; nothing static would have shown it.
    && rm -f /etc/sudoers.d/vscode \
    # A malformed sudoers file locks sudo out entirely, which would break the firewall at every
    # start. Fail the BUILD instead.
    && visudo -c >/dev/null

# THE NAMESPACE-IDENTITY MARKER, spelled once for the same reason JKB_REAPER is. entrypoint.sh
# records the container's own pid and mount namespace ids here as its last act before handing over,
# and verify.sh compares its OWN against them -- which is how that file tells "this container" from
# a nested sandbox whose fresh /proc makes every assertion answer about bwrap instead. An ENV
# because the path is needed at BUILD (the directory below must exist and be owned by the user who
# writes it) and by TWO processes at runtime, and ENV persists into the image config so it reaches
# both, `docker exec` included. A literal in three files is the duplication this directory deletes
# rather than guards (README.md, "The reaper path is single-sourced").
ENV JKB_NS_MARKER=/run/jkb/ns

# Created root-owned and handed to vscode, because entrypoint.sh runs as vscode and /run is root's.
# A named volume whose mount point does not exist in the image is created root-owned (D49); the
# same rule applies to a directory a non-root process must write.
#
# THE MARKER IS AGENT-WRITABLE, AND THAT IS STATED RATHER THAN DESIGNED AWAY. vscode owns it, so a
# session inside can forge it. Forging it can only produce a REFUSAL for anyone genuinely in the
# container's namespaces (their ids will not match the forged ones); the only false pass it buys is
# for a run inside the same sandbox as the forger -- an agent lying to its own verifier, which no
# file ownership prevents. Routing it through the sudoers-granted root scripts would add a third
# root-writable path to protect a fact ownership cannot protect anyway.
RUN mkdir -p "$(dirname "$JKB_NS_MARKER")" && chown vscode:vscode "$(dirname "$JKB_NS_MARKER")"

# THE HARNESS'S OWN RULES, WHERE THE MODEL CANNOT REACH THEM (design D52.9, P1). Claude Code reads
# managed settings from this root-owned path, and nothing a session writes can override them. They
# add, for every session in this container: the attestation hook that tells jkb serve which agent
# made each `jkb` call, the workflow Stop hook, the transcript hook below (which keeps every tool
# away from other sessions' transcripts -- a live tool call's ticket is written there), and deny
# rules keeping the model's in-process tools (which the Bash sandbox does not confine) away from
# the container credential and from the files that configure the harness itself -- a hook, an
# agent definition or an MCP server it could add would run unsandboxed.
COPY --chown=root:root managed-settings.json /etc/claude-code/managed-settings.json
RUN chmod 0644 /etc/claude-code/managed-settings.json
# The transcript deny is a HOOK, not a permissions rule; .container/README.md ("The transcript deny
# is a hook") has why and the measurements. Root-owned and 0755 for the same reason
# pin-jkb-hook.sh is: it is the thing that confines the agent, so the agent must not rewrite it.
COPY --chown=root:root deny-transcripts.sh /usr/local/bin/deny-transcripts.sh
RUN chmod 0755 /usr/local/bin/deny-transcripts.sh
# The binary the managed hooks run, root-owned, where nothing in the sandbox can replace it (D52.9):
# pin-jkb-hook.sh copies setup's build there, run through sudo — which a sandboxed tool call cannot.
COPY --chown=root:root pin-jkb-hook.sh /usr/local/bin/pin-jkb-hook.sh
RUN chmod 0755 /usr/local/bin/pin-jkb-hook.sh \
    && install -d -o root -g root -m 0755 /usr/local/lib/jkb-hook \
    && printf 'vscode ALL=(root) NOPASSWD: /usr/local/bin/pin-jkb-hook.sh ""\n' > /etc/sudoers.d/pin-jkb-hook \
    && chmod 0440 /etc/sudoers.d/pin-jkb-hook \
    && visudo -c >/dev/null

# The mount points of the narrowed ~/.jkb binds and of the credential (D52.8), owned by the container
# user: docker creates a missing mount point root-owned, and ~/.jkb itself is no longer a bind — the
# operator's database and root token do not exist in here at all.
RUN mkdir -p /home/vscode/.jkb/claude-memory /home/vscode/.jkb/logs /home/vscode/.jkb-container \
    && chown -R vscode:vscode /home/vscode/.jkb /home/vscode/.jkb-container

# --- non-root from here on ------------------------------------------------------------------
USER vscode
ENV PATH=/home/vscode/.local/bin:/home/vscode/.cargo/bin:$PATH

# `--default-toolchain none` so rustup installs whatever rust-toolchain.toml pins on first use,
# rather than baking a version here that will silently drift from the file.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --no-modify-path --default-toolchain none

RUN curl -fsSL https://claude.ai/install.sh | bash

# Every path that receives a named VOLUME must exist in the image first, owned by this user.
# Docker seeds a fresh volume from the image directory at that path — and when there is no such
# directory it creates the volume root-owned, so the container user cannot write it and the first
# build dies with EACCES several minutes in. Cheap to create, expensive to debug.
RUN mkdir -p /home/vscode/.cargo/target \
             /home/vscode/.cargo/registry \
             /home/vscode/.claude-state \
             /home/vscode/.claude

# --- VS Code extensions, fetched HERE rather than at connect time ----------------------------
# A build has ordinary egress; the running container does not. Measured on a real "Reopen in
# Container": VS Code starts installing extensions ~1s before setup.sh raises the egress firewall,
# the downloads land just after it is up, and both failed with ECONNREFUSED — non-fatally, so the
# container came up with neither installed and nothing gated on it. Fetching at build time needs
# no widening of the posture's allowedDomains. setup.sh installs these from disk; verify.sh
# asserts the result. See fetch-extensions.sh for why the CDN was not allowlisted instead.
#
# Deliberately the LAST thing in the file: it copies container.json, which changes often, and
# nothing above it should be rebuilt when it does.
COPY --chown=vscode:vscode lib.sh container.json fetch-extensions.sh /home/vscode/.container-build/
RUN /home/vscode/.container-build/fetch-extensions.sh /home/vscode/.vsix

# Where an attached VS Code terminal lands by default. `/workspaces` was Dev Containers' own
# convention and nothing puts anything there now.
WORKDIR /home/vscode/repos

# The egress firewall is raised HERE, by the container itself, so that `docker start` and Docker
# Desktop's start button get it too — not only `run.sh`. See entrypoint.sh for why the two failure
# modes are answered differently.
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
