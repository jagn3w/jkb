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

# Dev Containers' variable syntax, with ONE deliberate difference: an unset ${localEnv:VAR} is a
# hard error here, where Dev Containers substitutes the empty string. That default is how
# `source=${localEnv:HOME}/repos` quietly becomes `source=/repos` — a different host directory,
# mounted into the container, with nothing to notice it. A boundary must not be able to move
# because a variable was not set.
#
# It RETURNS 1 rather than calling `die`, which is run.sh's and does not exist here: lib.sh is
# sourced by scripts with and without `set -e`, so a helper that exits would take a caller's shell
# down with it. run.sh wraps its calls with `|| die`.
dc_subst() { # dc_subst <string> <repo-root>
    local s="$1" root="$2" var val
    s="${s//\$\{localWorkspaceFolderBasename\}/$(basename "$root")}"
    s="${s//\$\{localWorkspaceFolder\}/$root}"
    while [[ "$s" =~ \$\{localEnv:([A-Za-z_][A-Za-z0-9_]*)\} ]]; do
        var="${BASH_REMATCH[1]}"
        if [ -z "${!var+set}" ]; then
            printf 'container.json references ${localEnv:%s}, which is not set\n' "$var" >&2
            return 1
        fi
        val="${!var}"
        s="${s//\$\{localEnv:$var\}/$val}"
    done
    printf '%s' "$s"
}

# THE DOCKER SECURITY FLAGS THE CONTAINER DECLARES, substituted, one argument per line (D52.6).
#
# container.json's `runArgs` is the declaration; run.sh has always derived from it, and
# mutate-verify.sh's HEALTHY re-typed it by hand. That is what let commit 8266a2b add
# `systempaths=unconfined` to the declaration while the harness went on starting — and certifying —
# a container without it, under a step named "The container is what it claims to be". It had
# silently omitted `--pids-limit 4096` since the day that was declared, which nobody had noticed at
# all.
#
# So the control's flags are READ from the declaration. `--user` comes with it (dc_remote_user)
# because it is the same kind of fact and run.sh already derives it: bubblewrap cannot create a
# namespace as root, so a harness running as a different user from the real container is not a
# control either. What mutate-verify.sh still spells by hand is only its own scratch binds, which
# are deliberately NOT container.json's mounts.
# EMPTY IS A REFUSAL, AND IT LIVES HERE rather than in a caller. A `while` loop that never runs its
# body exits 0, so an absent or unreadable `runArgs` used to leave this function reporting success
# with no output — and run.sh's `|| die` therefore could not fire, so the launcher that starts the
# container people attach to would build a `docker run` line with no seccomp profile, no
# `systempaths=unconfined`, no `NET_ADMIN` and no pid limit, and say nothing was wrong.
# mutate-verify.sh refused that state and run.sh did not, which is a rule two callers had to
# remember and one did. There is no legitimate empty here: `runArgs` IS this container's security
# configuration.
#
# The jq is read through `$( )` too, so "container.json does not parse" and "it declares no
# runArgs" arrive as different messages instead of as one empty stream — inside the very function
# the rule above is written over. (A malformed container.json also empties the mount and env
# readers, and there is still no single `jq empty` above `docker_args`' dispatch to catch all
# three at once; that is filed, not fixed here.)
dc_run_args() { # dc_run_args <container.json> <repo-root>  -> one docker argument per line
    local raw line sub
    [ -r "$1" ] || { printf 'container.json is not readable: %s\n' "$1" >&2; return 1; }
    raw="$(dc_strip "$1" | jq -r '(.runArgs // [])[]' 2>/dev/null)" \
        || { printf 'container.json does not parse: %s\n' "$1" >&2; return 1; }
    if [ -z "$raw" ]; then
        printf 'container.json declares no runArgs: %s\n' "$1" >&2
        return 1
    fi
    while IFS= read -r line; do
        [ -n "$line" ] || continue
        sub="$(dc_subst "$line" "$2")" || return 1
        printf '%s\n' "$sub"
    done <<<"$raw"
}

