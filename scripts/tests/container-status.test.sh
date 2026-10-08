#!/usr/bin/env bash
# What the Code Factory app's Container tab drives (docs/code-factory.md, D53.8): run.sh's --status,
# --verify and --install-extensions, the image's jkb.built-at / jkb.source-commit / jkb.source-branch
# labels, and lib.sh's dc_git_head, which reads the commit and branch without running git.
#
# Against a stub `docker` whose answers are files in a scratch directory, so no container is needed.
# The stub records every call, which is how a case asks what run.sh did -- and did not do.
set -uo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=.container/lib.sh
. "$repo_root/.container/lib.sh"
# shellcheck source=scripts/tests/harness.sh
. "$(dirname "$0")/harness.sh"

new_workdir
isolate_git "$work/git-home"
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.com GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.com

# stub_docker <dir> -- a docker that answers from <dir>:
#   state         the container's .State.Status (running, exited, ...); absent = no container
#   args-hash     its jkb.args-hash label;  ctr-image  its image id
#   images/<name> `docker image inspect` JSON, by tag or id (`/` and `:` spelled `_`)
#   down          present = the daemon is unreachable
#   build.json    what a `docker build` produces (its Config.Labels gain the build's --label values)
# Every call is appended to <dir>/calls, one per line.
stub_docker() {
    local d="$1"
    mkdir -p "$d/bin" "$d/images"
    cat > "$d/bin/docker" <<STUB
#!/usr/bin/env bash
D="$d"
printf '%s\\n' "\$*" >> "\$D/calls"
key() { printf '%s' "\$1" | tr '/:' '__'; }
running() { [ "\$(cat "\$D/state" 2>/dev/null)" = running ] && echo true || echo false; }
ctr_json() {
    [ -f "\$D/state" ] || return 1
    /usr/bin/jq -n --arg s "\$(cat "\$D/state")" --arg h "\$(cat "\$D/args-hash" 2>/dev/null)" --arg i "\$(cat "\$D/ctr-image" 2>/dev/null)" \\
        '[{State: {Status: \$s, Running: (\$s == "running")}, Image: \$i, Config: {Labels: (if \$h == "" then {} else {"jkb.args-hash": \$h} end)}}]'
}
fmt=""
case "\$1" in
    info) [ -e "\$D/down" ] && exit 1; exit 0 ;;
    image)
        shift 2
        [ "\${1:-}" = -f ] && { fmt="\$2"; shift 2; }
        f="\$D/images/\$(key "\$1")"; [ -f "\$f" ] || exit 1
        if [ -n "\$fmt" ]; then /usr/bin/jq -r '.[0].Id' "\$f"; else cat "\$f"; fi ;;
    container|inspect)
        [ "\$1" = container ] && shift
        shift
        [ "\${1:-}" = -f ] && { fmt="\$2"; shift 2; }
        [ -f "\$D/state" ] || exit 1
        case "\$fmt" in
            "") ctr_json ;;
            *State.Status*) cat "\$D/state" ;;
            *State.Running*) running ;;
            *jkb.args-hash*) cat "\$D/args-hash" 2>/dev/null || echo '<no value>' ;;
            *.Image*) cat "\$D/ctr-image" 2>/dev/null ;;
        esac ;;
    build)
        shift; iid=""; tag=""; labels="{}"
        while [ \$# -gt 0 ]; do
            case "\$1" in
                --iidfile) iid="\$2"; shift 2 ;;
                -t) tag="\$2"; shift 2 ;;
                --label) labels="\$(/usr/bin/jq -c --arg kv "\$2" '. + {(\$kv | sub("=.*"; "")): (\$kv | sub("^[^=]*="; ""))}' <<<"\$labels")"; shift 2 ;;
                -f) shift 2 ;;
                *) shift ;;
            esac
        done
        n=\$(grep -c '^build' "\$D/calls")
        out="\$(/usr/bin/jq -c --argjson l "\$labels" --arg id "sha256:built\$n" '.[0].Id = \$id | .[0].Config.Labels = ((.[0].Config.Labels // {}) + \$l)' "\$D/build.json")"
        printf '%s\\n' "\$out" > "\$D/images/\$(key "sha256:built\$n")"
        [ -z "\$tag" ] || printf '%s\\n' "\$out" > "\$D/images/\$(key "\$tag")"
        [ -z "\$iid" ] || printf 'sha256:built%s' "\$n" > "\$iid"
        exit 0 ;;
    exec)
        case "\$*" in
            *"ps -o args="*) echo "sleep infinity" ;;
            *"echo done"*) echo done ;;
        esac
        exit 0 ;;
