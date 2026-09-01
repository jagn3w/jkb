#!/usr/bin/env bash
# Regenerate .container/seccomp-bwrap.json (design D49).
#
# The profile is Docker's DEFAULT profile plus an unconditional allow for the namespace and mount
# syscalls bubblewrap needs. It is vendored rather than generated at run time so the security
# policy the container actually runs under is reviewable in a diff, and so a build works offline.
# Re-run this to refresh it against a newer upstream default.
#
# WHY NOT `--security-opt seccomp=unconfined`: that discards the whole default profile, ~40-odd
# blocked syscalls, to get back the 14 below. Measured, this narrower profile is sufficient.
# WHY THESE ARE A SMALLER CONCESSION THAN THEY LOOK: they are only reachable inside the user
# namespace bubblewrap creates, where the process holds no privilege over the host.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="$here/seccomp-bwrap.json"
# moby moved this file out of moby/moby; pin the repo that actually serves it today.
url="https://raw.githubusercontent.com/moby/profiles/main/seccomp/default.json"

# check-drift.sh asks every generator what it writes rather than carrying a list of artifacts that
# has to be kept in step with the set of generators. A generator added tomorrow joins the drift
# check by existing.
if [ "${1:-}" = --print-target ]; then printf '%s\n' "$out"; exit 0; fi

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
curl -fsSL -o "$tmp" "$url"
jq empty "$tmp" || { echo "upstream default profile is not JSON (moved again?): $url" >&2; exit 1; }

python3 - "$tmp" "$out" "$url" <<'PY'
import hashlib, json, sys
src, dst, url = sys.argv[1], sys.argv[2], sys.argv[3]
# Recorded so check-drift.sh can tell "upstream moved" from "this file was hand-edited" -- two
# failures repaired by looking at different diffs. Carried INSIDE the existing `comment` string
# rather than as a new top-level key: `comment` is already there and demonstrably accepted by
# Docker's parser, and a security profile is the wrong place to find out whether an unknown field
# is ignored or rejected.
digest = hashlib.sha256(open(src, "rb").read()).hexdigest()
# Found by iteration, not guesswork: re-allowing only the namespace calls moves bubblewrap's
# failure from namespace creation to `mount` ("Failed to make / slave"), which is what named the
# second group. Both groups are required.
NEEDED = sorted({
    "clone", "clone3", "unshare", "setns",                    # create the namespaces
    "mount", "umount2", "pivot_root", "mount_setattr",        # build the root inside them
    "open_tree", "move_mount", "fsopen", "fsconfig", "fsmount", "fspick",
})
p = json.load(open(src))
kept = []
for s in p["syscalls"]:
    names = set(s.get("names") or ([s["name"]] if "name" in s else []))
    overlap = names & set(NEEDED)
    if overlap:
        # Drop the restricted entry for these names only; leave the rest of the rule intact.
        s = dict(s)
        s["names"] = sorted(names - set(NEEDED))
        if not s["names"]:
            continue
    kept.append(s)
kept.append({"names": NEEDED, "action": "SCMP_ACT_ALLOW"})
p["syscalls"] = kept
p["comment"] = ("Docker default seccomp profile + unconditional allow for the namespace/mount "
                "syscalls bubblewrap needs, so Claude Code's sandbox can run nested inside this "
                "container. GENERATED FILE -- DO NOT EDIT; run .container/generate-seccomp.sh. "
                "Source: " + url + " upstream-sha256: " + digest)
json.dump(p, open(dst, "w"), indent=1, sort_keys=False)
open(dst, "a").write("\n")
print(f"wrote {dst}: {len(kept)} syscall groups, {len(NEEDED)} re-allowed")
PY
