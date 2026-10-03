#!/usr/bin/env bash
# `dc_persist_login` (.container/lib.sh): carrying the container's Claude login into the state
# volume after Claude Code has replaced the link with a regular file.
#
# Why it exists: the login used to be a symlink made once at setup, on the theory that Claude Code
# writes through it. It does not for the credential file — a login leaves a regular file where the
# link was — so the login lived in the container's writable layer and a rebuild lost it, while
# verify.sh reported the one symptom as a failure. These cases pin the repair against a scratch
# home, so they need no container.
set -uo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=.container/lib.sh
. "$repo_root/.container/lib.sh"
# shellcheck source=scripts/tests/harness.sh
. "$(dirname "$0")/harness.sh"

new_workdir

# fresh_home — an empty scratch home, as a new container has before setup.
fresh_home() { h="$work/home-$RANDOM"; mkdir -p "$h"; }

cred() { printf '%s' "$h/.claude/.credentials.json"; }
vol_cred() { printf '%s' "$h/.claude-state/.credentials.json"; }

# The list is the contract verify.sh asserts, so its exact shape is pinned: two files, link:target.
case1_the_login_files_are_the_two_known_pairs() {
    local got want
    got="$(dc_login_files /h)"
    want="$(printf '%s\n' "/h/.claude/.credentials.json:/h/.claude-state/.credentials.json" \
                          "/h/.claude.json:/h/.claude-state/claude.json")"
    if [ "$got" = "$want" ]; then ok "dc_login_files names the credential and account-state pairs"
    else fail "dc_login_files names the credential and account-state pairs" "got: $got"; fi
}

# A new container: nothing exists yet, so both links are made dangling and nothing is reported.
case2_fresh_home_gets_dangling_links() {
    fresh_home
    local out; out="$(dc_persist_login "$h")"; local rc=$?
    if [ "$rc" -eq 0 ] && [ -z "$out" ] && [ -L "$(cred)" ] && [ "$(readlink "$(cred)")" = "$(vol_cred)" ] \
       && [ -L "$h/.claude.json" ] && [ ! -e "$(vol_cred)" ]; then
        ok "a fresh home gets both links, dangling, and reports nothing"
    else
        fail "a fresh home gets both links, dangling, and reports nothing" "rc=$rc out=$out"
    fi
}

# THE CASE THIS EXISTS FOR. A login replaced the link with a regular file; the volume holds an
# older token. The newer one must end up in the volume, the link must come back, and the file's
# owner-only mode must survive the move — a credential widened to 0644 in transit is its own bug.
case3_a_replaced_link_is_carried_into_the_volume() {
    fresh_home
    dc_persist_login "$h" >/dev/null
    printf 'old-token' > "$(vol_cred)"
    rm "$(cred)"; printf 'new-token' > "$(cred)"; chmod 600 "$(cred)"
    local out; out="$(dc_persist_login "$h")"; local rc=$?
    local mode; mode="$(stat -c '%a' "$(vol_cred)" 2>/dev/null || stat -f '%Lp' "$(vol_cred)")"
    if [ "$rc" -eq 0 ] && [ "$(cat "$(vol_cred)")" = "new-token" ] && [ -L "$(cred)" ] \
       && [ "$(readlink "$(cred)")" = "$(vol_cred)" ] && [ "$mode" = 600 ] \
       && [ "$out" = "moved $(cred) into the state volume" ]; then
        ok "a regular credential file replaces the volume copy, is linked again, keeps mode 600, and is reported"
    else
        fail "a regular credential file replaces the volume copy, is linked again, keeps mode 600, and is reported" \
             "rc=$rc out=$out vol=$(cat "$(vol_cred)" 2>/dev/null) mode=$mode link=$(readlink "$(cred)")"
    fi
}

