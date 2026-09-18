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
# git config is its own. Unset and relative values mirror nothing: no archive is ever extracted.
case5_the_host_step_reads_the_global_value() {
    need_gnu || return 0   # every host step ends in the root step that writes the record
    need_gnu || return 0
    make_stub; host_hooks
    local cfg="$HOME/.gitconfig" out
    rm -f "$cfg"
    : > "$cfg"
    out="$(GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" 2>&1)"
    local unset_ok=no; [[ "$out" == *"no core.hooksPath on the host"* ]] && [ ! -s "$TAR_LOG" ] && unset_ok=yes
    git config --file "$cfg" core.hooksPath .githooks
    out="$(GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" 2>&1)"
    local rel_ok=no; [[ "$out" == *"needs no mirror"* ]] && [ ! -s "$TAR_LOG" ] && rel_ok=yes
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
# remote_setup <1 if the stub cargo changes the binary, 0 if not> -- run a COPY of setup.sh in remote
# mode (see CONFINED below) and set $out, $rc, $calls and $d.
remote_setup() { # remote_setup <0|1> [setup.sh flags...]
    local t changes="$1"; shift
    d="$work/setup-$RANDOM"
    mkdir -p "$d/home" "$d/cargo/bin" "$d/repo/scripts/hooks"
    cp "$repo_root/scripts/setup.sh" "$repo_root/scripts/lib.sh" "$d/repo/scripts/"
    cp "$repo_root/scripts/hooks/post-merge" "$d/repo/scripts/hooks/"
    git init -q "$d/repo"
    printf '#!/bin/sh\necho "jkb $*" >> "%s"\necho "jkb 0.0.0-stub"\n' "$d/calls" > "$d/cargo/bin/jkb"
    # The stub cargo "installs" by appending to the jkb stub when asked to change it, which is what
    # a rebuild from changed sources does to the binary's bytes.
    if [ "$changes" = 1 ]; then
        printf '#!/bin/sh\necho "cargo $*" >> "%s"\necho "# rebuilt" >> "%s"\n' "$d/calls" "$d/cargo/bin/jkb" > "$d/cargo/bin/cargo"
    else
        printf '#!/bin/sh\necho "cargo $*" >> "%s"\n' "$d/calls" > "$d/cargo/bin/cargo"
    fi
    printf '#!/bin/sh\necho link-claude-memory >> "%s"\n' "$d/calls" > "$d/repo/scripts/link-claude-memory.sh"
    chmod +x "$d/repo/scripts/link-claude-memory.sh"
    for t in systemctl launchctl pnpm code; do
        printf '#!/bin/sh\necho "%s $*" >> "%s"\nexit 1\n' "$t" "$d/calls" > "$d/cargo/bin/$t"
    done
    chmod +x "$d/cargo/bin/"*
    : > "$d/calls"
    rc=0
    out="$(HOME="$d/home" CARGO_HOME="$d/cargo" PATH="$d/cargo/bin:$PATH" JKB_REMOTE=host.docker.internal:7117 \
           bash "$d/repo/scripts/setup.sh" --no-extension --no-service "$@" 2>&1)" || rc=$?
    calls="$(tr '\n' ';' < "$d/calls")"
}

# CONFINED, because the watch-it-fail step deletes the very guard under test, and then setup.sh runs
# its host installer for real (a review found that: services restarted, VS Code's extension
# installed, hooks written into this checkout). So it runs a COPY of setup.sh in a scratch repo, with
# --no-extension --no-service, and with every service manager and installer it could reach on PATH
# as a stub that fails and logs. Mutated, it can only write under $d.
case6_setup_sh_in_remote_mode_rebuilds_the_binary_and_stops() {
    remote_setup 1
    if [ "$rc" -eq 0 ] && [[ "$calls" == "cargo install --path crates/jkb-cli --locked --force;"* ]] \
       && [ "$(grep -c '^jkb ' "$d/calls")" = "$(grep -c '^jkb --version$' "$d/calls")" ] \
       && [[ "$out" == *"remote mode"* ]] && [[ "$out" != *"installing git hooks"* ]] \
       && [[ "$out" == *"this jkb changed"* ]] && [[ "$out" == *".container/install-extensions.sh"* ]]; then
        ok "setup.sh with JKB_REMOTE rebuilds the binary, asks jkb nothing but its version, warns when it changed, names the container's extension step, and stops before the hooks"
    else
        fail "setup.sh with JKB_REMOTE rebuilds the binary, asks jkb nothing but its version, warns when it changed, names the container's extension step, and stops before the hooks" \
             "rc=$rc calls=$calls out=$(tail -8 <<<"$out")"
    fi
}

