#!/usr/bin/env bash
# Watch every check-config.sh assertion fail (design D49).
#
#   ./.container/mutate-config.sh
#
# WHY THIS EXISTS. verify.sh has mutate-verify.sh; check-config.sh had nothing, and three review
# rounds each found the same defect in it — an assertion that cannot fail. A guard matching text
# present on both the pass and fail paths; a regex that could not cross a shell quote and so never
# caught the exact code it existed to prevent; a rewrite that silently dropped the `type=volume`
# half of its own check while keeping the failure message about volumes. Each was found by a
# reviewer or by hand-mutating afterwards, which is a process that works right up until nobody
# does it. Needs no Docker, so unlike mutate-verify.sh this runs in ./scripts/check.sh.
#
# THE RULE, same as mutate-verify.sh: a mutation is CAUGHT only when check-config.sh exits non-zero
# AND prints a FAIL line matching the expectation. And the harness has a negative control — an
# unmutated tree must be reported MISSED, or the matcher is matching something that is present
# when nothing is wrong.
set -uo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
# The subject of this harness skips itself when jq is absent, so without the same precondition a
# machine without jq gets every mutation MISSED and a red shared gate — a security-shaped alarm for
# what is only a fact about the host. Every other machine-dependent gate step degrades to a named
# skip; this must too.
for t in jq python3; do
    command -v "$t" >/dev/null 2>&1 || { echo "==> container config guards"; echo "   (skipped: $t not installed; CI runs this gate)"; exit 0; }
done
work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT
fails=0

# check-config.sh reads $here/* and $here/../scripts/auto-mode-posture.json, so the copy has to
# preserve that shape.
seed() {
    rm -rf "$work/t"; mkdir -p "$work/t/scripts"
    cp -R "$repo/.container" "$work/t/.container"
    cp "$repo/scripts/auto-mode-posture.json" "$work/t/scripts/"
    # check-config.sh now compares the gate's --self-test list against CI's, so both files have to
    # be in the copy or that assertion would fail for every mutation and pass for none.
    cp "$repo/scripts/check.sh" "$work/t/scripts/"
    mkdir -p "$work/t/.github/workflows"
    cp "$repo/.github/workflows/ci.yml" "$work/t/.github/workflows/"
    # check-config.sh derives the locally-built extension's id from here, and skips when the repo
    # has none — so without this the mutation below could not be watched failing.
    mkdir -p "$work/t/ui/vscode"
    cp "$repo/ui/vscode/package.json" "$work/t/ui/vscode/"
    # ...and the repo's committed Claude settings, whose `env` check-config.sh holds to the names the
    # container sets.
    mkdir -p "$work/t/.claude"
    cp "$repo/.claude/settings.json" "$work/t/.claude/"
    # ...and jkb-daemon's DEFAULT_ADDR, which check-config.sh holds the firewall's daemon port to.
    mkdir -p "$work/t/crates/jkb-daemon/src"
    cp "$repo/crates/jkb-daemon/src/lib.rs" "$work/t/crates/jkb-daemon/src/"
    # ...and jkb-cli's remote.rs, which names the variable the notification hook reads.
    mkdir -p "$work/t/crates/jkb-cli/src"
    cp "$repo/crates/jkb-cli/src/remote.rs" "$work/t/crates/jkb-cli/src/"
    # ...and transcripts.rs, which owns the container name and the in-image path the host reaper
    # pokes -- check-config.sh holds both to run.sh and the Dockerfile.
    cp "$repo/crates/jkb-cli/src/transcripts.rs" "$work/t/crates/jkb-cli/src/"
    # ...and jkb-api, which owns the JSON field name run.sh's jq filter reads.
    mkdir -p "$work/t/crates/jkb-api/src"
    cp "$repo/crates/jkb-api/src/lib.rs" "$work/t/crates/jkb-api/src/"
    # ...and swarm-status.sh, which OWNS the name the transcript sweep has to spare: check-config.sh
    # reads it out of that file's discovery predicate rather than spelling it. Without this in the
    # copy the extraction reads nothing, which is a failure by design -- so it would redden the
    # unmutated tree and take the negative control with it.
    cp "$repo/scripts/swarm-status.sh" "$work/t/scripts/"
    # The manifest is what lets mutated() see a DELETION or a MODE CHANGE. Taken here rather than
    # derived from a list, so it still covers a file added to seed() tomorrow.
    tree_manifest > "$work/manifest"
}

# One line per file: its executability and its path. Executability is in here because `chmod -x` is
# a mutation that changes no BYTES -- the second blind spot found in this guard, after deletion,
# and by the same method: writing the mutation and watching a correct assertion be called a no-op.
# Only the x bit, not the full mode: it is portable (BSD and GNU `stat` disagree on flags) and it
# is the only mode any mutation here changes.
tree_manifest() {
    local f
    find "$work/t" -type f | sort | while IFS= read -r f; do
        if [ -x "$f" ]; then printf 'x %s\n' "$f"; else printf -- '- %s\n' "$f"; fi
    done
}

DC() { printf '%s' "$work/t/.container/container.json"; }

EXPECTS=()
# EVERY MUTATION MUST ACTUALLY HAVE MUTATED SOMETHING. Most of these edit the seeded copy with a
# literal `s.replace(...)` against a whitespace-exact target -- ten leading spaces of YAML, an exact
# path expression -- and a bare replace that matches NOTHING is silent. Reindent a CI step or move
# a path into a variable and the unmutated tree is handed to check-config.sh, which passes, and the
# harness prints MISSED: a red shared gate blaming a guard that is perfectly fine, pointing at the
# wrong file. `sub_dc` and a couple of the blocks assert their target is present for exactly this
# reason; nineteen others did not.
#
# Asked ONCE here rather than added to each block, so a mutation written next year cannot omit it.
# Comparing the whole seeded tree against the pristine one also catches a mutation that edits the
# wrong file, which a per-block assertion would not.
# DERIVED FROM THE SEEDED TREE, not from a list of the files seed() copies. The first version
# named three paths and immediately called two perfectly good mutations no-ops, because they edit
# ui/vscode/package.json and scripts/auto-mode-posture.json -- a second list that has to be kept in
# step with seed(), which is the defect this file keeps finding elsewhere. Walking what was
# actually seeded needs no list and covers a file added to seed() tomorrow.
# PRESENCE FIRST, THEN CONTENT. Walking only the files that are THERE cannot see a mutation that
# DELETES one -- the removed file simply stops appearing, every survivor matches the repo, and a
# deletion mutation is reported as changing nothing. Found by writing the first such mutation
# (removing generate-apparmor.sh) and watching a correct guard be called a no-op.
mutated() { # mutated -> 0 if the seeded tree differs from what seed() laid down, or from the repo
    local line f
    tree_manifest | cmp -s - "$work/manifest" || return 0
    while IFS= read -r line; do
        f="${line#? }"
        cmp -s "$f" "$repo/${f#"$work/t/"}" || return 0
    done < "$work/manifest"
    return 1
}

run() { # run <label> <expect-substring>
    local label="$1" expect="$2" out rc
    EXPECTS+=("$expect")
    if ! mutated; then
        fails=$((fails+1))
        printf '  \033[31mNO-OP\033[0m    %s\n' "$label"
        printf '           the mutation changed nothing — its target has moved, so this tests the\n'
        printf '           UNMUTATED tree and would report MISSED about a guard that is fine.\n'
        return
    fi
    out="$(cd "$work/t" && ./.container/check-config.sh 2>&1)"; rc=$?
    judge "$label" "$expect" "$out" "$rc"
}

# Separated from executing so the control can judge the SAME run it health-checked, rather than
# starting a second one that could differ from it. See mutate-verify.sh for the failure this
# prevents.
judge() { # judge <label> <expect> <output> <rc>
    local label="$1" expect="$2" out="$3" rc="$4"
    # A mutation is CAUGHT only when check-config.sh FAILS and says why, with both on the SAME line.
    # Fixed-string, because the regex form escaped only some ERE metacharacters and silently
    # mis-matched "host bind source(s) parsed"; `-e`, because an expect may start with a dash,
    # which grep would otherwise read as an option.
    if [ "$rc" -ne 0 ] && grep -q "FAIL" <<<"$(grep -F -e "$expect" <<<"$out")"; then
        printf '  CAUGHT   %s\n' "$label"
    else
        fails=$((fails+1))
        printf '  MISSED   %s  (exit %s; wanted a FAIL line mentioning: %s)\n' "$label" "$rc" "$expect"
        sed 's/^/           /' <<<"$out" | grep -E "FAIL|passed|failed" | head -3
    fi
}

# jq edit on the copied container.json, preserving its // comments by editing the raw text
# through a strip/emit cycle only where jq is genuinely needed.
jq_dc() { local f; f="$(DC)"; sed 's://.*$::' "$f" | jq "$1" > "$f.new" && mv "$f.new" "$f"; }
sub_dc() { local f; f="$(DC)"; python3 - "$f" "$1" "$2" <<'PY'
import sys
p, old, new = sys.argv[1], sys.argv[2], sys.argv[3]
s = open(p).read()
assert old in s, "mutation target not present: " + old
open(p, 'w').write(s.replace(old, new, 1))
PY
}

echo "==> mutations of the container config (each must be CAUGHT)"

seed; sub_dc '"remoteUser": "vscode"' '"remoteUser": "root"'
run "remoteUser becomes root" "remoteUser is root"

seed; jq_dc '.runArgs |= map(select(. != "--security-opt"))'
run "the --security-opt flag is dropped, leaving its value orphaned" "--security-opt"

# ONE THING: drops only the systempaths VALUE, so the seccomp pair still holds and this can only
# be the new assertion failing. Dropping the flag (above) breaks both, which establishes neither.
seed; jq_dc '.runArgs |= map(select(. != "systempaths=unconfined"))'
run "the /proc unmask is dropped, so bubblewrap could not mount proc" "systempaths=unconfined"

seed; sub_dc '"--cap-add=NET_ADMIN",' ''
run "NET_ADMIN is dropped" "no longer declares --cap-add=NET_ADMIN"

# The generated profile: drop `unshare` from its unconditional allow group.
seed; python3 - "$work/t/.container/seccomp-bwrap.json" <<'PY'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
for e in d["syscalls"]:
    if e.get("action") == "SCMP_ACT_ALLOW" and not e.get("args") and "unshare" in e.get("names", []):
        e["names"].remove("unshare")
json.dump(d, open(p, "w"))
PY
run "a needed syscall is not allowed" "does not unconditionally allow"

# ...and the negative half: re-add a restriction naming one, so the allow is shadowed.
seed; python3 - "$work/t/.container/seccomp-bwrap.json" <<'PY'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["syscalls"].append({"names": ["pivot_root"], "action": "SCMP_ACT_ERRNO", "args": []})
json.dump(d, open(p, "w"))
PY
run "a restricted entry still names a needed syscall" "the removal loop missed"

seed; python3 - "$work/t/.container/generate-seccomp.sh" <<'PY'
import re, sys
p = sys.argv[1]; s = open(p).read()
# Break the extraction the two assertions above loop over.
s = s.replace('"unshare",', 'unshare').replace('"pivot_root",', 'pivot_root')
open(p, 'w').write(s)
PY
run "the generator's syscall list stops parsing" "no longer yields"

seed; jq_dc 'del(.mounts[] | select(test("/home/vscode/.jkb/")))'
run "the narrowed ~/.jkb mounts are dropped" "declared mount set is missing"

seed; jq_dc '.mounts += ["source=${localEnv:HOME}/.jkb,target=/home/vscode/.jkb-whole,type=bind"]'
run "the whole host ~/.jkb is bound again (D52.8)" "the whole ~/.jkb: the operator's database and root token"

seed; jq_dc '.mounts |= map(sub(",readonly$"; ""))'
run "the container credential is bound writable" "the container credential mount is not read-only"

# EVERY KEY IN container.json IS APPLIED BY SOMETHING. This replaced four mutations about
# workspaceFolder and initializeCommand, which were Dev Containers' rules and are gone with it —
# and it guards the risk the move introduced: VS Code no longer reads this file, so a key nobody
# applies is now possible and looks exactly like configuration.
seed; jq_dc '.postCreateCommand = "bash .container/setup.sh"'
run "a key is added that nothing applies" "which run.sh does not read"

# ...and both sides of it, pinned against a vacuous pass. An empty consumed list would report
# every key as unread (a different lie), and no declared keys would check nothing at all.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("consumed_keys() {\n    cat <<'KEYS'", "consumed_keys() {\n    return 0\n    cat <<'KEYS'", 1))
PYX
run "run.sh stops naming the keys it applies" "cannot tell an applied key from an ignored one"