# The environment the container declares, substituted, one `KEY=VALUE` per line.
#
# It is on the DECLARATION side of the line that decides what a control container must copy. The
# control copies everything that is a property of the declaration — `runArgs`, `remoteUser`,
# `containerEnv` — and supplies its own only for what is a property of the HOST'S DATA, which is
# the mount sources: the real ~/repos and ~/.jkb become the harness's scratch binds. `containerEnv`
# names container paths, never host ones, so there is nothing about it for a harness to substitute.
#
# Unlike runArgs, empty is legitimate: a container may declare no environment. Unreadable is not.
dc_container_env() { # dc_container_env <container.json> <repo-root>  -> one KEY=VALUE per line
    local raw line sub
    [ -r "$1" ] || { printf 'container.json is not readable: %s\n' "$1" >&2; return 1; }
    raw="$(dc_strip "$1" | jq -r '(.containerEnv // {}) | to_entries[] | "\(.key)=\(.value)"' 2>/dev/null)" \
        || { printf 'container.json does not parse: %s\n' "$1" >&2; return 1; }
    [ -n "$raw" ] || return 0
    while IFS= read -r line; do
        [ -n "$line" ] || continue
        sub="$(dc_subst "$line" "$2")" || return 1
        printf '%s\n' "$sub"
    done <<<"$raw"
}

# READ A REFUSING PRODUCER THROUGH `$( )`, NEVER THROUGH `< <( )`. `dc_subst`, `dc_run_args`,
# `dc_container_env` and run.sh's `docker_args` all REFUSE — that is the whole point of the unset-${localEnv:…} error
# above — and bash discards a process substitution's exit status, so a refusal inside one kills
# only the subshell while the reading loop keeps whatever was emitted before it. These producers
# emit as they go, so what the caller is left holding is not nothing (which every caller checks
# for) but a TRUNCATED list: a container started without the mounts or the security flags it
# declares, a fingerprint taken over half a declaration, a control certified without the flag it
# was assembled to carry. Command substitution propagates the status; check-config.sh fails the
# gate on a `< <(` reading any of the three, so this is a rule the checker keeps rather than one
# each call site has to remember.

# The user the container runs as, or empty when it declares none.
dc_remote_user() { # dc_remote_user <container.json>
    # REFUSES IN ITS OWN RIGHT, like dc_container_env beside it. This was a bare pipe with both
    # errors suppressed, which refuses only because every caller happens to set `pipefail` -- and
    # THIS FILE SETS NO SHELL OPTIONS, so that is a rule every present and future caller has to
    # remember, in a reader whose empty output is a legitimate answer ("declares no remoteUser").
    # Sourced into a shell without pipefail it answered 0-with-no-output for a file it could not
    # read, i.e. "could not tell" spelled exactly like "declares nothing".
    #
    # (The claim first written here -- that verify.sh's refusal branch was already unreachable --
    # was WRONG: verify.sh sets pipefail too. What is true is that nothing in the reader made it
    # so, and the two siblings that do this explicitly are the ones to match.)
    [ -r "$1" ] || { printf 'container.json is not readable: %s\n' "$1" >&2; return 1; }
    local out
    out="$(dc_strip "$1" | jq -r '.remoteUser // empty' 2>/dev/null)" \
        || { printf 'container.json does not parse: %s\n' "$1" >&2; return 1; }
    printf '%s' "$out"
}

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
    # The two whole-file pieces of login state. Not fatal here, as the links above are not:
    # verify.sh reports what state they were left in.
    dc_persist_login "$h" || true
}

# THE LOGIN FILES, one statement of them: "<link in the home>:<file in the state volume>" per line.
# dc_persist_login makes them and verify.sh asserts them, so the two cannot disagree about which
# files a login is.
dc_login_files() { # dc_login_files [home]
    local h="${1:-/home/vscode}"
    printf '%s:%s\n' "$h/.claude/.credentials.json" "$h/.claude-state/.credentials.json" \
                     "$h/.claude.json"              "$h/.claude-state/claude.json"
}