# ...and stays quiet when the rebuild produced the same binary (a pull touching only scripts/):
# a warning on every pull is one nobody reads.
case19_setup_sh_in_remote_mode_is_quiet_when_the_binary_did_not_change() {
    remote_setup 0
    if [ "$rc" -eq 0 ] && [[ "$out" == *"remote mode"* ]] && [[ "$out" != *"this jkb changed"* ]]; then
        ok "setup.sh with JKB_REMOTE does not warn about the server when the rebuild changed nothing"
    else
        fail "setup.sh with JKB_REMOTE does not warn about the server when the rebuild changed nothing" "rc=$rc out=$(tail -8 <<<"$out")"
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
# nothing is extracted, and nothing says hooks are missing.
case8_a_hooks_path_inside_a_bind_is_left_to_the_bind() {
    need_gnu || return 0   # every host step ends in the root step that writes the record
    make_stub
    local cfg="$HOME/.gitconfig" out
    rm -f "$cfg"
    git config --file "$cfg" core.hooksPath "~/repos/dotfiles/hooks"
    mkdir -p "$HOME/repos/dotfiles/hooks"
    out="$(GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" 2>&1)"
    if [ ! -s "$TAR_LOG" ] && [[ "$out" == *"already runs the host's own copy"* ]]; then
        ok "a hooksPath inside a bind mount is left to the bind: nothing copied"
    else
        fail "a hooksPath inside a bind mount is left to the bind: nothing copied" "out=$out log=$(tr '\n' ';' < "$DOCKER_LOG")"
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


# A split config: ~/.gitconfig includes another file, and THAT sets core.hooksPath. The effective
# read follows includes; the container cannot see the included file, so the value reaches git in
# there through the ~/.config/git/config the root step writes, not through VS Code's copy.
case12_a_hooks_path_set_through_an_include_is_found() {
    need_gnu || return 0
    make_stub; host_hooks
    local cfg="$HOME/.gitconfig" inc="$work/included-$RANDOM" got xdg
    rm -f "$cfg"
    git config --file "$inc" core.hooksPath "$src"
    git config --file "$cfg" include.path "$inc"
    got="$(GIT_CONFIG_GLOBAL="$cfg" dc_global_hooks_path)"
    GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" >/dev/null 2>&1
    xdg="$(git config --file "$CTR_ROOT$DC_HOOKS_XDG_CONFIG" --get core.hooksPath 2>/dev/null)"
    if [ "$got" = "$src" ] && [ "$("$CTR_ROOT$src/commit-msg" 2>/dev/null)" = "commit-msg ran" ] \
       && [ "$xdg" = "$src" ] && grep -qx "origin=$inc" "$CTR_ROOT$DC_HOST_HOOKS_RECORD"; then
        ok "a hooksPath from an included file is read, mirrored, recorded with its origin, and written to the container's ~/.config/git/config"
    else
        fail "a hooksPath from an included file is read, mirrored, recorded with its origin, and written to the container's ~/.config/git/config" "got=$got xdg=$xdg record=$(cat "$CTR_ROOT$DC_HOST_HOOKS_RECORD" 2>&1)"
    fi
}

# Set to the empty string, git reads "/" and runs nothing: that is said, and nothing is copied. The
# reader tells it apart from unset by its exit status, which is what verify.sh's `empty` arm needs.
case13_an_empty_value_is_not_unset() {
    need_gnu || return 0   # every host step ends in the root step that writes the record
    make_stub
    local cfg="$HOME/.gitconfig" out rc_empty rc_unset
    rm -f "$cfg"
    : > "$cfg"
    GIT_CONFIG_GLOBAL="$cfg" dc_global_hooks_path >/dev/null; rc_unset=$?
    git config --file "$cfg" core.hooksPath ""
    GIT_CONFIG_GLOBAL="$cfg" dc_global_hooks_path >/dev/null; rc_empty=$?
    out="$(GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" 2>&1)"
    if [ "$rc_unset" -eq 1 ] && [ "$rc_empty" -eq 0 ] && [[ "$out" == *"empty string"* ]] && [ ! -s "$TAR_LOG" ]; then
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


# Every start rewrites the record, root-owned, and the container's ~/.config/git/config with it: a
# host that drops the setting leaves neither a stale value in the record nor one in that file.
case15_the_host_value_is_recorded_on_every_start() {
    need_gnu || return 0
    make_stub; host_hooks
    local cfg="$HOME/.gitconfig" rec="$CTR_ROOT$DC_HOST_HOOKS_RECORD" xdg="$CTR_ROOT$DC_HOOKS_XDG_CONFIG" set_ok=no unset_ok=no
    rm -f "$cfg"; git config --file "$cfg" core.hooksPath "$src"
    GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" >/dev/null 2>&1
    grep -qx "state=value" "$rec" && grep -qx "value=$src" "$rec" && grep -qx "origin=$cfg" "$rec" \
        && grep -qx "mirror=ok" "$rec" && grep -qx "applied=$src" "$rec" \
        && [ "$(git config --file "$xdg" --get core.hooksPath)" = "$src" ] && set_ok=yes
    : > "$cfg"
    GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" >/dev/null 2>&1
    [ "$(head -1 "$rec")" = state=unset ] && grep -qx "applied=-" "$rec" \
        && ! git config --file "$xdg" --get core.hooksPath >/dev/null 2>&1 && unset_ok=yes
    if [ "$set_ok$unset_ok" = yesyes ]; then ok "every start records the host's state, value, origin and mirror outcome, and sets or unsets the key in the container's hooks config"
    else fail "every start records the host's state, value, origin and mirror outcome, and sets or unsets the key in the container's hooks config" "set=$set_ok unset=$unset_ok record=$(cat "$rec" 2>&1) xdg=$(cat "$xdg" 2>&1)"; fi
}


# The record's READER, which verify.sh uses, against the format its writer emits. Pure file reads,
# so it runs on a Mac without GNU tools. The round-4 review found nothing covered it: not the -nt
# freshness guard, not the parse.
case16_the_record_reader() {
    local d="$work/rec-$RANDOM" got bad=""
    mkdir -p "$d"; : > "$d/marker"; sleep 1
    printf 'state=value\nvalue=\norigin=/h/.gitconfig\nmirror=failed\n' > "$d/rec"
    got="$(dc_read_host_record "$d/rec" "$d/marker" | tr '\037' '|')"
    [ "$got" = "value||/h/.gitconfig|failed|-" ] || bad="$bad [empty value, no applied line: $got]"
    printf 'state=value\nvalue=.githooks\norigin=/h/.gitconfig\nmirror=\napplied=.githooks\n' > "$d/rec"
    got="$(dc_read_host_record "$d/rec" "$d/marker" | tr '\037' '|')"
    [ "$got" = "value|.githooks|/h/.gitconfig||.githooks" ] || bad="$bad [applied: $got]"
    printf 'state=unset\n' > "$d/rec"
    got="$(dc_read_host_record "$d/rec" "$d/marker" | tr '\037' '|')"
    [ "$got" = "unset||||-" ] || bad="$bad [unset: $got]"
    got="$(dc_read_host_record "$d/rec" "$d/no-marker" | tr '\037' '|')"
    [ "$got" = "unset||||-" ] || bad="$bad [no marker: $got]"
    sleep 1; : > "$d/marker"   # a start after the record was written
    got="$(dc_read_host_record "$d/rec" "$d/marker" | tr '\037' '|')"
    [ "$got" = "none||||-" ] || bad="$bad [older than this start: $got]"
    got="$(dc_read_host_record "$d/absent" "$d/marker" | tr '\037' '|')"
    [ "$got" = "none||||-" ] || bad="$bad [absent: $got]"
    if [ -z "$bad" ]; then ok "the record reader keeps empty fields, and reads a record older than this start, or none, as none"
    else fail "the record reader keeps empty fields, and reads a record older than this start, or none, as none" "$bad"; fi
}

# The container's ~/.config/git/config is SHARED with whatever else lives there: run.sh sets one key
# and leaves every other line as it was.
case17_the_hooks_config_keeps_everything_else() {
    need_gnu || return 0
    make_stub; host_hooks
    local cfg="$HOME/.gitconfig" xdg="$CTR_ROOT$DC_HOOKS_XDG_CONFIG"
    mkdir -p "$(dirname "$xdg")"; printf '[user]\n\tname = mine\n' > "$xdg"
    rm -f "$cfg"; git config --file "$cfg" core.hooksPath "$src"
    GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" >/dev/null 2>&1
    if [ "$(git config --file "$xdg" --get user.name)" = mine ] && [ "$(git config --file "$xdg" --get core.hooksPath)" = "$src" ]; then
        ok "the hooks config keeps the other settings in the file and sets only core.hooksPath"
    else
        fail "the hooks config keeps the other settings in the file and sets only core.hooksPath" "$(cat "$xdg")"
    fi
}

# The writer and the reader agree: the root step's real output, read back by the function verify.sh
# uses, yields the fields it was given.
case18_the_writer_and_reader_agree() {
    need_gnu || return 0
    make_stub; host_hooks
    local cfg="$HOME/.gitconfig" got
    rm -f "$cfg"; git config --file "$cfg" core.hooksPath "$src"
    GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" >/dev/null 2>&1
    got="$(dc_read_host_record "$CTR_ROOT$DC_HOST_HOOKS_RECORD" "$work/no-marker" | tr '\037' '|')"
    if [ "$got" = "value|$src|$cfg|ok|$src" ]; then ok "the record the root step writes reads back field for field"
    else fail "the record the root step writes reads back field for field" "got=$got"; fi
}


# The reader answers what git uses OUTSIDE any repository, whatever directory it is called from: a
# repository's own core.hooksPath travels with the repository and is not the host's setting.
case20_the_reader_ignores_the_repository_it_is_called_from() {
    local r="$work/repo-$RANDOM" cfg="$HOME/.gitconfig" got
    git init -q "$r"; git -C "$r" config core.hooksPath .local-hooks
    rm -f "$cfg"; git config --file "$cfg" core.hooksPath /global/hooks
    got="$(cd "$r" && GIT_CONFIG_GLOBAL="$cfg" dc_global_hooks_path)"
    if [ "$got" = /global/hooks ]; then ok "the reader answers the setting outside any repository, even when called from inside one that sets its own"
    else fail "the reader answers the setting outside any repository, even when called from inside one that sets its own" "got=$got"; fi
}


# THE ROUND-5 MUST-FIX. In a container with no ~/.gitconfig yet, `git config --global` writes the
# XDG file, rewriting it through a lock and a rename. With the file root-owned, that handed it to the
# container user and every later start refused it. Driven with real git: after the user's write,
# the next start still sets the key, keeps the user's line, and says nothing is wrong.
case21_a_user_write_to_the_config_does_not_freeze_the_hooks_path() {
    need_gnu || return 0
    make_stub; host_hooks
    local cfg="$HOME/.gitconfig" xdg="$CTR_ROOT$DC_HOOKS_XDG_CONFIG" out ctrhome="$CTR_ROOT/home/vscode"
    rm -f "$cfg"; git config --file "$cfg" core.hooksPath "$src"
    GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" >/dev/null 2>&1
    # What the user does in there: no ~/.gitconfig, so --global means the XDG file.
    env -u GIT_CONFIG_GLOBAL -u XDG_CONFIG_HOME HOME="$ctrhome" git config --global user.email me@example.com
    printf '#!/bin/sh\necho edited\n' > "$src/commit-msg"
    out="$(GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" 2>&1)"
    if [ "$(git config --file "$xdg" --get user.email)" = me@example.com ] \
       && [ "$(git config --file "$xdg" --get core.hooksPath)" = "$src" ] \
       && [ "$("$CTR_ROOT$src/commit-msg")" = edited ] && [[ "$out" != *warning* ]]; then
        ok "after the user's own git config --global in there, the next start still sets the hooks path and keeps their setting"
    else
        fail "after the user's own git config --global in there, the next start still sets the hooks path and keeps their setting" "out=$out xdg=$(cat "$xdg")"
    fi
}

