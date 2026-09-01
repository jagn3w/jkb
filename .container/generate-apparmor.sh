#!/usr/bin/env bash
# Regenerate .container/apparmor-jkb-dev (design D49/D52).
#
# The profile is Docker's OWN `docker-default` with exactly one rule changed: `deny mount,` becomes
# `mount,`. It is vendored rather than generated at run time for the reason generate-seccomp.sh
# gives: the security policy the container actually runs under is then reviewable in a diff, and
# loading works offline. Re-run this to refresh it against a newer upstream.
#
# WHY THIS SCRIPT EXISTS AT ALL. The first version of the profile was TRANSCRIBED BY HAND from
# memory of moby's template, and hand-transcribing a security policy is exactly as bad as it
# sounds. Against the real upstream it was missing `deny network alg,` (Linux kernel crypto API),
# `deny network vsock,` (host/guest channel), `deny /sys/devices/virtual/powercap/** rwklx,`
# (the PLATYPUS side channel) and the `abi <abi/3.0>,` declaration whose absence makes AppArmor
# 4.0 read `network,` as excluding unix sockets; it carried a stale 2017-era @{PROC} pattern and
# a duplicated signal rule in place of the runc/crun peers `docker stop` needs. Every one of those
# loads fine, keeps the profile's name, and passes a check that the named restrictions are
# present. None of it is discoverable by reading the file. So the file is GENERATED, and CI
# re-runs this and requires no diff -- which catches both a bad transcription and upstream drift.
#
# WHY NOT `apparmor=unconfined`: that discards the whole profile -- the /proc write denials,
# sysrq-trigger, kcore, the /sys restrictions, the network denials and the ptrace confinement --
# none of which bubblewrap needs. This gives up one rule instead of all of them.
# WHY THE ONE RULE IS A SMALLER CONCESSION THAN IT LOOKS: `.container/seccomp-bwrap.json` already
# re-allows the mount syscalls for the same purpose and states the same reason -- they are only
# reachable inside the user namespace bubblewrap creates, where the process holds no privilege
# over the host. AppArmor was silently overriding a decision this design had already taken and
# reviewed, which is why the nested sandbox had never actually started on Linux.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="$here/apparmor-jkb-dev"
# Same repo generate-seccomp.sh pins: moby moved these profiles out of moby/moby, and
# moby/moby's own profiles/apparmor/template.go now 404s.
url="https://raw.githubusercontent.com/moby/profiles/main/apparmor/template.go"

# check-drift.sh asks every generator what it writes rather than carrying a list of artifacts that
# has to be kept in step with the set of generators. A generator added tomorrow joins the drift
# check by existing.
if [ "${1:-}" = --print-target ]; then printf '%s\n' "$out"; exit 0; fi

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
curl -fsSL -o "$tmp" "$url"
grep -q 'baseTemplate' "$tmp" || {
    echo "upstream file has no baseTemplate (moved again?): $url" >&2; exit 1; }

python3 - "$tmp" "$out" "$url" <<'PY'
import hashlib, re, sys

src, dst, url = sys.argv[1], sys.argv[2], sys.argv[3]
raw = open(src, "rb").read()
digest = hashlib.sha256(raw).hexdigest()
text = raw.decode("utf-8")

# ---------------------------------------------------------------- extract the template literal
m = re.search(r"const baseTemplate = `(.*?)`\n", text, re.S)
if not m:
    sys.exit("could not find the baseTemplate backtick literal — upstream restructured the file")
tpl = m.group(1)

# ------------------------------------------------------------------------------ render inputs
# Read off moby/profiles/apparmor/apparmor.go's generate() rather than guessed. Each of the three
# macro-conditional values is taken in its "macro present" arm, which is the case on every host
# that has AppArmor at all (they are files under /etc/apparmor.d). If a macro really is absent,
# `apparmor_parser` REFUSES the profile rather than loading a weaker one -- run.sh then refuses to
# start the container, which is the safe direction and is why this is not made conditional here.
# DaemonProfile is dockerd's own label from /proc/self/attr/current, defaulting to "unconfined";
# it appears only as a signal peer, so a daemon running confined would need this regenerated.
DATA = {
    "Abi": "abi/3.0",
    "Name": "jkb-dev",
    "DaemonProfile": "unconfined",
    "Imports": ["#include <tunables/global>"],
    "InnerImports": ["#include <abstractions/base>"],
}

# ------------------------------------------------- a faithful evaluator for the subset in use
# text/template, restricted to what this template contains: {{if X}}, {{range $v := X}}, {{$v}},
# {{.Field}}, {{end}}, and the {{- / -}} whitespace-trim markers. Deliberately NOT a general
# implementation -- an unrecognised action raises rather than rendering as nothing, because a
# silently-dropped action is precisely the failure this whole script exists to remove.
TOKEN = re.compile(r"\{\{(-?)\s*(.*?)\s*(-?)\}\}", re.S)

def tokenize(t):
    nodes, pos, trim_next = [], 0, False
    def push_text(s):
        nonlocal trim_next
        if trim_next:
            s, trim_next = s.lstrip(), False
        nodes.append(("text", s))
    for tok in TOKEN.finditer(t):
        pre = t[pos:tok.start()]
        if tok.group(1) == "-":
            pre = pre.rstrip()
        push_text(pre)
        nodes.append(("action", tok.group(2)))
        trim_next = tok.group(3) == "-"
        pos = tok.end()
    push_text(t[pos:])
    return nodes