# Put the login back in the state volume, and link it there again.
#
# THE LINK DOES NOT SURVIVE A LOGIN. Each file is linked while it still dangles, on the theory that
# Claude Code creates it on first write by following the link into the volume. For the credential
# file it does not: after an in-container login, verify.sh found a REGULAR FILE at
# ~/.claude/.credentials.json (observed 2026-09-18, Claude Code 2.1.276). The write replaced the
# link instead of following it, and the saver writes a temporary file first, so a rename over the
# link is the likely mechanism. Reading, by contrast, follows the link: `claude auth status`
# reported `loggedIn: true` through a symlinked credential file (measured the same day, with dummy
# credentials in a scratch CLAUDE_CONFIG_DIR). So every login and every token refresh moves the
# credentials out of the volume and into the container's writable layer, and a rebuild loses them.
#
# So this does not rely on the link surviving. It runs at the moments that matter, and moves a
# regular file found at a link site back into the volume. The file in the home is always the newer
# one: it exists only because something replaced the link after the volume copy was last written.
# setup.sh runs it through dc_link_state, run.sh runs it on every start and before `--stop` and
# `--rm`, so a rebuild through run.sh carries the newest token. The residual is stated in README.md.
#
# WHICH COPY IS NEWER is decided by whether this container has linked the file before, and that is
# recorded in DC_LOGIN_LINKED, a marker in ~/.claude, which is the writable layer, not the volume:
#   - marker PRESENT: a link was made here and something replaced it. That can only be a Claude Code
#     write, so the home file is newer, and it moves into the volume.
#   - marker ABSENT: nothing here has linked it yet (a fresh container at setup). A regular file now
#     came with the image or predates setup. If the volume already holds a copy, that copy is the
#     login carried from the last container, and it wins. The home file is set aside as
#     `<file>.pre-link` rather than deleted, because it could be a login made before setup
#     finished. Only when the volume has no copy does the home file move in.
# "The home file is always newer" was the first version of this rule. It is false at setup: moving
# an image-shipped ~/.claude.json over the volume's would lose the carried account state on every
# rebuild, where the plain `ln -sfn` it replaced had kept it.
#
# A FAILED MOVE IS RECORDED in DC_LOGIN_CARRY_FAILED (also in the layer) and cleared by the next
# clean run, so verify.sh can tell a file waiting to be carried from one that could not be.
#
# Prints one line per file moved or set aside. Returns 1 if any file could not be moved or linked.
dc_persist_login() { # dc_persist_login [home]
    local h="${1:-/home/vscode}" pair link want rc=0 errs=""
    local linked="$h/$DC_LOGIN_LINKED" failed="$h/$DC_LOGIN_CARRY_FAILED"
    mkdir -p "$h/.claude-state" "$h/.claude" || return 1
    while IFS= read -r pair; do
        link="${pair%%:*}"; want="${pair##*:}"
        if [ -d "$link" ] && [ ! -L "$link" ]; then
            # `ln -sfn` onto a real directory makes a link INSIDE it and succeeds, so this would
            # otherwise report the file linked while nothing was.
            errs="$errs$link is a directory, not a file; left alone. "
            rc=1; continue
        fi
        if [ -f "$link" ] && [ ! -L "$link" ]; then
            if [ ! -e "$linked" ] && [ -f "$want" ]; then
                if ! mv -f "$link" "$link.pre-link"; then
                    errs="${errs}could not set $link aside. "
                    rc=1; continue
                fi
                echo "kept the state volume's $want; set aside $link as $link.pre-link"
            elif ! mv -f "$link" "$want"; then
                errs="${errs}could not move $link into the state volume. "
                rc=1; continue
            else
                echo "moved $link into the state volume"
            fi
        fi
        ln -sfn "$want" "$link" 2>/dev/null || { errs="${errs}could not link $link. "; rc=1; }
    done <<EOF
$(dc_login_files "$h")
EOF
    if [ "$rc" -eq 0 ]; then
        : > "$linked" || rc=1
        rm -f "$failed"
    else
        echo "dc_persist_login: $errs" >&2
        printf '%s\n' "$errs" > "$failed" 2>/dev/null || true
    fi
    return "$rc"
}