# A relative host value means the same in there (git resolves it in each repository), so it is
# applied as it is, and recorded as applied: round 5 found it applied as nothing, and 3e agreeing.
case22_a_relative_value_is_applied_as_it_is() {
    need_gnu || return 0
    make_stub
    local cfg="$HOME/.gitconfig" xdg="$CTR_ROOT$DC_HOOKS_XDG_CONFIG" rec="$CTR_ROOT$DC_HOST_HOOKS_RECORD"
    rm -f "$cfg"; git config --file "$cfg" core.hooksPath .githooks
    GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$repo_root/.container/container.json" "$docker_cmd" >/dev/null 2>&1
    if [ "$(git config --file "$xdg" --get core.hooksPath)" = .githooks ] && grep -qx "applied=.githooks" "$rec" && [ ! -s "$TAR_LOG" ]; then
        ok "a relative hooks path is applied as it is, recorded as applied, and nothing is copied"
    else
        fail "a relative hooks path is applied as it is, recorded as applied, and nothing is copied" "xdg=$(cat "$xdg" 2>&1) rec=$(cat "$rec" 2>&1)"
    fi
}

# Remote mode honours --link-memory (valid in the container), before it stops.
case23_setup_sh_in_remote_mode_honours_link_memory() {
    remote_setup 0 --link-memory
    if [ "$rc" -eq 0 ] && grep -qx "link-claude-memory" "$d/calls" && [[ "$out" == *"remote mode"* ]] \
       && [[ "$out" != *"installing git hooks"* ]]; then
        ok "setup.sh with JKB_REMOTE and --link-memory links memory, then stops"
    else
        fail "setup.sh with JKB_REMOTE and --link-memory links memory, then stops" "rc=$rc calls=$calls out=$(tail -5 <<<"$out")"
    fi
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
          case16_the_record_reader \
          case17_the_hooks_config_keeps_everything_else \
          case18_the_writer_and_reader_agree \
          case19_setup_sh_in_remote_mode_is_quiet_when_the_binary_did_not_change \
          case20_the_reader_ignores_the_repository_it_is_called_from \
          case21_a_user_write_to_the_config_does_not_freeze_the_hooks_path \
          case22_a_relative_value_is_applied_as_it_is \
          case23_setup_sh_in_remote_mode_honours_link_memory
finish
