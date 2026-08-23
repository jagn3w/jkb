# Dev container for jkb with Claude Code running unattended (design D49).
#
# TWO LAYERS, NESTED. The container gives kernel-enforced default-deny FILE ACCESS — a host path
# that is not mounted does not exist in here, so Claude Code's in-process tools (Read/Glob/Grep/
# Edit) are bounded by the mount namespace rather than by permission rules, which is the one
# property the rule model cannot express. Claude Code's OWN sandbox then runs nested inside it,
# giving per-command confinement and the egress allowlist. .devcontainer/README.md has the
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
RUN apt-get update -qq \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
        bubblewrap socat ripgrep jq \
        build-essential pkg-config libssl-dev ca-certificates curl git \
        iptables ipset dnsutils \
    && curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq nodejs \
    && corepack enable \
    && rm -rf /var/lib/apt/lists/*

# The firewall script is installed INTO THE IMAGE, root-owned, and sudoers grants exactly that one
# path. It must never be run from the mounted workspace: a script the agent can edit, that the
# agent can also sudo, is a root shell with extra steps.
COPY --chown=root:root init-firewall.sh /usr/local/bin/init-firewall.sh
RUN chmod 0755 /usr/local/bin/init-firewall.sh \
    && printf 'vscode ALL=(root) NOPASSWD: /usr/local/bin/init-firewall.sh ""\n' > /etc/sudoers.d/init-firewall \
    && chmod 0440 /etc/sudoers.d/init-firewall \
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

WORKDIR /workspaces