# The account-state file is carried by the same rule, not remembered separately.
case4_the_account_state_file_is_carried_too() {
    fresh_home
    dc_persist_login "$h" >/dev/null
    rm "$h/.claude.json"; printf '{"n":2}' > "$h/.claude.json"
    dc_persist_login "$h" >/dev/null
    if [ -L "$h/.claude.json" ] && [ "$(cat "$h/.claude-state/claude.json")" = '{"n":2}' ]; then
        ok "a regular ~/.claude.json is carried into the volume and linked again"
    else
        fail "a regular ~/.claude.json is carried into the volume and linked again" "$(ls -la "$h" "$h/.claude-state")"
    fi
}

# Already healthy: nothing moves, nothing is said, the volume copy is untouched.
case5_a_healthy_link_is_left_alone() {
    fresh_home
    dc_persist_login "$h" >/dev/null
    printf 'token' > "$(vol_cred)"
    local out; out="$(dc_persist_login "$h")"; local rc=$?
    if [ "$rc" -eq 0 ] && [ -z "$out" ] && [ "$(cat "$(cred)")" = "token" ]; then
        ok "a healthy link is left alone and nothing is reported"
    else
        fail "a healthy link is left alone and nothing is reported" "rc=$rc out=$out"
    fi
}

# A link somewhere else is pointed back at the volume.
case6_a_link_elsewhere_is_repointed() {
    fresh_home
    dc_persist_login "$h" >/dev/null
    ln -sfn "$work/elsewhere" "$(cred)"
    dc_persist_login "$h" >/dev/null
    if [ "$(readlink "$(cred)")" = "$(vol_cred)" ]; then ok "a link to anywhere else is pointed back at the volume"
    else fail "a link to anywhere else is pointed back at the volume" "readlink=$(readlink "$(cred)")"; fi
}

# A directory at a link site: `ln -sfn` would make a link INSIDE it and succeed, so the function
# has to refuse and say so rather than report the file handled.
case7_a_directory_is_refused_not_masked() {
    fresh_home
    dc_persist_login "$h" >/dev/null
    rm "$(cred)"; mkdir "$(cred)"
    local err; err="$(dc_persist_login "$h" 2>&1 >/dev/null)"; local rc=$?
    if [ "$rc" -eq 1 ] && [ -d "$(cred)" ] && [ ! -L "$(cred)" ] && [ ! -e "$(cred)/.credentials.json" ] \
       && [[ "$err" == *"is a directory"* ]]; then
        ok "a directory at the credential link is refused with a message, and nothing is linked inside it"
    else
        fail "a directory at the credential link is refused with a message, and nothing is linked inside it" "rc=$rc err=$err"
    fi
}

# setup.sh reaches it through dc_link_state, so a regular file present at setup is carried too.
case8_dc_link_state_carries_the_login() {
    fresh_home
    mkdir -p "$h/.claude"; printf 'setup-token' > "$(cred)"
    dc_link_state "$h" >/dev/null
    if [ -L "$(cred)" ] && [ "$(cat "$(vol_cred)")" = "setup-token" ]; then
        ok "dc_link_state carries a regular credential file into the volume"
    else
        fail "dc_link_state carries a regular credential file into the volume" "$(ls -la "$h/.claude" "$h/.claude-state")"
    fi
}

# THE SETUP CASE A REVIEW CAUGHT. A recreated container: nothing here has linked yet, the image
# (or anything before setup) left a regular ~/.claude.json, and the volume holds the account state
# carried from the last container. The volume's copy must win, and the home file must be set aside
# rather than lost. "The home copy is always newer" moved the image's stub over it on every rebuild.
case9_at_setup_the_volume_copy_wins_over_an_image_file() {
    fresh_home
    mkdir -p "$h/.claude-state" "$h/.claude"
    printf '{"carried":true}' > "$h/.claude-state/claude.json"
    printf '{"image":true}' > "$h/.claude.json"
    local out; out="$(dc_persist_login "$h")"; local rc=$?
    if [ "$rc" -eq 0 ] && [ -L "$h/.claude.json" ] \
       && [ "$(cat "$h/.claude-state/claude.json")" = '{"carried":true}' ] \
       && [ "$(cat "$h/.claude.json.pre-link")" = '{"image":true}' ] \
       && [[ "$out" == *"kept the state volume's"* ]]; then
        ok "at setup the volume's carried copy wins, and the image's file is set aside, not lost"
    else
        fail "at setup the volume's carried copy wins, and the image's file is set aside, not lost" \
             "rc=$rc out=$out vol=$(cat "$h/.claude-state/claude.json")"
    fi
}