# The firewall-argument guard now DERIVES its callers instead of naming setup.sh and run.sh, so it
# covers a caller added later — the entrypoint being the first one that would have been missed.
# Both halves of that derivation get watched failing, because a derived list that silently comes
# back empty is a guard reporting `ok` about nothing, which is the failure the hand-written list at
# least could not have.
seed; for f in "$work"/t/.container/*.sh; do
    case "$(basename "$f")" in
        init-firewall.sh|pin-jkb-hook.sh|check-config.sh|mutate-config.sh) continue ;;
    esac
    # Every mention removed, so nothing looks like a caller any more — of either guarded script.
    python3 - "$f" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("init-firewall.sh", "some-other-script.sh").replace("pin-jkb-hook.sh", "yet-another-script.sh"))
PYX
done
run "no script reaches the firewall-argument guard" "the derivation below is checking nothing"

# ...and the completeness half: a caller that stops calling it must be reported, not quietly
# dropped from a list that then still prints `ok`.
seed; python3 - "$work/t/.container/entrypoint.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("init-firewall.sh", "some-other-script.sh"))
PYX
run "the entrypoint stops raising the firewall" "no longer reaches the firewall-argument guard"

# THE TWO SETUP-MARKER MUTATIONS ARE GONE with the guard they exercised (D52.5). One drifted the
# two spellings of the marker path apart, the other made the extraction find nothing; both required
# check-config.sh to notice. There is one spelling now -- JKB_SETUP_MARKER in lib.sh, sourced by
# run.sh on the host and by setup.sh inside the container -- so there is nothing to induce, and a
# mutation for a deleted assertion reports MISSED for ever.

# ...and the second verifier put back, which is how the fix for "a fatal verify hides the attach
# instructions" comes to hold on one path and not the other again.
seed; python3 - "$work/t/.container/setup.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s + '\n"$repo/.container/verify.sh"\n')
PYX
run "setup.sh verifies as well as run.sh" "a second verifier there"

# ...and the half that guard's passing line CLAIMS but never checked: that run.sh still verifies.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("verify.sh", "nothing.sh"))
PYX
run "run.sh stops verifying at all" "nothing verifies the container"

# THE TRANSCRIPT SWEEP, one mutation per way its enumeration goes wrong, and one per way the
# wiring does. Every one of them is silent at run time — the sweep reports success while archiving
# the wrong set or the empty set — so this is the only place any of them is ever observed failing.
#
# ANCHORED ON CODE THE FILE HAS TO KEEP: the `find` line inside `transcript_records`, the function
# name check-config.sh extracts by, and run.sh's invocation statement. Never on a message: a
# mutation anchored on wording silently becomes a NO-OP the day the wording is improved, and then
# reports MISSED about a guard that is perfectly fine. `assert` on each, so that if one of these
# anchors DOES move, this says so instead of certifying the unmutated tree.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'find -L "$abs"'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'find "$abs"', 1))
PYX
run "the sweep stops following a symlinked root" "does not pass -L"

seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'find -L "$abs" -type d'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'find -L "$abs" -maxdepth 2 -type d', 1))
PYX
run "the sweep bounds its depth, missing the nested agent transcripts" "it caps the depth"

seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = "-name '*.jsonl' -exec"
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, "-exec", 1))
PYX
run "the sweep stops filtering on *.jsonl, so auto-memory is in the plan" "does not filter on *.jsonl"

# THE HARNESS'S OWN RUN JOURNAL, and BOTH ways of getting it wrong, because the repair for the
# first shipped the second. `*.jsonl` matches journal.jsonl, so a name of its own is the only thing
# between the sweep and the state swarm-status.sh finds every run by -- and it shipped without one,
# which is how the sweep came to archive 23 of them oldest-first. The fix then put that exclusion
# in the WALK, which feeds the PROJECTION as well as the plan, so the sweep sized the argv at
# 65,250 bytes of a real 96,612 and printed "nothing to archive" while every Bash call went on
# dying at spawn. One mutation each, in the two directions.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = "-name '*.jsonl' -exec"
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, "-name '*.jsonl' ! -name journal.jsonl -exec", 1))
PYX
run "the sweep holds the journal back in the walk, hiding its bytes from the budget" "holds a name back in the WALK"

# ...AND THE NAME ITSELF, which an external harness owns. Renaming it here has to be caught by
# DISAGREEMENT with swarm-status.sh and not by a literal this file also spells, because a literal
# is green through the case that actually matters: the harness renaming its own run-state file.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = "\nHELD_NAME=journal.jsonl\n"
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, "\nHELD_NAME=run.jsonl\n", 1))
PYX
run "the sweep spares a name swarm-status.sh does not look for" "while swarm-status.sh finds runs by"

# ...and that reader going away, which is the case a literal could never have seen: the guard must
# refuse to establish anything rather than pass on a name it could not read.
seed; python3 - "$work/t/scripts/swarm-status.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = "-name journal.jsonl -path '*/subagents/workflows/wf_*'"
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, "-name journal.jsonl", 1))
PYX
run "swarm-status.sh stops discovering runs by that predicate" "cannot be read from the reader that defines it"

# THE ARCHIVE MUST BE OUTSIDE THE ROOT. Anchored on the case pattern, which is the refusal itself,
# not on its message.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '        "$phys_root"/*)'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '        "$phys_root"/never-matches-anything)', 1))
PYX
run "the sweep accepts an archive inside the root it enumerates" "does not refuse an archive inside the root"

# ...AND THE REFUSAL COMPARING SPELLINGS RATHER THAN RESOLVED PATHS, which is how it shipped: a
# string-prefix test against the caller's spelling, while the walk is `-L` and the container's root
# IS a symlink. The refusal was present, pinned, mutated -- and inoperative in production.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'phys_root="$(cd "$root" && pwd -P)"'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'phys_root="$(cd "$root" && pwd)"', 1))
PYX
run "the containment test goes back to comparing the caller's spellings" "rather than resolved paths"

# ...AND THE OTHER SIDE OF THAT COMPARISON, which the round that added the refusal left pinned by
# nothing. A comparison has two operands; resolving one of them is half a guard.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'phys_archive="$(transcript_resolve "$archive")"'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'phys_archive="$archive"', 1))
PYX
run "the archive side of the containment test stops being resolved" "does not resolve the ARCHIVE side"

seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'transcript_resolve() {'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'resolve_it() {', 1))
PYX
run "the resolver is renamed, so the guard reads nothing" "no transcript_resolve"

seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'base="$(cd "$p" && pwd -P)" || return 1'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'base="$(cd "$p" && pwd)" || return 1', 1))
PYX
run "the resolver stops reaching a physical path" "does not reach a physical path"

# THE POST-CONDITION'S CALL SITE. Its arithmetic is watched by the self-test from literals; the
# CALL is watched only here, because the state it guards is refused upstream and so cannot be
# reached from the fixture.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '! transcript_projection_fell "$moved" "$before" "$after"'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'false', 1))
PYX
run "the sweep stops asking whether the projection fell" "it never asks whether the projection actually fell"

# THE VANISHED-SOURCE SKIP, whose only watcher this is: no executable test reaches a file that
# disappears between the plan and the move, so the static pin and this mutation are the whole of it.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '        [ -e "$f" ] || continue\n'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '', 1))
PYX
run "a transcript that raced away is counted as a failure again" "counts a transcript that vanished"

# THE SELF-TEST SEAM WIRED INTO THE CONTAINER, which would disable the sweep while every start went
# on reporting success -- the exact state the script exists to end, wearing a clean log.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'bash "$DC_CTR_KIT/.container/sweep-transcripts.sh" || true'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'JKB_DENY_BUDGET_BYTES=99999999 bash "$DC_CTR_KIT/.container/sweep-transcripts.sh" || true', 1))
PYX
run "run.sh wires the self-test budget seam into the container" "sets JKB_DENY_BUDGET_BYTES"

# ...AND EVERY OTHER SEAM, because the guard that shipped named one of three. A root that does not
# exist is the cheapest way to switch the sweep off and leave a clean log.
seed; python3 - "$work/t/.container/Dockerfile" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = "\nUSER vscode\n"
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, "\nENV JKB_TRANSCRIPT_ROOT=/nonexistent\nUSER vscode\n", 1))
PYX
run "the Dockerfile pins the transcript root to a tree that is not there" "sets JKB_TRANSCRIPT_ROOT"

# ...and the list itself going away, which is what makes the loop above establish anything.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '\nSEAMS="'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '\nSEAM_NAMES="', 1))
PYX
run "the seam list is renamed, so the guard reads nothing" "no SEAMS= line to read"

# THE OPERATOR-FACING HALF. run.sh discards the sweep's exit code on purpose, so verify.sh -- the
# thing anyone actually reads after a start -- is the only durable place the budget is reported.
seed; python3 - "$work/t/.container/verify.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'bash "$sweep_sh" --dry-run'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'true --not-a-dry-run', 1))
PYX
run "verify.sh stops reporting the transcript deny list" "does not ask whether the deny list still fits"

seed; rm -f "$work/t/.container/verify.sh"
run "verify.sh is deleted outright" "verify.sh is not there to report the deny list"

# ...AND THE VERDICT ARMS, which the call-site check above cannot see. The realistic drift is not
# deletion but a refactor that keeps the call and demotes the verdict to a note -- the same green.
seed; python3 - "$work/t/.container/verify.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'accept_bad "the transcript deny list cannot be brought under budget'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'echo "  note the transcript deny list cannot be brought under budget', 1))
PYX
run "verify.sh demotes the accepted deny-list verdict to a note" "reaches no \`accept_bad\` verdict"

# THE SEAM DECLARATION DRIFTING FROM THE CODE, which is the shape that put one of three seams
# behind the refusal in the first place.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'INPUTS="$SEAMS '
assert s.count(old) == 1, "mutation target absent"
i = s.index(old) + len(old)
j = s.index('"', i)
open(p, 'w').write(s[:i] + s[j:])
PYX
run "an input the script reads is dropped from INPUTS" "so an input is covered by neither"

# ...and verify.sh, the caller that runs INSIDE the container, where a seam actually takes effect.
seed; python3 - "$work/t/.container/verify.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'sweep_dry="$(bash "$sweep_sh" --dry-run 2>&1)"'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'sweep_dry="$(JKB_DENY_BUDGET_BYTES=99999999 bash "$sweep_sh" --dry-run 2>&1)"', 1))
PYX
run "verify.sh wires the budget seam into its own call" "verify.sh sets JKB_DENY_BUDGET_BYTES"

# THE PHRASES THE TWO FILES SHARE. verify.sh tells "over budget" from "beyond any sweep's help" from
# "could not answer" by matching the sweep's own wording; reword one end and the container is
# silently reclassified for ever, with no gate noticing.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'archiving every transcript cannot bring this tree under it'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'no amount of archiving helps here', 1))
PYX
run "the sweep rewords the phrase verify.sh classifies on" "which sweep-transcripts.sh never prints"

seed; python3 - "$work/t/.container/verify.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '*"E2BIG"*'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '*"E2BIG-renamed"*', 1))
PYX
run "verify.sh classifies on a phrase the sweep does not print" "which sweep-transcripts.sh never prints"

seed; python3 - "$work/t/.container/verify.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
for old, new in (('*"does not exist"*) printf', '*) printf'),
                 ('*"cannot bring this tree under it"*) printf', '*) printf'),
                 ('*"E2BIG"*) printf', '*) printf'),
                 ("grep -F 'no sweep can remove'", "head -0")):
    assert old in s, "mutation target absent"
    s = s.replace(old, new, 1)
open(p, 'w').write(s)
PYX
run "verify.sh stops classifying on any phrase" "classifies on no phrase at all"

# ...AND THE FUNCTION THAT DECIDES, renamed so the extraction reads nothing. The classifiers moved
# into it when the chain was made pure, and the guard went on reading the reporting block -- where
# there were then none left, so it established nothing and two mutations went MISSED.
seed; python3 - "$work/t/.container/verify.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'sweep_verdict() {'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'decide_sweep() {', 1))
PYX
run "the sweep verdict function is renamed, so the guard reads nothing" "no sweep_verdict() to read"

# THE NAMES THAT MUST AGREE for the host reaper to reach this container at all. Each is silent
# when wrong: a reaper poking a name nothing creates, or running a path the image does not carry,
# reports nothing for ever -- the same end state as having no trigger between starts, with a green log.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'NAME="${JKB_CONTAINER_NAME:-jkb-dev}"'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'NAME="${JKB_CONTAINER_NAME:-jkb-devbox}"', 1))
PYX
run "run.sh renames the container the reaper pokes" "so the only trigger between container starts reaches nothing"

# ...AND THE NAME BECOMING UNREADABLE, which is the third "extraction read nothing" branch in this
# block and the one that shipped without a mutation. Behaviour-preserving: the quotes simply go, a
# shape run.sh uses elsewhere.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'NAME="${JKB_CONTAINER_NAME:-jkb-dev}"'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'NAME=${JKB_CONTAINER_NAME:-jkb-dev}', 1))
PYX
run "run.sh spells its container name in a shape the guard cannot read" "cannot be read from both run.sh and transcripts.rs"

seed; python3 - "$work/t/crates/jkb-cli/src/transcripts.rs" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'include_str!("../../../.container/sweep-transcripts.sh")'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '"echo no-op"', 1))
PYX
run "the reaper stops embedding the sweep" "no longer embeds the sweep"

seed; python3 - "$work/t/crates/jkb-cli/src/transcripts.rs" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '"exec", "-i", "-e", &keep, name, "/bin/bash", "-s"'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '"exec", name, "/bin/bash", "/usr/local/bin/sweep-transcripts.sh"', 1))
PYX
run "the reaper goes back to a path inside the container" "no longer feeds the sweep in on stdin"

seed; python3 - "$work/t/crates/jkb-cli/src/transcripts.rs" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '"nothing to archive"'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '"nothing needs archiving"', 1))
PYX
run "the reaper classifies a quiet tick on words the sweep does not print" "which the sweep never prints"

# A LIVE SESSION'S TRANSCRIPT, which the newest-32 floor stopped protecting the day the sweep gained
# a timer. Four ways to lose it, each silent: the two ends naming the variable differently, the
# sweep ignoring the list, the sweep ignoring recency, and the list going unread altogether.
seed; python3 - "$work/t/crates/jkb-cli/src/transcripts.rs" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'KEEP_SESSIONS_VAR: &str = "JKB_KEEP_SESSIONS"'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'KEEP_SESSIONS_VAR: &str = "JKB_LIVE_SESSIONS"', 1))
PYX
run "the reaper names the keep-list something the sweep does not read" "so a live session's transcript can be archived"

seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '        if (index(path, "/" id ".jsonl") > 0) return 1\n'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '', 1))
PYX
run "the sweep stops skipping the sessions it is told are live" "keep-list is data nothing acts on"

seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '    if (now > 0 && fresh > 0 && mtime > now - fresh) return 1\n'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '', 1))
PYX
run "the sweep stops sparing recently-written transcripts" "does not spare recently-written transcripts"

seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'KEEP_SESSIONS="${JKB_KEEP_SESSIONS:-}"'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'KEEP_SESSIONS=""', 1))
PYX
run "the sweep stops reading the keep-list at all" "cannot be read from both transcripts.rs and sweep-transcripts.sh"

# ...AND WHAT A LIVE SESSION WRITES BENEATH ITSELF, which is the bulk of the population and has no
# registry row of its own: a Task-tool subagent opens no session.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '        if (index(path, "/" id "/") > 0) return 1\n'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '', 1))
PYX
run "a live session stops protecting its own subagents' transcripts" "BENEATH its own directory"

# ...and the two readers of that predicate drifting apart, which is a live transcript archived by a
# sweep that reported itself unable to reclaim anything.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '          if (protected($2, $1)) { tot += length($2); next }\n'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '', 1))
PYX
run "the irreducible measure stops asking what a live session holds" "transcript_irreducible() does not ask whether a file is protected"

