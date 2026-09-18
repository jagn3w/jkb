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
run_sh_with_stub() { # run_sh_with_stub <state> <flag> -> sets $calls (one docker call per line)
    local d="$work/rs-$RANDOM"; mkdir -p "$d/bin" "$d/home"
    ln -s "$(dirname "$repo_root")" "$d/home/repos"
    cat > "$d/bin/docker" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$STUB_LOG"
case "$1" in
    inspect) [ "$STUB_STATE" = missing ] && exit 1; printf '%s\n' "$STUB_STATE" ;;
esac
exit 0
STUB
    chmod +x "$d/bin/docker"
    : > "$d/calls"
    HOME="$d/home" PATH="$d/bin:$PATH" STUB_LOG="$d/calls" STUB_STATE="$1" \
        bash "$repo_root/.container/run.sh" "$2" >/dev/null 2>&1
    calls="$(cut -d' ' -f1 "$d/calls" | tr '\n' ' ')"
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

run_cases case1_the_login_files_are_the_two_known_pairs case2_fresh_home_gets_dangling_links \
          case3_a_replaced_link_is_carried_into_the_volume case4_the_account_state_file_is_carried_too \
          case5_a_healthy_link_is_left_alone case6_a_link_elsewhere_is_repointed \
          case7_a_directory_is_refused_not_masked case8_dc_link_state_carries_the_login \
          case9_at_setup_the_volume_copy_wins_over_an_image_file case10_a_failed_move_is_recorded_and_then_cleared \
          case11_run_sh_carries_the_login_before_stop_and_rm case12_run_sh_starts_a_stopped_container_to_carry_it
finish