# A failed move is recorded, so verify.sh can fail it instead of calling it pending forever; the
# next clean run clears the record.
case10_a_failed_move_is_recorded_and_then_cleared() {
    fresh_home
    dc_persist_login "$h" >/dev/null
    rm "$(cred)"; printf 'token' > "$(cred)"
    chmod 555 "$h/.claude-state"
    dc_persist_login "$h" >/dev/null 2>&1; local rc=$?
    local recorded=no; [ -s "$h/$DC_LOGIN_CARRY_FAILED" ] && recorded=yes
    chmod 755 "$h/.claude-state"
    dc_persist_login "$h" >/dev/null 2>&1; local rc2=$?
    if [ "$rc" -eq 1 ] && [ "$recorded" = yes ] && [ "$(cat "$(cred)" 2>/dev/null)" = token ] \
       && [ "$rc2" -eq 0 ] && [ ! -e "$h/$DC_LOGIN_CARRY_FAILED" ] && [ -L "$(cred)" ]; then
        ok "a failed move keeps the file, is recorded, and the next clean run carries it and clears the record"
    else
        fail "a failed move keeps the file, is recorded, and the next clean run carries it and clears the record" \
             "rc=$rc recorded=$recorded rc2=$rc2"
    fi
}

# run.sh's --stop and --rm, against a stub `docker` that logs its calls. HOME/repos points at this
# checkout's parent so run.sh's container_path resolves as it does on a real host.
# STUB_STATE is what `docker inspect` answers: true, false, or missing (no such container).
# THE CHECKOUT'S run.sh REFUSES to start or stop anything unless JKB_RUN_FROM_CHECKOUT=1 (lib.sh's
# DC_KIT_DIR says why), so these cases set it -- RS_SCRIPT and RS_ENV let the kit cases below run
# the KIT's run.sh, or the checkout's without the override, through the same stub.
run_sh_with_stub() { # run_sh_with_stub <state> <flag> -> sets $calls (one docker call per line) and $execs
    local d="$work/rs-$RANDOM"; mkdir -p "$d/bin" "$d/home"
    rs_home="$d/home"
    ln -s "$(dirname "$repo_root")" "$d/home/repos"
    # PATHS BAKED IN, not read from the environment: run.sh re-executes itself under `env -i` with an
    # allowlist (review round 23), so a STUB_LOG in the env would never reach the stub. It also records
    # the environment it was run with, so a case can ask what run.sh's children inherit.
    cat > "$d/bin/docker" <<STUB
#!/usr/bin/env bash
printf '%s\\n' "\$*" >> "$d/calls"
env >> "$d/child-env"
case "\$1" in
    inspect) [ "$1" = missing ] && exit 1; printf '%s\\n' "$1" ;;