# ...AND THE PLAN'S HALF OF IT, which the loop above covers for one function and this covers for the
# other. Only the irreducible arm had a mutation, so the guard that keeps a live transcript out of
# the PLAN -- the one that actually prevents the move -- had never been watched failing.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '          if (protected($2, $1)) next\n'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '', 1))
PYX
run "the plan stops asking what a live session holds" "transcript_plan() does not ask whether a file is protected"

# THE START TRIGGER FIRING WHEN SOMETHING IS OPEN. It carries no live-session list, so its whole
# safety is that nothing is running when it fires.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '-e "JKB_KEEP_SESSIONS=$sweep_keep" '
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '', 1))
PYX
run "run.sh sweeps without telling the container which sessions are live" "sweeps without passing the live-session list"

# ...AND THE DERIVATION BEHIND THE FLAG. Deleting the block that fills it leaves `-e
# "JKB_KEEP_SESSIONS=$sweep_keep"` passing an empty string for ever, with the guard above reporting
# a protection that no longer exists -- a one-line reversion to the unprotected state.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'jkb notify sessions --live-ids'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'true --no-sessions', 1))
PYX
run "run.sh stops asking jkb which sessions are live" "never asks jkb which sessions are live"

# ...AND THE TWO EMPTY STATES. A sweep that ran with no protection must not read, in the scroll-back,
# like one that had nothing to protect.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'could not ask jkb which sessions are live'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'no live sessions', 1))
PYX
run "run.sh stops saying when it could not read the registry" "does not say when it could not read the registry"

# THE REAPER'S QUIET-TICK PHRASES going unread, the last extraction-read-nothing branch with no
# mutation of its own.
seed; python3 - "$work/t/crates/jkb-cli/src/transcripts.rs" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'const NOTHING_TO_DO'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'const QUIET_PHRASES', 1))
PYX
run "the reaper renames the phrases it calls a quiet tick by" "declares no NOTHING_TO_DO phrases"

# A QUIET EXIT WITH NO MARKER. The reaper prints `Said` unconditionally, so a success path the sweep
# adds without a phrase in NOTHING_TO_DO is one identical line every quarter of an hour for ever.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
# Anchored on sweep_transcripts's own last statement: a bare `    return 0` also appears inside the
# AWK_PROTECTED string, which is not the shell function the guard counts.
old = '        transcript_over_budget "$after" "remain after this sweep" || return 1'
assert s.count(old) == 1, "mutation target absent"
open(p, 'w').write(s.replace(old, old + '\n        [ -n "$abs" ] && return 0', 1))
PYX
run "the sweep gains a success exit with no phrase behind it" "success exits, pinned at 4"

# ...AND THE VERIFY EXEC'S OWN KEEP LIST, which was round 11's must-fix and which nothing pinned:
# deleting only that occurrence left every gate green.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'in_container -e "JKB_KEEP_SESSIONS=$sweep_keep" -e "JKB_REPO_ROOT=$ctr_repo" -w "$ctr_repo" "$NAME" /bin/bash "$DC_CTR_KIT/.container/verify.sh"'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'in_container -e "JKB_REPO_ROOT=$ctr_repo" -w "$ctr_repo" "$NAME" /bin/bash "$DC_CTR_KIT/.container/verify.sh"', 1))
PYX
run "verify.sh measures a different tree from the one the sweep acted on" "verify.sh is measured without the live-session list"

seed; rm -f "$work/t/crates/jkb-cli/src/transcripts.rs"
run "the reaper's container module is deleted" "nothing sweeps the container between starts"

# THE PLAIN `bad` ARMS, which the accept_bad mutation above leaves standing because `accept_bad "`
# contains `bad "` -- the substring the guard is now anchored against. All three, because a STATIC
# guard cannot tell "this block reaches a bad verdict" from "this block reaches a bad verdict FOR
# THE BUDGET": demoting two of the three leaves the third, and the grep is satisfied. What that
# costs is written down in check-config.sh beside the loop, and the behavioural half -- that an
# over-budget tree really produces exit 1 and an unreclaimable one exit 3 -- belongs in
# mutate-verify.sh, which needs a container.
seed; python3 - "$work/t/.container/verify.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
n = 0
for old in ('bad "the transcript sweep could not answer',
            'bad "the transcript deny list is over budget',
            'bad "the transcript deny list was never measured',
            'bad "there is no sweep-transcripts.sh beside this script'):
    assert old in s, "mutation target absent"
    s = s.replace(old, 'echo "  note: ' + old[5:], 1); n += 1
assert n == 4, "mutation target absent"
open(p, 'w').write(s)
PYX
run "verify.sh demotes its plain deny-list verdicts to notes" "reaches no \`bad\` verdict"

# THE EMITTING HALF'S BOUNDARY, which is an extraction bounded by a literal and so fails by reading
# EVERYTHING rather than nothing. Behaviour-preserving: the two tests simply swap.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'if [ "${1:-}" = "--self-test" ] && [ "$#" -eq 1 ]; then'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'if [ "$#" -eq 1 ] && [ "${1:-}" = "--self-test" ]; then', 1))
PYX
run "the self-test dispatch is reworded, widening the emitting half to the whole file" "is no longer where the emitting half"

# THE TWO "EXTRACTION READ NOTHING" BRANCHES that shipped with no mutation, which is what the
# coverage line used to claim could not happen.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
assert '${JKB_' in s, "mutation target absent"
open(p, 'w').write(s.replace('${JKB_', '${XKB_'))
PYX
run "the sweep stops reading any JKB_ override, so SEAMS cannot be checked" "it reads no \${JKB_"

seed; python3 - "$work/t/.container/verify.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
assert 'sweep_sh=' in s, "mutation target absent"
open(p, 'w').write(s.replace('sweep_sh', 'sweeper_path'))
PYX
run "verify.sh renames the handle its deny-list block is read by" "has no deny-list block to read"

# BOTH HALVES OF "NO SWEEP CAN FIX THIS". verify.sh decides exit 3 against exit 1 on that sentence,
# which is the difference between a remedy that applies and three that do not (both codes refuse a
# window; run.sh tests `-ne 0`),
# and the floor half is the one that shipped unwatched: with it gone, the window the README says this
# closed reports an unhelpable tree as a broken boundary and run.sh refuses to open a window.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '          if (kept < keep)       { tot += length($2); kept++ } }'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '          }', 1))
PYX
run "the floor stops counting toward what no sweep can remove" "does not count the newest KEEP_NEWEST"

seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '          if (base == held)        { tot += length($2); next }'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '          if (base == held)  { next }', 1))
PYX
run "the held-back files stop counting toward what no sweep can remove" "does not count the held-back files toward what no sweep can remove"

seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'transcript_irreducible() {'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'irreducible_bytes() {', 1))
PYX
run "the irreducible measure is renamed, so the guard reads nothing" "no transcript_irreducible"

# THE VERIFY LINE THE ORDERING IS MEASURED AGAINST. Reading nothing there used to make the ordering
# test SKIP rather than fail -- the one extraction in this block that was not pinned against an
# empty read, which is the failure mode this whole file exists to refuse.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'bash "$DC_CTR_KIT/.container/verify.sh"'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'bash "$DC_CTR_KIT/.container/verify-renamed.sh"', 1))
PYX
run "run.sh has no verify statement to order the sweep against" "no verify.sh statement to order it against"

# ONE SPELLING OF THE CLAUDE CONFIG BASE, shared with commands.rs, auto-mode.sh and swarm-status.sh.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '${CLAUDE_CONFIG_DIR:-'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '${JKB_NOT_THE_CONFIG_DIR:-', 1))
PYX
run "the sweep spells the config base its own way" "does not honour CLAUDE_CONFIG_DIR"

# ...AND THE SCRIPT SIMPLY ABSENT, which is a FAIL path check-config.sh has always had and nothing
# watched: the composed assertion emits every condition through ONE `bad "`, so PINNED_BAD_SITES
# moved by one when the whole block arrived and could not notice which branches had a mutation.
# That is what PINNED_SWEEP_APPENDS is for, and why no count is written in prose here.
seed; rm -f "$work/t/.container/sweep-transcripts.sh"
run "the sweep script is deleted outright" "sweep-transcripts.sh is not there at all"

seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '-name memory -prune'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '-name memory-never-matches -prune', 1))
PYX
run "the sweep walks out through the memory symlink into ~/.jkb" "does not prune memory/"

# ...AND THE EXTRACTION ITSELF, pinned against reading nothing. check-config.sh pulls the function
# body out by name, and a rename would leave its four greps matching an empty string — which is
# four `ok`s about a file nobody read, the exact shape this harness exists to refuse.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'transcript_records() {'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'enumerate_them() {', 1))
PYX
run "the enumeration is renamed, so the guard reads nothing" "no transcript_records"

# THE WIRING, every part of it. Removing it is a container that fills up again; moving it after the
# verify is the reap's own bug back — one failing assertion about something else and the sweep
# never runs, which is how the deleted postStartCommand came to be unconditional; and dropping the
# `|| true` makes one unarchivable file abort the start under run.sh's own `set -euo pipefail`.
#
# ANCHORED ON THE INVOCATION, NOT ON THE WHOLE LINE. These two used to carry `|| true` inside their
# anchor strings, which made them the only watcher the non-fatality had: strip it from run.sh and
# both reported `NO-OP the mutation changed nothing`, pointing at the mutation rather than at the
# property, whose natural repair (relax the anchor) greens the gate. They now find the line by its
# statement and take the whole line, so the third mutation below owns `|| true` alone.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
lines = s.split('\n')
hit = [i for i, l in enumerate(lines) if 'bash "$DC_CTR_KIT/.container/sweep-transcripts.sh"' in l and not l.lstrip().startswith('#')]
assert len(hit) == 1, "mutation target absent"
del lines[hit[0]]
open(p, 'w').write('\n'.join(lines))
PYX
run "run.sh stops sweeping transcripts" "run.sh does not invoke it"

seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
lines = s.split('\n')
def only(needle):
    hit = [i for i, l in enumerate(lines) if needle in l and not l.lstrip().startswith('#')]
    assert len(hit) == 1, "mutation target absent"
    return hit[0]
sweep, verify = only('bash "$DC_CTR_KIT/.container/sweep-transcripts.sh"'), only('bash "$DC_CTR_KIT/.container/verify.sh"')
assert sweep < verify, "mutation target absent"
line = lines.pop(sweep)
lines.insert(verify, line)
open(p, 'w').write('\n'.join(lines))
PYX
run "the sweep moves after the verify, where a failing assertion disables it" "AFTER verify.sh"

# ...AND THE `|| true` ON ITS OWN, the entirety of "never fatal". Nothing watched it: the two
# mutations above happened to contain it, so its removal made THEM misreport and left the lost
# property unnamed. run.sh is `set -euo pipefail`, and the sweep returns 1 on paths the script
# itself documents, so one transcript that raced away aborts the start before verify.sh runs and
# before the attach instructions the comment beside it exists to protect.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'bash "$DC_CTR_KIT/.container/sweep-transcripts.sh" || true'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'bash "$DC_CTR_KIT/.container/sweep-transcripts.sh"', 1))
PYX
run "the sweep invocation stops being non-fatal" "does not append \`|| true\` to it"

# THE ENTRYPOINT LINE. One line in the Dockerfile is the whole of "the firewall is raised on every
# start"; deleting it leaves both config harnesses green because run.sh raises it too, and breaks
# only `docker start`, Docker Desktop and a daemon restart — where nothing else looks.
seed; python3 - "$work/t/.container/Dockerfile" <<'PYX'
import sys
p = sys.argv[1]
open(p, 'w').write("".join(l for l in open(p) if not l.startswith("ENTRYPOINT")))
PYX
run "the image stops running entrypoint.sh" "does not set ENTRYPOINT"

# THERE ARE NO REAPER-PATH MUTATIONS HERE, exactly as there are no verdict-path ones below: the
# path is single-sourced as `ENV JKB_REAPER`, so there is no agreement between two spellings to
# break. What DOES get mutated is the handover itself, one file over -- `entrypoint.sh --self-test`
# drives a stub that records having run, so a bare `exec "$@"` fails it -- and ./scripts/check.sh
# runs that.
#
# THE VERDICT-PATH MUTATIONS ARE GONE with the guard they exercised (D52.5). They broke one
# reader's spelling of /run/jkb-egress-verdict and required check-config.sh to notice the drift.
# There is now one spelling -- `VERDICT_PATH` in egress-lib.sh, sourced by the writer and both
# readers -- so there is no drift to induce and nothing to catch. A mutation for a deleted
# assertion reports MISSED for ever, which is a tooling outcome dressed as a guard that did not fire.

seed; jq_dc '{}'
run "the declaration is emptied" "this check just certified nothing"

# The verdict VOCABULARY. The path agreeing is not enough: a reader with no arm for a state the
# writer records reads it as unknown, and unknown is a container that refuses to boot for ever.
seed; python3 - "$work/t/.container/verify.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("    denied)      bad ", "    refused)     bad ", 1))
PYX
run "a reader loses the arm for a verdict state" "has no case arm for the 'denied' verdict"

seed; python3 - "$work/t/.container/egress-lib.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('VERDICT_STATES="allowlisted', 'NOT_THE_STATES="allowlisted', 1))
PYX
run "the library stops declaring its states" "no longer declares VERDICT_STATES"

# THE HOST DAEMON'S OPENING (design r3.2 H5). One mutation per property check-config.sh holds, and
# the port and host ones mutate ONE side so the pair disagrees rather than moving together.
seed; python3 - "$work/t/.container/verify.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
assert "    wide)       $dm_bad " in s, "mutation target absent"
open(p, 'w').write(s.replace("    wide)       $dm_bad ", "    broad)      $dm_bad ", 1))
PYX
run "verify.sh loses the arm for a daemon state" "has no case arm for the 'wide' daemon state"

seed; python3 - "$work/t/.container/egress-lib.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
assert 'DAEMON_STATES="port' in s, "mutation target absent"
open(p, 'w').write(s.replace('DAEMON_STATES="port', 'NOT_DAEMON_STATES="port', 1))
PYX
run "the library stops declaring the daemon states" "no longer declares DAEMON_STATES"