esac
exit 0
STUB
    chmod +x "$d/bin/docker"
    : > "$d/calls"
}

# kit_run <dir> <flag>... -- run the KIT's run.sh (installed from this checkout) against the stub,
# with <dir>/home as the account's home and ~/repos leading to this checkout's parent. Sets $out
# (stdout), $err (stderr), $rc, and $calls (one docker call per line).
kit_run() {
    local d="$1"; shift
    local home="$d/home" kit
    kit="$home/.local/share/jkb-container-kit/kit"
    if [ ! -d "$kit" ]; then
        mkdir -p "$home/.local/share/jkb-container-kit"
        ln -s "$(dirname "$repo_root")" "$home/repos"
        dc_install_kit "$repo_root" "$kit" >/dev/null 2>&1 || { echo "could not install a kit" >&2; return 1; }
        printf '%s\n' "$d/bin" > "$home/.local/share/jkb-container-kit/path-keep"
    fi
    rc=0
    env HOME="$home" PATH="$d/bin:$PATH" ${KR_ENV:-} bash "$kit/.container/run.sh" --test-home "$home" "$@" >"$d/out" 2>"$d/err" || rc=$?
    out="$(cat "$d/out")"; err="$(cat "$d/err")"
    calls="$(cat "$d/calls")"
}

image_json() { # image_json <id> [labels-json] -> `docker image inspect` output
    local l="${2:-}"; [ -n "$l" ] || l='{}'
    /usr/bin/jq -nc --arg id "$1" --argjson l "$l" \
        '[{Id: $id, Created: "2026-10-01T00:00:00Z", Os: "linux", Architecture: "arm64", RootFS: {Type: "layers", Layers: ["sha256:l1"]}, Config: {Hostname: "h", Env: ["A=1"], Labels: $l}}]'
}

# --------------------------------------------------------------------------------------------
# dc_git_head: the commit and branch, read from the repository's files.
# --------------------------------------------------------------------------------------------

case1_dc_git_head_reads_loose_packed_detached_and_worktrees() {
    local r="$work/repo1" sha got
    git init -q -b main "$r" && git -C "$r" commit -q --allow-empty -m one || { fail "case1" "could not make a repository"; return; }
    sha="$(git -C "$r" rev-parse HEAD)"
    got="$(dc_git_head "$r")"
    if [ "$got" = "$sha	main" ]; then ok "dc_git_head reads a loose branch ref"
    else fail "dc_git_head reads a loose branch ref" "got [$got] want [$sha	main]"; fi

    git -C "$r" pack-refs --all
    if [ ! -e "$r/.git/refs/heads/main" ] && [ "$(dc_git_head "$r")" = "$sha	main" ]; then ok "...and a packed one"
    else fail "...and a packed one" "got [$(dc_git_head "$r")]"; fi

    git -C "$r" checkout -q -b feature/x && git -C "$r" commit -q --allow-empty -m two
    local wt="$work/wt1"
    git -C "$r" worktree add -q "$wt" main 2>/dev/null
    git -C "$r" checkout -q --detach HEAD
    if [ "$(dc_git_head "$r")" = "$(git -C "$r" rev-parse HEAD)	" ]; then ok "...a detached HEAD, with no branch"
    else fail "...a detached HEAD, with no branch" "got [$(dc_git_head "$r")]"; fi

    if [ "$(dc_git_head "$wt")" = "$sha	main" ]; then ok "...and a linked worktree, whose branches live in the common directory"
    else fail "...and a linked worktree, whose branches live in the common directory" "got [$(dc_git_head "$wt")]"; fi
}