esac
exit 0
STUB
    chmod +x "$d/bin/docker"
    : > "$d/calls"
    # RS_GONE: install from a scratch copy of the checkout and delete it before running, as
    # `jkb task land` deletes a worktree a kit could have been installed from.
    local kit_src="$repo_root" p
    if [ -n "${RS_GONE:-}" ]; then
        kit_src="$d/gone-checkout"; mkdir -p "$kit_src/scripts"
        cp -R "$repo_root/.container" "$kit_src/.container"
        for p in lib.sh link-claude-memory.sh auto-mode.sh auto-mode-posture.json; do cp "$repo_root/scripts/$p" "$kit_src/scripts/$p"; done
    fi
    [ -z "${RS_KIT:-}" ] || bash -c '. "$1/.container/lib.sh" && dc_install_kit "$1" "$2"' _ "$kit_src" "$d/home/.local/share/jkb-container-kit/kit" >/dev/null 2>&1
    [ -z "${RS_GONE:-}" ] || rm -rf "$kit_src"
    local script="$repo_root/.container/run.sh"
    [ -z "${RS_KIT:-}" ] || script="$d/home/.local/share/jkb-container-kit/kit/.container/run.sh"
    # The keep file: run.sh drops PATH entries under /tmp, where this stub lives (see its top).
    mkdir -p "$d/home/.local/share/jkb-container-kit"; printf '%s\n' "$d/bin" > "$d/home/.local/share/jkb-container-kit/path-keep"
    rs_dir="$d"
    env ${RS_ENV_I:+-i} HOME="$d/home" PATH="${RS_PATH_PREFIX:+$RS_PATH_PREFIX:}$d/bin:$PATH" ${RS_ENV:-JKB_RUN_FROM_CHECKOUT=1} ${RS_EXTRA_ENV:-} \
        ${RS_FUNC:+"BASH_FUNC_jkbx%%=$RS_FUNC"} \
        bash "$script" "$2" >"$d/out" 2>&1
    rs_out="$(cat "$d/out")"
    calls="$(cut -d' ' -f1 "$d/calls" | tr '\n' ' ')"
    execs="$(grep '^exec ' "$d/calls" || true)"
}

case11_run_sh_carries_the_login_before_stop_and_rm() {
    local flag verb
    for flag in --stop --rm; do
        verb=stop; [ "$flag" = --rm ] && verb=rm
        run_sh_with_stub true "$flag"
        if [ "$calls" = "inspect exec $verb " ]; then ok "run.sh $flag on a running container carries the login, then ${verb}s"
        else fail "run.sh $flag on a running container carries the login, then ${verb}s" "calls: $calls"; fi
    done
}

# The review's second finding: a stopped container is the one most likely to hold a refreshed
# token, so it is started to carry it rather than skipped.
case12_run_sh_starts_a_stopped_container_to_carry_it() {
    local flag verb
    for flag in --stop --rm; do
        verb=stop; [ "$flag" = --rm ] && verb=rm
        run_sh_with_stub false "$flag"
        if [ "$calls" = "inspect start exec $verb " ]; then ok "run.sh $flag on a stopped container starts it, carries the login, then ${verb}s"
        else fail "run.sh $flag on a stopped container starts it, carries the login, then ${verb}s" "calls: $calls"; fi
    done
    run_sh_with_stub missing --rm
    if [ "$calls" = "inspect rm " ]; then ok "run.sh --rm with no container carries nothing and still removes"
    else fail "run.sh --rm with no container carries nothing and still removes" "calls: $calls"; fi
}

# THE KIT. From a checkout, run.sh starts and stops nothing without the developer's override, and
# runs no docker command at all; it names the kit's run.sh instead (review round 8's self-review:
# the checkout's run.sh is a file the agent can write, and it ran as you).
case13_the_checkouts_run_sh_refuses_without_the_override() {
    RS_ENV=JKB_RUN_FROM_CHECKOUT=0 run_sh_with_stub true --stop
    if [ -z "$calls" ] && grep -q 'installed kit' <<<"$rs_out"; then ok "the checkout's run.sh --stop refuses, runs no docker command, and names the kit"
    else fail "the checkout's run.sh --stop refuses, runs no docker command, and names the kit" "calls: $calls out: $rs_out"; fi
}

# From the kit, with no override, it stops -- and the login step sources lib.sh from the root-owned
# mirror, never from the checkout.
case14_the_kits_run_sh_stops_and_sources_the_mirror() {
    RS_KIT=1 RS_ENV=JKB_RUN_FROM_CHECKOUT=0 run_sh_with_stub true --stop
    if [ "$calls" = "inspect exec stop " ] && grep -qF '/usr/local/lib/jkb-container/.container/lib.sh' <<<"$execs" \
       && ! grep -qE "(^| |')\.container/lib\.sh" <<<"$execs"; then
        ok "the kit's run.sh --stop carries the login from the mirror's lib.sh, then stops"
    else fail "the kit's run.sh --stop carries the login from the mirror's lib.sh, then stops" "calls: $calls execs: $execs out: $rs_out"; fi
}

