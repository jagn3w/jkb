#!/usr/bin/env bash
# The host's global git hooks reaching the dev container (.container/lib.sh's dc_container_hooks_dir,
# dc_mirror_hooks, dc_mirror_host_hooks), and setup.sh's remote-mode profile.
#
# Why it exists: VS Code copies the host's ~/.gitconfig into the container, and its global
# core.hooksPath names a host directory the container cannot see. Git treats a missing hooks
# directory as an empty one, so no hook ran in there and nothing said so: commits skipped the
# host's hooks and post-merge never fired. These cases pin the mirror that fixes it, against a stub
# `docker` that runs the container-side script locally under a scratch root, and pin setup.sh
# stopping after the binary when post-merge runs it in the container.
set -uo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=.container/lib.sh
. "$repo_root/.container/lib.sh"
# shellcheck source=scripts/tests/harness.sh
. "$(dirname "$0")/harness.sh"

new_workdir
isolate_git "$work/home"

# A stub `docker` for `exec`. Every absolute path argument is re-rooted under $CTR_ROOT, so "the
# container" is a scratch directory. `-u root` runs the script with three tools shimmed to behave
# as they do for root, because the test is not root:
#   chown  records the inode of every file it would have given to root (mv keeps an inode);
#   stat   answers uid 0 for exactly those inodes, and the real uid for everything else, so a
#          directory the mirror did not make still reads as not root's (the forged-marker case);
#   tar    extracts keeping each mode exactly (-p). As a normal user GNU tar applies the umask,
#          which silently did the mirror's chmod for it: with the chmod deleted, this stayed green.
# Each call is logged to $DOCKER_LOG as its user and verb.
# THE ROOT STEP IS GNU CODE: it runs in the container, which is Ubuntu, and uses `mv -T`, `stat -c`
# and `tar --warning`, none of which BSD's tools accept. So the stub hands it GNU's, found as
# gmv/gstat/gtar (Homebrew's coreutils and gnu-tar) or as the plain names where those ARE GNU. On a
# machine with neither, every case that reaches the root step SKIPS, loudly and with the reason,
# instead of failing on a flag the container never lacks. A review found this suite red on the Mac,
# where check.sh runs it, for exactly that. STUB_FORCE_NO_GNU=1 takes that arm on purpose.
gnu_tool() { # gnu_tool <name> -> the path of a GNU build of it
    local c p
    [ "${STUB_FORCE_NO_GNU:-0}" = 1 ] && return 1
    for c in "g$1" "$1"; do
        p="$(command -v "$c" 2>/dev/null)" || continue
        grep -q GNU <<<"$("$p" --version 2>/dev/null | head -1)" && { printf '%s' "$p"; return 0; }
    done
    return 1
}
gnu_mv="$(gnu_tool mv)" && gnu_stat="$(gnu_tool stat)" && gnu_tar="$(gnu_tool tar)" && have_gnu=1 || have_gnu=0
# need_gnu -- first line of every case that reaches the root step.
need_gnu() {
    [ "$have_gnu" = 1 ] && return 0
    skip "${FUNCNAME[1]}: the container's root step needs GNU mv, stat and tar (brew install coreutils gnu-tar); Linux CI runs it"
    return 1
}

