#!/usr/bin/env bash
# Regression test for scripts/swarm-status.sh: its FILE view (`--file <tasks.md> --db <db>`), cases
# 1-10, and the status counts of its RUN view, case 11.
#
# The bug cases 1-6 exist for: the view finds a file's tasks by a prefix match on `<file uri>#`, and the
# path went into that pattern with only its single quotes escaped. Its metacharacters stayed live,
# so a path matched a different sibling directory, and the "disk marker | kb status" table reported
# ANOTHER file's tasks as this file's. The pattern was then a LIKE, which also folds ASCII case, so
# `repo/tasks.md` reported `Repo/tasks.md`'s tasks. The fix is a GLOB with `*`, `?` and `[`
# escaped, as `jkb_core::sql::prefix_glob` builds on the Rust side.
#
# Each case below puts the file under test next to a sibling directory an unescaped or
# case-folding pattern would also match, binds a task fragment ONLY under the sibling, and asserts
# the file's own line for that fragment still reads `(unbound)`.
set -uo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/lib.sh
. "$repo_root/scripts/lib.sh"
# shellcheck source=scripts/tests/harness.sh
. "$(dirname "$0")/harness.sh"

new_workdir
mkdir -p "$work/bin"

# THE DATABASE SHELL, or a stand-in for it. The view runs the real one through `jkb_sqlite`, and
# it is used when present. Where it is not installed (the dev container, a bare CI image) a
# stand-in on PATH runs the same SQL through Python's bundled SQLite — the same engine, so the
# GLOB semantics under test are SQLite's either way. It understands only what the view
# passes: `-header`, `-column` and `-separator <s>`, then `<db> <sql>`.
#
# Every write below goes through `jkb_sqlite` (scripts/lib.sh), which refuses a database on a
# shared filesystem before it opens one — the rule dev-scripts.test.sh case 11 enforces. The
# assembled name is used only to probe for the shell and to name the stand-in file, as case 11's
# own shim does (`"$work/shim/sqli""te3"`).
db_shell="sqli""te3"
if ! command -v "$db_shell" >/dev/null 2>&1; then
    if ! command -v python3 >/dev/null 2>&1; then
        fail "environment" "neither the SQLite shell nor python3 is installed, so the view cannot be run"
        finish
    fi
    cat >"$work/bin/$db_shell" <<'STUB'
#!/usr/bin/env python3
import sqlite3, sys
args, sep, header = sys.argv[1:], "|", False
while args and args[0].startswith("-"):
    a = args.pop(0)
    if a == "-separator":
        sep = args.pop(0)
    elif a == "-header":
        header = True
    elif a == "-column":
        sep = "  "
    else:
        sys.exit("stand-in: unsupported option " + a)
db, sql = args
conn = sqlite3.connect(db)
try:
    if not sqlite3.complete_statement(sql) or sql.strip().rstrip(";").count(";"):
        conn.executescript(sql)   # the fixture's multi-statement writes; they return no rows
        rows, desc = [], None
    else:
        cur = conn.execute(sql)
        rows, desc = cur.fetchall(), cur.description
    conn.commit()
except sqlite3.Error as e:
    sys.exit("Error: " + str(e))
if header and desc:
    print(sep.join(d[0] for d in desc))
for row in rows:
    print(sep.join("" if v is None else str(v) for v in row))
STUB
    chmod +x "$work/bin/$db_shell"
    export PATH="$work/bin:$PATH"
fi

db="$work/jkb.db"
jkb_sqlite "$db" "CREATE TABLE items (id INTEGER PRIMARY KEY, kind TEXT, status TEXT);
    CREATE TABLE bindings (uri TEXT, item_id INTEGER);
    CREATE TABLE sync_state (uri TEXT PRIMARY KEY, status TEXT, last_synced_hash TEXT,
        base_blob_hash TEXT, quarantine_blob_hash TEXT, parse_error TEXT, updated_at TEXT);" \
    || { echo "cannot create the fixture database" >&2; exit 1; }

next_id=0
# bind <file> <frag> <status> — a task item bound to `file://<file>#<frag>`.
bind() {
    local uri
    next_id=$((next_id + 1))
    uri="file://$1#$2"
    uri="${uri//\'/\'\'}"
    jkb_sqlite "$db" "INSERT INTO items (id, kind, status) VALUES ($next_id, 'task', '$3');
        INSERT INTO bindings (uri, item_id) VALUES ('$uri', $next_id);" \
        || { echo "cannot write the fixture database" >&2; exit 1; }
}

# tasks_file <dir> — a tasks.md in <dir> with two tasks, ^own and ^stray.
tasks_file() {
    mkdir -p "$1"
    printf '%s\n' '## Tasks' '- [ ] owntask ^own' '- [x] straytask ^stray' >"$1/tasks.md"
}

# status_of <view output> <title> — the KB-STATUS column of that task's row.
status_of() {
    printf '%s\n' "$1" | awk -v t="$2" '/^\[/ && $NF == t { print $(NF - 1); exit }'
}