# A KIT WHOSE CHECKOUT IS GONE still stops and removes its container; only a start or an install
# needs the checkout (review round 10: it died before reading its arguments, --stop included).
case15_a_kit_whose_checkout_is_gone_still_stops() {
    RS_KIT=1 RS_GONE=1 RS_ENV=JKB_RUN_FROM_CHECKOUT=0 run_sh_with_stub true --stop
    local stop_calls="$calls" stop_out="$rs_out"
    RS_KIT=1 RS_GONE=1 RS_ENV=JKB_RUN_FROM_CHECKOUT=0 run_sh_with_stub true --dry-run
    if [ "$stop_calls" = "stop " ] && grep -q 'is gone' <<<"$stop_out" && grep -q 'no longer exists' <<<"$rs_out"; then
        ok "a kit whose checkout is gone still stops its container, saying so, and refuses a start"
    else
        fail "a kit whose checkout is gone still stops its container, saying so, and refuses a start" "stop calls: $stop_calls stop out: $stop_out start out: $rs_out"
    fi
}

# NO PROGRAM FROM A PLACE AN AGENT CAN WRITE: a jq planted first on PATH, in ~/.cargo/bin, does not
# run when run.sh does (review round 11 -- the host posture lets a sandboxed agent write ~/.cargo).
case16_a_program_planted_on_path_under_home_does_not_run() {
    # OUTSIDE THE TEMP ROOTS: under /tmp the filter's /tmp arm drops the entry, so the case could not
    # fail with the "$HOME" arm removed (review round 18). The real ~/.cache is writable here and on CI.
    mkdir -p "$HOME/.cache" 2>/dev/null
    local h; h="$(mktemp -d "$HOME/.cache/jkb-plant.XXXXXX")" || { fail "case16" "no scratch home under ~/.cache"; return; }
    mkdir -p "$h/.cargo/bin"
    printf '#!/bin/sh\n: > "%s/RAN"\nexit 0\n' "$h" > "$h/.cargo/bin/jq"; chmod +x "$h/.cargo/bin/jq"
    env HOME="$h" PATH="$h/.cargo/bin:$PATH" JKB_RUN_FROM_CHECKOUT=1 bash "$repo_root/.container/run.sh" --print-args >/dev/null 2>&1
    local rc=$?
    if [ ! -e "$h/RAN" ] && [ "$rc" -eq 0 ]; then ok "a jq planted in ~/.cargo/bin, first on PATH, does not run; run.sh uses the system one"
    else fail "a jq planted in ~/.cargo/bin, first on PATH, does not run; run.sh uses the system one" "rc=$rc ran=$([ -e "$h/RAN" ] && echo yes || echo no)"; fi
    case "$h" in */jkb-plant.*) rm -rf -- "$h" ;; esac
}

# A TOOL THE FILTER HID IS NAMED, with where it was and the way to keep it: a per-user Docker in
# ~/.docker/bin failed as a bare "docker is not on PATH" (review round 12). Skipped where a docker
# outside the home would be found anyway.
case17_a_tool_the_path_filter_hid_is_named() {
    if type -P docker >/dev/null 2>&1; then skip "case17: a docker outside the home is on PATH here, so the filter hides nothing"; return 0; fi
    mkdir -p "$HOME/.cache" 2>/dev/null
    local h out; h="$(mktemp -d "$HOME/.cache/jkb-hid.XXXXXX")" || { fail "case17" "no scratch home under ~/.cache"; return; }
    mkdir -p "$h/.docker/bin"; printf '#!/bin/sh\nexit 0\n' > "$h/.docker/bin/docker"; chmod +x "$h/.docker/bin/docker"
    ln -s "$(dirname "$repo_root")" "$h/repos"
    out="$(env HOME="$h" PATH="$h/.docker/bin:$PATH" JKB_RUN_FROM_CHECKOUT=1 bash "$repo_root/.container/run.sh" 2>&1)"
    if grep -qF "docker is in $h/.docker/bin" <<<"$out" && grep -qF "jkb-container-kit/path-keep" <<<"$out"; then
        ok "a docker the PATH filter hid is named with its directory and how to keep it"
    else fail "a docker the PATH filter hid is named with its directory and how to keep it" "out: $(tail -3 <<<"$out")"; fi
    case "$h" in */jkb-hid.*) rm -rf -- "$h" ;; esac
}