make_stub() {
    stub_dir="$work/stub-$RANDOM"; mkdir -p "$stub_dir/bin" "$stub_dir/shim" "$stub_dir/root/tmp"
    # Resolved, because the root step refuses a parent reached through a symlink, and on macOS the
    # temp directory itself is one (/var -> /private/var).
    stub_dir="$(cd "$stub_dir" && pwd -P)"
    : > "$stub_dir/root-inodes"
    cat > "$stub_dir/shim/chown" <<SHIM
#!/bin/sh
for a in "\$@"; do case "\$a" in /*) find "\$a" -exec ${gnu_stat:-stat} -c %i {} + >> "$stub_dir/root-inodes" ;; esac; done
exit 0
SHIM
    cat > "$stub_dir/shim/stat" <<SHIM
#!/bin/sh
real=${gnu_stat:-stat}
if [ "\$1" = -c ] && [ "\$2" = %u ]; then
    ino="\$("\$real" -c %i "\$3")" || exit 1
    grep -qx "\$ino" "$stub_dir/root-inodes" && { echo 0; exit 0; }
fi
exec "\$real" "\$@"
SHIM
    # ...and records the directory it extracts into, so a case can assert WHERE the copy is staged.
    printf '#!/bin/sh\nprintf "%%s\\n" "$*" >> "%s"\nexec %s -p "$@"\n' "$stub_dir/tar-log" "${gnu_tar:-tar}" > "$stub_dir/shim/tar"
    printf '#!/bin/sh\nexec %s "$@"\n' "${gnu_mv:-mv}" > "$stub_dir/shim/mv"
    chmod +x "$stub_dir/shim/chown" "$stub_dir/shim/stat" "$stub_dir/shim/tar" "$stub_dir/shim/mv"
    cat > "$stub_dir/bin/docker" <<'STUB'
#!/usr/bin/env bash
[ "$1" = exec ] || { echo "stub docker: only exec" >&2; exit 2; }
shift; user=vscode
while [ $# -gt 0 ]; do
    case "$1" in
        -i) shift ;;
        -u) user="$2"; shift 2 ;;
        *)  break ;;
    esac
done
shift   # the container name
printf '%s %s\n' "$user" "$1" >> "$DOCKER_LOG"
args=()
for a in "$@"; do case "$a" in /*) args+=("$CTR_ROOT$a") ;; *) args+=("$a") ;; esac; done
# TMPDIR inside the scratch root, so the root step's staging directory is on the same filesystem
# as its target and a move keeps inodes, as it does in the container (/tmp there).
if [ "$user" = root ]; then PATH="$ROOT_SHIM:$PATH" TMPDIR="$CTR_ROOT/tmp" exec "${args[@]}"; fi
exec "${args[@]}"
STUB
    chmod +x "$stub_dir/bin/docker"
    export CTR_ROOT="$stub_dir/root" ROOT_SHIM="$stub_dir/shim" DOCKER_LOG="$stub_dir/log" TAR_LOG="$stub_dir/tar-log"
    : > "$DOCKER_LOG"
    docker_cmd="$stub_dir/bin/docker"
}

# host_hooks — a host hooks directory: an executable hook, a symlinked hook, a non-hook file.
host_hooks() {
    src="$work/host-$RANDOM/hooks"; mkdir -p "$src" "${src%/hooks}/real"
    # Group-writable on purpose: the mirror must not carry a write bit for anyone but root.
    printf '#!/bin/sh\necho commit-msg ran\n' > "$src/commit-msg"; chmod 775 "$src/commit-msg"
    printf '#!/bin/sh\necho post-merge ran\n' > "${src%/hooks}/real/post-merge"; chmod 755 "${src%/hooks}/real/post-merge"
    ln -s "${src%/hooks}/real/post-merge" "$src/post-merge"
}

case1_the_container_path_is_what_git_in_there_resolves() {
    local bad="" got
    for pair in "/Users/me/.config/git/hooks=/Users/me/.config/git/hooks" \
                "~/.config/git/hooks=/home/vscode/.config/git/hooks" "~=/home/vscode" \
                "~/.config/git/hooks/=/home/vscode/.config/git/hooks" "/Users/me/hooks//=/Users/me/hooks" "/=/"; do
        got="$(dc_container_hooks_dir "${pair%%=*}")" || got="(rc $?)"
        [ "$got" = "${pair#*=}" ] || bad="$bad [${pair%%=*} -> $got]"
    done
    for raw in "~other/hooks" ".githooks" "hooks" ""; do
        if got="$(dc_container_hooks_dir "$raw")"; then bad="$bad [$raw -> $got, wanted nothing]"; fi
    done
    if [ -z "$bad" ]; then ok "absolute and ~/ paths map to where git in the container looks, trailing slashes dropped; ~user, relative and empty map to nothing"
    else fail "absolute and ~/ paths map to where git in the container looks, trailing slashes dropped; ~user, relative and empty map to nothing" "$bad"; fi
}

# THE CASE THIS EXISTS FOR: the host's hooks arrive, runnable, at the path git resolves, with the
# symlinked one dereferenced (it would dangle in there), marked, and not writable by the group or
# others. The container-side step ran as root, and the parent under the home was made as vscode.
# The copy is STAGED in the root step's private temp directory, never beside the target: beside it,
# in a directory the container user can write, a symlink raced into the staging name would steer
# root's extraction and chmod anywhere. The race itself cannot be staged deterministically, so the
# property that removes it is what is pinned.
case2_a_mirror_arrives_runnable_marked_and_root_side() {
    need_gnu || return 0
    make_stub; host_hooks
    local dst=/home/vscode/.config/git/hooks out rc
    out="$(dc_mirror_hooks "$src" "$dst" ctr "$docker_cmd" 2>&1)"; rc=$?
    local d="$CTR_ROOT$dst" mode
    mode="$(stat -c '%a' "$d/commit-msg" 2>/dev/null || stat -f '%Lp' "$d/commit-msg")"
    if [ "$rc" -eq 0 ] && [ "$("$d/commit-msg")" = "commit-msg ran" ] && [ ! -L "$d/post-merge" ] \
       && [ "$("$d/post-merge")" = "post-merge ran" ] && [ -f "$d/$DC_HOOKS_MIRROR_MARKER" ] \
       && [ "$mode" = 755 ] && grep -q '^root sh$' "$DOCKER_LOG" && grep -q '^vscode mkdir$' "$DOCKER_LOG" \
       && [[ "$(cat "$TAR_LOG")" == "-C $CTR_ROOT/tmp/"* ]]; then
        ok "the host's hooks arrive runnable, symlinks dereferenced, marked, a group-writable hook made 755, written by root"
    else
        fail "the host's hooks arrive runnable, symlinks dereferenced, marked, a group-writable hook made 755, written by root" \
             "rc=$rc out=$out mode=$mode log=$(tr '\n' ';' < "$DOCKER_LOG") tar=$(cat "$TAR_LOG") ls=$(ls -la "$d" 2>&1)"
    fi
}

# Every start re-mirrors: an edited hook arrives, a deleted one goes. A copy that only ever added
# would keep running a hook the host had removed.
case3_a_re_mirror_replaces_rather_than_merges() {
    need_gnu || return 0
    make_stub; host_hooks
    local dst=/Users/me/.config/git/hooks
    dc_mirror_hooks "$src" "$dst" ctr "$docker_cmd" >/dev/null 2>&1
    printf '#!/bin/sh\necho edited\n' > "$src/commit-msg"; rm "$src/post-merge"
    dc_mirror_hooks "$src" "$dst" ctr "$docker_cmd" >/dev/null 2>&1; local rc=$?
    local d="$CTR_ROOT$dst"
    if [ "$rc" -eq 0 ] && [ "$("$d/commit-msg")" = edited ] && [ ! -e "$d/post-merge" ] && [ -z "$(ls -A "$CTR_ROOT/tmp")" ]; then
        ok "a second mirror carries an edit and drops a deleted hook, leaving no staging directory"
    else
        fail "a second mirror carries an edit and drops a deleted hook, leaving no staging directory" "rc=$rc $(ls -la "$d" "$d.jkb-new" 2>&1)"
    fi
}

# A directory at the target that the mirror did not make is somebody's, and is refused untouched.
case4_a_directory_it_did_not_make_is_left_alone() {
    need_gnu || return 0
    make_stub; host_hooks
    local dst=/Users/me/.config/git/hooks d
    d="$CTR_ROOT$dst"; mkdir -p "$d"; printf 'mine\n' > "$d/pre-push"
    local err; err="$(dc_mirror_hooks "$src" "$dst" ctr "$docker_cmd" 2>&1 >/dev/null)"; local rc=$?
    if [ "$rc" -eq 1 ] && [ "$(cat "$d/pre-push")" = mine ] && [ ! -e "$d/commit-msg" ] \
       && [[ "$err" == *"was not made by this mirror"* ]]; then
        ok "a directory the mirror did not make is refused, untouched, with the reason"
    else
        fail "a directory the mirror did not make is refused, untouched, with the reason" "rc=$rc err=$err"
    fi
}

# run.sh's step, reading the host's GLOBAL config. Driven through GIT_CONFIG_GLOBAL so the test's
# git config is its own. Unset and relative values mirror nothing, and no root step runs.
case5_the_host_step_reads_the_global_value() {
    need_gnu || return 0
    make_stub; host_hooks
    local cfg="$HOME/.gitconfig" out
    rm -f "$cfg"
    : > "$cfg"
    out="$(GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" 2>&1)"
    local unset_ok=no; [[ "$out" == *"no global core.hooksPath"* ]] && ! grep -q "^root " "$DOCKER_LOG" && unset_ok=yes
    git config --file "$cfg" core.hooksPath .githooks
    out="$(GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" 2>&1)"
    local rel_ok=no; [[ "$out" == *"needs no mirror"* ]] && ! grep -q "^root " "$DOCKER_LOG" && rel_ok=yes
    git config --file "$cfg" core.hooksPath "$src"
    out="$(GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" 2>&1)"
    local abs_ok=no; [ "$("$CTR_ROOT$src/commit-msg" 2>/dev/null)" = "commit-msg ran" ] && abs_ok=yes
    git config --file "$cfg" core.hooksPath "$work/nowhere"
    out="$(GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" 2>&1)"; local rc=$?
    local gone_ok=no; [ "$rc" -eq 0 ] && [[ "$out" == *"not a directory here"* ]] && gone_ok=yes
    if [ "$unset_ok$rel_ok$abs_ok$gone_ok" = yesyesyesyes ]; then
        ok "the host step: unset and relative mirror nothing, an absolute path is mirrored to itself, a missing one warns and does not fail"
    else
        fail "the host step: unset and relative mirror nothing, an absolute path is mirrored to itself, a missing one warns and does not fail" \
             "unset=$unset_ok relative=$rel_ok absolute=$abs_ok missing=$gone_ok out=$out"
    fi
}

# post-merge runs setup.sh after a pull. In the container (JKB_REMOTE set) it must rebuild the
# binary and stop: scaffold and topic need a local database, services a service manager, and the
# git-hooks step would try to write a chainer into the read-only mirror. Stubbed cargo and jkb log
# every call, so "stopped" is observed rather than inferred from the message.
# CONFINED, because the watch-it-fail step deletes the very guard under test, and then setup.sh runs
# its host installer for real (a review found that: services restarted, VS Code's extension
# installed, hooks written into this checkout). So it runs a COPY of setup.sh in a scratch repo, with
# --no-extension --no-service, and with every service manager and installer it could reach on PATH
# as a stub that fails and logs. Mutated, it can only write under $d.
case6_setup_sh_in_remote_mode_rebuilds_the_binary_and_stops() {
    local d="$work/setup-$RANDOM" t
    mkdir -p "$d/home" "$d/cargo/bin" "$d/repo/scripts/hooks"
    cp "$repo_root/scripts/setup.sh" "$repo_root/scripts/lib.sh" "$d/repo/scripts/"
    cp "$repo_root/scripts/hooks/post-merge" "$d/repo/scripts/hooks/"
    git init -q "$d/repo"
    printf '#!/bin/sh\necho "cargo $*" >> "%s"\n' "$d/calls" > "$d/cargo/bin/cargo"
    printf '#!/bin/sh\necho "jkb $*" >> "%s"\necho "jkb 0.0.0-stub"\n' "$d/calls" > "$d/cargo/bin/jkb"
    for t in systemctl launchctl pnpm code; do
        printf '#!/bin/sh\necho "%s $*" >> "%s"\nexit 1\n' "$t" "$d/calls" > "$d/cargo/bin/$t"
    done
    chmod +x "$d/cargo/bin/"*
    : > "$d/calls"
    local out rc
    out="$(HOME="$d/home" CARGO_HOME="$d/cargo" PATH="$d/cargo/bin:$PATH" JKB_REMOTE=host.docker.internal:7117 \
           bash "$d/repo/scripts/setup.sh" --no-extension --no-service 2>&1)"; rc=$?
    local calls; calls="$(tr '\n' ';' < "$d/calls")"
    if [ "$rc" -eq 0 ] && [[ "$calls" == "cargo install --path crates/jkb-cli --locked --force;"* ]] \
       && [ "$(grep -c '^jkb ' "$d/calls")" = "$(grep -c '^jkb --version$' "$d/calls")" ] \
       && [[ "$out" == *"remote mode"* ]] && [[ "$out" != *"installing git hooks"* ]] \
       && [[ "$out" == *"run ./scripts/setup.sh there"* ]]; then
        ok "setup.sh with JKB_REMOTE rebuilds the binary, asks jkb nothing but its version, warns the server is older, and stops before the hooks"
    else
        fail "setup.sh with JKB_REMOTE rebuilds the binary, asks jkb nothing but its version, warns the server is older, and stops before the hooks" \
             "rc=$rc calls=$calls out=$(tail -5 <<<"$out")"
    fi
}


# THE REVIEW'S MUST-FIX. A trailing slash (tab completion writes one, git accepts it) put the old
# staging directory inside the target, and every start deleted both and left no hooks. Driven
# through the host step, twice, because the second start is the one that replaces.
case7_a_trailing_slash_mirrors_and_re_mirrors() {
    need_gnu || return 0
    make_stub
    local cfg="$HOME/.gitconfig" d="$CTR_ROOT/home/vscode/.config/git/hooks"
    rm -f "$cfg"
    mkdir -p "$HOME/.config/git/hooks"; printf '#!/bin/sh\necho slash ran\n' > "$HOME/.config/git/hooks/commit-msg"
    chmod 755 "$HOME/.config/git/hooks/commit-msg"
    git config --file "$cfg" core.hooksPath "~/.config/git/hooks/"
    GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" >/dev/null 2>&1
    local first; first="$("$d/commit-msg" 2>/dev/null)"
    GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" >/dev/null 2>&1
    if [ "$first" = "slash ran" ] && [ "$("$d/commit-msg" 2>/dev/null)" = "slash ran" ]; then
        ok "a hooksPath with a trailing slash mirrors, and the next start's replace still leaves the hooks"
    else
        fail "a hooksPath with a trailing slash mirrors, and the next start's replace still leaves the hooks" "first=$first $(ls -la "$d" 2>&1)"
    fi
}

# A hooksPath inside a bind (here ~/repos) IS the host's own directory in there. Nothing is copied,
# no root step runs, and nothing says hooks are missing.
case8_a_hooks_path_inside_a_bind_is_left_to_the_bind() {
    make_stub
    local cfg="$HOME/.gitconfig" out
    rm -f "$cfg"
    git config --file "$cfg" core.hooksPath "~/repos/dotfiles/hooks"
    mkdir -p "$HOME/repos/dotfiles/hooks"
    out="$(GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" 2>&1)"
    if ! grep -q "^root " "$DOCKER_LOG" && [[ "$out" == *"already runs the host's own copy"* ]]; then
        ok "a hooksPath inside a bind mount is left to the bind: no copy, no root step"
    else
        fail "a hooksPath inside a bind mount is left to the bind: no copy, no root step" "out=$out log=$(tr '\n' ';' < "$DOCKER_LOG")"
    fi
}

# THE FORGERY. A directory carrying the marker but not owned by root is not the mirror: anything
# that can write a directory can write a file of that name into it. Root must not replace it.
case9_a_forged_marker_is_refused() {
    need_gnu || return 0
    make_stub; host_hooks
    local dst=/Users/me/.config/git/hooks d
    d="$CTR_ROOT$dst"; mkdir -p "$d"; : > "$d/$DC_HOOKS_MIRROR_MARKER"; printf 'mine\n' > "$d/pre-push"
    local err; err="$(dc_mirror_hooks "$src" "$dst" ctr "$docker_cmd" 2>&1 >/dev/null)"; local rc=$?
    if [ "$rc" -eq 1 ] && [ "$(cat "$d/pre-push")" = mine ] && [ ! -e "$d/commit-msg" ] \
       && [[ "$err" == *"root-owned directory carrying"* ]]; then
        ok "a marker in a directory root does not own is refused, and the directory is untouched"
    else
        fail "a marker in a directory root does not own is refused, and the directory is untouched" "rc=$rc err=$err"
    fi
}

# A host directory tar cannot read whole (a dangling symlink, measured) sends NOTHING: the previous
# mirror stays exactly as it was, and no root step runs.
case10_a_partial_archive_is_never_installed() {
    need_gnu || return 0
    make_stub; host_hooks
    local dst=/Users/me/.config/git/hooks d="$CTR_ROOT/Users/me/.config/git/hooks"
    dc_mirror_hooks "$src" "$dst" ctr "$docker_cmd" >/dev/null 2>&1
    : > "$DOCKER_LOG"
    printf '#!/bin/sh\necho new\n' > "$src/commit-msg"; ln -s "$work/does-not-exist" "$src/pre-commit"
    local err; err="$(dc_mirror_hooks "$src" "$dst" ctr "$docker_cmd" 2>&1 >/dev/null)"; local rc=$?
    if [ "$rc" -eq 1 ] && [ "$("$d/commit-msg")" = "commit-msg ran" ] && ! grep -q '^root ' "$DOCKER_LOG" \
       && [[ "$err" == *"nothing was copied"* ]]; then
        ok "an archive that could not be built whole sends nothing: the previous mirror is untouched"
    else
        fail "an archive that could not be built whole sends nothing: the previous mirror is untouched" \
             "rc=$rc err=$err now=$("$d/commit-msg" 2>&1) log=$(tr '\n' ';' < "$DOCKER_LOG")"
    fi
}

# A parent reached through a symlink would carry root's swap somewhere else, so it is refused.
case11_a_symlinked_parent_is_refused() {
    need_gnu || return 0
    make_stub; host_hooks
    mkdir -p "$CTR_ROOT/elsewhere" "$CTR_ROOT/Users"; ln -s "$CTR_ROOT/elsewhere" "$CTR_ROOT/Users/me"
    # Two levels below the symlink, so `mkdir -p` (which follows it) would CREATE elsewhere/deep before
    # any check ran; building the parent one component at a time refuses at /Users/me first.
    local err; err="$(dc_mirror_hooks "$src" /Users/me/deep/hooks ctr "$docker_cmd" 2>&1 >/dev/null)"; local rc=$?
    if [ "$rc" -eq 1 ] && [ -z "$(ls -A "$CTR_ROOT/elsewhere")" ] && [[ "$err" == *"/Users/me is a symlink"* ]]; then
        ok "a target whose parent is reached through a symlink is refused, and nothing is written there"
    else
        fail "a target whose parent is reached through a symlink is refused, and nothing is written there" "rc=$rc err=$err $(ls -la "$CTR_ROOT/elsewhere")"
    fi
}


# A split config: ~/.gitconfig includes another file, and THAT sets core.hooksPath. `--global`
# reads no include without `--includes`, so the value was invisible and nothing was mirrored.
case12_a_hooks_path_set_through_an_include_is_found() {
    need_gnu || return 0
    make_stub; host_hooks
    local cfg="$HOME/.gitconfig" inc="$work/included-$RANDOM" got out
    rm -f "$cfg"
    git config --file "$inc" core.hooksPath "$src"
    git config --file "$cfg" include.path "$inc"
    got="$(GIT_CONFIG_GLOBAL="$cfg" dc_global_hooks_path)"
    out="$(GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" 2>&1)"
    # ...and SAYS that the container sees it only if the included file resolves in there: VS Code
    # copies ~/.gitconfig alone. verify.sh compares the record with git's answer in there.
    if [ "$got" = "$src" ] && [ "$("$CTR_ROOT$src/commit-msg" 2>/dev/null)" = "commit-msg ran" ] \
       && [[ "$out" == *"sets core.hooksPath in $inc, not in ~/.gitconfig"* ]] \
       && grep -qx "origin=$inc" "$CTR_ROOT$DC_HOST_HOOKS_RECORD"; then
        ok "a hooksPath set in an included file is read, mirrored, recorded with its origin, and flagged as invisible to VS Code's copy"
    else
        fail "a hooksPath set in an included file is read, mirrored, recorded with its origin, and flagged as invisible to VS Code's copy" "got=$got out=$out record=$(cat "$CTR_ROOT$DC_HOST_HOOKS_RECORD" 2>&1)"
    fi
}

# Set to the empty string, git reads "/" and runs nothing: that is said, and nothing is copied. The
# reader tells it apart from unset by its exit status, which is what verify.sh's `empty` arm needs.
case13_an_empty_value_is_not_unset() {
    make_stub
    local cfg="$HOME/.gitconfig" out rc_empty rc_unset
    rm -f "$cfg"
    : > "$cfg"
    GIT_CONFIG_GLOBAL="$cfg" dc_global_hooks_path >/dev/null; rc_unset=$?
    git config --file "$cfg" core.hooksPath ""
    GIT_CONFIG_GLOBAL="$cfg" dc_global_hooks_path >/dev/null; rc_empty=$?
    out="$(GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" 2>&1)"
    if [ "$rc_unset" -eq 1 ] && [ "$rc_empty" -eq 0 ] && [[ "$out" == *"empty string"* ]] && ! grep -q "^root " "$DOCKER_LOG"; then
        ok "an empty hooksPath reads as set (rc 0), not unset (rc 1), and is reported without a copy"
    else
        fail "an empty hooksPath reads as set (rc 0), not unset (rc 1), and is reported without a copy" "unset=$rc_unset empty=$rc_empty out=$out"
    fi
}


# A target that is itself a symlink is not the mirror, even when it points at one and is owned by
# root: the swap would be following a name, not replacing a directory it made.
case14_a_symlinked_target_is_refused() {
    need_gnu || return 0
    make_stub; host_hooks
    dc_mirror_hooks "$src" /Users/me/real ctr "$docker_cmd" >/dev/null 2>&1
    ln -s "$CTR_ROOT/Users/me/real" "$CTR_ROOT/Users/me/hooks"
    # Owned by root, as far as the root step can tell. `stat` reads the LINK, so a link the container
    # user made already fails the ownership check; this pins the symlink refusal on its own.
    stat -c %i "$CTR_ROOT/Users/me/hooks" >> "$ROOT_SHIM/../root-inodes"
    local err; err="$(dc_mirror_hooks "$src" /Users/me/hooks ctr "$docker_cmd" 2>&1 >/dev/null)"; local rc=$?
    if [ "$rc" -eq 1 ] && [ -L "$CTR_ROOT/Users/me/hooks" ] && [[ "$err" == *"was not made by this mirror"* ]]; then
        ok "a target that is a symlink is refused, even to a real mirror, and the link is left as it was"
    else
        fail "a target that is a symlink is refused, even to a real mirror, and the link is left as it was" "rc=$rc err=$err"
    fi
}


# The record verify.sh compares against: what the host resolved and where from, rewritten on every
# start, so a host that drops the setting does not leave a stale value behind.
case15_the_host_value_is_recorded_on_every_start() {
    need_gnu || return 0
    make_stub; host_hooks
    local cfg="$HOME/.gitconfig" rec="$CTR_ROOT$DC_HOST_HOOKS_RECORD" set_ok=no unset_ok=no
    rm -f "$cfg"; git config --file "$cfg" core.hooksPath "$src"
    GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" >/dev/null 2>&1
    grep -qx "value=$src" "$rec" && grep -qx "origin=$cfg" "$rec" && set_ok=yes
    : > "$cfg"
    GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" >/dev/null 2>&1
    [ "$(cat "$rec")" = unset ] && unset_ok=yes
    if [ "$set_ok$unset_ok" = yesyes ]; then ok "every start records the host's value and origin, and 'unset' replaces it when the host drops it"
    else fail "every start records the host's value and origin, and 'unset' replaces it when the host drops it" "set=$set_ok unset=$unset_ok record=$(cat "$rec" 2>&1)"; fi
}


# The record and the origin warning, with NO root step, so they are tested on a Mac without GNU
# tools too (the mirroring cases that also check them are skipped there). A value from an included
# file: recorded with that origin, and said. The hooks directory does not exist, so nothing is
# copied and the root step never runs.
case16_an_included_value_is_recorded_and_flagged_without_a_root_step() {
    make_stub
    local cfg="$HOME/.gitconfig" inc="$work/included-$RANDOM" rec="$CTR_ROOT$DC_HOST_HOOKS_RECORD" out
    rm -f "$cfg"
    git config --file "$inc" core.hooksPath "$work/no-such-hooks"
    git config --file "$cfg" include.path "$inc"
    out="$(GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" 2>&1)"
    if grep -qx "value=$work/no-such-hooks" "$rec" && grep -qx "origin=$inc" "$rec" \
       && [[ "$out" == *"sets core.hooksPath in $inc, not in ~/.gitconfig"* ]] && ! grep -q '^root ' "$DOCKER_LOG"; then
        ok "a value from an included file is recorded with its origin and flagged, with no root step"
    else
        fail "a value from an included file is recorded with its origin and flagged, with no root step" "out=$out record=$(cat "$rec" 2>&1)"
    fi
}

# ...and quiet when the value IS in ~/.gitconfig, which is the common case: a warning that fires
# for everyone is one nobody reads. Then `unset` replaces the record when the host drops it.
case17_a_value_in_gitconfig_is_not_flagged_and_unset_replaces_the_record() {
    make_stub
    local cfg="$HOME/.gitconfig" rec="$CTR_ROOT$DC_HOST_HOOKS_RECORD" out quiet=no unset=no
    rm -f "$cfg"; git config --file "$cfg" core.hooksPath "$work/no-such-hooks"
    out="$(GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" 2>&1)"
    [[ "$out" != *"not in ~/.gitconfig"* ]] && grep -qx "origin=$cfg" "$rec" && quiet=yes
    : > "$cfg"
    GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" >/dev/null 2>&1
    [ "$(cat "$rec")" = unset ] && unset=yes
    if [ "$quiet$unset" = yesyes ]; then ok "a value in ~/.gitconfig is recorded without the origin warning, and 'unset' replaces it"
    else fail "a value in ~/.gitconfig is recorded without the origin warning, and 'unset' replaces it" "quiet=$quiet unset=$unset out=$out record=$(cat "$rec" 2>&1)"; fi
}

run_cases case1_the_container_path_is_what_git_in_there_resolves \
          case2_a_mirror_arrives_runnable_marked_and_root_side \
          case3_a_re_mirror_replaces_rather_than_merges \
          case4_a_directory_it_did_not_make_is_left_alone \
          case5_the_host_step_reads_the_global_value \
          case6_setup_sh_in_remote_mode_rebuilds_the_binary_and_stops \
          case7_a_trailing_slash_mirrors_and_re_mirrors \
          case8_a_hooks_path_inside_a_bind_is_left_to_the_bind \
          case9_a_forged_marker_is_refused \
          case10_a_partial_archive_is_never_installed \
          case11_a_symlinked_parent_is_refused \
          case12_a_hooks_path_set_through_an_include_is_found \
          case13_an_empty_value_is_not_unset \
          case14_a_symlinked_target_is_refused \
          case15_the_host_value_is_recorded_on_every_start \
          case16_an_included_value_is_recorded_and_flagged_without_a_root_step \
          case17_a_value_in_gitconfig_is_not_flagged_and_unset_replaces_the_record
finish