# The two markers above, relative to the home, named once for lib.sh and verify.sh.
DC_LOGIN_LINKED=".claude/.jkb-login-linked"
DC_LOGIN_CARRY_FAILED=".claude/.jkb-login-carry-failed"

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
dc_require_apparmor_profile() { # dc_require_apparmor_profile <profile file> -> the name, or exits
    # THE CHECK LIVES HERE BECAUSE EVERY CALLER THAT SPENDS THE ANSWER ON DOCKER MUST MAKE IT, and
    # two of the three did not. `dc_apparmor_profile` answers empty when the file's `profile …`
    # line does not match (a reformat, a regenerated header) and fails outright when the file is
    # unreadable -- and an empty name reaches docker as `--security-opt apparmor=`, which docker
    # reads as its DEFAULT profile. That is docker-default, whose `mount` denial is precisely the
    # silent state the refusal at the call site exists to prevent: the container starts, the nested
    # sandbox does not, and nothing says so. Returning the name or exiting means no future caller
    # can forget, which a comment beside each call site could not achieve.
    local name
    name="$(dc_apparmor_profile "$1" 2>/dev/null)" || name=""
    if [ -z "$name" ]; then
        printf 'cannot read an AppArmor profile name from %s.\n' "$1" >&2
        printf 'Its `profile <name> flags=(...)` line is missing or unreadable, and an empty name\n' >&2
        printf 'reaches docker as the DEFAULT profile -- which is docker-default, whose `mount`\n' >&2
        printf 'denial is the thing this profile exists to lift. Regenerate it:\n\n' >&2
        printf '    ./.container/generate-apparmor.sh\n' >&2
        exit 1
    fi
    printf '%s' "$name"
}

# DOES APPARMOR MEDIATE ON THIS HOST? One definition, for the same reason the profile NAME has one.
#
# It was hand-written in four places -- run.sh's `aa_enabled` (which decides whether to pass
# `--security-opt` at all), mutate-verify.sh, ci.yml, and verify.sh's `aa_mediates` -- and the
# fourth was not the same rule as the other three: it preferred `/proc/self/attr/apparmor/current`,
# so the VERIFIER and the LAUNCHER could answer one question from different primary evidence about
# the same host, which is precisely the disagreement verify.sh's own comment says its copy was
# added to avoid. Nothing compared them.
#
# The kernel parameter is the answer here, not the attr file: the question is whether the LSM
# mediates at all, which is a property of the host, while an attr file is a property of one
# process's label. verify.sh still reads the attr file -- to learn WHICH profile is in force -- but
# it asks this first, so the two scripts start from the same fact.
dc_apparmor_mediates() { # -> 0 if AppArmor mediates on this host
    [ -r /sys/module/apparmor/parameters/enabled ] \
        && grep -qi '^Y' /sys/module/apparmor/parameters/enabled
}

dc_apparmor_profile() { # dc_apparmor_profile <profile file> -> the declared profile name
    # The name may be quoted -- moby's template writes `profile "{{.Name}}" flags=(...)`, and an
    # unquoted pattern silently returned NOTHING against a correctly generated profile, which
    # callers spend on `docker run --security-opt apparmor=`. Quotes optional, never captured.
    sed -n 's/^profile[[:space:]]\{1,\}"\{0,1\}\([A-Za-z0-9_.-]\{1,\}\)"\{0,1\}[[:space:]].*/\1/p' "$1" 2>/dev/null | head -1
}

