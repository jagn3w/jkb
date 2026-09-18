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
# container" is a scratch directory; `-u root` runs the script with `chown` shimmed out, because
# the test is not root, and `tar` made to extract as root does. Each call is logged to $DOCKER_LOG as its verb and user.
make_stub() {
    stub_dir="$work/stub-$RANDOM"; mkdir -p "$stub_dir/bin" "$stub_dir/shim" "$stub_dir/root"
    printf '#!/bin/sh\nexit 0\n' > "$stub_dir/shim/chown"; chmod +x "$stub_dir/shim/chown"
    # ...and `tar` extracts the way it does AS ROOT, keeping each file's mode exactly (-p). As a
    # normal user GNU tar applies the umask on extraction, which silently did the mirror's chmod
    # for it: with the chmod deleted, this suite stayed green.
    printf '#!/bin/sh\nexec %s -p "$@"\n' "$(command -v tar)" > "$stub_dir/shim/tar"; chmod +x "$stub_dir/shim/tar"
    cat > "$stub_dir/bin/docker" <<'STUB'
#!/usr/bin/env bash
[ "$1" = exec ] || { echo "stub docker: only exec" >&2; exit 2; }
shift; user=vscode; interactive=0
while [ $# -gt 0 ]; do
    case "$1" in
        -i) interactive=1; shift ;;
        -u) user="$2"; shift 2 ;;
        *)  break ;;
    esac