seed; python3 - "$work/t/.container/egress-lib.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
assert "\nDAEMON_PORT=7117\n" in s, "mutation target absent"
open(p, 'w').write(s.replace("\nDAEMON_PORT=7117\n", "\nDAEMON_PORT=7118\n", 1))
PYX
run "the firewall opens a port the daemon does not bind" "but jkb serve binds 7117"

seed; python3 - "$work/t/crates/jkb-daemon/src/lib.rs" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
assert 'DEFAULT_ADDR: &str = "127.0.0.1:' in s, "mutation target absent"
open(p, 'w').write(s.replace('DEFAULT_ADDR: &str = "127.0.0.1:', 'DEFAULT_ADDR: &str = "localhost:', 1))
PYX
run "the daemon's port can no longer be read" "could not read the daemon port"

seed; python3 - "$work/t/.container/init-firewall.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = "iptables -w 5 -A OUTPUT $RULE_DAEMON"
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, "iptables -w 5 -A OUTPUT -p tcp -m set --match-set jkb-daemon dst -j ACCEPT", 1))
PYX
run "the raise spells the daemon rule itself, without its port" "init-firewall.sh spells an OUTPUT rule inline"

seed; jq '.require.sandbox.network.allowedDomains |= map(select(. != "host.docker.internal"))' \
    "$work/t/scripts/auto-mode-posture.json" > "$work/p.json" && mv "$work/p.json" "$work/t/scripts/auto-mode-posture.json"
run "the posture stops naming the host daemon" "does not name host.docker.internal"

seed; python3 - "$work/t/.container/egress-lib.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
assert "\nDAEMON_HOST=host.docker.internal\n" in s, "mutation target absent"
open(p, 'w').write(s.replace("\nDAEMON_HOST=host.docker.internal\n", "\nDAEMON_HOST=\"$(printf host.docker.internal)\"\n", 1))
PYX
run "the daemon's host can no longer be read" "no longer declares DAEMON_HOST"

seed; sub_dc '"--add-host=host.docker.internal:host-gateway",' '"--add-host=host.internal:host-gateway",'
run "the pinned --add-host names a different host" "but egress-lib.sh looks up DAEMON_HOST"

seed; sub_dc '"--add-host=host.docker.internal:host-gateway",' ''
run "the --add-host pin is dropped" "pin no --add-host"

seed; jq_dc '.runArgs |= map(select(. != "--label" and (startswith("devcontainer.metadata=") | not)))'
run "the VS Code metadata label is dropped" "carry no devcontainer.metadata label"

seed; sub_dc '\"onAutoForward\":\"ignore\"' '\"onAutoForward\":\"notify\"'
run "the label lets VS Code forward the daemon port" "does not set portsAttributes"

seed; sub_dc '"JKB_REMOTE": "host.docker.internal:7117",' '"JKB_REMOTE": "host.docker.internal:7118",'
run "remote mode is pointed at a port the firewall does not open" "but the firewall opens host.docker.internal:7117"

seed; sub_dc '"JKB_REMOTE": "host.docker.internal:7117",' ''
run "the container is not in remote mode" "sets no JKB_REMOTE"

seed; sub_dc '"JKB_REMOTE": "host.docker.internal:7117",' '"JKB_REMOTE": "host.docker.internal:7117", "JKB_DB": "/home/vscode/.local/state/jkb/jkb.db",'
run "the container is given a database of its own" "containerEnv sets JKB_DB"

seed; jq_dc '.mounts += ["source=jkb-kb-local,target=/home/vscode/.local/state/jkb,type=volume"]'
run "the retired container-local knowledge base volume is mounted again" "still mounts the container-local knowledge base"

seed; jq_dc '.mounts += ["source=jkb-kb,target=/home/vscode/.local/state/jkb,type=volume"]'
run "the retired knowledge base comes back under another volume name" "still mounts the container-local knowledge base"

seed; jq_dc '.mounts += ["source=jkb-kb-local,target=/home/vscode/.jkb-local,type=volume"]'
run "the retired knowledge base volume comes back at another path" "still mounts the container-local knowledge base"

seed; sub_dc '"JKB_REMOTE": "host.docker.internal:7117",' '"JKB_REMOTE": "host.docker.internal:7117", "JKB_VERIFY_NO_DAEMON": "1",'
run "the real container waives the daemon check" "sets JKB_VERIFY_NO_DAEMON"

seed; jq_dc '.runArgs += ["--env", "JKB_VERIFY_NO_DAEMON=1"]'
run "the launcher waives the daemon check" "runArgs set JKB_VERIFY_NO_DAEMON"

seed; printf '\nENV JKB_VERIFY_NO_DAEMON=1\n' >> "$work/t/.container/Dockerfile"
run "the image waives the daemon check" "Dockerfile sets JKB_VERIFY_NO_DAEMON"

seed; python3 - "$work/t/crates/jkb-daemon/src/lib.rs" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'pub const CLIENT_FILE_ROOT: &str = "repos";'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'pub const CLIENT_FILE_ROOT: &str = "projects";', 1))
PYX
run "the daemon admits file-backed writes under a directory the container does not bind" "does not bind \${localEnv:HOME}/projects"

seed; python3 - "$work/t/crates/jkb-daemon/src/lib.rs" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'pub const CLIENT_FILE_ROOT: &str = "repos";'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'pub const CLIENT_FILE_ROOT: &str = concat!("re", "pos");', 1))
PYX
run "the daemon's client file root can no longer be read" "could not read CLIENT_FILE_ROOT"

seed; python3 - "$work/t/crates/jkb-cli/src/remote.rs" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'pub const REMOTE_VAR: &str = "JKB_REMOTE";'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'pub const REMOTE_VAR: &str = "JKB_REMOTE_URL";', 1))
PYX
run "remote mode is read from a variable the container does not set" "sets no JKB_REMOTE_URL"

seed; python3 - "$work/t/crates/jkb-cli/src/remote.rs" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'pub const REMOTE_VAR: &str = "JKB_REMOTE";'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'pub const REMOTE_VAR: &str = concat!("JKB_", "REMOTE");', 1))
PYX
run "remote mode's variable name can no longer be read" "could not read REMOTE_VAR"

# THE PROBE AND THE RAISE MUST STATE ONE RULE. Re-inlining the spec on the probe side is exactly
# what shipped: `--match-set allowed-new` is the staging set, destroyed before the raise returns, so
# allowlist_state could never answer `yes`. Mutating ONE side is the point — a mutation that
# rewrote both would leave them agreeing and prove nothing about the drift this catches.
seed; python3 - "$work/t/.container/egress-lib.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('iptables -w 5 -C OUTPUT $RULE_ALLOWLIST',
                             'iptables -w 5 -C OUTPUT -m set --match-set allowed-new dst -j ACCEPT', 1))
PYX
run "the probe spells the allowlist rule itself" "egress-lib.sh spells an OUTPUT rule inline"

# ...and the emptiness half. Routing the calls through a wrapper, or a line continuation, yields no
# lines for the loop at all -- so it examines nothing and prints its ok, which is the vacuous pass
# every other derived list in check-config.sh is pinned against.
seed; python3 - "$work/t/.container/egress-lib.sh" <<'PYX'
import re, sys
p = sys.argv[1]; s = open(p).read()
out = re.sub(r'\b(ip6tables|iptables) -w 5 -C OUTPUT', 'IPT -C CHAIN', s)
assert out != s, "mutation target absent"
open(p, 'w').write(out)
PYX
run "the shared-rule check can find no rules to check" "just certified nothing"

# THE APPARMOR PROFILE, mutated four ways -- one per thing it promises. A profile that keeps its
# name while being gutted is the failure mode here: it loads, verify.sh's name check passes, and
# every docker-default restriction is gone.
seed; python3 - "$work/t/.container/apparmor-jkb-dev" <<'PYX'
import re, sys
# Anchored on the RULE, not on the exact line text: the generated profile carries a trailing
# `# PATCHED:` comment, and matching the bare line made this a silent no-op the moment the
# generator started emitting it. The header's prose mentions `mount,` too, but every header line
# begins with `#`, so a two-space-indented anchor cannot reach it.
p = sys.argv[1]; s = open(p).read()
out = re.sub(r'^  mount,.*$', '  deny mount,', s, count=1, flags=re.M)
assert out != s, "mutation target absent"
open(p, 'w').write(out)
PYX
run "the profile denies mount again" 'still denies `mount`'

# pivot_root is the half that is easy to lose, because a profile carrying only `mount,` reads
# exactly like one that works -- that was the state the second CI run measured, and it failed with
# `pivot_root: Permission denied` after the first had failed at `Failed to make / slave`.
seed; python3 - "$work/t/.container/apparmor-jkb-dev" <<'PYX'
import re, sys
p = sys.argv[1]; s = open(p).read()
out = re.sub(r'^  pivot_root,.*\n', '', s, count=1, flags=re.M)
assert out != s, "mutation target absent"
open(p, 'w').write(out)
PYX
run "the profile drops pivot_root, which mount, does not cover" 'does not allow `pivot_root`'

seed; python3 - "$work/t/.container/apparmor-jkb-dev" <<'PYX'
import re, sys
p = sys.argv[1]; s = open(p).read()
out = re.sub(r'^\s*deny @\{PROC\}/sysrq-trigger.*$', '', s, count=1, flags=re.M)
assert out != s, "mutation target absent"
open(p, 'w').write(out)
PYX
run "the profile drops a docker-default restriction" "no longer denies sysrq-trigger"

seed; python3 - "$work/t/.container/apparmor-jkb-dev" <<'PYX'
import re, sys
p = sys.argv[1]; s = open(p).read()
out = re.sub(r'^profile ', '# profile ', s, count=1, flags=re.M)
assert out != s, "mutation target absent"
open(p, 'w').write(out)
PYX
run "the profile declares no name" "declares no profile"

# THE VENDORED ARTIFACTS ARE GENERATED, one mutation per precondition the drift check needs. A
# return to a hand-maintained policy is the state that lost three deny rules, the ABI declaration
# and the runc/crun signal peers without any check noticing -- and the drift check that WOULD have
# noticed runs in CI, so these are what stop the tree drifting out from under it between runs.
seed; python3 - "$work/t/.container/apparmor-jkb-dev" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
out = s.replace("# GENERATED FILE -- DO NOT EDIT.", "# Hand-maintained profile.", 1)
assert out != s, "mutation target absent"
open(p, 'w').write(out)
PYX
run "an artifact stops declaring itself generated" "does not declare itself generated"

seed; python3 - "$work/t/.container/apparmor-jkb-dev" <<'PYX'
import re, sys
p = sys.argv[1]; s = open(p).read()
out = re.sub(r'^# upstream-sha256: [0-9a-f]{64}$', '# upstream-sha256: unknown', s, count=1, flags=re.M)
assert out != s, "mutation target absent"
open(p, 'w').write(out)
PYX
run "an artifact stops recording its upstream digest" "records no upstream-sha256"

seed; python3 - "$work/t/.container/generate-apparmor.sh" <<'PYX'
import re, sys
p = sys.argv[1]; s = open(p).read()
out = re.sub(r'^url="[^"]*"', 'url="https://example.invalid/template.go"', s, count=1, flags=re.M)
assert out != s, "mutation target absent"
open(p, 'w').write(out)
PYX
run "a generator fetches a different upstream than its artifact records" "the URL generate-apparmor.sh fetches"

seed; python3 - "$work/t/.container/generate-apparmor.sh" <<'PYX'
import re, sys
p = sys.argv[1]; s = open(p).read()
out = re.sub(r'^out="\$here/[^"]*"', 'out="/tmp/wherever"', s, count=1, flags=re.M)
assert out != s, "mutation target absent"
open(p, 'w').write(out)
PYX
run "a generator stops declaring where it writes" "cannot be paired without running it"

seed; python3 - "$work/t/.container/generate-apparmor.sh" <<'PYX'
import re, sys
p = sys.argv[1]; s = open(p).read()
out = re.sub(r'^if \[ "\$\{1:-\}" = --print-target \].*\n', '', s, count=1, flags=re.M)
out = re.sub(r'^# check-drift\.sh asks every generator.*\n(?:# .*\n)*', '', out, count=1, flags=re.M)
assert 'print-target' not in out, "the flag is still mentioned, so the text check would still pass"
open(p, 'w').write(out)
PYX
run "a generator drops --print-target, leaving it outside the drift check" "does not support --print-target"

seed; chmod -x "$work/t/.container/generate-apparmor.sh"
run "a generator is not executable" "is not executable"

seed; rm -f "$work/t/.container/apparmor-jkb-dev"
run "a generator's artifact is missing" "which does not exist"

seed; rm -f "$work/t/.container/generate-apparmor.sh"
run "a generator is deleted, orphaning its artifact" "outside the drift check"

seed; bash -c 'rm -f "$1"/generate-*.sh' _ "$work/t/.container"
run "every generator is deleted" "no generate-*.sh found"

# THE DECLARATION'S OWN SHAPE. What the CONTROL carries is no longer asserted here: the control
# asks run.sh for it (D54.1), so there is one derivation and the five mutations that used to break
# the comparison between two of them have gone with the comparison. What is left is the
# declaration itself -- the pairing of `--security-opt` with each value it introduces.
seed; python3 - "$work/t/.container/container.json" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
out = s.replace('"--security-opt",', '"--ignored-opt",')
assert out != s, "mutation target absent"
open(p, 'w').write(out)
PYX
run "container.json declares no --security-opt pair at all" "no --security-opt pairs could be read"

seed; printf '#!/usr/bin/env bash\nexit 0\n' > "$work/t/.container/mutate-verify.sh"
chmod +x "$work/t/.container/mutate-verify.sh"
# THE VERIFY GUARD MUST SEE THE CALL, not a mention of the name (D51.8). The previous mutation
# replaced EVERY occurrence of the token, which rewrote run.sh's three failure messages too — so it
# never established which occurrence the guard reads, and the guard was in fact reading those
# messages. This deletes only the invocation line, which is the one thing that must be seen.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys, re
p = sys.argv[1]; s = open(p).read()
out = [l for l in s.split("\n")
       if not re.match(r'^\s*(in_container|docker exec).*bash "\$DC_CTR_KIT/\.container/verify\.sh"', l)]