# check_isolated <label> <dir> <sibling dir>
check_isolated() {
    local label="$1" dir="$2" sibling="$3" out own stray
    tasks_file "$dir"
    bind "$dir/tasks.md" own todo
    bind "$sibling/tasks.md" stray done
    if ! out="$(JKB_DB="$db" bash "$repo_root/scripts/swarm-status.sh" --file "$dir/tasks.md" 2>&1)"; then
        fail "$label: exit" "swarm-status.sh --file failed: $out"
        return
    fi
    own="$(status_of "$out" owntask)"
    stray="$(status_of "$out" straytask)"
    if [ "$own" != todo ]; then
        fail "$label: own task" "expected the file's own task to read 'todo', got '$own' in:
$out"
        return
    fi
    if [ "$stray" = "(unbound)" ]; then
        ok "$label: a sibling's task is not reported as this file's"
    else
        fail "$label: leaked" "a task bound only under $sibling/tasks.md read '$stray' here:
$out"
    fi
}

# --- 1. an `_` in the path does not match a one-character-different sibling ---------------
case1() {
    check_isolated "underscore" "$work/u/my_repo" "$work/u/myXrepo"
}

# --- 2. a `%` in the path does not match an arbitrary run of characters -------------------
case2() {
    check_isolated "percent" "$work/p/50%" "$work/p/50-and-more"
}

# --- 3. a path differing only in case is another file (LIKE folded ASCII case) -------------
case3() {
    check_isolated "case" "$work/c/repo" "$work/c/Repo"
}

# --- 4. GLOB's own metacharacters in the path are literal --------------------------------
case4() {
    check_isolated "star" "$work/s/a*b" "$work/s/aXb"
    check_isolated "question" "$work/q/a?b" "$work/q/aXb"
    check_isolated "bracket" "$work/b/a[x]b" "$work/b/axb"
}

# --- 5. LIKE's metacharacters, which GLOB takes literally, are not escaped into the path ---
case5() {
    check_isolated "like-chars" "$work/l/a_b%c" "$work/l/aXbYc"
}

# --- 6. a `#` in a directory name does not cut the fragment short -------------------------
case6() {
    check_isolated "hash-dir" "$work/h/a#b" "$work/h/a#c"
}

# --- 7. a tasks.md with no id-stamped checkbox is reported, not a silent abort --------------
# `grep … | awk` under `pipefail`: grep's "no match" exit failed the pipeline and `errexit` ended
# the script after the table header — no legend, exit 1, nothing on stderr.
case7() {
    local dir="$work/e" out rc=0
    mkdir -p "$dir"
    printf '%s\n' '## Tasks' '- [ ] not yet stamped' 'prose only' >"$dir/tasks.md"
    out="$(JKB_DB="$db" bash "$repo_root/scripts/swarm-status.sh" --file "$dir/tasks.md" 2>&1)" || rc=$?
    if [ "$rc" -ne 0 ]; then
        fail "unstamped: exit" "swarm-status.sh --file exited $rc on a tasks.md with no stamped checkbox:
$out"
    elif ! grep -q '^legend:' <<<"$out" || ! grep -q 'no id-stamped checkbox' <<<"$out"; then
        fail "unstamped: report" "expected the 'no id-stamped checkbox' line and the legend in:
$out"
    else
        ok "a tasks.md with no stamped checkbox finishes the report and says so"
    fi
}

# --- 8. the sync_state section tells a missing row from a present one ---------------------
# For no rows the database shell prints nothing and exits 0, and the "not yet synced" line was the
# fallback for a FAILED read — so it never appeared for the case it names.
case8() {
    local dir="$work/ss" out uri
    tasks_file "$dir"
    out="$(JKB_DB="$db" bash "$repo_root/scripts/swarm-status.sh" --file "$dir/tasks.md" 2>&1)" || true
    if grep -q 'no sync_state row' <<<"$out"; then
        ok "a file with no sync_state row is reported as not yet synced"
    else
        fail "sync_state: absent" "expected '(no sync_state row …)' in:
$out"
    fi
    uri="file://$dir/tasks.md"
    jkb_sqlite "$db" "INSERT INTO sync_state (uri, status) VALUES ('${uri//\'/\'\'}', 'needs_attention');" \
        || { fail "sync_state: fixture" "cannot write the fixture database"; return; }
    out="$(JKB_DB="$db" bash "$repo_root/scripts/swarm-status.sh" --file "$dir/tasks.md" 2>&1)" || true
    if grep -q 'needs_attention' <<<"$out" && ! grep -q 'no sync_state row' <<<"$out"; then
        ok "a file with a sync_state row shows it"
    else
        fail "sync_state: present" "expected the row's status and no 'not yet synced' line in:
$out"
    fi
}