def parse(nodes, i=0):
    body = []
    while i < len(nodes):
        kind, val = nodes[i]
        if kind == "text":
            body.append(("text", val)); i += 1
        elif val == "end":
            return body, i + 1
        elif val.startswith("if "):
            inner, i = parse(nodes, i + 1)
            body.append(("if", val[3:].strip(), inner))
        elif val.startswith("range "):
            inner, i = parse(nodes, i + 1)
            body.append(("range", val[6:].strip(), inner))
        elif val.startswith(".") or val.startswith("$"):
            body.append(("var", val)); i += 1
        else:
            sys.exit(f"unsupported template action {{{{{val}}}}} — upstream uses a construct this "
                     f"renderer does not implement; extend it rather than letting it render empty")
    return body, i

def lookup(expr, scope):
    if expr.startswith("."):
        if expr[1:] not in DATA:
            sys.exit(f"template references {expr}, which this script supplies no value for")
        return DATA[expr[1:]]
    if expr in scope:
        return scope[expr]
    sys.exit(f"template references undefined variable {expr}")

def render(body, scope):
    out = []
    for node in body:
        if node[0] == "text":
            out.append(node[1])
        elif node[0] == "var":
            out.append(str(lookup(node[1], scope)))
        elif node[0] == "if":
            if lookup(node[1], scope):
                out.append(render(node[2], scope))
        elif node[0] == "range":
            var, source = (x.strip() for x in node[1].split(":=", 1))
            for item in lookup(source, scope):
                out.append(render(node[2], {**scope, var: item}))
    return "".join(out)

tree, _ = parse(tokenize(tpl))
profile = render(tree, {})

# ---------------------------------------------------------------------- the one deliberate patch
DENY_MOUNT = "  deny mount,"
PATCHED = "  mount,  # PATCHED: upstream docker-default has `deny mount,` here. See the header."
n = profile.count(DENY_MOUNT + "\n")
if n != 1:
    sys.exit(f"expected exactly one `{DENY_MOUNT}` line in the rendered profile, found {n} — "
             f"upstream changed the rule this patch targets, so the patch must be re-derived "
             f"rather than silently applied to nothing")
profile = profile.replace(DENY_MOUNT + "\n", PATCHED + "\n")

# --------------------------------------------------------------------------- verify the render
# The renderer is code I wrote, so it gets checked rather than trusted. Every literal line of the
# template must survive into the output: that is what catches an action evaluated wrongly or a
# branch dropped, which would otherwise produce a profile that loads and is quietly weaker.
if "{{" in profile or "}}" in profile:
    sys.exit("rendered profile still contains template actions")
missing = [ln for ln in tpl.splitlines()
           if "{{" not in ln and ln.strip() and ln != DENY_MOUNT and ln not in profile.splitlines()]
if missing:
    sys.exit("the renderer dropped upstream lines:\n  " + "\n  ".join(missing))
if f'profile "{DATA["Name"]}"' not in profile:
    sys.exit("rendered profile does not declare the expected profile name")
if not profile.rstrip().endswith("}"):
    sys.exit("rendered profile does not end with a closing brace")

HEADER = f"""\
# AppArmor profile for the jkb dev container (design D49/D52).
#
# GENERATED FILE -- DO NOT EDIT. Run .container/generate-apparmor.sh to refresh it.
# Source: {url}
# upstream-sha256: {digest}
#
# This is Docker's own `docker-default` with EXACTLY ONE rule changed: `deny mount,` becomes
# `mount,` (the line marked PATCHED below). Everything else is rendered verbatim from moby's
# template, so every other restriction docker-default imposes is kept.
#
# WHY THE CHANGE IS NEEDED. Claude Code's Linux sandbox shells out to bubblewrap, which creates a
# user namespace and then re-mounts the root inside it. docker-default denies `mount` outright, so
# bwrap fails at its first mount with `Failed to make / slave: Permission denied` -- namespaces
# created, mounts refused. Measured, not assumed: a container reports
# `apparmor profile in force: docker-default (enforce)` and fails exactly there, while the seccomp
# profile demonstrably allows the mount syscalls.
#
# WHY IT IS NOT A NEW CONCESSION. `.container/seccomp-bwrap.json` already re-allows `mount`,
# `umount2`, `pivot_root` and the rest for precisely this purpose, and states the reason: those
# calls are only reachable inside the user namespace bubblewrap creates, where the process holds
# no privilege over the host. AppArmor was silently overriding a decision this design had already
# taken and reviewed -- which is why the nested sandbox had never actually started on Linux.
#
# AN EXPLICIT `deny` CANNOT BE OVERRIDDEN LATER IN APPARMOR -- deny wins over allow whatever the
# order -- so the rule has to be REPLACED rather than followed by an allow. That is why this is a
# whole profile rather than an include-plus-override.
#
# NARROWING, stated rather than pretended: `mount,` permits every mount operation, not only the
# ones bwrap performs. AppArmor can qualify by fstype and mount point, but bwrap's sequence spans
# several (tmpfs, proc, sysfs, bind, MS_SLAVE propagation changes) and a too-narrow rule fails at
# run time in a way that reads exactly like the bug this fixes. Left broad deliberately, bounded
# by being reachable only inside the user namespace, and noted as the next tightening.
#
# LOADING IT IS A HOST ACTION and needs root, so a container cannot do it for itself:
#     sudo apparmor_parser -r -W .container/apparmor-jkb-dev
# run.sh checks it is loaded and refuses with that command rather than falling back to
# docker-default, which would start a container whose nested sandbox silently does not work --
# the state this whole change exists to end.

"""

with open(dst, "w") as f:
    f.write(HEADER)
    f.write(profile if profile.endswith("\n") else profile + "\n")

kept = sum(1 for ln in profile.splitlines() if ln.strip().startswith("deny "))
print(f"wrote {dst}: profile {DATA['Name']}, {kept} deny rules kept, 1 rule relaxed")
print(f"  from {url}")
print(f"  upstream sha256 {digest}")
PY
