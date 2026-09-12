#!/usr/bin/env bash
# Status for a task-swarm run (the /task-swarm coordinator). Two views:
#
#   ./scripts/swarm-status.sh [run_id|wf_dir]
#       RUN view (default): SCHEDULER passes + group counts, IMPLEMENTER/REVIEWER
#       outcomes, merge-queue landed/eject counts, the integration branch commits
#       (base recovered from the merge detail), and jkb task-status counts (the
#       needs_review vs done breakdown shows in-review work). With no arg it picks
#       the newest workflow run under ~/.claude.
#
#   ./scripts/swarm-status.sh --file <tasks.md> [--db <db>]
#       FILE view: for one code-review tasks.md, each task's disk checkbox marker
#       vs its KB item status + binding, plus the file's sync_state health.
#
# Read-only where it matters: it reads journals, git, jkb and the SQLite DB and mutates none of
# them. It does write two scratch files (.swarm-base, .swarm-scope) into the run directory to
# hand values from the embedded python back to bash, and deletes them again as it consumes them.
set -euo pipefail

# The caller's repository selection, dropped before the first git runs. `rev-parse
# --show-toplevel` with an exported `GIT_DIR`/`GIT_WORK_TREE` answers about THAT repository, so
# `$REPO` — which every listing below is scoped to — would name a repo the operator never asked
# about, and the status display would be confidently about the wrong tree. Read-only here, so this
# is wrong information rather than the data loss `merge-queue.sh` had, but it is the same variable
# and the same one-line fix.
unset GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR \
      GIT_INDEX_FILE GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES
# `stat` IS NOT PORTABLE, and the two spellings do different things rather than failing. GNU
# coreutils reads `-c '%Y %n'` for mtime+name; BSD/macOS reads `-f '%m %N'`. On Linux `-f` means
# "filesystem status", so the no-argument path — `./scripts/swarm-status.sh` with no run named —
# produced the wrong answer on Linux. Probed once rather than guessed from uname.
#
# NOT "silently found nothing and reported no runs at all", which this line claimed until the
# `find_run_dir` comment below was written in the same commit and contradicted it: the script
# printed nothing on either stream because `find` over a missing root aborted the whole run under
# `pipefail`. A silent death looks exactly like an empty result, and describing one as the other
# is how the abort went unexamined for a round.
if stat -c '%Y' . >/dev/null 2>&1; then
    STAT_FLAG=-c; STAT_FMT='%Y %n'          # GNU coreutils
else
    STAT_FLAG=-f; STAT_FMT='%m %N'          # BSD / macOS
fi

REPO="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"