# no_shell_path — a PATH directory holding every command on $PATH except the database shell, so
# the view runs exactly as it would on a machine where it is not installed.
no_shell_path() {
    local d f name shell="sqli""te3" dirs
    mkdir -p "$work/noshell"
    IFS=: read -r -a dirs <<<"$PATH"
    for d in "${dirs[@]}"; do
        [ -d "$d" ] || continue
        for f in "$d"/*; do
            name="${f##*/}"
            [ "$name" = "$shell" ] && continue
            [ -e "$work/noshell/$name" ] || [ ! -x "$f" ] || ln -s "$f" "$work/noshell/$name"
        done
    done
    printf '%s\n' "$work/noshell"
}

# --- 9. a missing database shell is reported as itself, never as KB state -----------------
# It read as "(no sync_state row — file not yet synced)" and every task "(unbound)": the failure
# fell into the no-row fallback, and the task read's `|| true` captured the error as data.
case9() {
    local dir="$work/ns" out rc=0 p
    tasks_file "$dir"
    bind "$dir/tasks.md" own todo
    p="$(no_shell_path)"
    out="$(PATH="$p" JKB_DB="$db" bash "$repo_root/scripts/swarm-status.sh" --file "$dir/tasks.md" 2>&1)" || rc=$?
    if [ "$rc" -eq 0 ]; then
        fail "no shell: exit" "the view exited 0 with no database shell to read the KB:
$out"
    elif grep -qE 'not yet synced|\(unbound\)' <<<"$out"; then
        fail "no shell: wrong answer" "a missing database shell was reported as KB state:
$out"
    elif ! grep -q 'not installed' <<<"$out"; then
        fail "no shell: reason" "expected the missing shell to be named as the reason in:
$out"
    else
        ok "a missing database shell is reported as missing, with no KB state shown"
    fi
}

# --- 10. an unreadable database is a failure, not an empty answer -------------------------
case10() {
    local dir="$work/bad" out rc=0
    tasks_file "$dir"
    printf 'this is not a database, it only has the name of one\n%.0s' 1 2 3 4 5 6 7 8 >"$work/bad.db"
    out="$(JKB_DB="$work/bad.db" bash "$repo_root/scripts/swarm-status.sh" --file "$dir/tasks.md" 2>&1)" || rc=$?
    if [ "$rc" -ne 0 ] && grep -q 'cannot read the KB' <<<"$out" \
        && ! grep -qE 'not yet synced|\(unbound\)' <<<"$out"; then
        ok "an unreadable database stops the view with the reason"
    else
        fail "unreadable db" "expected a non-zero exit, 'cannot read the KB', and no KB state (exit $rc):
$out"
    fi
}

# --- 11. the run view's status counts cover the whole scope -------------------------------
# It tallied `jkb query --json … --limit 500`, which lists in id order, so a scope of 600 tasks was
# summarised by its 500 oldest under a heading that names the whole scope. The stand-in jkb lists
# exactly that way, and answers `--count` (which ignores any limit) for the scope and per status.
case11() {
    local run="$work/runs/wf_count" out
    mkdir -p "$run" "$work/fakejkb"
    printf '%s\n' '{"type":"result","result":{"groups":[{"tasks":[{"namespace":"tasks/demo/a"},{"namespace":"tasks/demo/b"}]}],"remaining":0}}' \
        >"$run/journal.jsonl"
    cat >"$work/fakejkb/jkb" <<'FAKE'
#!/usr/bin/env bash
# 600 tasks in id order: 400 done, 50 cancelled, 100 open, 50 in_progress.
status_of() {
    if [ "$1" -le 400 ]; then echo done; elif [ "$1" -le 450 ]; then echo cancelled
    elif [ "$1" -le 550 ]; then echo open; else echo in_progress; fi
}
case " $* " in
    *" --count "*)
        case "$*" in
            *status:done*) echo 400 ;; *status:cancelled*) echo 50 ;;
            *status:open*) echo 100 ;; *status:in_progress*) echo 50 ;;
            *status:*) echo 0 ;; *) echo 600 ;;
        esac ;;
    *)
        limit=600; prev=""
        for a in "$@"; do [ "$prev" = --limit ] && limit="$a"; prev="$a"; done
        [ "$limit" -gt 600 ] && limit=600
        printf '['
        for ((i = 1; i <= limit; i++)); do
            [ "$i" -gt 1 ] && printf ','
            printf '{"id":%d,"status":"%s"}' "$i" "$(status_of "$i")"
        done
        printf ']\n' ;;
esac
FAKE
    chmod +x "$work/fakejkb/jkb"
    out="$(cd "$work" && PATH="$work/fakejkb:$PATH" bash "$repo_root/scripts/swarm-status.sh" "$run" 2>&1)" || true
    if grep -q 'total=600 open=100 in_progress=50 needs_review=0 done=400 cancelled=50$' <<<"$out"; then
        ok "the run view counts every task in scope, not the oldest 500"
    else
        fail "run view counts" "expected 'total=600 open=100 in_progress=50 needs_review=0 done=400 cancelled=50' in:
$out"
    fi
}

echo "==> scripts/swarm-status.sh --file / run view"
run_cases case1 case2 case3 case4 case5 case6 case7 case8 case9 case10 case11
finish