# ---------------------------------------------------------------------------------------------
# THE HOST'S GLOBAL GIT HOOKS, MIRRORED INTO THE CONTAINER
# ---------------------------------------------------------------------------------------------
# VS Code copies the host's ~/.gitconfig into the container on every attach, `core.hooksPath`
# included. When that names a global hooks directory (the Mac's is /Users/<you>/.config/git/hooks),
# the path does not exist in here, and git treats a missing hooks directory as an empty one: no
# error, and no hook runs. Observed 2026-09-18: in here `core.hooksPath` named
# /Users/jagnew/.config/git/hooks, which does not exist, so commits made here carried a trailer the
# host's hooks stop, and `post-merge` never ran on a pull. So run.sh copies the host's directory to
# the path git in here will resolve, on every start.
#
# A COPY, NOT A BIND MOUNT. The mount list is the security boundary and verify.sh asserts it
# exactly. A mount whose source is missing is a hard error that stops the container starting, and
# not every host has a global hooks directory. A copy needs neither, and cannot write back to the
# host. The cost is staleness: a hook edited on the host arrives at the next run.sh start.
#
# ROOT-OWNED AND READ-ONLY. A hook runs whenever git does, including from the terminal you attach,
# which is not sandboxed. A hooks directory the agent could write would let a sandboxed command
# plant code that runs outside the sandbox on your next commit. verify.sh asserts the owner is root
# and that it is not writable here.
#
# ONLY ITS OWN DIRECTORY IS EVER REPLACED: one carrying DC_HOOKS_MIRROR_MARKER, owned by root, and
# not a symlink. The marker alone is not enough, because a sandboxed command can write a file of
# that name anywhere it may write (a review found the forgery: marker plus `chmod 555` in a
# directory under ~/repos read as the mirror, and root would then have deleted and re-owned it).
# A directory that fails any of the three is refused and reported, never touched.
DC_HOOKS_MIRROR_MARKER=".jkb-host-mirror"

# dc_global_hooks_path [--path] -- THE ONE READER of the global core.hooksPath, for run.sh on the
# host and verify.sh in the container alike, so the two cannot disagree about what the setting is.
# `--includes`: a `[include]` in ~/.gitconfig is how a split config sets it, and `--global` reads
# none without it (scripts/lib.sh's `_hooks_path_read` learned the same). `--path` asks for git's
# own expansion instead of the raw value.
# Prints the value. rc 0: set (the value may be EMPTY, which git reads as "/" and so runs nothing);
# rc 1: not set; rc 2: git could not answer (a malformed file, or a value `--path` cannot expand).
# A subshell function, so the scrub of the caller's repository selection (the rule every script
# that runs git follows, docs/git-hooks-installer.md) cannot leak into the caller.
dc_global_hooks_path() ( # dc_global_hooks_path [--path]
    unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES
    out="$(git config --global --includes "$@" --get core.hooksPath 2>/dev/null)"; rc=$?
    case "$rc" in
        0) printf '%s' "$out" ;;
        1) exit 1 ;;
        *) exit 2 ;;
    esac
)