case2_dc_git_head_says_unknown_rather_than_guess() {
    local r="$work/repo2"
    mkdir -p "$r"
    if ! dc_git_head "$r" >/dev/null; then ok "a directory that is not a repository is unknown (rc 1)"
    else fail "a directory that is not a repository is unknown (rc 1)" "got [$(dc_git_head "$r")]"; fi
    mkdir -p "$r/.git/refs/heads"
    printf 'ref: refs/heads/none\n' > "$r/.git/HEAD"
    if ! dc_git_head "$r" >/dev/null; then ok "...as is a branch with no commit (an unborn HEAD)"
    else fail "...as is a branch with no commit (an unborn HEAD)" "got [$(dc_git_head "$r")]"; fi
    printf 'not-a-sha\n' > "$r/.git/HEAD"
    if ! dc_git_head "$r" >/dev/null; then ok "...and a HEAD that is not a full hex object name"
    else fail "...and a HEAD that is not a full hex object name" "got [$(dc_git_head "$r")]"; fi
    # Never by running git: a repository config naming a program git runs is not run.
    local g="$work/repo3"
    git init -q -b main "$g" && git -C "$g" commit -q --allow-empty -m one
    git -C "$g" config core.fsmonitor "touch $work/RAN-fsmonitor;"
    dc_git_head "$g" >/dev/null
    if [ ! -e "$work/RAN-fsmonitor" ]; then ok "reading HEAD runs nothing the repository's config names"
    else fail "reading HEAD runs nothing the repository's config names" "core.fsmonitor ran"; fi
}

case3_the_kit_records_what_it_was_copied_from() {
    local kit="$work/kit3/kit" want got
    dc_install_kit "$repo_root" "$kit" >/dev/null 2>&1 || { fail "case3" "could not install a kit"; return; }
    want="$(dc_git_head "$repo_root")"
    got="$(dc_kit_source "$kit")"
    if [ -n "$want" ] && [ "$got" = "$want" ] && [ "$(dc_kit_checkout "$kit")" = "$(cd "$repo_root" && pwd -P)" ]; then
        ok "the kit records the commit and branch its checkout stood on, beside the checkout"
    else fail "the kit records the commit and branch its checkout stood on, beside the checkout" "got [$got] want [$want]"; fi
    printf 'checkout=%s\n' "$repo_root" > "$kit/$DC_KIT_MARKER"
    if ! dc_kit_source "$kit" >/dev/null; then ok "a kit installed before it recorded one has no source (rc 1)"
    else fail "a kit installed before it recorded one has no source (rc 1)" "got [$(dc_kit_source "$kit")]"; fi
}

# --------------------------------------------------------------------------------------------
# --status
# --------------------------------------------------------------------------------------------

case4_status_reports_the_image_labels_and_no_container() {
    local d="$work/s4"; stub_docker "$d"
    image_json sha256:img '{"jkb.built-at":"2026-10-08T10:00:00Z","jkb.source-commit":"abc123","jkb.source-branch":"main"}' > "$d/images/jkb-dev"
    kit_run "$d" --status
    local got
    got="$(jq -c '[.schema, .docker, .name, .image_on_disk.id, .image_on_disk.built_at, .image_on_disk.source_commit, .image_on_disk.source_branch, .container, .drift.args, .drift.image]' <<<"$out" 2>/dev/null)"
    if [ "$rc" -eq 0 ] && [ "$got" = '[1,"reachable","jkb-dev","sha256:img","2026-10-08T10:00:00Z","abc123","main",null,null,null]' ]; then
        ok "--status reports the image's labels, and no container as null with no drift"
    else fail "--status reports the image's labels, and no container as null with no drift" "rc=$rc got=$got err=$err"; fi
    if [ "$(jq -r '.want_args_hash | length' <<<"$out" 2>/dev/null)" = 64 ] && [ "$(jq -r '.kit != null and .checkout != null' <<<"$out")" = true ]; then
        ok "...with the declaration's args-hash and where the kit and its checkout are"
    else fail "...with the declaration's args-hash and where the kit and its checkout are" "out=$out"; fi
    if ! grep -qE '^(build|run|start|stop|rm|exec)( |$)' <<<"$calls"; then ok "...and changes nothing: no build, run, start, stop, rm or exec"
    else fail "...and changes nothing: no build, run, start, stop, rm or exec" "calls: $(tr '\n' ';' <<<"$calls")"; fi
}