# =====================================================================
# FILE view — per-task disk marker vs KB status for one tasks.md
# =====================================================================
file_view() {
    local file="$1"; shift || true
    local db="${JKB_DB:-$HOME/.jkb/jkb.db}"
    if [ "${1:-}" = "--db" ]; then
        db="${2:?--db needs a path}"
    fi
    [ -f "$file" ] || { echo "no such file: $file" >&2; exit 1; }
    [ -f "$db" ]   || { echo "no such db: $db" >&2; exit 1; }

    local abs uri uri_sql
    abs=$(cd "$(dirname "$file")" && pwd)/$(basename "$file")
    uri="file://$abs"
    # Escape single quotes for safe embedding in a SQL string literal (a path with an
    # apostrophe would otherwise close the literal early and break the query).
    uri_sql=${uri//\'/\'\'}
    echo "file: $abs"
    echo "db:   $db"
    echo

    echo "=== sync_state ==="
    sqlite3 -header -column "$db" \
        "SELECT status, substr(last_synced_hash,1,12) AS last_hash,
                substr(base_blob_hash,1,12)  AS base_hash,
                substr(quarantine_blob_hash,1,12) AS quar_hash,
                parse_error, updated_at
         FROM sync_state WHERE uri = '$uri_sql';" 2>&1 \
      || echo "(no sync_state row — file not yet synced)"
    echo

    local kb
    kb=$(sqlite3 -separator $'\t' "$db" \
        "SELECT substr(b.uri, instr(b.uri,'#')+1) AS frag, i.status
         FROM bindings b JOIN items i ON i.id = b.item_id
         WHERE b.uri LIKE '$uri_sql#%' AND i.kind = 'task';" 2>&1 || true)

    echo "=== tasks (disk marker | kb status) ==="
    printf '%-4s  %-13s  %s\n' "DISK" "KB-STATUS" "TASK"
    printf '%-4s  %-13s  %s\n' "----" "---------" "----"
    # One awk pass: preload the KB frag→status map once, then look up each checkbox line
    # against it (an associative lookup, not a re-scan of $kb per line — avoids O(T²)).
    grep -nE '^[[:space:]]*- \[.\].*\^[A-Za-z0-9-]+[[:space:]]*$' "$file" \
      | KB="$kb" awk '
        BEGIN {
            n = split(ENVIRON["KB"], klines, "\n")
            for (i = 1; i <= n; i++) {
                p = index(klines[i], "\t")
                if (p > 0) st[substr(klines[i], 1, p - 1)] = substr(klines[i], p + 1)
            }
        }
        {
            line = $0
            sub(/^[0-9]+:/, "", line)                              # drop grep -n prefix
            if (!match(line, /- \[.\]/)) next
            marker = substr(line, RSTART + 3, 1)                   # char inside [ ]
            frag = line; sub(/^.*\^/, "", frag); sub(/[[:space:]]*$/, "", frag)
            title = line
            sub(/^[[:space:]]*- \[.\][[:space:]]*/, "", title)     # strip checkbox
            sub(/[[:space:]]*\^[A-Za-z0-9-]+[[:space:]]*$/, "", title)
            sub(/ —.*$/, "", title)                                # drop trailing " — note"
            kbstatus = (frag in st) ? st[frag] : "(unbound)"
            printf "[%s]   %-13s  %.60s\n", marker, kbstatus, title
        }'
    echo
    echo "legend: [ ] todo  [x] done  [~] partial  [-] cancelled  [?] needs_review"
}

# =====================================================================
# RUN view — workflow-run status (agents, merges, integration, jkb)
# =====================================================================
find_run_dir() {
    local arg="$1"
    if [ -n "$arg" ] && [ -d "$arg" ]; then printf '%s\n' "$arg"; return; fi
    local roots=("$HOME/.claude/projects") present=()
    [ -n "${CLAUDE_CONFIG_DIR:-}" ] && roots=("$CLAUDE_CONFIG_DIR/projects" "${roots[@]}")
    # ONLY THE ROOTS THAT EXIST, and a pipeline that cannot abort the caller.
    #
    # `find` exits non-zero when any argument is missing or any directory under it is unreadable,
    # and `set -euo pipefail` at the top of this file turns that into an abort INSIDE the command
    # substitution that calls this function — so the script printed zero bytes and exited 1, and
    # the "no swarm run found" message below was unreachable. Measured here: CLAUDE_CONFIG_DIR
    # unset and `$HOME/.claude/projects` absent, `./scripts/swarm-status.sh` produced no output at
    # all on either stream.
    #
    # That also explains why the BSD-`stat` bug one round ago was described as "found nothing and
    # reported no runs": it never reported anything. The same abort swallowed the report, which is
    # why the abort itself went unexamined — a silent death looks exactly like an empty result.
    # The success path is no safer: one unreadable sibling directory makes `find` print the right
    # answer and still exit 1, and `pipefail` then discards it.
    local r
    for r in "${roots[@]}"; do [ -d "$r" ] && present+=("$r"); done
    if [ "${#present[@]}" -eq 0 ]; then return 1; fi
    if [ -n "$arg" ]; then
        # `sed -n 1p`, not `head -1`: head EXITS after its line, find dies on the unwritten
        # tail, and `set -euo pipefail` two dozen lines up turns that into an abort with no
        # message. Measured: `producer | sort -rn | head -1` aborted 20/20 at 3000 lines,
        # `| sed -n 1p` 0/20 — sed reads to EOF, so there is no early exit to race.
        { find "${present[@]}" -type d -name "$arg" 2>/dev/null || true; } | sed -n 1p
    else
        # THE JOURNAL'S MTIME, NOT THE DIRECTORY'S, because this script writes into the
        # directory. `run_view` drops `.swarm-base` and `.swarm-scope` into `$run_dir` and
        # removes them again — four directory-modifying operations — so inspecting a finished
        # run by name once bumped that run's mtime above the live one, and every later
        # no-argument invocation picked the finished run FOR EVER. Measured with two runs
        # 1.1s apart: correct before, permanently wrong after a single `swarm-status wf_OLD`.
        # Appending to `journal.jsonl` does not touch the parent directory, which is exactly
        # why the directory was the wrong thing to ask and the journal is the right one: it is
        # the file the harness writes and this script only reads.
        { find "${present[@]}" -type f -name journal.jsonl -path '*/subagents/workflows/wf_*' \
            2>/dev/null -exec stat "$STAT_FLAG" "$STAT_FMT" {} + 2>/dev/null || true; } \
            | sort -rn | sed -n 1p | cut -d' ' -f2- | sed 's|/journal\.jsonl$||'
    fi
}

run_view() {
    local run_dir
    # `|| run_dir=""`, because a bare assignment from a command substitution is subject to
    # `errexit`: `find_run_dir` returning non-zero — which it now does when no search root exists
    # — would abort here, before the message below that exists to explain exactly that.
    run_dir="$(find_run_dir "${1:-}")" || run_dir=""
    if [ -z "$run_dir" ] || [ ! -f "$run_dir/journal.jsonl" ]; then
        echo "no swarm run found (arg='${1:-}'). Pass a run id (wf_...) or transcript dir." >&2
        exit 1
    fi
    echo "run: $(basename "$run_dir")"
    echo "dir: $run_dir"
    echo

    JOURNAL="$run_dir/journal.jsonl" SCOPE_OUT="$run_dir/.swarm-scope" BASE_OUT="$run_dir/.swarm-base" python3 - <<'PY'
import json, os, re
# A PARTIAL LAST LINE IS THE NORMAL STATE OF A LIVE RUN. This was a list comprehension over
# `json.loads`, so one malformed line — most often the final line of a journal still being
# written, caught mid-flush — raised and replaced the entire report with a traceback and exit 1.
# The whole point of this view is watching a run that is still going. Unparseable lines are
# counted and mentioned, never fatal.
rows, skipped = [], 0
for l in open(os.environ["JOURNAL"]):
    if not l.strip():
        continue
    try:
        rows.append(json.loads(l))
    except ValueError:
        skipped += 1
if skipped:
    print(f"(note: {skipped} journal line(s) could not be parsed — a run still being written "
          f"usually has one, and it is the last)")
# New swarm shape (D27): SCHEDULER groups → IMPLEMENTER → REVIEWER → merge queue.
sched, impls, reviews, merges, started = [], [], [], [], 0
for e in rows:
    if e.get("type") == "started":
        started += 1; continue
    if e.get("type") != "result":
        continue
    r = e.get("result")
    if not isinstance(r, dict):
        continue
    if "groups" in r:          sched.append(r)
    elif "outcome" in r:       impls.append(r)         # IMPL {outcome, branch, summary}
    elif "verdict" in r:       reviews.append(r)       # REVIEW {verdict, notes, handoff}
    elif "landed" in r:        merges.append(r)        # MERGE {landed, detail}

namespaces, base = [], None
for s in sched:
    for g in s.get("groups", []):
        for t in g.get("tasks", []):
            if t.get("namespace"):
                namespaces.append(t["namespace"])
n_groups = sum(len(s.get("groups", [])) for s in sched)
n_impl_ok = sum(1 for r in impls if r.get("outcome") == "ready")
n_approve = sum(1 for r in reviews if r.get("verdict") == "approve")
n_reqchg  = sum(1 for r in reviews if r.get("verdict") == "request_changes")
n_land = sum(1 for r in merges if r.get("landed"))
n_eject = sum(1 for r in merges if not r.get("landed"))

print(f"scheduler passes: {len(sched)}   agents started: {started}")
if sched:
    last = sched[-1]
    print(f"last pass: groups={len(last.get('groups',[]))} remaining={last.get('remaining')}")
print(f"groups scheduled: {n_groups}   implemented(ok): {n_impl_ok}")
print(f"reviews: approve={n_approve} request_changes={n_reqchg}")
print(f"merge queue: landed={n_land} eject={n_eject}")
if merges:
    print()
    hdr = f"{'merge outcome':14} {'detail':60}"
    print(hdr); print("-" * len(hdr))
    for m in merges:
        out = "landed" if m.get("landed") else "eject"
        print(f"{out:14} {str(m.get('detail',''))[:60]:60}")
        # Parse the base branch from a "landed: <branch> → <base> in …" line.
        #
        # BOTH ARROWS. `merge-queue.sh` prints U+2192; this matched only ASCII `->`, so `mm` was
        # always None, `.swarm-base` was never written, and the run view reported "no landed
        # merges recorded yet" however many branches had landed. Accepting both costs nothing and
        # means a future edit to either spelling does not silently break the other.
        mm = re.search(r'(?:->|\u2192)\s*(\S+)', str(m.get("detail","")))
        if mm and not base:
            base = mm.group(1)
if base:
    open(os.environ["BASE_OUT"], "w").write(base)
if namespaces:
    segs = [n.split("/") for n in namespaces]
    common = []
    for parts in zip(*segs):
        if len(set(parts)) == 1:
            common.append(parts[0])
        else:
            break
    if common:
        open(os.environ["SCOPE_OUT"], "w").write("/".join(common))
PY

    echo
    echo "=== integration branch ==="
    # The integration/feature branch is an ordinary branch name (no swarm/* artifact,
    # D27.7). Recover its base from the merge-queue detail lines when available.
    local base_file="$run_dir/.swarm-base"
    if [ -f "$base_file" ]; then
        local base; base="$(cat "$base_file")"; rm -f "$base_file"
        echo "base branch (from merge detail): $base"
        git -C "$REPO" log --oneline -15 "$base" 2>/dev/null || echo "  (branch '$base' not found locally)"
    else
        echo "(no landed merges recorded yet — integration branch name is in the coordinator's memory, not git)"
    fi

    local scope_file="$run_dir/.swarm-scope"
    if [ -f "$scope_file" ] && command -v jkb >/dev/null 2>&1; then
        local scope; scope="$(cat "$scope_file")"
        echo
        echo "=== jkb task statuses (ns:$scope/**) ==="
        jkb query --global --json "kind:task ns:$scope/**" --limit 500 2>/dev/null \
            | python3 -c "import json,sys;from collections import Counter;d=json.load(sys.stdin);print(dict(Counter(t['status'] for t in d)))" \
            2>/dev/null || echo "(jkb query failed)"
    fi
    rm -f "$scope_file"
}

# ---- dispatch --------------------------------------------------------
if [ "${1:-}" = "--file" ]; then
    shift
    file_view "${1:?usage: swarm-status.sh --file <tasks.md> [--db <db>]}" "${@:2}"
else
    run_view "${1:-}"
fi
