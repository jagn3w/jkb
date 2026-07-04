#!/usr/bin/env bash
# Status for a task-swarm run (the /task-swarm coordinator). Two views:
#
#   ./scripts/swarm-status.sh [run_id|wf_dir]
#       RUN view (default): per-task implement/resolve outcomes, rounds, agents,
#       the swarm/* integration-branch commits, and jkb task-status counts.
#       With no arg it picks the newest workflow run under ~/.claude.
#
#   ./scripts/swarm-status.sh --file <tasks.md> [--db <db>]
#       FILE view: for one code-review tasks.md, each task's disk checkbox marker
#       vs its KB item status + binding, plus the file's sync_state health.
#
# Read-only: reads journals, git, jkb, and the SQLite DB; never writes anything.
set -euo pipefail

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

    local abs uri
    abs=$(cd "$(dirname "$file")" && pwd)/$(basename "$file")
    uri="file://$abs"
    echo "file: $abs"
    echo "db:   $db"
    echo

    echo "=== sync_state ==="
    sqlite3 -header -column "$db" \
        "SELECT status, substr(last_synced_hash,1,12) AS last_hash,
                substr(base_blob_hash,1,12)  AS base_hash,
                substr(quarantine_blob_hash,1,12) AS quar_hash,
                parse_error, updated_at
         FROM sync_state WHERE uri = '$uri';" 2>&1 \
      || echo "(no sync_state row — file not yet synced)"
    echo

    local kb
    kb=$(sqlite3 -separator $'\t' "$db" \
        "SELECT substr(b.uri, instr(b.uri,'#')+1) AS frag, i.status
         FROM bindings b JOIN items i ON i.id = b.item_id
         WHERE b.uri LIKE '$uri#%' AND i.kind = 'task';" 2>&1 || true)

    echo "=== tasks (disk marker | kb status) ==="
    printf '%-4s  %-13s  %s\n' "DISK" "KB-STATUS" "TASK"
    printf '%-4s  %-13s  %s\n' "----" "---------" "----"
    grep -nE '^[[:space:]]*- \[.\].*\^[A-Za-z0-9-]+[[:space:]]*$' "$file" | while IFS= read -r line; do
        local marker frag title kbstatus
        marker=$(printf '%s' "$line" | sed -E 's/^[0-9]+:[[:space:]]*- \[(.)\].*/\1/')
        frag=$(printf '%s' "$line"   | sed -E 's/.*\^([A-Za-z0-9-]+)[[:space:]]*$/\1/')
        title=$(printf '%s' "$line"  | sed -E 's/^[0-9]+:[[:space:]]*- \[.\] //; s/ \^[A-Za-z0-9-]+[[:space:]]*$//; s/ —.*$//')
        kbstatus=$(printf '%s\n' "$kb" | awk -F'\t' -v f="$frag" '$1==f{print $2; found=1} END{if(!found)print "(unbound)"}')
        printf '[%s]   %-13s  %.60s\n' "$marker" "$kbstatus" "$title"
    done
    echo
    echo "legend: [ ] todo  [x] done  [~] partial  [-] cancelled  [?] needs_review"
}

# =====================================================================
# RUN view — workflow-run status (agents, merges, integration, jkb)
# =====================================================================
find_run_dir() {
    local arg="$1"
    if [ -n "$arg" ] && [ -d "$arg" ]; then printf '%s\n' "$arg"; return; fi
    local roots=("$HOME/.claude/projects")
    [ -n "${CLAUDE_CONFIG_DIR:-}" ] && roots=("$CLAUDE_CONFIG_DIR/projects" "${roots[@]}")
    if [ -n "$arg" ]; then
        find "${roots[@]}" -type d -name "$arg" 2>/dev/null | head -1
    else
        find "${roots[@]}" -type d -name 'wf_*' -path '*/subagents/workflows/*' \
            2>/dev/null -exec stat -f '%m %N' {} + 2>/dev/null \
            | sort -rn | head -1 | cut -d' ' -f2-
    fi
}

run_view() {
    local run_dir
    run_dir="$(find_run_dir "${1:-}")"
    if [ -z "$run_dir" ] || [ ! -f "$run_dir/journal.jsonl" ]; then
        echo "no swarm run found (arg='${1:-}'). Pass a run id (wf_...) or transcript dir." >&2
        exit 1
    fi
    echo "run: $(basename "$run_dir")"
    echo "dir: $run_dir"
    echo

    JOURNAL="$run_dir/journal.jsonl" SCOPE_OUT="$run_dir/.swarm-scope" python3 - <<'PY'
import json, os
rows = [json.loads(l) for l in open(os.environ["JOURNAL"]) if l.strip()]
sched, impl, resolve, started = [], {}, {}, 0
for e in rows:
    if e.get("type") == "started":
        started += 1; continue
    if e.get("type") != "result":
        continue
    r = e.get("result") or {}
    if not isinstance(r, dict):
        continue
    if "ready" in r:
        sched.append(r); continue
    uid = r.get("uid")
    if not uid:
        continue
    if r.get("branch"):
        impl[uid] = r
    elif "merged" in r:
        resolve[uid] = r
label = lambda u: u.split("#")[-1] if "#" in u else u.rsplit("/", 1)[-1]
order, seen, namespaces = [], set(), []
for s in sched:
    for t in s.get("ready", []):
        if t["uid"] not in seen:
            seen.add(t["uid"]); order.append(t["uid"])
        if t.get("namespace"):
            namespaces.append(t["namespace"])
print(f"rounds scheduled: {len(sched)}   agents started: {started}")
if sched:
    last = sched[-1]
    print(f"last scheduler: ready={len(last.get('ready',[]))} remaining={last.get('remaining')}")
n_merged  = sum(1 for r in resolve.values() if r.get("merged"))
n_flagged = sum(1 for r in resolve.values() if not r.get("merged"))
print(f"implemented: {len(impl)}/{len(order)}   merged: {n_merged}   resolver-flagged: {n_flagged}")
print()
hdr = f"{'task':40} {'impl':6} {'resolve':10}"
print(hdr); print("-" * len(hdr))
for u in order:
    i = "ok" if u in impl else "-"
    if u in resolve:
        rr = resolve[u]
        rs = "merged" if rr.get("merged") else (rr.get("flag") or rr.get("outcome") or "flagged")
    else:
        rs = "-"
    print(f"{label(u)[:40]:40} {i:6} {rs:10}")
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
    echo "=== integration branch(es) ==="
    local found=""
    while read -r br; do
        [ -z "$br" ] && continue
        found=1
        local base="${br#swarm/}"
        echo "-- $br (vs $base) --"
        git -C "$REPO" log --oneline "$base".."$br" 2>/dev/null || echo "  (base '$base' not found)"
    done < <(git -C "$REPO" for-each-ref --format='%(refname:short)' refs/heads/swarm/ 2>/dev/null)
    [ -z "$found" ] && echo "(no swarm/* branches)"

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