# dc_container_hooks_dir <raw core.hooksPath> -> the absolute path git in the container resolves it
# to, or nothing (rc 1) when there is nothing to mirror. The value is taken RAW, as the host stores
# it, because the container's ~/.gitconfig is a byte copy and git in here expands it for itself:
#   /abs/path    -> /abs/path, the same path in here
#   ~/rel        -> /home/vscode/rel, because git expands ~ against the container's HOME
#   ~user/...    -> nothing: user `user` has no home in here
#   relative     -> nothing: git resolves it inside each repository, which is already mounted
#   empty        -> nothing: git reads that as "/", and there is nothing to mirror
# TRAILING SLASHES ARE DROPPED. `~/.config/git/hooks/` is what tab completion writes and git
# accepts it, and a review reproduced what it did here: the staging directory, spelled "$dst" plus
# a suffix, landed INSIDE the target, was deleted with it, and every start left no hooks at all.
dc_container_hooks_dir() { # dc_container_hooks_dir <raw>
    local p
    case "$1" in
        /*)    p="$1" ;;
        "~")   p=/home/vscode ;;
        "~/"*) p="/home/vscode/${1#\~/}" ;;
        *)     return 1 ;;
    esac
    while [ "${#p}" -gt 1 ] && [ "${p%/}" != "$p" ]; do p="${p%/}"; done
    printf '%s' "$p"
}

# dc_hooks_mount_verdict <container dir> <container.json> -> `bind`, `volume` or `none`: whether the
# directory sits in a mount. Under a BIND it already IS the host's directory, live, so there is
# nothing to copy, and copying would mean root deleting and re-owning the host's own files through
# the bind. Under a VOLUME it is container state no host path feeds. Either way the mirror stays
# out.
# The DEEPEST containing target decides, since mounts nest (a volume inside a bound directory).
dc_hooks_mount_verdict() { # dc_hooks_mount_verdict <dir> <container.json>
    local dir="$1" t best=""
    while IFS= read -r t; do
        [ -n "$t" ] || continue
        case "$dir/" in
            "$t"/*) [ "${#t}" -gt "${#best}" ] && best="$t" ;;
        esac
    done <<EOF
$(dc_mount_targets "$2")
EOF
    if [ -n "$best" ]; then dc_type_for_target "$2" "$best"; else printf 'none'; fi
}

# dc_mirror_hooks <host dir> <container dir> <container name> [docker command]
# Copy <host dir> into the running container at <container dir>, root-owned and not writable by
# anyone else, with the marker added. Prints one line saying what it did. Returns 1 when it did not
# mirror, with the reason on stderr, and then the container is left exactly as it was. The docker
# command is a parameter so a test can stub it.
dc_mirror_hooks() {
    local src="$1" dst="$2" name="$3" docker="${4:-docker}" parent archive rc=0
    [ -d "$src" ] || { echo "dc_mirror_hooks: $src is not a directory on the host" >&2; return 1; }
    case "$dst" in /?*) ;; *) echo "dc_mirror_hooks: '$dst' is not an absolute directory" >&2; return 1 ;; esac
    parent="$(dirname "$dst")"
    # A parent under the container user's home is created AS that user, so a later `git config
    # --global` or a tool writing ~/.config still owns its own directory. Elsewhere (/Users/...),
    # only root can create it, and the root step below does.
    case "$parent" in
        /home/vscode/*) "$docker" exec "$name" mkdir -p "$parent" >/dev/null 2>&1 || true ;;
    esac
    # THE ARCHIVE IS BUILT WHOLE BEFORE ANYTHING IS SENT. Streamed, a tar that failed part way (a
    # hook that is a dangling symlink, measured) still delivered what it had, the container side
    # installed that partial copy, and the two ends then reported opposite things about one
    # directory. So tar's status is checked first, and a failed build sends nothing.
    # -h: a hook that is a symlink on the host would dangle in here, so its target is copied.
    # COPYFILE_DISABLE and --no-xattrs keep macOS metadata out of the archive, which GNU tar
    # would otherwise warn about, file by file, on every start.
    archive="$(mktemp "${TMPDIR:-/tmp}/jkb-hooks.XXXXXX")" || { echo "dc_mirror_hooks: could not make a temporary file on the host" >&2; return 1; }
    if ! (cd "$src" && COPYFILE_DISABLE=1 tar -h --no-xattrs -cf - .) > "$archive"; then
        rm -f "$archive"
        echo "dc_mirror_hooks: could not read all of $src on the host (tar above says which file); nothing was copied" >&2
        return 1
    fi
    # THE ROOT STEP. Everything it trusts is checked, because it runs as root in a directory tree the
    # container user can partly write:
    #   - the new copy is assembled in a fresh root-only mktemp directory, never beside the target,
    #     where a planted symlink could steer the extraction and the chmod elsewhere;
    #   - the parent must resolve to itself, so no symlink in the path leads the swap somewhere else;
    #   - an existing target is replaced only when it is a real directory, owned by root, carrying
    #     the marker (see the section header for why the marker alone is not proof);
    #   - `mv -T` replaces the target NAME, so a symlink raced into its place is not followed.
    # shellcheck disable=SC2016
    "$docker" exec -i -u root "$name" sh -c '
        set -e
        dst="$1"; marker="$2"; parent="$(dirname "$dst")"
        mkdir -p "$parent"
        if [ "$(cd "$parent" && pwd -P)" != "$parent" ]; then
            echo "dc_mirror_hooks: $parent is reached through a symlink; not writing through it" >&2
            exit 3
        fi
        if [ -e "$dst" ] || [ -L "$dst" ]; then
            if [ -L "$dst" ] || [ ! -d "$dst" ] || [ ! -e "$dst/$marker" ] || [ "$(stat -c %u "$dst")" != 0 ]; then
                echo "dc_mirror_hooks: $dst exists and was not made by this mirror (it must be a root-owned directory carrying $marker); left alone" >&2
                exit 3
            fi
        fi
        new="$(mktemp -d)"   # root-only, 0700; /tmp in here, since docker exec passes no TMPDIR
        trap "rm -rf \"\$new\"" EXIT
        tar -C "$new" -xf - --no-same-owner --warning=no-unknown-keyword
        printf "%s\n" "Copied from the host by .container/run.sh on every start. Edit the host copy." > "$new/$marker"
        chown -R 0:0 "$new"
        chmod -R u+rwX,go+rX,go-w "$new"
        rm -rf "$dst"
        mv -T "$new" "$dst"
        trap - EXIT
    ' sh "$dst" "$DC_HOOKS_MIRROR_MARKER" < "$archive" || rc=$?
    rm -f "$archive"
    if [ "$rc" -ne 0 ]; then
        echo "dc_mirror_hooks: could not copy $src into $name at $dst" >&2
        return 1
    fi
    echo "mirrored the host's git hooks ($src) to $dst"
}

# dc_mirror_host_hooks <container name> <container.json> [docker command] -- run.sh's step, ON THE
# HOST: read the host's GLOBAL core.hooksPath and mirror that directory in. Prints what happened;
# always returns 0, because a start must not stop over hooks. verify.sh reports a dead hooks path.
#
# Global scope only, deliberately. A repository's own core.hooksPath travels with the repository,
# which is mounted, and a relative one resolves inside it. The global one is the only kind that
# names a host path the container cannot see.
dc_mirror_host_hooks() { # dc_mirror_host_hooks <name> <container.json> [docker]
    local name="$1" config="$2" docker="${3:-docker}" raw src dst rc=0
    raw="$(dc_global_hooks_path)" || rc=$?
    case "$rc" in
        0) ;;
        1) echo "no global core.hooksPath on the host; git in the container uses each repository's .git/hooks"; return 0 ;;
        *) echo "warning: git on the host could not read its global core.hooksPath, so no hooks were mirrored (run: git config --global --includes --show-origin --get core.hooksPath)" >&2; return 0 ;;
    esac
    if [ -z "$raw" ]; then
        echo "warning: the host's global core.hooksPath is set to the empty string, which git reads as '/': it runs no hooks there, and there is nothing to mirror" >&2
        return 0
    fi
    if ! dst="$(dc_container_hooks_dir "$raw")"; then
        echo "the host's global core.hooksPath ($raw) needs no mirror, or cannot have one: git resolves it inside each repository, or it names another user's home"
        return 0
    fi
    case "$(dc_hooks_mount_verdict "$dst" "$config")" in
        bind)   echo "the host's global core.hooksPath ($raw) is inside a directory the container mounts from the host, so git in there already runs the host's own copy; nothing to mirror"; return 0 ;;
        volume) echo "warning: the host's global core.hooksPath ($raw) lands inside one of the container's volumes in there, so it is not mirrored, and git in the container runs whatever that volume holds" >&2; return 0 ;;
    esac
    # The HOST's expansion of the same value is where the files are.
    if ! src="$(dc_global_hooks_path --path)" || [ ! -d "$src" ]; then
        echo "warning: the host's global core.hooksPath ($raw) is not a directory here, so there is nothing to mirror, and git in the container runs no hooks" >&2
        return 0
    fi
    dc_mirror_hooks "$src" "$dst" "$name" "$docker" \
        || echo "warning: the host's git hooks were not mirrored into $name; git in there runs whatever was there before (see above)" >&2
    return 0
}
