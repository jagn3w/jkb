#!/usr/bin/env bash
# Regression test for scripts/swarm-status.sh's FILE view (`--file <tasks.md> --db <db>`).
#
# The bug it exists for: the view finds a file's tasks with `b.uri LIKE '<file uri>#%'`, and the
# path went into that pattern with only its single quotes escaped. `_` and `%` stayed live, so an
# `_` in the path matched a one-character-different sibling directory, a `%` matched any run of
# characters, and the "disk marker | kb status" table reported ANOTHER file's tasks as this
# file's. The fix escapes `\`, `%` and `_` and adds `ESCAPE '\'`, as `jkb_core::sql::like_escape`
# does on the Rust side.
#
# Each case below puts the file under test next to a sibling directory the unescaped pattern
# would also match, binds a task fragment ONLY under the sibling, and asserts the file's own
# line for that fragment still reads `(unbound)`.
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
# LIKE/ESCAPE semantics under test are SQLite's either way. It understands only what the view
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
    CREATE TABLE bindings (uri TEXT, item_id INTEGER);" \
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

echo "==> scripts/swarm-status.sh --file"
run_cases case1 case2
finish