done
shift   # the container name
printf '%s %s\n' "$user" "$1" >> "$DOCKER_LOG"
args=()
for a in "$@"; do case "$a" in /*) args+=("$CTR_ROOT$a") ;; *) args+=("$a") ;; esac; done
if [ "$user" = root ]; then PATH="$CHOWN_SHIM:$PATH" exec "${args[@]}"; fi
exec "${args[@]}"
STUB
    chmod +x "$stub_dir/bin/docker"
    export CTR_ROOT="$stub_dir/root" CHOWN_SHIM="$stub_dir/shim" DOCKER_LOG="$stub_dir/log"
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
                "~/.config/git/hooks=/home/vscode/.config/git/hooks" "~=/home/vscode"; do
        got="$(dc_container_hooks_dir "${pair%%=*}")" || got="(rc $?)"
        [ "$got" = "${pair#*=}" ] || bad="$bad [${pair%%=*} -> $got]"
    done
    for raw in "~other/hooks" ".githooks" "hooks" ""; do
        if got="$(dc_container_hooks_dir "$raw")"; then bad="$bad [$raw -> $got, wanted nothing]"; fi
    done
    if [ -z "$bad" ]; then ok "absolute and ~/ paths map to where git in the container looks; ~user, relative and empty map to nothing"
    else fail "absolute and ~/ paths map to where git in the container looks; ~user, relative and empty map to nothing" "$bad"; fi
}

# THE CASE THIS EXISTS FOR: the host's hooks arrive, runnable, at the path git resolves, with the
# symlinked one dereferenced (it would dangle in there), marked, and not writable by the group or
# others. The container-side step ran as root, and the parent under the home was made as vscode.
case2_a_mirror_arrives_runnable_marked_and_root_side() {
    make_stub; host_hooks
    local dst=/home/vscode/.config/git/hooks out rc
    out="$(dc_mirror_hooks "$src" "$dst" ctr "$docker_cmd" 2>&1)"; rc=$?
    local d="$CTR_ROOT$dst" mode
    mode="$(stat -c '%a' "$d/commit-msg" 2>/dev/null || stat -f '%Lp' "$d/commit-msg")"
    if [ "$rc" -eq 0 ] && [ "$("$d/commit-msg")" = "commit-msg ran" ] && [ ! -L "$d/post-merge" ] \
       && [ "$("$d/post-merge")" = "post-merge ran" ] && [ -f "$d/$DC_HOOKS_MIRROR_MARKER" ] \
       && [ "$mode" = 755 ] && grep -q '^root sh$' "$DOCKER_LOG" && grep -q '^vscode mkdir$' "$DOCKER_LOG"; then
        ok "the host's hooks arrive runnable, symlinks dereferenced, marked, a group-writable hook made 755, written by root"
    else
        fail "the host's hooks arrive runnable, symlinks dereferenced, marked, a group-writable hook made 755, written by root" \
             "rc=$rc out=$out mode=$mode log=$(tr '\n' ';' < "$DOCKER_LOG") ls=$(ls -la "$d" 2>&1)"
    fi
}

# Every start re-mirrors: an edited hook arrives, a deleted one goes. A copy that only ever added
# would keep running a hook the host had removed.
case3_a_re_mirror_replaces_rather_than_merges() {
    make_stub; host_hooks
    local dst=/Users/me/.config/git/hooks
    dc_mirror_hooks "$src" "$dst" ctr "$docker_cmd" >/dev/null 2>&1
    printf '#!/bin/sh\necho edited\n' > "$src/commit-msg"; rm "$src/post-merge"
    dc_mirror_hooks "$src" "$dst" ctr "$docker_cmd" >/dev/null 2>&1; local rc=$?
    local d="$CTR_ROOT$dst"
    if [ "$rc" -eq 0 ] && [ "$("$d/commit-msg")" = edited ] && [ ! -e "$d/post-merge" ] && [ ! -e "$d.jkb-new" ]; then
        ok "a second mirror carries an edit and drops a deleted hook, leaving no staging directory"
    else
        fail "a second mirror carries an edit and drops a deleted hook, leaving no staging directory" "rc=$rc $(ls -la "$d" "$d.jkb-new" 2>&1)"
    fi
}

# A directory at the target that the mirror did not make is somebody's, and is refused untouched.
case4_a_directory_it_did_not_make_is_left_alone() {
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
# git config is its own. Unset and relative values mirror nothing and never call docker.
case5_the_host_step_reads_the_global_value() {
    make_stub; host_hooks
    local cfg="$work/gitconfig-$RANDOM" out
    : > "$cfg"
    out="$(GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$docker_cmd" 2>&1)"
    local unset_ok=no; [[ "$out" == *"no global core.hooksPath"* ]] && [ ! -s "$DOCKER_LOG" ] && unset_ok=yes
    git config --file "$cfg" core.hooksPath .githooks
    out="$(GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$docker_cmd" 2>&1)"
    local rel_ok=no; [[ "$out" == *"needs no mirror"* ]] && [ ! -s "$DOCKER_LOG" ] && rel_ok=yes
    git config --file "$cfg" core.hooksPath "$src"
    out="$(GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$docker_cmd" 2>&1)"
    local abs_ok=no; [ "$("$CTR_ROOT$src/commit-msg" 2>/dev/null)" = "commit-msg ran" ] && abs_ok=yes
    git config --file "$cfg" core.hooksPath "$work/nowhere"
    out="$(GIT_CONFIG_GLOBAL="$cfg" dc_mirror_host_hooks ctr "$docker_cmd" 2>&1)"; local rc=$?
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
case6_setup_sh_in_remote_mode_rebuilds_the_binary_and_stops() {
    local d="$work/setup-$RANDOM"; mkdir -p "$d/home" "$d/cargo/bin"
    printf '#!/bin/sh\necho "cargo $*" >> "%s"\n' "$d/calls" > "$d/cargo/bin/cargo"
    printf '#!/bin/sh\necho "jkb $*" >> "%s"\necho "jkb 0.0.0-stub"\n' "$d/calls" > "$d/cargo/bin/jkb"
    chmod +x "$d/cargo/bin/cargo" "$d/cargo/bin/jkb"
    : > "$d/calls"
    local out rc
    out="$(HOME="$d/home" CARGO_HOME="$d/cargo" PATH="$d/cargo/bin:$PATH" JKB_REMOTE=host.docker.internal:7117 \
           bash "$repo_root/scripts/setup.sh" 2>&1)"; rc=$?
    local calls; calls="$(tr '\n' ';' < "$d/calls")"
    if [ "$rc" -eq 0 ] && [[ "$calls" == "cargo install --path crates/jkb-cli --locked --force;"* ]] \
       && [ "$(grep -c '^jkb ' "$d/calls")" = "$(grep -c '^jkb --version$' "$d/calls")" ] \
       && [[ "$out" == *"remote mode"* ]] && [[ "$out" != *"installing git hooks"* ]]; then
        ok "setup.sh with JKB_REMOTE rebuilds the binary, asks jkb nothing but its version, and stops before the hooks"
    else
        fail "setup.sh with JKB_REMOTE rebuilds the binary, asks jkb nothing but its version, and stops before the hooks" \
             "rc=$rc calls=$calls out=$(tail -5 <<<"$out")"
    fi
}

run_cases case1_the_container_path_is_what_git_in_there_resolves \
          case2_a_mirror_arrives_runnable_marked_and_root_side \
          case3_a_re_mirror_replaces_rather_than_merges \
          case4_a_directory_it_did_not_make_is_left_alone \
          case5_the_host_step_reads_the_global_value \
          case6_setup_sh_in_remote_mode_rebuilds_the_binary_and_stops
finish