assert len(out) < len(s.split("\n")), "no verify invocation line to delete"
open(p, 'w').write("\n".join(out))
PYX
run "run.sh stops invoking verify.sh (messages still name it)" "no longer runs verify.sh"

# THE SECOND ROOT GRANT is subject to the same no-arguments rule as the firewall's, and both halves
# of it get watched failing — the sudoers pin and the script's own refusal.
seed; python3 - "$work/t/.container/Dockerfile" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('/usr/local/bin/egress-status.sh ""', '/usr/local/bin/egress-status.sh', 1))
PYX
run "the egress probe's sudoers grant stops pinning its argument" "no longer pins it to no arguments"

seed; python3 - "$work/t/.container/egress-status.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('takes no arguments', 'ignores extra arguments'))
PYX
run "the egress probe stops refusing arguments" "egress-status.sh no longer refuses arguments"

# THE THIRD ROOT GRANT (D52.9), and what makes it matter: the managed hooks run the binary it pins.
seed; python3 - "$work/t/.container/Dockerfile" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('/usr/local/bin/pin-jkb-hook.sh ""', '/usr/local/bin/pin-jkb-hook.sh', 1))
PYX
run "the hook-pinning sudoers grant stops pinning its argument" "pin-jkb-hook.sh no longer pins it to no arguments"

# The caller half of that grant: `""` is an argument, and sudo refuses one here.
seed; python3 - "$work/t/.container/setup.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('sudo -n /usr/local/bin/pin-jkb-hook.sh\n', 'sudo -n /usr/local/bin/pin-jkb-hook.sh ""\n', 1))
PYX
run "setup passes an argument to the hook-pinning script" "passes an argument to init-firewall.sh or pin-jkb-hook.sh"

seed; python3 - "$work/t/.container/managed-settings.json" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('"/usr/local/lib/jkb-hook/jkb workflow next --stop-hook"', '"jkb workflow next --stop-hook"', 1))
PYX
run "a managed hook runs jkb found on PATH" "runs neither pinned root-owned program"

seed; python3 - "$work/t/.container/managed-settings.json" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('"hooks": {', '"hooks_moved": {', 1))
PYX
run "the managed hooks cannot be read" "no hook commands could be read"

# THE TRANSCRIPT DENY (2026-09-30), which is a HOOK because a glob cannot be both cheap and
# correct here. A `Read(...*.jsonl)` glob is named per-matching-file in the bubblewrap argv (206
# files = 52% of MAX_ARG_STRLEN, and past it every Bash call dies at spawn); the collapsing
# `projects/**` form is cheap and swallows auto-memory, which fails SILENTLY. Both dead ends are
# pinned below alongside the wiring of the hook that replaced them, because with the globs gone
# the hook is the only thing left holding the boundary.
seed; python3 - "$work/t/.container/managed-settings.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["hooks"]["PreToolUse"] = [h for h in d["hooks"]["PreToolUse"] if "deny-transcripts.sh" not in json.dumps(h)]
json.dump(d, open(p, "w"), indent=2)
PYX
run "the transcript hook is unwired" "no longer runs /usr/local/bin/deny-transcripts.sh"

seed; python3 - "$work/t/.container/Dockerfile" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("COPY --chown=root:root deny-transcripts.sh /usr/local/bin/deny-transcripts.sh\n", "", 1))
PYX
run "the transcript hook is not installed" "does not install deny-transcripts.sh root-owned"

seed; python3 - "$work/t/.container/Dockerfile" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("COPY --chown=root:root deny-transcripts.sh", "COPY deny-transcripts.sh", 1))
PYX
run "the transcript hook is installed agent-writable" "does not install deny-transcripts.sh root-owned"

seed; python3 - "$work/t/.container/managed-settings.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
for h in d["hooks"]["PreToolUse"]:
    if "deny-transcripts.sh" in json.dumps(h): h["matcher"] = "Read"
json.dump(d, open(p, "w"), indent=2)
PYX
run "the hook misses the tools that can also read" "transcript hook's matcher is [Read]"

seed; python3 - "$work/t/.container/managed-settings.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["permissions"]["deny"].append("Read(~/.claude/projects/**/*.jsonl)")
json.dump(d, open(p, "w"), indent=2)
PYX
run "a per-file deny glob comes back" "is enumerated per match"

seed; python3 - "$work/t/.container/managed-settings.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["permissions"]["deny"].append("Read(~/.claude/projects/**)")
json.dump(d, open(p, "w"), indent=2)
PYX
run "the collapsing shape swallows auto-memory" "covers ~/.claude/projects/<slug>/memory"

# THE BARE-DIRECTORY BELT, which is the shape that actually shipped and broke memory in a real
# container -- and which the memory arm passed until it matched `$pat/*` as well as `$pat`.
seed; python3 - "$work/t/.container/managed-settings.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["permissions"]["deny"].append("Read(~/.claude/projects)")
json.dump(d, open(p, "w"), indent=2)
PYX
run "a bare directory rule covers auto-memory's subtree" "covers ~/.claude/projects/<slug>/memory"

# THE REVIEW ROUND'S ROWS (2026-10-01): each one a guard the first cut had and could not fire.
# `Edit` is a substring of `NotebookEdit`, so a substring matcher check passed a matcher without Edit.
seed; python3 - "$work/t/.container/managed-settings.json" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('"matcher": ".*"', '"matcher": "Read|Edit|Write|Grep|Glob"', 1))
PYX
run "the hook matcher is narrowed back to an allowlist" "transcript hook's matcher is [Read|Edit|Write|Grep|Glob]"

# Claude Code's absolute spelling of the bare-directory rule that drops MEMORY.md.
seed; python3 - "$work/t/.container/managed-settings.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["permissions"]["deny"].append("Read(//home/vscode/.claude/projects)")
json.dump(d, open(p, "w"), indent=2)
PYX
run "a //absolute bare-directory rule covers auto-memory" "covers ~/.claude/projects/<slug>/memory"

# A mid-path ** with a literal tail is enumerated per match; only the seven named rules may be.
seed; python3 - "$work/t/.container/managed-settings.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["permissions"]["deny"].append("Read(~/repos/**/.env)")
json.dump(d, open(p, "w"), indent=2)
PYX
run "a new mid-path ** rule outside the named seven" "is enumerated per match"

# The hook file itself gone from the tree the Dockerfile copies.
seed; rm -f "$work/t/.container/deny-transcripts.sh"
run "the transcript hook file is missing" "there is no .container/deny-transcripts.sh"

# The shared reader renamed in the sweep: both loaders must fail loudly, not pass on nothing.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("posture_rule_covers() {", "posture_rule_covers_renamed() {", 1))
PYX
run "the shared deny-rule reader cannot be loaded" "could not be loaded from sweep-transcripts.sh"