# A MARKER PLANTED IN A CHECKOUT does not make its run.sh the kit, so --install-kit copies the
# checkout itself, never the directory the marker names (review round 16).
case18_a_planted_kit_marker_is_ignored() {
    local d="$work/mk-$RANDOM" co evil p kit
    mkdir -p "$d/home"
    for co in "$d/co" "$d/evil"; do
        mkdir -p "$co/scripts"; cp -R "$repo_root/.container" "$co/.container"
        for p in lib.sh link-claude-memory.sh auto-mode.sh auto-mode-posture.json; do cp "$repo_root/scripts/$p" "$co/scripts/$p"; done
    done
    co="$d/co"; evil="$d/evil"; echo '# EVIL' >> "$evil/.container/run.sh"
    printf 'checkout=%s\n' "$evil" > "$co/.jkb-container-kit"
    env HOME="$d/home" bash "$co/.container/run.sh" --install-kit >/dev/null 2>&1
    kit="$d/home/.local/share/jkb-container-kit/kit"
    if [ "$(sed -n 's/^checkout=//p' "$kit/.jkb-container-kit" 2>/dev/null)" = "$(cd "$co" && pwd -P)" ] && ! grep -q '# EVIL' "$kit/.container/run.sh"; then
        ok "a kit marker planted in a checkout is ignored: --install-kit copies the checkout itself"
    else
        fail "a kit marker planted in a checkout is ignored: --install-kit copies the checkout itself" "marker: $(cat "$kit/.jkb-container-kit" 2>&1)"
    fi
}

# run.sh RUNS AS YOU, UNSANDBOXED, from whatever terminal launches it, so it must not run that
# terminal's BASH_ENV or the functions it exports: a committed .vscode/settings.json can set either for
# every VS Code terminal, and a planted `cd` or `docker` function is called before any check (review
# round 21, measured: `#!/bin/bash` ran both, `#!/bin/bash -p` neither). EXECUTED directly, so the
# shebang is what is tested; every other case runs `bash run.sh`, which bypasses it.
case19_run_sh_ignores_bash_env_and_exported_functions() {
    local d="$work/be-$RANDOM"
    mkdir -p "$d/home"
    printf ': > "%s/RAN-BASH_ENV"\n' "$d" > "$d/env.sh"
    env HOME="$d/home" BASH_ENV="$d/env.sh" "BASH_FUNC_cd%%=() { : > \"$d/RAN-cd\"; builtin cd \"\$@\"; }" \
        "$repo_root/.container/run.sh" --kit-path >"$d/out" 2>&1
    if [ ! -e "$d/RAN-BASH_ENV" ] && [ ! -e "$d/RAN-cd" ] && grep -q 'jkb-container-kit' "$d/out"; then
        ok "run.sh runs neither a BASH_ENV script nor an exported function from the terminal that launches it"
    else
        fail "run.sh runs neither a BASH_ENV script nor an exported function from the terminal that launches it" \
            "ran: $(ls "$d" | grep '^RAN-' | tr '\n' ' ') out: $(head -c 200 "$d/out")"
    fi
}

