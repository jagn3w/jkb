#!/usr/bin/env bash
# Shared by verify.sh (inside the container) and check-config.sh (on the host, in ./scripts/
# check.sh). It exists for one reason: the mount boundary must be READ THE SAME WAY by the runtime
# check and the gate that reviews it. check-config.sh's stated job is "the derivation verify.sh
# depends on still yields the right mounts", and it was doing that against its own verbatim copy
# of the jq — so it would have gone on passing while verify.sh's copy was broken or deleted.
#
# Sourced, never executed: no `set -e` here, so a caller's own shell options are left alone.

# container.json permits // comments; strip them the way the spec's parsers do.
dc_strip() { sed 's://.*$::' "$1"; }

# Every mount point the container declares, one per line, sorted.
#
# The devcontainer spec allows a mount as either a comma-separated string or an object, and the
# two are interchangeable at any time, so both are read — handling only strings would turn a
# purely cosmetic edit of container.json into a container that cannot verify itself.
dc_mount_targets() { # dc_mount_targets <container.json>
    dc_strip "$1" 2>/dev/null \
      | jq -r '(.mounts // [])
               | .[]
               | if type == "string" then capture("target=(?<t>[^,]+)").t else .target end' \
        2>/dev/null | sort -u
}

# `<source>|<type>` for every declared mount, so a caller can tell a host bind (which reaches the
# host filesystem and must be reviewed) from a named volume (which cannot).
dc_mount_sources() { # dc_mount_sources <container.json>
    dc_strip "$1" 2>/dev/null \
      | jq -r '(.mounts // [])
               | .[]
               | if type == "string"
                 then (capture("source=(?<s>[^,]+)").s + "|" + ((capture("type=(?<t>[^,]+)") // {t:"bind"}).t))
                 else ((.source // "") + "|" + (.type // "bind")) end' \
        2>/dev/null
}

# Every declared mount as ONE docker `--mount` spec per line. The string form already IS that
# syntax (comma-separated key=value), so it passes through; the object form is joined into it.
#
# It lives here beside the other three readers of `.mounts` rather than in run.sh, which is the
# only caller: all four have to agree about the string-vs-object spelling the spec allows at any
# time, and a fourth private copy of that rule is how the thing that APPLIES the mount list comes
# to disagree with the thing that VERIFIES it.
# The declaration's human name, and the Dockerfile it names. Both exist so that `name` and `build`
# are keys something actually READS — they were listed as consumed while nothing looked at either,
# which made "every key in container.json is applied by run.sh" a true sentence about two inert
# declarations. Each falls back to what run.sh used to hard-code, so a file omitting them behaves
# exactly as before rather than breaking.
dc_name() { # dc_name <container.json>
    local n; n="$(dc_strip "$1" 2>/dev/null | jq -r '.name // empty' 2>/dev/null)"
    printf '%s' "${n:-jkb dev container}"
}

dc_dockerfile() { # dc_dockerfile <container.json>
    local f; f="$(dc_strip "$1" 2>/dev/null | jq -r '.build.dockerfile // empty' 2>/dev/null)"
    printf '%s' "${f:-Dockerfile}"
}

dc_mount_specs() { # dc_mount_specs <container.json>
    dc_strip "$1" 2>/dev/null \
      | jq -r '(.mounts // [])[]
               | if type == "string" then .
                 else ([to_entries[] | "\(.key)=\(.value)"] | join(",")) end' \
        2>/dev/null
}

# The declared type for one target, or empty if it is not declared.
dc_type_for_target() { # dc_type_for_target <container.json> <target>
    dc_strip "$1" 2>/dev/null \
      | jq -r --arg want "$2" '(.mounts // [])
               | .[]
               | if type == "string"
                 then {t: capture("target=(?<t>[^,]+)").t, k: ((capture("type=(?<k>[^,]+)") // {k:"bind"}).k)}
                 else {t: (.target // ""), k: (.type // "bind")} end
               | select(.t == $want) | .k' \
        2>/dev/null | head -1
}

# Every VS Code extension the container declares, one per line, exactly as written — normally
# `publisher.name@version`.
#
# ONE list, read four times: the Dockerfile fetches these at build time, setup.sh installs them
# from disk, check-config.sh asserts each is version-pinned, and verify.sh asserts each is
# actually present. Restating it in any of those would be the two-lists-that-must-agree defect
# that already went stale once in verify.sh's mount set.
dc_extensions() { # dc_extensions <container.json>
    dc_strip "$1" 2>/dev/null \
      | jq -r '(.customizations.vscode.extensions // [])[]' 2>/dev/null
}

# `<publisher>.<name>` and `<version>` for one declared entry, tab-separated; empty if it carries
# no `@version`. An unpinned entry is a hard error at every call site rather than a default,
# because the fallback for "no matching local VSIX" is a marketplace download, and that download
# is refused by the egress firewall — silently, as a non-fatal log line nothing gates on.
dc_extension_split() { # dc_extension_split <publisher.name@version>
    case "$1" in
        *@*) printf '%s\t%s\n' "${1%@*}" "${1##*@}" ;;
        *)   return 1 ;;
    esac
}

# The `publisher.name` of the extension this repo BUILDS ITSELF, or nothing when it has none.
# It is deliberately NOT in container.json: that list is what VS Code downloads from the
# marketplace, and this extension is not published there. Which is exactly why the jkb side panel
# was absent from every container ever built — nothing installed it, nothing declared it, and so
# nothing could assert it either. setup.sh builds and installs it; verify.sh asserts the result;
# both ask HERE, so neither can drift from the package that defines the id.
dc_local_extension() { # dc_local_extension <repo-root>
    local pkg="$1/ui/vscode/package.json" publisher name
    [ -f "$pkg" ] || return 1
    publisher="$(jq -r '.publisher // empty' "$pkg" 2>/dev/null)"
    name="$(jq -r '.name // empty' "$pkg" 2>/dev/null)"
    [ -n "$publisher" ] && [ -n "$name" ] || return 1
    printf '%s.%s\n' "$publisher" "$name"
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
        # `ln -sfn` REPLACES a regular file but silently declines a real directory, leaving that
        # state in the container layer to die with the next rebuild — and nothing noticed, because
        # only the two file links were asserted. Migrate anything already there into the volume
        # first, so the link can be made and no data is dropped to achieve it.
        if [ -d "$h/.claude/$d" ] && [ ! -L "$h/.claude/$d" ]; then
            # `.` glob so dotfiles come too; a failure here must not silently lose the directory.
            if ! (shopt -s dotglob nullglob 2>/dev/null || setopt dotglob 2>/dev/null || true
                  mv "$h/.claude/$d"/* "$h/.claude-state/$d"/ 2>/dev/null); then :; fi
            rmdir "$h/.claude/$d" 2>/dev/null || {
                echo "dc_link_state: $h/.claude/$d is a non-empty directory that could not be migrated;" >&2
                echo "  leaving it alone — this state will NOT survive a rebuild." >&2
                continue
            }
        fi
        ln -sfn "$h/.claude-state/$d" "$h/.claude/$d" 2>/dev/null || true
    done
    # The two whole-file pieces of login state, linked while still dangling: Claude Code creates
    # each on first write and follows the symlink into the volume.
    ln -sfn "$h/.claude-state/.credentials.json" "$h/.claude/.credentials.json" 2>/dev/null || true
    ln -sfn "$h/.claude-state/claude.json"       "$h/.claude.json"              2>/dev/null || true
}

# THE SETUP-COMPLETE MARKER, named once for the two scripts that use it (D52.5).
#
# setup.sh `touch`es it inside the container as its LAST act, so its presence means "setup
# finished" and not "setup started"; run.sh probes for it from the host to decide whether a
# container still needs setting up. Those are two different processes on two sides of a container
# boundary, which is why check-config.sh carried a guard comparing the two spellings, justified as
# "they cannot share a variable".
#
# They can: run.sh sources this file on the host, and setup.sh sources it inside the container from
# the same bind-mounted checkout. So there is one spelling and the guard goes with the duplication.
#
# In the writable layer deliberately, not a volume: `run.sh --rm` must genuinely redo setup, and a
# volume would carry the marker into a container that had never been set up.
JKB_SETUP_MARKER="/home/vscode/.jkb-container-setup-complete"

# EVERY PATH THE DOCKERFILE INSTALLS AS ROOT, derived from its own COPY lines (D52.5).
#
# verify.sh asserts at runtime that these cannot be replaced by the agent -- the sudoers grant runs
# one of them as root, so a writable copy of it is a root shell. That list was hand-written and
# named only init-firewall.sh, so the three scripts added since (entrypoint.sh, egress-status.sh
# and egress-lib.sh -- the last of which init-firewall.sh SOURCES as root) had no ownership check
# at all, while the Dockerfile's own comment claimed "verify.sh asserts the result at runtime".
#
# A list every new COPY has to be remembered into is the defect. Derived, a fourth installed script
# is covered by existing.
dc_root_installed() { # dc_root_installed <Dockerfile> -> one absolute container path per line
    grep -E '^COPY[[:space:]]+--chown=root:root[[:space:]]' "$1" 2>/dev/null \
        | awk '{print $NF}' | grep '^/' | sort -u
}

# THE APPARMOR PROFILE'S NAME, read out of the profile itself (D52.5). run.sh passes it to docker,
# mutate-verify.sh passes the same, verify.sh checks the profile in force against it, and ci.yml
# names it in its probe -- five spellings of one fact, and a mismatch means a container silently
# started under docker-default, which is precisely the state that made bubblewrap fail. The file
# that DECLARES the profile is the one place it cannot be wrong.
dc_apparmor_profile() { # dc_apparmor_profile <profile file> -> the declared profile name
    sed -n 's/^profile[[:space:]]\{1,\}\([A-Za-z0-9_.-]\{1,\}\)[[:space:]].*/\1/p' "$1" 2>/dev/null | head -1
}