case5_status_drift_is_the_start_paths_answer() {
    local d="$work/s5" want
    stub_docker "$d"
    image_json sha256:new '{}' > "$d/images/jkb-dev"
    image_json sha256:old '{"jkb.source-commit":"old1"}' > "$d/images/sha256_old"
    kit_run "$d" --status
    want="$(jq -r '.want_args_hash' <<<"$out")"
    printf 'running' > "$d/state"; printf '%s' "$want" > "$d/args-hash"; printf 'sha256:new' > "$d/ctr-image"
    kit_run "$d" --status
    if [ "$(jq -c '[.container.state, .drift.args, .drift.image]' <<<"$out")" = '["running","same","same"]' ]; then
        ok "a container made from this declaration, on the tag's image, has no drift"
    else fail "a container made from this declaration, on the tag's image, has no drift" "out=$out err=$err"; fi
    printf 'deadbeef' > "$d/args-hash"; printf 'sha256:old' > "$d/ctr-image"
    kit_run "$d" --status
    if [ "$(jq -c '[.drift.args, .drift.image, .container.image.source_commit]' <<<"$out")" = '["differs","differs","old1"]' ]; then
        ok "...another declaration and an older build both read as drift, with the container's own image's labels"
    else fail "...another declaration and an older build both read as drift, with the container's own image's labels" "out=$out"; fi
    rm -f "$d/args-hash"
    kit_run "$d" --status
    if [ "$(jq -r '.drift.args' <<<"$out")" = unrecorded ]; then ok "...and a container with no args-hash label is unrecorded"
    else fail "...and a container with no args-hash label is unrecorded" "out=$out"; fi
}

case6_status_with_the_daemon_down_is_an_answer() {
    local d="$work/s6"; stub_docker "$d"; : > "$d/down"
    kit_run "$d" --status
    if [ "$rc" -eq 0 ] && [ "$(jq -c '[.docker, .image_on_disk, .container]' <<<"$out")" = '["unreachable",null,null]' ]; then
        ok "--status with the daemon down says so, exit 0, rather than failing"
    else fail "--status with the daemon down says so, exit 0, rather than failing" "rc=$rc out=$out err=$err"; fi
}

# --------------------------------------------------------------------------------------------
# --verify and --install-extensions act on a running container only, and never make one.
# --------------------------------------------------------------------------------------------

case7_a_mode_refuses_a_container_that_is_not_running() {
    local d="$work/s7" mode
    for mode in --verify --install-extensions; do
        stub_docker "$d"; rm -f "$d/state"; printf 'exited' > "$d/state"
        kit_run "$d" "$mode"
        if [ "$rc" -ne 0 ] && grep -q 'acts on a running container only' <<<"$err" \
           && ! grep -qE '^(build|run|start|create)( |$)' <<<"$calls"; then
            ok "run.sh $mode on a stopped container refuses, and builds, creates and starts nothing"
        else fail "run.sh $mode on a stopped container refuses, and builds, creates and starts nothing" "rc=$rc err=$err calls: $(tr '\n' ';' <<<"$calls")"; fi
    done
    stub_docker "$d"
    kit_run "$d" --verify --build
    if [ "$rc" -ne 0 ] && grep -q 'takes no other flag' <<<"$err" && [ -z "$calls" ]; then
        ok "a mode with --build is refused before docker is asked anything"
    else fail "a mode with --build is refused before docker is asked anything" "rc=$rc err=$err calls: $calls"; fi
}