# THE PATH FILTER TAKES NO ORDERS FROM THE ENVIRONMENT: a launching terminal's env (a committed
# terminal.integrated.env) set JKB_RUN_PATH_KEEP to ~/.cargo/bin, or spelled a home entry `<parent>//<user>`,
# which the textual match missed, and a planted jq ran as you (review round 22). The keep list is a
# file under the 0700 kit home, and entries are compared by physical, case-folded path.
case20_the_path_filter_ignores_the_environment() {
    mkdir -p "$HOME/.cache" 2>/dev/null
    local h form ran=""; h="$(mktemp -d "$HOME/.cache/jkb-env.XXXXXX")" || { fail "case20" "no scratch home under ~/.cache"; return; }
    mkdir -p "$h/.cargo/bin"
    printf '#!/bin/sh\n: > "%s/RAN"\nexit 0\n' "$h" > "$h/.cargo/bin/jq"; chmod +x "$h/.cargo/bin/jq"
    for form in keep slashes; do
        rm -f "$h/RAN"
        case "$form" in
            keep)    env HOME="$h" PATH="$h/.cargo/bin:$PATH" JKB_RUN_PATH_KEEP="$h/.cargo/bin" JKB_RUN_FROM_CHECKOUT=1 \
                         bash "$repo_root/.container/run.sh" --print-args >/dev/null 2>&1 ;;
            slashes) env HOME="$h" PATH="$(dirname "$h")//$(basename "$h")/.cargo/bin:$PATH" JKB_RUN_FROM_CHECKOUT=1 \
                         bash "$repo_root/.container/run.sh" --print-args >/dev/null 2>&1 ;;
        esac
        [ -e "$h/RAN" ] && ran="$ran $form"
    done
    if [ -z "$ran" ]; then ok "a keep list in the environment, and a // spelling of the home, put no planted jq back on run.sh's PATH"
    else fail "a keep list in the environment, and a // spelling of the home, put no planted jq back on run.sh's PATH" "ran under:$ran"; fi
    case "$h" in */jkb-env.*) rm -rf -- "$h" ;; esac
}

# run.sh's CHILDREN INHERIT NOTHING FROM THE LAUNCHING TERMINAL beyond an allowlist: -p kept run.sh's
# own shell from BASH_ENV, but docker, tar and `code` still got DOCKER_CONFIG (whose cli-plugins
# docker runs), TAR_OPTIONS (checkpoint-action=exec) and BASH_ENV from a committed
# terminal.integrated.env (review round 23).
case21_run_sh_children_inherit_only_the_allowlist() {
    RS_EXTRA_ENV="BASH_ENV=/tmp/x.sh DOCKER_CONFIG=/tmp/d TAR_OPTIONS=--checkpoint=1 JKB_PLANTED=1" run_sh_with_stub true --stop
    local leaked
    leaked="$(grep -E '^(BASH_ENV|DOCKER_CONFIG|TAR_OPTIONS|JKB_PLANTED)=' "$rs_dir/child-env" 2>/dev/null | cut -d= -f1 | sort -u | tr '\n' ' ')"
    if [ -s "$rs_dir/child-env" ] && [ -z "$leaked" ] && grep -q '^HOME=' "$rs_dir/child-env"; then
        ok "run.sh's children get the allowlisted environment (HOME kept) and none of BASH_ENV, DOCKER_CONFIG, TAR_OPTIONS or an unknown variable"
    else
        fail "run.sh's children get the allowlisted environment (HOME kept) and none of BASH_ENV, DOCKER_CONFIG, TAR_OPTIONS or an unknown variable" \
            "leaked: [$leaked] child-env lines: $(wc -l < "$rs_dir/child-env" 2>/dev/null)"
    fi
}

# ...WHILE THE DOCUMENTED OVERRIDES SURVIVE THE RE-EXEC: JKB_CONTAINER_NAME was dropped with the rest, so
# `--stop` acted on jkb-dev while the reaper and `jkb task work` looked for the override (review round
# 24). An unknown variable beside it forces the re-exec, as any real terminal's SHELL or TMPDIR does.
case22_the_container_name_override_survives_the_allowlist() {
    RS_EXTRA_ENV="JKB_CONTAINER_NAME=jkb-alt JKB_PLANTED=1" run_sh_with_stub true --stop
    if grep -q 'jkb-alt' "$rs_dir/calls" && ! grep -qw 'jkb-dev' "$rs_dir/calls"; then
        ok "JKB_CONTAINER_NAME survives run.sh's environment allowlist: --stop acts on the named container"
    else
        fail "JKB_CONTAINER_NAME survives run.sh's environment allowlist: --stop acts on the named container" "calls: $(tr '\n' ';' < "$rs_dir/calls")"
    fi
}