# REVIEW ROUND 2. The .claude-state spelling of the memory probe had never been made to fail: every
# row used ~/.claude/projects, and deleting the second probe left all of them CAUGHT.
seed; python3 - "$work/t/.container/managed-settings.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["permissions"]["deny"].append("Read(~/.claude-state/projects/**)")
json.dump(d, open(p, "w"), indent=2)
PYX
run "a subtree rule over the .claude-state spelling covers auto-memory" "covers ~/.claude/projects/<slug>/memory"

# The bare-directory rule with a trailing slash: canonicalised, it is the same rule.
seed; python3 - "$work/t/.container/managed-settings.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["permissions"]["deny"].append("Read(~/.claude/projects/)")
json.dump(d, open(p, "w"), indent=2)
PYX
run "a bare-directory rule with a trailing slash covers auto-memory" "covers ~/.claude/projects/<slug>/memory"

# A commented-out COPY ships an image without the hook; a raw grep passed it.
seed; python3 - "$work/t/.container/Dockerfile" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("COPY --chown=root:root deny-transcripts.sh", "# COPY --chown=root:root deny-transcripts.sh", 1))
PYX
run "the hook's COPY is commented out" "does not install deny-transcripts.sh root-owned"

# REVIEW ROUND 3. With no deny rule naming the tree, an allow entry reaching it opens it to Bash.
seed; python3 - "$work/t/scripts/auto-mode-posture.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["require"]["sandbox"]["filesystem"]["allowRead"].append("~/.claude")
json.dump(d, open(p, "w"), indent=2)
PYX
run "a sandbox allowRead entry widened to ~/.claude" "reaches the transcript tree"

# The hook's roots lose CLAUDE_CONFIG_DIR: the sweep still honours it, the hook does not.
seed; python3 - "$work/t/.container/deny-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('${CLAUDE_CONFIG_DIR:+"$CLAUDE_CONFIG_DIR/projects"}', '', 1))
PYX
run "the hook forgets CLAUDE_CONFIG_DIR" "disagree about where the transcript tree is"

# A helper the reader uses goes missing: the load guard must fail, not the reader pass on nothing.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("posture_hook_matcher() {", "posture_hook_matcher_renamed() {", 1))
PYX
run "the shared matcher definition is renamed" "could not be loaded from sweep-transcripts.sh"

# REVIEW ROUND 4. The unsandboxed hook must not trust PATH: an env shebang, or no PATH reset.
seed; python3 - "$work/t/.container/deny-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("#!/bin/bash -p\n", "#!/usr/bin/env bash\n", 1))
PYX
run "the hook's shebang goes back to env" "not an absolute bash"

seed; python3 - "$work/t/.container/deny-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("PATH=/usr/bin:/bin\nexport PATH\n", "", 1))
PYX
run "the hook stops fixing PATH" "does not fix PATH before it runs anything"

# REVIEW ROUND 7. Losing `-p` alone keeps the shebang absolute and lets BASH_ENV in.
seed; python3 - "$work/t/.container/deny-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace("#!/bin/bash -p\n", "#!/bin/bash\n", 1))
PYX
run "the hook's shebang loses -p" "not an absolute bash in privileged mode"

# The sweep's PATH pin, and the absolute bash at each place an unsandboxed script is started.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('[ "${1:-}" = --self-test ] || { PATH=/usr/bin:/bin; export PATH; }\n', "", 1))
PYX
run "the sweep stops pinning PATH" "the sweep's first command does not pin PATH"

# REVIEW ROUND 8. The pin back in the real-run arm: still present, and too late for date and stat.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
s = s.replace('[ "${1:-}" = --self-test ] || { PATH=/usr/bin:/bin; export PATH; }\n', "", 1)
open(p, 'w').write(s.replace('    ""|--dry-run)\n', '    ""|--dry-run)\n        PATH=/usr/bin:/bin\n        export PATH\n', 1))
PYX
run "the sweep's PATH pin moves back into the real-run arm" "the sweep's first command does not pin PATH"

seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('/bin/bash "$DC_CTR_KIT/.container/sweep-transcripts.sh"', 'bash "$DC_CTR_KIT/.container/sweep-transcripts.sh"', 1))
PYX
run "run.sh starts the sweep with a bare bash" "run.sh does not start sweep-transcripts.sh with /bin/bash"

# REVIEW ROUND 8. Every exec in run.sh, not only the sweep's and verify's.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'in_container -e PATH=/usr/bin:/bin -w "$ctr_repo" "$NAME" /usr/local/lib/jkb-hook/jkb task reap || true'
assert old in s
open(p, 'w').write(s.replace(old, 'in_container -w "$ctr_repo" "$NAME" bash -lc \'jkb task reap || true\' || true', 1))
PYX
run "the reap goes back to a login bash found on PATH" "by PATH lookup"

seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'in_container -e PATH=/usr/bin:/bin -w "$ctr_repo" "$NAME" /bin/bash -c'
assert old in s
open(p, 'w').write(s.replace(old, 'in_container -w "$ctr_repo" "$NAME" /bin/bash -c', 1))
PYX
run "the login step stops pinning PATH for what its shell runs" "without -e PATH=/usr/bin:/bin"

seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'in_container "$NAME" /usr/bin/sudo -n'
assert old in s
open(p, 'w').write(s.replace(old, 'in_container "${NAME}" sudo -n', 1))
PYX
run "an exec spelled so the scan cannot see it" "has a container exec the scan cannot read"

# The two exec branches round 7 added and left unmutated.
seed; python3 - "$work/t/crates/jkb-cli/src/transcripts.rs" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '"exec", "-i", "-e", &keep, name, "/bin/bash", "-s"'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '"exec", "-i", "-e", &keep, name, "bash", "-s"', 1))
PYX
run "the reaper's docker exec finds bash on PATH" "the reaper's docker exec does not name /bin/bash"

seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('/bin/bash "$DC_CTR_KIT/.container/verify.sh"', 'bash "$DC_CTR_KIT/.container/verify.sh"', 1))
PYX
run "run.sh starts verify.sh with a bare bash" "run.sh does not start verify.sh with /bin/bash"

# REVIEW ROUNDS 8-9. ~/.jq: one HOME=/dev/null wrapper per unsandboxed script, never bypassed.
seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'jq() { HOME=/dev/null command jq "$@"; }\n'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, "", 1))
PYX
run "the sweep loses its jq wrapper" "does not define the HOME=/dev/null jq wrapper"

seed; python3 - "$work/t/.container/verify.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'jq() { HOME=/dev/null command jq "$@"; }\n'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, "", 1))
PYX
run "verify.sh loses its jq wrapper" "does not define the HOME=/dev/null jq wrapper"

seed; python3 - "$work/t/.container/verify.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'mem_match="$(HOME=/dev/null jq -r'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'mem_match="$(command jq -r', 1))
PYX
run "verify.sh calls jq around its wrapper" "calls jq around its wrapper"

seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = "perm) HOME=/dev/null jq -r"
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, "perm) /usr/bin/jq -r", 1))
PYX
run "the sweep names jq by absolute path" "calls jq around its wrapper"

# REVIEW ROUND 9. An ADDED exec the scan cannot read is a failure, not a skip.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'say "login state"\n'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, old + 'in_container --user root "$CONTAINER" bash -c true\n', 1))
PYX
run "an exec added with a container the scan cannot read" "has a container exec the scan cannot read"

# The archive is a root of the hook's.
seed; python3 - "$work/t/.container/deny-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '/.claude-state/transcript-archive"'
assert s.count(old) == 2, "mutation target absent"
open(p, 'w').write(s.replace(old, '/.claude-state/elsewhere"'))
PYX
run "the hook forgets the transcript archive" "hook:.claude-state/transcript-archive"

# THE KIT (review round 8's self-review). What runs unsandboxed comes from the kit, never the checkout.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '/bin/bash "$DC_CTR_KIT/.container/setup.sh"'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '/bin/bash .container/setup.sh', 1))
PYX
run "run.sh runs setup.sh from the checkout again" "runs a script from the checkout's .container/"

seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = """/bin/bash -c '. "$1" && dc_persist_login' _ "$DC_CTR_KIT/.container/lib.sh" \\\n    || say"""
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, """/bin/bash -c '. .container/lib.sh && dc_persist_login' \\\n    || say""", 1))
PYX
run "the login step sources the checkout's lib.sh again" "runs a script from the checkout's .container/"

seed; python3 - "$work/t/.container/lib.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '"$docker" exec -i -u root -e PATH=/usr/bin:/bin "$name" /bin/sh -c \''
assert s.count(old) == 2, "mutation target absent"
open(p, 'w').write(s.replace(old, '"$docker" exec -i -u root "$name" sh -c \'', 1))
PYX
run "the hook mirror's root step finds sh on PATH again" "starts [sh] by PATH lookup"

seed; python3 - "$work/t/.container/lib.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '"$docker" exec -e PATH=/usr/bin:/bin "$name" /bin/mkdir -p'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '"$docker" exec "$name" /bin/mkdir -p', 1))
PYX
run "the hook mirror's mkdir stops pinning PATH" "without -e PATH=/usr/bin:/bin"

seed; python3 - "$work/t/.container/verify.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'if "$kit_dc/scripts/auto-mode.sh" check'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'if "$mem_repo/scripts/auto-mode.sh" check', 1))
PYX
run "verify.sh runs the checkout's auto-mode.sh again" "reaches the checkout's scripts"

seed; python3 - "$work/t/.container/lib.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = ' scripts/auto-mode.sh scripts/auto-mode-posture.json'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, ' scripts/auto-mode-posture.json', 1))
PYX
run "the kit stops carrying a script verify.sh runs" "which the kit does not carry"

seed; python3 - "$work/t/.container/lib.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = "    printf '%s\\n' .container scripts/lib.sh"
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, "    : printf '%s\\n' .container scripts/lib.sh", 1))
PYX
run "the kit list prints nothing" "dc_kit_paths printed nothing"

# REVIEW ROUND 10. No agent can write the kit, and the fingerprint strips the root it was assembled from.
seed; python3 - "$work/t/scripts/auto-mode-posture.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["require"]["sandbox"]["filesystem"]["allowWrite"].append("~/.local/share")
json.dump(d, open(p, "w"), indent=2)
PYX
run "the posture lets sandboxed Bash write over the kit" "covers ~/.local/share/jkb-container-kit"

seed; python3 - "$work/t/scripts/auto-mode-posture.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["require"]["permissions"]["deny"].remove("Edit(~/.local/share/jkb-container-kit/**)")
json.dump(d, open(p, "w"), indent=2)
PYX
run "the posture stops denying Edit on the kit" "so the in-process file tools can write it"

seed; python3 - "$work/t/.container/lib.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'DC_KIT_HOME="$HOME/.local/share/jkb-container-kit"'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'DC_KIT_HOME="$HOME/.jkb/container-kit"', 1))
PYX
run "the kit moves back under ~/.jkb" "covers ~/.jkb/container-kit"

seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'want_hash="$(fingerprint "$args_root" '
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'want_hash="$(fingerprint "$repo" ', 1))
PYX
run "the fingerprint strips a different root from the one the arguments came from" "do not both use"

# REVIEW ROUND 11. run.sh runs as you on the host: an absolute shebang and the PATH filter.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
assert s.startswith("#!/bin/bash -p\n"), "mutation target absent"
open(p, 'w').write("#!/usr/bin/env bash\n" + s[len("#!/bin/bash -p\n"):])
PYX
run "run.sh's shebang goes back to env" "shebang is not #!/bin/bash -p"

# REVIEW ROUND 21. ...and privileged mode, or the launching terminal's BASH_ENV runs first.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
assert s.startswith("#!/bin/bash -p\n"), "mutation target absent"
open(p, 'w').write("#!/bin/bash\n" + s[len("#!/bin/bash -p\n"):])
PYX
run "run.sh's shebang drops -p" "shebang is not #!/bin/bash -p"

# REVIEW ROUND 22. ...and its PATH filter takes no keep list from the environment.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
o = 'case "$jkb_keep" in *":$jkb_d:"*)'
assert o in s, "mutation target absent"
open(p, 'w').write(s.replace(o, 'case "$jkb_keep:${JKB_RUN_PATH_KEEP:-}:" in *":$jkb_d:"*)', 1))
PYX
run "run.sh's PATH filter reads a keep list from the environment again" "reads JKB_RUN_PATH_KEEP from the environment"

seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
o = 'jkb_p="$(cd -P -- "$jkb_d" 2>/dev/null && pwd -P)"'
assert o in s, "mutation target absent"
open(p, 'w').write(s.replace(o, 'jkb_p="$jkb_d"', 1))
PYX
run "run.sh's PATH filter compares spellings, not physical paths" "compare physical paths"

seed; python3 - "$work/t/.container/lib.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'DC_KIT_HOME="$HOME/.local/share/jkb-container-kit"'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'DC_KIT_HOME="${JKB_CONTAINER_KIT_HOME:-$HOME/.local/share/jkb-container-kit}"', 1))
PYX
run "lib.sh lets the environment move the kit again" "lets JKB_CONTAINER_KIT_HOME move the kit"

# REVIEW ROUND 23. Every piece of the physical comparison, the env re-exec, and where path-keep lives.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
o = 'jkb_p="$(printf \'%s\' "$jkb_p" | /usr/bin/tr \'[:upper:]\' \'[:lower:]\')"; '
assert o in s, "mutation target absent"
open(p, 'w').write(s.replace(o, '', 1))
PYX
run "run.sh's PATH filter stops case-folding entries" "so a home spelled with other case"

seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
o = 'jkb_r="$(cd -P -- "$jkb_r" 2>/dev/null && pwd -P || printf \'%s\' "$jkb_r")"; '
assert o in s, "mutation target absent"
open(p, 'w').write(s.replace(o, '', 1))
PYX
run "run.sh's PATH filter stops resolving its roots physically" "so a home spelled with other case"

seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
o = '[ "$jkb_env_extra" -eq 0 ] || exec /usr/bin/env -i "${jkb_env[@]}" /bin/bash -p "$0" "$@"; '
assert o in s, "mutation target absent"
open(p, 'w').write(s.replace(o, '', 1))
PYX
run "run.sh stops re-executing under an allowlisted environment" "does not re-execute under an allowlisted environment"

seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
o = '|DOCKER_HOST|'
assert o in s, "mutation target absent"
open(p, 'w').write(s.replace(o, '|DOCKER_CONFIG|DOCKER_HOST|', 1))
PYX
run "run.sh's environment allowlist lets DOCKER_CONFIG through" "names a variable that steers what its children run"

seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
o = 'jkb_keepf="$HOME/.local/share/jkb-container-kit/path-keep"'
assert o in s, "mutation target absent"
open(p, 'w').write(s.replace(o, 'jkb_keepf="$HOME/.config/jkb/path-keep"', 1))
PYX
run "run.sh reads its PATH keep list from outside the kit home" "not from the kit home"

seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
lines = s.split("\n")
hit = [i for i, l in enumerate(lines) if l.startswith('if [ "${1:-}" != --self-test ]; then [ -n "${HOME:-}" ]')]
assert len(hit) == 1, "mutation target absent"
del lines[hit[0]]
open(p, 'w').write("\n".join(lines))
PYX
run "run.sh stops dropping agent-writable PATH entries" "does not drop agent-writable PATH entries"

# The transcript archive is one of the roots the allow-list guard protects.
seed; python3 - "$work/t/scripts/auto-mode-posture.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["require"]["sandbox"]["filesystem"]["allowRead"].append("~/.claude-state/transcript-archive")
json.dump(d, open(p, "w"), indent=2)
PYX
run "an allowRead entry over the transcript archive" "reaches the transcript tree"

seed; python3 - "$work/t/.container/sweep-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '"~/.claude-state/transcript-archive"\n}'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '"~/.claude-state/transcript-archive" "~/.claude-state/elsewhere"\n}', 1))
PYX
run "the shared root list names a root the hook does not" "hook:.claude-state/elsewhere"

# A Claude settings `env` must not replace what the container sets (2026-10-02: a Mac PATH did).
seed; python3 - "$work/t/.claude/settings.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d.setdefault("env", {})["PATH"] = "/Users/me/.cargo/bin:/usr/bin:/bin"
json.dump(d, open(p, "w"), indent=2)
PYX
run "the repo's settings set env.PATH" "sets env that the container itself sets"

seed; python3 - "$work/t/.container/Dockerfile" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = "ENV PATH=/home/vscode/.local/bin:/home/vscode/.cargo/bin:$PATH\n"
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, "", 1))
PYX
run "the image's ENV PATH can no longer be found" "environment names the container sets could not be derived"

# REVIEW ROUND 12. run.sh's jq wrapper, and the macOS temp root in its PATH filter.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'jq() { HOME=/dev/null command jq "$@"; }\n'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, "", 1))
PYX
run "run.sh loses its jq wrapper" "run.sh does not define the HOME=/dev/null jq wrapper"

seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '|/var/folders|/var/folders/*)'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, ')', 1))
PYX
run "run.sh's PATH filter stops dropping the macOS temp root" "does not drop agent-writable PATH entries"

# REVIEW ROUND 14. The Homebrew prefixes run.sh keeps, and allow entries read as the sandbox reads them.
seed; python3 - "$work/t/scripts/auto-mode-posture.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["require"]["permissions"]["deny"].remove("Edit(//usr/local/**)")
json.dump(d, open(p, "w"), indent=2)
PYX
run "the posture stops denying Edit on /usr/local" "no Edit(//usr/local/**) deny"

seed; python3 - "$work/t/scripts/auto-mode-posture.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["require"]["sandbox"]["filesystem"]["allowWrite"].append("~/.local/**")
json.dump(d, open(p, "w"), indent=2)
PYX
run "a glob allowWrite entry over the kit" "covers ~/.local/share/jkb-container-kit"

seed; python3 - "$work/t/scripts/auto-mode-posture.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["require"]["sandbox"]["filesystem"]["allowWrite"].append("/Users/someone/.local/share")
json.dump(d, open(p, "w"), indent=2)
PYX
run "an absolute allowWrite entry over the kit" "covers ~/.local/share/jkb-container-kit"

# REVIEW ROUND 15. Every place run.sh's trust rests on, and the kit checked before it is mirrored.
seed; python3 - "$work/t/scripts/auto-mode-posture.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["require"]["sandbox"]["filesystem"]["allowWrite"].append("/opt/homebrew")
json.dump(d, open(p, "w"), indent=2)
PYX
run "an allowWrite entry over /opt/homebrew" "covers /opt/homebrew"

seed; python3 - "$work/t/scripts/auto-mode-posture.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["require"]["permissions"]["deny"].remove("Edit(//opt/homebrew/**)")
json.dump(d, open(p, "w"), indent=2)
PYX
run "the posture stops denying Edit on /opt/homebrew" "no Edit(//opt/homebrew/**) deny"

seed; python3 - "$work/t/scripts/auto-mode-posture.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["require"]["permissions"]["deny"].remove("Edit(~/.cargo/env)")
json.dump(d, open(p, "w"), indent=2)
PYX
run "the posture stops denying Edit on ~/.cargo/env" "no Edit(~/.cargo/env) deny"

seed; python3 - "$work/t/scripts/auto-mode-posture.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["require"]["permissions"]["deny"].remove("Edit(~/.zshenv)")
json.dump(d, open(p, "w"), indent=2)
PYX
run "the posture stops denying Edit on ~/.zshenv" "no Edit(~/.zshenv) deny"

seed; python3 - "$work/t/scripts/auto-mode-posture.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["require"]["permissions"]["deny"].remove("Edit(~/.zlogout)")
json.dump(d, open(p, "w"), indent=2)
PYX
run "the posture stops denying Edit on ~/.zlogout" "no Edit(~/.zlogout) deny"

seed; python3 - "$work/t/scripts/auto-mode-posture.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["require"]["permissions"]["deny"].remove("Edit(~/.config/git/**)")
json.dump(d, open(p, "w"), indent=2)
PYX
run "the posture stops denying Edit on git's global config directory" "no Edit(~/.config/git/**) deny"

seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'kit_odd="$(dc_unsafe_entries "$kit_src")"\n'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'kit_odd=""\n', 1))
PYX
run "run.sh stops checking the kit before mirroring it" "does not check the kit with dc_unsafe_entries"

# REVIEW ROUND 19. The served checkout through dc_repo_root alone.
seed; python3 - "$work/t/.container/setup.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'repo="$(dc_repo_root "$kit")"'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, 'repo="${JKB_REPO_ROOT:-$kit}"', 1))
PYX
run "setup.sh derives the checkout itself again" "derives the checkout itself"

# A per-file transcript glob in sandbox.filesystem.denyRead reaches the same argv.
seed; python3 - "$work/t/.container/managed-settings.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d.setdefault("sandbox", {}).setdefault("filesystem", {}).setdefault("denyRead", []).append("~/.claude/projects/**/*.jsonl")
json.dump(d, open(p, "w"), indent=2)
PYX
run "a per-file glob in sandbox.filesystem.denyRead" "is enumerated per match"

# An ABSOLUTE allow entry under a home reaches the tree as surely as a ~ one.
seed; python3 - "$work/t/scripts/auto-mode-posture.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["require"]["sandbox"]["filesystem"]["allowRead"].append("/home/vscode/.claude")
json.dump(d, open(p, "w"), indent=2)
PYX
run "an absolute allowRead entry over ~/.claude" "reaches the transcript tree"

# The roots extraction reads nothing: the agreement check must say so, not pass.
seed; python3 - "$work/t/.container/deny-transcripts.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('\nroot_list=()\n', '\nroot_list_moved=()\n', 1))
PYX
run "the hook's roots can no longer be found" "transcript roots could not be found"

# REVIEW ROUND 5. Allow arrays merge across layers: an entry in the MANAGED file opens the tree too.
seed; python3 - "$work/t/.container/managed-settings.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d.setdefault("sandbox", {}).setdefault("filesystem", {}).setdefault("allowRead", []).append("~/.claude")
json.dump(d, open(p, "w"), indent=2)
PYX
run "a managed allowRead entry over ~/.claude" "reaches the transcript tree"

# denyWrite is enumerated per match, as denyRead is.
seed; python3 - "$work/t/.container/managed-settings.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d.setdefault("sandbox", {}).setdefault("filesystem", {}).setdefault("denyWrite", []).append("~/.claude/projects/**/*.jsonl")
json.dump(d, open(p, "w"), indent=2)
PYX
run "a per-file glob in sandbox.filesystem.denyWrite" "is enumerated per match"

# REVIEW ROUND 6. An allow entry spelled as a glob is its literal base: `~/.claude/**` opens the tree.
seed; python3 - "$work/t/scripts/auto-mode-posture.json" <<'PYX'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
d["require"]["sandbox"]["filesystem"]["allowRead"].append("~/.claude/**")
json.dump(d, open(p, "w"), indent=2)
PYX
run "a sandbox allowRead glob over ~/.claude" "reaches the transcript tree"

# MCP tools run unsandboxed and take paths; a matcher without them leaves every one unguarded.
seed; python3 - "$work/t/.container/managed-settings.json" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('"matcher": ".*"', '"matcher": "mcp__.*"', 1))
PYX
run "the hook matcher covers MCP tools only" "transcript hook's matcher is [mcp__.*]"