# The running path: what each mode runs in the container, from the kit mirror, and where it stops.
case8_verify_and_install_extensions_on_a_running_container() {
    local d="$work/s8" want execs
    stub_docker "$d"
    image_json sha256:img '{}' > "$d/images/jkb-dev"
    kit_run "$d" --status
    want="$(jq -r '.want_args_hash' <<<"$out")"
    printf 'running' > "$d/state"; printf '%s' "$want" > "$d/args-hash"; printf 'sha256:img' > "$d/ctr-image"

    : > "$d/calls"
    kit_run "$d" --verify
    execs="$(grep '^exec ' <<<"$calls")"
    if [ "$rc" -eq 0 ] && grep -qF '/bin/bash /usr/local/lib/jkb-container/.container/verify.sh' <<<"$execs" \
       && ! grep -qF 'setup.sh' <<<"$execs" && ! grep -qE '^(build|start)( |$)' <<<"$calls" \
       && ! grep -qE '^run ' <<<"$(grep -v -- '--entrypoint true' <<<"$calls")" \
       && ! grep -q 'Attach to Running Container' <<<"$out"; then
        ok "--verify runs verify.sh from the mirror, builds and starts nothing, and stops before the attach instructions"
    else fail "--verify runs verify.sh from the mirror, builds and starts nothing, and stops before the attach instructions" "rc=$rc err=$(tail -3 <<<"$err") calls: $(cut -c1-80 <<<"$calls" | tr '\n' ';')"; fi

    # ...EVEN UNDER A NON-DEFAULT IMAGE NAME, which a start always builds.
    : > "$d/calls"
    KR_ENV=JKB_CONTAINER_IMAGE=jkb-alt kit_run "$d" --verify
    if [ "$rc" -eq 0 ] && ! grep -qE '^build( |$)' <<<"$calls"; then
        ok "--verify builds nothing under JKB_CONTAINER_IMAGE either, which a start always builds"
    else fail "--verify builds nothing under JKB_CONTAINER_IMAGE either, which a start always builds" "rc=$rc calls: $(cut -c1-60 <<<"$calls" | tr '\n' ';')"; fi

    : > "$d/calls"
    kit_run "$d" --install-extensions
    execs="$(grep '^exec ' <<<"$calls")"
    if [ "$rc" -eq 0 ] && grep -qF '/bin/bash /usr/local/lib/jkb-container/.container/install-extensions.sh' <<<"$execs" \
       && grep -qF -- '-u root' <<<"$execs" \
       && ! grep -qE 'verify\.sh|sweep-transcripts\.sh|setup\.sh' <<<"$execs"; then
        ok "--install-extensions refreshes the mirror, runs install-extensions.sh from it, and nothing after"
    else fail "--install-extensions refreshes the mirror, runs install-extensions.sh from it, and nothing after" "rc=$rc err=$(tail -3 <<<"$err") execs: $(cut -c1-80 <<<"$execs" | tr '\n' ';')"; fi
}

# --------------------------------------------------------------------------------------------
# The build's labels, and why a rebuild of the same content keeps its image.
# --------------------------------------------------------------------------------------------

case9_a_build_is_stamped_with_its_source_and_time() {
    local d="$work/s9" builds src
    stub_docker "$d"
    image_json sha256:base '{}' > "$d/build.json"
    kit_run "$d" --build
    builds="$(grep '^build ' <<<"$calls")"
    # What the kit recorded, mapped as build_image maps it: CI checks out a pull request detached.
    src="$(dc_source_labels "$(dc_kit_source "$d/home/.local/share/jkb-container-kit/kit")")"
    if [ "$(grep -c . <<<"$builds")" = 2 ] \
       && grep -q -- '--iidfile' <<<"$(sed -n 1p <<<"$builds")" && ! grep -q 'jkb.built-at' <<<"$(sed -n 1p <<<"$builds")" \
       && grep -q -- '-t jkb-dev' <<<"$(sed -n 2p <<<"$builds")"; then
        ok "a first build does the work unstamped, then stamps the tag"
    else fail "a first build does the work unstamped, then stamps the tag" "builds: $(tr '\n' ';' <<<"$builds") err=$(tail -3 <<<"$err")"; fi
    if [ "$(jq -r '.[0].Config.Labels["jkb.source-commit"]' "$d/images/jkb-dev")" = "${src%%	*}" ] \
       && [ "$(jq -r '.[0].Config.Labels["jkb.source-branch"]' "$d/images/jkb-dev")" = "${src#*	}" ] \
       && grep -qE '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$' <<<"$(jq -r '.[0].Config.Labels["jkb.built-at"]' "$d/images/jkb-dev")"; then
        ok "...with the kit's source commit and branch, and a UTC built-at"
    else fail "...with the kit's source commit and branch, and a UTC built-at" "labels: $(jq -c '.[0].Config.Labels' "$d/images/jkb-dev")"; fi

    # The same content again: the tag keeps its image, and so its built-at.
    local before; before="$(cat "$d/images/jkb-dev")"
    : > "$d/calls"
    kit_run "$d" --build
    builds="$(grep '^build ' <<<"$calls")"
    if [ "$(grep -c . <<<"$builds")" = 1 ] && [ "$(cat "$d/images/jkb-dev")" = "$before" ] && grep -q 'unchanged by this build' <<<"$out"; then
        ok "a rebuild of the same content keeps the tag's image and its built-at"
    else fail "a rebuild of the same content keeps the tag's image and its built-at" "builds: $(tr '\n' ';' <<<"$builds")"; fi

    # Other content: stamped again.
    jq -c '.[0].RootFS.Layers = ["sha256:l9"]' "$d/build.json" > "$d/build.json.new" && mv "$d/build.json.new" "$d/build.json"
    : > "$d/calls"
    kit_run "$d" --build
    builds="$(grep '^build ' <<<"$calls")"
    if [ "$(grep -c . <<<"$builds")" = 2 ] && [ "$(jq -r '.[0].RootFS.Layers[0]' "$d/images/jkb-dev")" = sha256:l9 ]; then
        ok "...and a build of other content moves the tag, stamped anew"
    else fail "...and a build of other content moves the tag, stamped anew" "builds: $(tr '\n' ';' <<<"$builds")"; fi
}