# A WRITABLE DIRECTORY REACHED THROUGH A LINK is dropped by where it leads: ~/repos linked to a volume
# outside the home put /Volumes/Dev/repos/tools/bin on PATH under its physical name, which matched no
# root, and a jq planted there through allowWrite ~/repos ran as you (review round 25).
case23_a_symlinked_writable_dir_outside_home_is_dropped() {
    mkdir -p "$HOME/.cache" 2>/dev/null
    local h tgt; h="$(mktemp -d "$HOME/.cache/jkb-lnh.XXXXXX")" && tgt="$(mktemp -d "$HOME/.cache/jkb-lnt.XXXXXX")" \
        || { fail "case23" "no scratch dirs under ~/.cache"; return; }
    mkdir -p "$tgt/tools/bin"; ln -s "$tgt" "$h/repos"
    printf '#!/bin/sh\n: > "%s/RAN"\nexit 0\n' "$tgt" > "$tgt/tools/bin/jq"; chmod +x "$tgt/tools/bin/jq"
    env HOME="$h" PATH="$tgt/tools/bin:$PATH" JKB_RUN_FROM_CHECKOUT=1 bash "$repo_root/.container/run.sh" --print-args >/dev/null 2>&1
    if [ ! -e "$tgt/RAN" ]; then ok "a jq planted where a symlinked ~/repos leads, outside the home, does not run"
    else fail "a jq planted where a symlinked ~/repos leads, outside the home, does not run" "it ran"; fi
    case "$h" in */jkb-lnh.*) rm -rf -- "$h" ;; esac; case "$tgt" in */jkb-lnt.*) rm -rf -- "$tgt" ;; esac
}

# AN EXPORTED FUNCTION is not a variable `compgen -e` lists, so with only allowlisted names beside it
# run.sh did not re-exec, and `bash -p` passed `BASH_FUNC_x%%` on to every bash child (review round 26,
# measured on bash 5.2). Launched with a clean environment, so nothing else forces the re-exec.
case24_an_exported_function_does_not_reach_run_sh_children() {
    RS_ENV_I=1 RS_FUNC='() { echo PWNED; }' run_sh_with_stub true --stop
    if [ -s "$rs_dir/child-env" ] && ! grep -q '^BASH_FUNC_' "$rs_dir/child-env"; then
        ok "an exported function in an otherwise allowlisted environment does not reach run.sh's children"
    else
        fail "an exported function in an otherwise allowlisted environment does not reach run.sh's children" \
            "child env: $(grep -c . "$rs_dir/child-env" 2>/dev/null) lines, BASH_FUNC: $(grep -c '^BASH_FUNC_' "$rs_dir/child-env" 2>/dev/null)"
    fi
}

run_cases case1_the_login_files_are_the_two_known_pairs case2_fresh_home_gets_dangling_links \
          case3_a_replaced_link_is_carried_into_the_volume case4_the_account_state_file_is_carried_too \
          case5_a_healthy_link_is_left_alone case6_a_link_elsewhere_is_repointed \
          case7_a_directory_is_refused_not_masked case8_dc_link_state_carries_the_login \
          case9_at_setup_the_volume_copy_wins_over_an_image_file case10_a_failed_move_is_recorded_and_then_cleared \
          case11_run_sh_carries_the_login_before_stop_and_rm case12_run_sh_starts_a_stopped_container_to_carry_it \
          case13_the_checkouts_run_sh_refuses_without_the_override case14_the_kits_run_sh_stops_and_sources_the_mirror \
          case15_a_kit_whose_checkout_is_gone_still_stops case16_a_program_planted_on_path_under_home_does_not_run \
          case17_a_tool_the_path_filter_hid_is_named case18_a_planted_kit_marker_is_ignored \
          case19_run_sh_ignores_bash_env_and_exported_functions case20_the_path_filter_ignores_the_environment \
          case21_run_sh_children_inherit_only_the_allowlist case22_the_container_name_override_survives_the_allowlist \
          case23_a_symlinked_writable_dir_outside_home_is_dropped case24_an_exported_function_does_not_reach_run_sh_children
finish
