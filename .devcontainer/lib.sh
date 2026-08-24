#!/usr/bin/env bash
# Shared by verify.sh (inside the container) and check-config.sh (on the host, in ./scripts/
# check.sh). It exists for one reason: the mount boundary must be READ THE SAME WAY by the runtime
# check and the gate that reviews it. check-config.sh's stated job is "the derivation verify.sh
# depends on still yields the right mounts", and it was doing that against its own verbatim copy
# of the jq — so it would have gone on passing while verify.sh's copy was broken or deleted.
#
# Sourced, never executed: no `set -e` here, so a caller's own shell options are left alone.

# devcontainer.json permits // comments; strip them the way the spec's parsers do.
dc_strip() { sed 's://.*$::' "$1"; }

# Every mount point the container declares, one per line, sorted.
#
# The devcontainer spec allows a mount as either a comma-separated string or an object, and the
# two are interchangeable at any time, so both are read — handling only strings would turn a
# purely cosmetic edit of devcontainer.json into a container that cannot verify itself.
dc_mount_targets() { # dc_mount_targets <devcontainer.json>
    dc_strip "$1" 2>/dev/null \
      | jq -r '[(.workspaceMount // empty)] + (.mounts // [])
               | .[]
               | if type == "string" then capture("target=(?<t>[^,]+)").t else .target end' \
        2>/dev/null | sort -u
}

# `<source>|<type>` for every declared mount, so a caller can tell a host bind (which reaches the
# host filesystem and must be reviewed) from a named volume (which cannot).
dc_mount_sources() { # dc_mount_sources <devcontainer.json>
    dc_strip "$1" 2>/dev/null \
      | jq -r '[(.workspaceMount // empty)] + (.mounts // [])
               | .[]
               | if type == "string"
                 then (capture("source=(?<s>[^,]+)").s + "|" + ((capture("type=(?<t>[^,]+)") // {t:"bind"}).t))
                 else ((.source // "") + "|" + (.type // "bind")) end' \
        2>/dev/null
}

# The declared type for one target, or empty if it is not declared.
dc_type_for_target() { # dc_type_for_target <devcontainer.json> <target>
    dc_strip "$1" 2>/dev/null \
      | jq -r --arg want "$2" '[(.workspaceMount // empty)] + (.mounts // [])
               | .[]
               | if type == "string"
                 then {t: capture("target=(?<t>[^,]+)").t, k: ((capture("type=(?<k>[^,]+)") // {k:"bind"}).k)}
                 else {t: (.target // ""), k: (.type // "bind")} end
               | select(.t == $want) | .k' \
        2>/dev/null | head -1
}

# Link Claude Code's state out of ~/.claude into the .claude-state volume, so sessions, memory and
# the login survive a rebuild without anything of the host's being mounted in.
#
# Shared because verify.sh ASSERTS the result: a harness that runs verify.sh without doing this
# would fail for a reason that is not the property under test, and the obvious repair — teaching
# the harness its own copy of the loop — is the duplication this file exists to remove.
dc_link_state() { # dc_link_state [home]
    local h="${1:-/home/vscode}"
    mkdir -p "$h/.claude-state" "$h/.claude" || return 1
    local d
    for d in projects sessions history file-history shell-snapshots todos statsig; do
        mkdir -p "$h/.claude-state/$d"
        ln -sfn "$h/.claude-state/$d" "$h/.claude/$d" 2>/dev/null || true
    done
    # The two whole-file pieces of login state, linked while still dangling: Claude Code creates
    # each on first write and follows the symlink into the volume.
    ln -sfn "$h/.claude-state/.credentials.json" "$h/.claude/.credentials.json" 2>/dev/null || true
    ln -sfn "$h/.claude-state/claude.json"       "$h/.claude.json"              2>/dev/null || true
}