seed; python3 - "$work/t/.container/managed-settings.json" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('"permissions": {', '"permissions_moved": {', 1))
PYX
run "the deny rules cannot be read" "examined nothing"

# The two self-test lists (D51.9): drop one from each side in turn.
seed; python3 - "$work/t/scripts/check.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('"$(dirname "$0")/../.container/egress-lib.sh" --self-test\n', '', 1))
PYX
run "a self-test is dropped from the gate" "disagree about which container self-tests"

seed; python3 - "$work/t/.github/workflows/ci.yml" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('          ./.container/egress-status.sh --self-test\n', '', 1))
PYX
run "a self-test is dropped from CI" "disagree about which container self-tests"

# ...and the half that stops both lists agreeing about running nothing.
seed; python3 - "$work/t/scripts/check.sh" "$work/t/.github/workflows/ci.yml" <<'PYX'
import sys
for p in sys.argv[1:]:
    s = open(p).read()
    for tok in ('"$(dirname "$0")/../.container/egress-status.sh" --self-test\n',
                '          ./.container/egress-status.sh --self-test\n'):
        s = s.replace(tok, '')
    open(p, 'w').write(s)
PYX
run "both lists drop the same self-test" "self-test that no gate runs"

# The second sudoers grant is read-only by argument, so the allowed SET is what verify.sh pins.
# ANCHORED ON THE LABEL, because the EXPECTATION is the thing under test and is meant to move. This
# named the expectation in full and went NO-OP the moment D52's hook pin grew verify.sh's list and
# mutate-verify.sh shortened its expect to the stable prefix -- the guard against stale expectations,
# itself stale, failing the gate while pointing at a file that was fine.
seed; python3 - "$work/t/.container/mutate-verify.sh" <<'PYX'
import re, sys
p = sys.argv[1]; s = open(p).read()
out = re.sub(r'(run "blanket passwordless root is restored" ")[^"]*(")',
             r'\g<1>a sudoers sentence verify.sh never prints\g<2>', s, count=1)
assert out != s, "mutation target absent"
open(p, 'w').write(out)
PYX
run "a harness expectation drifts from verify.sh" "expects text verify.sh never prints"

# ...and what makes the two ERR-trap rules safe to exempt the self-test from: that it exits.
seed; python3 - "$work/t/.container/init-firewall.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
# Drop the final `exit 0`, so the block would fall through into a real raise.
open(p, 'w').write(s.replace("    printf '\\033[32mfirewall self-test passed\\033[0m\\n'\n    exit 0\n",
                             "    printf '\\033[32mfirewall self-test passed\\033[0m\\n'\n", 1))
PYX
run "the self-test can fall through into a real raise" "does not end in an exit"

seed; python3 - "$work/t/.container/init-firewall.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
i = s.index('if [ "$#" -eq 1 ]'); j = s.index('\nfi\n', i)
open(p, 'w').write(s[:i] + s[j+4:])
PYX
run "the writer's self-test is deleted outright" "the writer's verdict logic is unexercised"

# A mutation harness whose expectation no longer matches its subject reports MISSED for ever.
# mutate-verify.sh needs Docker so this gate cannot run it; the strings are checkable statically.
seed; python3 - "$work/t/.container/mutate-verify.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('"is not inside any host BIND"', '"a string verify.sh never prints"', 1))
PYX
run "a mutate-verify expectation goes stale" "expects text verify.sh never prints"

# ...and the check that the extractor SEES every run() call. Re-anchoring the pattern at `^run`
# drops any indented mutation -- which is how it silently went from 14 expectations to 13 the
# moment one was wrapped in an `if`. An emptiness pin cannot catch a partial loss; this can.
seed; python3 - "$work/t/.container/check-config.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace(
    "'s/^[[:space:]]*run \"[^\"]*\" \"\\([^\"]*\\)\".*/\\1/p'",
    "'s/^run \"[^\"]*\" \"\\([^\"]*\\)\".*/\\1/p'", 1))
PYX
run "the expectation extractor stops seeing indented mutations" "run() calls — the rest are invisible"

# A refusal in the firewall that exits before installing any rule is not a refusal: iptables rules
# do not survive a restart, so the container comes up with unfiltered egress and a message.
seed; python3 - "$work/t/.container/init-firewall.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
s = s.replace('jq empty "$POSTURE" 2>/dev/null || fail_closed "$POSTURE is not valid JSON',
              'jq empty "$POSTURE" 2>/dev/null || exit 1 # "$POSTURE is not valid JSON', 1)
open(p, 'w').write(s)
PYX
run "a firewall refusal exits without installing a deny-all" "exits without installing a deny-all"

# The shape the Docker harness cannot reach: a bare command-substitution assignment, which under
# `set -eE` and the ERR trap takes the whole raise to deny-all the first time any one of the
# fifteen posture domains fails to resolve.
seed; python3 - "$work/t/.container/init-firewall.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace(
    's/sort -u)" || ips=""', 's/sort -u)"', 1) if False else s.replace(
    'sort -u)" || ips=""', 'sort -u)"', 1))
PYX
run "a firewall command substitution loses its fallback" "aborts the whole raise into fail_closed"

seed; jq_dc '.mounts += ["source=/var/run/docker.sock,target=/var/run/docker.sock,type=bind"]'
run "the docker socket is mounted in" "docker socket is root on the host"

seed; jq_dc '.mounts += ["source=${localEnv:HOME}/.claude,target=/home/vscode/.claude/settings.json,type=bind"]'
run "the posture file itself is mounted in" "inside the posture's own directory"

seed; jq_dc '.mounts += ["source=${localEnv:HOME},target=/home/vscode/elsewhere,type=bind"]'
run "the whole host HOME is bound under a benign name" "not on the reviewed bind allowlist"

seed; jq 'del(.publisher)' "$work/t/ui/vscode/package.json" > "$work/t/ui/vscode/pkg.new" \
        && mv "$work/t/ui/vscode/pkg.new" "$work/t/ui/vscode/package.json"
run "the locally-built extension loses its publisher" "no longer yields a publisher.name"

seed; sub_dc '"CARGO_TARGET_DIR": "/home/vscode/.cargo/target"' '"CARGO_TARGET_DIR": "/home/vscode/.cargo-target"'
run "CARGO_TARGET_DIR moves outside the allowlisted root" "is under no allowWrite root"

seed; sub_dc 'source=jkb-cargo-target,target=/home/vscode/.cargo/target,type=volume' \
             'source=${localWorkspaceFolder}/ct,target=/home/vscode/.cargo/target,type=bind'
run "the cargo target volume becomes a bind mount" "not declared type=volume"

seed; python3 - "$work/t/scripts/auto-mode-posture.json" <<'PY'
import json, sys
p = sys.argv[1]; d = json.load(open(p))
del d["require"]["sandbox"]["network"]["allowedDomains"]
json.dump(d, open(p, "w"))
PY
run "the posture loses its allowlist key" "the firewall would deny everything"

seed; python3 - "$work/t/.container/Dockerfile" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('init-firewall.sh ""', 'init-firewall.sh', 1))
PY
run "the sudoers grant stops pinning its argument" "does not pin the argument list"

seed; python3 - "$work/t/.container/Dockerfile" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('&& rm -f /etc/sudoers.d/vscode \\\n', '', 1))
PY
run "the blanket NOPASSWD:ALL removal is dropped" "the agent can sudo anything"

seed; python3 - "$work/t/.container/init-firewall.sh" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('takes no arguments', 'ignores arguments', 1))
PY
run "the firewall accepts a posture path again" "still accepts a posture path"

# Both spellings of the regression the caller guard exists for. The shell-quoted one is the case
# its first version could not match, so it is kept as its own mutation rather than folded in.
seed; python3 - "$work/t/.container/setup.sh" <<'PY'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('sudo -n /usr/local/bin/init-firewall.sh',
                             'sudo -n /usr/local/bin/init-firewall.sh "$repo/scripts/auto-mode-posture.json"', 1))
PY
run "a caller passes the workspace posture (shell-quoted)" "passes an argument to init-firewall.sh"

seed; python3 - "$work/t/.container/run.sh" <<'PYB'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('/usr/local/bin/init-firewall.sh',
                             '/usr/local/bin/init-firewall.sh /home/vscode/repos/jkb/scripts/auto-mode-posture.json', 1))
PYB
run "a caller passes the workspace posture (bare path)" "passes an argument to init-firewall.sh"

seed; printf '{oops' > "$(DC)"
run "container.json stops parsing" "does not parse"

seed; printf '{oops' > "$work/t/.container/seccomp-bwrap.json"
run "the seccomp profile stops parsing" "does not parse"

seed; sub_dc '"CARGO_TARGET_DIR"' '"CARGO_TARGET_DIR_TYPO"'
run "containerEnv.CARGO_TARGET_DIR disappears" "sets no containerEnv.CARGO_TARGET_DIR"

seed; jq_dc '.mounts |= map(select(test("jkb-cargo-target") | not))'
run "nothing is mounted at CARGO_TARGET_DIR" "nothing is mounted at CARGO_TARGET_DIR"

seed; python3 - "$work/t/.container/Dockerfile" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('mkdir -p /home/vscode/.cargo/target', 'mkdir -p /home/vscode/.unused', 1))
PYX
run "the Dockerfile stops pre-creating CARGO_TARGET_DIR" "does not pre-create"

seed; python3 - "$work/t/.container/Dockerfile" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '             /home/vscode/.claude-state \\\n'
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, '', 1))
PYX
run "the Dockerfile stops pre-creating a volume that is not CARGO_TARGET_DIR" "does not pre-create volume target"

seed; printf '\nif then fi\n' >> "$work/t/.container/setup.sh"
run "a container script gains a syntax error" "has a syntax error"

# An extension loses its version pin — the state VS Code resolves over the network, at connect
# time, against a raised egress firewall. It failed exactly this way on a real launch and said so
# only in a log line nothing gated on.
seed; jq_dc '.customizations.vscode.extensions |= map(split("@")[0])'
run "a VS Code extension loses its version pin" "unpinned VS Code extension(s)"

# ...and the same fail-open shape as the two below: the pinning check reads the list through
# lib.sh's dc_extensions, and a list it cannot find is an empty list. Nothing to check reads as
# nothing wrong — while the Dockerfile fetches no .vsix and setup.sh installs none.
seed; jq_dc 'del(.customizations.vscode.extensions)'
run "the extension list moves out from under dc_extensions" "this check just certified nothing"

# The source half of the mount review depends on lib.sh producing anything at all. mutate-config
# did not mutate lib.sh, so neither this nor the fail-open shape it protects was ever watched.
seed; python3 - "$work/t/.container/lib.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
open(p, 'w').write(s.replace('dc_mount_sources() { # dc_mount_sources <container.json>',
                             'dc_mount_sources() { return 0; #', 1))
PYX
run "the bind-source derivation returns nothing" "host bind source(s) parsed"

# ...and the same fail-open shape one file over. The expectation check reads mutate-verify.sh's
# `run "<label>" "<expect>"` lines with a sed; change that shape and the sed matches nothing,
# `stale_expects` is empty, and the check printed `ok (0 checked)` — passing by having found
# nothing to check, in the assertion whose whole job is finding guards that cannot fire.
seed; python3 - "$work/t/.container/mutate-verify.sh" <<'PYX'
import re, sys
p = sys.argv[1]; s = open(p).read()
# Rename the helper, exactly as an ordinary refactor would. Indentation included: a mutation
# anchored at `^run "` left an indented call behind, so the extractor still found one expectation
# and neither the emptiness pin nor the completeness check fired -- the mutation stopped
# expressing "the run-line shape changed" the moment one call was wrapped in an `if`.
open(p, 'w').write(re.sub(r'^(\s*)run "', r'\1drive "', s, flags=re.M))
PYX
run "mutate-verify's run-line shape changes" "this check just certified nothing"