# The detached form pinned, whatever this checkout stands on: a kit copied from a detached HEAD
# records an empty branch, and the label says `(detached)` -- never empty, never a guess.
case10_a_detached_source_is_labelled_detached() {
    local d="$work/s10" kit sha=0123456789abcdef0123456789abcdef01234567 r
    stub_docker "$d"
    image_json sha256:base '{}' > "$d/build.json"
    kit_run "$d" --status >/dev/null
    kit="$d/home/.local/share/jkb-container-kit/kit"
    r="$work/repo10"
    git init -q -b main "$r" && git -C "$r" commit -q --allow-empty -m one && git -C "$r" checkout -q --detach HEAD
    if [ "$(dc_source_labels "$(dc_git_head "$r")")" = "$(git -C "$r" rev-parse HEAD)	(detached)" ]; then
        ok "a detached checkout's source labels are its commit and (detached)"
    else fail "a detached checkout's source labels are its commit and (detached)" "got [$(dc_source_labels "$(dc_git_head "$r")")]"; fi
    if [ "$(dc_source_labels "")" = "unknown	unknown" ]; then ok "...and no source at all is unknown for both"
    else fail "...and no source at all is unknown for both" "got [$(dc_source_labels "")]"; fi

    sed -i -e '/^commit=/d' -e '/^branch=/d' "$kit/$DC_KIT_MARKER"
    printf 'commit=%s\nbranch=\n' "$sha" >> "$kit/$DC_KIT_MARKER"
    : > "$d/calls"
    kit_run "$d" --build
    if [ "$(jq -r '.[0].Config.Labels["jkb.source-commit"]' "$d/images/jkb-dev")" = "$sha" ] \
       && [ "$(jq -r '.[0].Config.Labels["jkb.source-branch"]' "$d/images/jkb-dev")" = "(detached)" ]; then
        ok "a kit copied from a detached HEAD builds an image labelled (detached)"
    else fail "a kit copied from a detached HEAD builds an image labelled (detached)" "rc=$rc labels: $(jq -c '.[0].Config.Labels' "$d/images/jkb-dev" 2>/dev/null) err=$(tail -3 <<<"$err")"; fi
}

run_cases case1_dc_git_head_reads_loose_packed_detached_and_worktrees case2_dc_git_head_says_unknown_rather_than_guess \
          case3_the_kit_records_what_it_was_copied_from case4_status_reports_the_image_labels_and_no_container \
          case5_status_drift_is_the_start_paths_answer case6_status_with_the_daemon_down_is_an_answer \
          case7_a_mode_refuses_a_container_that_is_not_running case8_verify_and_install_extensions_on_a_running_container \
          case9_a_build_is_stamped_with_its_source_and_time case10_a_detached_source_is_labelled_detached
finish