# THE CALL SHAPE OF dc_require_apparmor_profile. The helper ends in `exit 1`, which stops nothing
# when it is called inside a command substitution -- so the empty name it refuses reaches docker as
# `apparmor=`, i.e. docker-default, whose `mount` denial is what the profile exists to lift.
#
# RE-AIMED AT run.sh. It used to break mutate-verify.sh, which made the same call; under D54.1 that
# script asks run.sh for its flags and makes no AppArmor decision at all, so the only caller left
# is `assembled_args`. A mutation whose target has moved reports NO-OP, not a passing guard -- this
# harness refuses an unmutated tree -- which is how this was found rather than silently lost.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '''    prof="$(dc_require_apparmor_profile "$here/apparmor-jkb-dev")" || return 1
    [ -n "$prof" ] || return 1
    printf '%s\\n' --security-opt "apparmor=$prof"'''
assert s.count(old) == 1, "mutation target absent"
open(p, 'w').write(s.replace(old,
    '''    printf '%s\\n' --security-opt "apparmor=$(dc_require_apparmor_profile "$here/apparmor-jkb-dev")"'''))
PYX
run "a caller swallows dc_require_apparmor_profile's refusal" "cannot stop the script"

# A SECOND DEFINITION OF "DOES APPARMOR MEDIATE", reintroduced in verify.sh -- which is where the
# divergent copy actually was: it preferred /proc/self/attr/apparmor/current, so the verifier and
# the launcher answered one question about one host from different primary evidence.
seed; python3 - "$work/t/.container/verify.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = 'if ! dc_apparmor_mediates; then'
assert s.count(old) == 1, "mutation target absent"
open(p, 'w').write(s.replace(old,
    'if ! { [ -r /sys/module/apparmor/parameters/enabled ] && grep -qi "^Y" /sys/module/apparmor/parameters/enabled; }; then'))
PYX
run "a second AppArmor-mediates predicate appears" "instead of calling dc_apparmor_mediates"

# A REFUSING PRODUCER READ BACK THROUGH A PROCESS SUBSTITUTION, which is how all three consumers
# were written before the rule was stated. BOTH files, because the guard globs $here/*.sh and a
# pattern that only ever matched one of them would look identical to one that covers them all.
#
# The offending text is BUILT rather than written out: check-config.sh scans every *.sh in the
# directory, this harness among them, and it cannot tell a shell command from the same characters
# sitting inside a python payload. Spelling it here would make the guard fail on a healthy tree —
# so it is `PS`, and re-inlining it is what would turn the gate red.
PS='< <'
seed; python3 - "$work/t/.container/run.sh" "$PS" <<'PYX'
import sys
p, ps = sys.argv[1], sys.argv[2]; s = open(p).read()
old = ('ARGS_OUT="$(assembled_args "$args_root")" || die "container.json could not be read; '
       'refusing to start a container from a partial declaration"\n'
       'while IFS= read -r line; do ARGS+=("$line"); done <<<"$ARGS_OUT"')
assert old in s, "mutation target absent"
new = 'while IFS= read -r line; do ARGS+=("$line"); done %s(assembled_args "$args_root")' % ps
open(p, 'w').write(s.replace(old, new, 1))
PYX
run "run.sh reads its assembly through a process substitution again" "which discards its refusal"

seed; python3 - "$work/t/.container/mutate-verify.sh" "$PS" <<'PYX'
import sys
p, ps = sys.argv[1], sys.argv[2]; s = open(p).read()
old = '_pa="$("$REPO/.container/run.sh" --print-args --posture "$REPO")" || {'
assert old in s, "mutation target absent"
i = s.index(old)
j = s.index('while IFS= read -r _l', i)
k = s.index('\n', j)
new = ('while IFS= read -r _l; do [ -n "$_l" ] && POSTURE+=("$_l"); done '
       '%s("$REPO/.container/run.sh" --print-args --posture "$REPO")' % ps)
open(p, 'w').write(s[:i] + new + s[k:])
PYX
run "the control reads run.sh's assembly through a process substitution again" "which discards its refusal"

# THE PRODUCER THE OLD GUARD COULD NOT SEE. It kept a hand-written list of four producer names to
# REFUSE, and its own comment recorded that one had "joined it late" after a reader escaped it --
# so `dc_mount_specs`, read exactly that way inside run.sh's mount loop, was invisible to the guard
# written for it. The mount list is the security boundary; a jq that failed part way would have
# left the container started with SOME of its mounts. It is an allowlist now, so a producer nobody
# classified turns the gate red instead of going unwatched, and this is that direction watched.
seed; python3 - "$work/t/.container/run.sh" "$PS" <<'PYX'
import sys
p, ps = sys.argv[1], sys.argv[2]; s = open(p).read()
old = ('        specs="$(dc_mount_specs "$cfg")" || die "container.json\'s mounts could not be read"\n')
assert old in s, "mutation target absent"
s = s.replace(old, "", 1)
old2 = '        done <<<"$specs"'
assert old2 in s, "mutation target absent"
open(p, 'w').write(s.replace(old2, '        done %s(dc_mount_specs "$cfg")' % ps, 1))
PYX
run "a declaration reader nobody classified is read through a process substitution" "which discards its refusal"

# ...AND THAT GUARD PASSING HAVING READ NOTHING. "No producer is read unsafely" and "the scan found
# no files" are the same empty value, in a check whose entire subject is a guard that certifies
# without observing. The comment stripper it reads through is the one moving part, so that is what
# is broken here.
seed; python3 - "$work/t/.container/check-config.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = "dc_strip_comments() { sed 's/[[:space:]]#.*$//; s/^#.*$//' \"$1\"; }"
assert old in s, "mutation target absent"
open(p, 'w').write(s.replace(old, "dc_strip_comments() { return 1; }", 1))
PYX
run "the process-substitution scan reads no files at all" "certified nothing"

# THE POSTURE HALF LEAKING AN INSTANCE FLAG, which is the one property keeping the harness's
# containers off the real ~/repos and ~/.jkb. A third emission site outside docker_args' two
# `[ "$half" != posture ]` gates is how it goes -- and it is why the guard RUNS the program instead
# of grepping those gates, which any such site would satisfy.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
old = '    local user\n    user="$(dc_remote_user "$cfg")"'
assert old in s, "mutation target absent"
leak = '    printf \'%s\\n\' "--mount" "source=$HOME/.jkb,target=/home/vscode/.jkb,type=bind"\n'
open(p, 'w').write(s.replace(old, leak + old, 1))
PYX
run "the posture half leaks a mount the harness would inherit" "instance flag(s) the harness must not inherit"

# ...AND THE VACUOUS DIRECTION. A docker_args emitting no instance flags at all passes "posture
# carries none" while breaking the launcher, so the guard also asserts the full half CARRIES them.
# That contrasting case is what makes it discriminate rather than merely not-fail.
seed; python3 - "$work/t/.container/run.sh" <<'PYX'
import sys
p = sys.argv[1]; s = open(p).read()
i = s.index('    [ "$half" = posture ] || printf ')
j = s.index('\n', i) + 1
s = s[:i] + '    :\n' + s[j:]
i = s.index('    if [ "$half" != posture ]; then')
j = s.index('    fi\n', i) + len('    fi\n')
open(p, 'w').write(s[:i] + s[j:])
PYX
run "run.sh stops emitting any instance flag" "emits no instance flag at all"

# COVERAGE, PINNED rather than claimed. The old summary said "every check-config assertion fired"
# while six of its failure paths had no mutation at all — so a 22nd assertion that cannot fail
# (this repo's most repeated defect, found in check-config.sh three rounds running) would have left
# the gate green under a line stating it had been watched failing.
#
# Pinned by COUNT, not by matching message text: three failure paths build their message in a
# variable, so the text a mutation matches does not appear at the `bad` call at all, and a matcher
# that cannot see them reports false gaps. A count cannot say WHICH path is unwatched, but it
# cannot be fooled either, and it forces the decision at the moment an assertion is added.
echo
echo "==> coverage"
# COUNTED OVER CODE, NOT COMMENTS, for the same reason PINNED_SWEEP_APPENDS is: check-config.sh's
# prose quotes the idioms it is talking about, so a comment explaining this very scan moved the
# count by one and demanded "a mutation for the new one" about a sentence.
# OCCURRENCES, NOT LINES, for the reason written out beside PINNED_SWEEP_APPENDS below -- which was
# rewritten for exactly this and left its neighbour counting lines in the same commit. Two failure
# paths on one line (`grep -q A || bad "…"; grep -q B || bad "…"`, and this file already spells
# compound `|| { bad "…"; gen_ok=0; }` forms) moved the pin by one, so one of the two shipped with no
# mutation while the harness printed a coverage number over it.
bad_sites="$(sed 's/[[:space:]]#.*$//; s/^#.*$//' "$repo/.container/check-config.sh" \
    | grep -o 'bad "' | grep -c .)"
PINNED_BAD_SITES=118
if [ "$bad_sites" -ne "$PINNED_BAD_SITES" ]; then
    fails=$((fails+1))
    printf '  check-config.sh has %s failure paths, pinned at %s.\n' "$bad_sites" "$PINNED_BAD_SITES"
    echo "  Add a mutation for the new one (or drop the stale one) and update PINNED_BAD_SITES."
    echo "  An assertion nothing breaks is the defect this harness exists to catch."
else
    printf '  %s failure paths in check-config.sh, %s mutations, count pinned\n' \
        "$bad_sites" "${#EXPECTS[@]}"
fi

# THE COMPOSED ASSERTION'S OWN BRANCHES. The sweep guard appends to one variable and emits it through
# a single `bad "`, so `bad_sites` moves by ONE however many conditions arrive -- and one of the
# first batch shipped with no mutation, invisibly. Counting the branches forces the decision per branch, which is
# the granularity the mutations are written at.
#
# EVERY OCCURRENCE OF THE VARIABLE, not `grep -c` on one assignment spelling. That counted LINES
# matching `sweep_problems="$sweep_problems`, so a condition written `sweep_problems+=" ...;"` --
# semantically identical, and the idiom check-config.sh itself uses elsewhere for accumulation --
# left the count unmoved, left `bad_sites` unmoved (all branches emit through one `bad "`), and
# shipped with no mutation: exactly the gap this pin exists to close. Two appends on one line
# bypassed it the same way. `grep -o` counts occurrences, so any spelling of a new branch moves it.
# COUNTED OVER CODE, NOT COMMENTS. The sweep guard's own prose names PINNED_SWEEP_APPENDS,
# transcript_projection_fell, HELD_NAME and phys_archive by identifier, so a round that DOCUMENTS a
# branch would otherwise move this count and print "Add a mutation for it" about a sentence.
sweep_appends="$(sed 's/[[:space:]]#.*$//; s/^#.*$//' "$repo/.container/check-config.sh" \
    | grep -o 'sweep_problems' | grep -c .)"
PINNED_SWEEP_APPENDS=111
if [ "$sweep_appends" -ne "$PINNED_SWEEP_APPENDS" ]; then
    fails=$((fails+1))
    printf '  the sweep guard mentions sweep_problems %s time(s), pinned at %s.\n' "$sweep_appends" "$PINNED_SWEEP_APPENDS"
    echo "  They all emit through one \`bad \"\`, so the failure-path count above cannot see a new one."
    echo "  Add a mutation for it and update PINNED_SWEEP_APPENDS."
else
    # WHAT THE COUNT ESTABLISHES, AND NOT MORE. This said "each branch with a mutation", which the
    # count cannot see -- it forces a new branch to be DECIDED about, not written for. Two branches
    # had none when that line was printed, both of them "the extraction read nothing" guards this
    # file says must be watched failing. Claiming more than was established, printed by the harness
    # that exists to catch exactly that.
    printf '  %s mentions of sweep_problems behind the sweep guard, count pinned so a new branch must be decided about\n' "$sweep_appends"
fi

# THE SAME, FOR THE UNSANDBOXED-EXEC GUARD. dc_unsb gathers every way an unsandboxed script can be
# steered through PATH and emits once, and round 8 found two of its round-7 branches unmutated.
unsb_appends="$(sed 's/[[:space:]]#.*$//; s/^#.*$//' "$repo/.container/check-config.sh" \
    | grep -o 'dc_unsb' | grep -c .)"
PINNED_UNSB_APPENDS=39
if [ "$unsb_appends" -ne "$PINNED_UNSB_APPENDS" ]; then
    fails=$((fails+1))
    printf '  the exec guard mentions dc_unsb %s time(s), pinned at %s.\n' "$unsb_appends" "$PINNED_UNSB_APPENDS"
    echo "  They all emit through one \`bad \"\`; add a mutation for the new branch and update PINNED_UNSB_APPENDS."
else
    printf '  %s mentions of dc_unsb behind the exec guard, count pinned so a new branch must be decided about\n' "$unsb_appends"
fi

echo
echo "==> self-test: an unmutated tree must be reported MISSED"
before="$fails"
seed
# THE CONTROL MUST OBSERVE A HEALTHY SUBJECT, not merely fail to trip. `run` reports MISSED when
# check-config.sh passes cleanly AND when it cannot run at all — a missing tree, a missing tool —
# so accepting MISSED on its own blesses a run in which nothing was exercised. Same rule as
# mutate-verify.sh, which had the same hole; a harness that judges other guards has to hold itself
# to the standard it enforces.
control_out="$(cd "$work/t" && ./.container/check-config.sh 2>&1)"
control_rc=$?
if [ "$control_rc" -ne 0 ] || ! grep -q "container config checks passed" <<<"$control_out"; then
    printf '\033[31mthe unmutated config does not pass check-config.sh (exit %s) — every MISSED above is\n' "$control_rc"
    printf 'unattributable, because a subject that cannot run looks exactly like a guard that did not fire\033[0m\n'
    sed 's/^/    /' <<<"$control_out" | grep -E "FAIL|failed|not found" | head -5
    exit 1
fi

judge "control: nothing mutated (MISSED is correct here)" "remoteUser is root" \
      "$control_out" "$control_rc"
if [ "$fails" -gt "$before" ]; then
    self_ok=1; fails="$before"
    echo "  (correct: an unmutated config does not trip the matcher)"
else
    self_ok=0
    echo "  MATCHER IS BROKEN: an unmutated config was reported CAUGHT"
fi

echo
[ "$self_ok" -eq 1 ] || { printf '\033[31mthe matcher reports CAUGHT for a healthy config — no result here is trustworthy\033[0m\n'; exit 1; }
[ "$fails" -eq 0 ] || { printf '\033[31m%d check-config assertion(s) did not fire\033[0m\n' "$fails"; exit 1; }
# No `- 1`: the control is judged directly rather than through run(), so it no longer registers an
# expect, and EXPECTS is exactly the mutations.
printf '\033[32m%s mutations caught over %s failure paths, and the matcher was shown to discriminate\033[0m\n' "${#EXPECTS[@]}" "$bad_sites"
