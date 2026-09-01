#!/usr/bin/env bash
# Regenerate .container/apparmor-jkb-dev (design D49/D52).
#
# The profile is Docker's OWN `docker-default` with the mount family opened up: `deny mount,`
# becomes `mount,`, and `pivot_root,` is added (AppArmor denies a rule type nobody names). It is
# vendored rather than generated at run time for the reason generate-seccomp.sh gives: the security
# policy the container actually runs under is then reviewable in a diff, and loading works offline.
# Re-run this to refresh it against a newer upstream.
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
# none of which bubblewrap needs. This gives up the mount family instead of all of them.
# WHY THAT IS A SMALLER CONCESSION THAN IT LOOKS: `.container/seccomp-bwrap.json` already re-allows
# `mount`, `umount2` and `pivot_root` by name for the same purpose and states the same reason --
# they are only reachable inside the user namespace bubblewrap creates, where the process holds no
# privilege over the host. AppArmor was silently overriding a decision this design had already
# taken and reviewed, which is why the nested sandbox had never actually started on Linux.
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

# The render/patch/verify half, factored out so --self-test can drive it with fixture templates
# instead of the network. Everything that can REFUSE lives in here, and until this was callable
# none of those refusals was exercised by anything -- the drift check compares our output against
# our own output, so a bug in this logic is invisible to it, and what it produces is a security
# policy.
render_profile() { # render_profile <template.go> <out> <url>
    python3 - "$1" "$2" "$3" <<'PY'
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
PATCHED = """\
  # PATCHED, and this is the ONLY difference from docker-default. Upstream has `deny mount,` here
  # and names no pivot_root rule at all -- and in AppArmor an unnamed rule type is DENIED, so both
  # lines are needed. Measured in that order: allowing `mount,` alone moved bubblewrap's failure
  # from `Failed to make / slave` to `pivot_root: Permission denied`, which is what named the
  # second line. AppArmor's mount family is mount / remount / umount / pivot_root; `umount,` is
  # already granted above and `remount` is a mount option covered by the unqualified `mount,`, so
  # with these two the family is complete and no third line should become necessary.
  mount,
  pivot_root,"""
n = profile.count(DENY_MOUNT + "\n")
if n != 1:
    sys.exit(f"expected exactly one `{DENY_MOUNT}` line in the rendered profile, found {n} — "
             f"upstream changed the rule this patch targets, so the patch must be re-derived "
             f"rather than silently applied to nothing")
# Upstream naming a pivot_root rule of its own would mean this patch is adding a line beside one
# that already exists -- a duplicate at best, and a contradiction if theirs is a deny.
if "pivot_root" in profile:
    sys.exit("upstream now names a pivot_root rule of its own — re-derive the patch rather than "
             "adding a second one beside it")
profile = profile.replace(DENY_MOUNT + "\n", PATCHED + "\n")

# --------------------------------------------------------------------------- verify the render
# The renderer is code I wrote, so it gets checked rather than trusted. Every literal line of the
# template must survive into the output: that is what catches an action evaluated wrongly or a
# branch dropped, which would otherwise produce a profile that loads and is quietly weaker.
if "{{" in profile or "}}" in profile:
    sys.exit("rendered profile still contains template actions")
# THE ACTION-PRODUCED LINES, ASSERTED BY NAME. The `missing` check below compares literal template
# lines and SKIPS every line containing `{{` -- which is precisely the set of lines an action
# produces, so it is blind by construction to all of them. Deleting render()'s `if` arm left all
# seven self-test rows green and the profile carrying no `abi` line at all: nothing else covers it,
# since check-config.sh's kept-restrictions list greps only `deny` lines. Losing the ABI
# declaration is one of the five failures this generator exists to prevent -- without it AppArmor
# 4.0 reads bare `network,` as EXCLUDING unix sockets, which is a functional break, not a weaker
# policy. Each of these comes from an action and from nowhere else.
for required in (f"abi <{DATA['Abi']}>,",
                 DATA["Imports"][0],
                 "  " + DATA["InnerImports"][0],
                 f'profile "{DATA["Name"]}" flags=(attach_disconnected,mediate_deleted) {{'):
    if required not in profile.splitlines():
        sys.exit(f"the rendered profile is missing an action-produced line: {required!r} — the "
                 f"renderer dropped a branch, and `missing` cannot see it because it skips every "
                 f"template line containing an action")
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
# This is Docker's own `docker-default` with ONE CHANGE, at the single site marked PATCHED below:
# the mount family is opened up. `deny mount,` becomes `mount,`, and `pivot_root,` is added --
# AppArmor denies a rule type no rule names, and docker-default names none for pivot_root.
# Everything else is rendered verbatim from moby's template, so every other restriction
# docker-default imposes is kept.
#
# WHY THE CHANGE IS NEEDED. Claude Code's Linux sandbox shells out to bubblewrap, which creates a
# user namespace, re-mounts the root inside it, and pivots into it. docker-default denies `mount`
# outright, so bwrap failed at its first mount with `Failed to make / slave: Permission denied`.
# Allowing `mount,` moved the failure to `pivot_root: Permission denied` -- reported by a container
# whose profile in force was `jkb-dev (enforce)`, which is also the evidence that the profile
# applies INSIDE the user namespace and that Ubuntu's apparmor_restrict_unprivileged_userns is not
# transitioning bwrap into some other profile. Each step measured, not assumed.
#
# WHY NEITHER IS A NEW CONCESSION. `.container/seccomp-bwrap.json` already re-allows `mount`,
# `umount2` and `pivot_root` BY NAME for precisely this purpose, and states the reason: those calls
# are only reachable inside the user namespace bubblewrap creates, where the process holds no
# privilege over the host. AppArmor was silently overriding a decision this design had already
# taken and reviewed -- twice, once per rule -- which is why the nested sandbox had never actually
# started on Linux.
#
# AN EXPLICIT `deny` CANNOT BE OVERRIDDEN LATER IN APPARMOR -- deny wins over allow whatever the
# order -- so the rule has to be REPLACED rather than followed by an allow. That is why this is a
# whole profile rather than an include-plus-override.
#
# NARROWING, stated rather than pretended: `mount,` permits every mount operation and
# `pivot_root,` every pivot, not only the ones bwrap performs. AppArmor can qualify by fstype and
# mount point, but bwrap's sequence spans several (tmpfs, proc, sysfs, bind, MS_SLAVE propagation
# changes) and a too-narrow rule fails at run time in a way that reads exactly like the bug this
# fixes. Left broad deliberately, bounded
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
relaxed = sum(1 for ln in PATCHED.splitlines() if ln.strip() and not ln.strip().startswith("#"))
print(f"wrote {dst}: profile {DATA['Name']}, {kept} deny rules kept, {relaxed} rule(s) relaxed")
print(f"  from {url}")
print(f"  upstream sha256 {digest}")
PY
}

if [ "${1:-}" = --self-test ]; then
    t="$(mktemp -d)"; trap 'rm -rf "$t"' EXIT
    fails=0
    # Fixtures are WHOLE Go files, so the backtick-literal extraction is exercised too rather than
    # bypassed by handing the renderer a bare template string.
    python3 - "$t" <<'FIX'
import os, sys
d = sys.argv[1]
GOOD = """{{if .Abi}}abi <{{.Abi}}>,
{{- end}}
{{range $value := .Imports}}
{{$value}}
{{- end}}

profile "{{.Name}}" flags=(attach_disconnected,mediate_deleted) {
{{- range $value := .InnerImports}}
  {{$value}}
{{- end}}
  umount,
  signal (receive) peer="{{.DaemonProfile}}",
  deny @{PROC}/sysrq-trigger rwklx,

  deny mount,
}"""
V = {
    "good":   GOOD,
    "nodeny": GOOD.replace("  deny mount,", "  # upstream stopped denying mount"),
    "twice":  GOOD.replace("  deny mount,", "  deny mount,\n  deny mount,"),
    "pivot":  GOOD.replace("  umount,", "  umount,\n  pivot_root,"),
    "action": GOOD.replace("  umount,", '  {{template "partial"}}'),
    "field":  GOOD.replace("{{.Name}}", "{{.Nope}}"),
}
for name, body in V.items():
    with open(os.path.join(d, name + ".go"), "w") as f:
        f.write("package apparmor\n\nconst baseTemplate = `" + body + "`\n")
FIX

    expect() { # expect <label> <fixture> <ok|refuse> [substring the refusal must contain]
        # `|| rc=$?`, never a bare `out="$(...)"; rc=$?`. Every fixture below is EXPECTED to make
        # render_profile exit non-zero, and a bare assignment from a failing command substitution
        # is a simple command in no conditional context -- so `errexit` fires and the self-test
        # dies after its two passing rows, reporting nothing about the five refusals it exists to
        # exercise. Written here after fixing the identical bug in check-drift.sh the same day.
        local out rc=0
        out="$(render_profile "$t/$2.go" "$t/$2.out" "https://fixture.invalid/x" 2>&1)" || rc=$?
        if [ "$3" = ok ]; then
            if [ "$rc" -eq 0 ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
            else printf '  \033[31mFAIL\033[0m %s (refused: %s)\n' "$1" "$out"; fails=$((fails+1)); fi
        elif [ "$rc" -eq 0 ]; then
            printf '  \033[31mFAIL\033[0m %s — it was ACCEPTED, so the refusal cannot fire\n' "$1"; fails=$((fails+1))
        elif grep -qF -e "$4" <<<"$out"; then
            printf '  \033[32mok\033[0m   %s\n' "$1"
        else
            printf '  \033[31mFAIL\033[0m %s — refused for the wrong reason: %s\n' "$1" "$out"; fails=$((fails+1))
        fi
    }

    expect "a well-formed template renders and patches" good ok

    # What the patch is FOR, asserted against the generator's own output rather than only in
    # check-config.sh: a profile is answerable for carrying both rules and for keeping the rest.
    if [ -f "$t/good.out" ]; then
        bad_out=0
        for want in '^  mount,$' '^  pivot_root,$'; do
            grep -qE "$want" "$t/good.out" || { printf '  \033[31mFAIL\033[0m the rendered profile has no %s\n' "$want"; bad_out=1; }
        done
        grep -qE '^[[:space:]]*deny[[:space:]]+mount,' "$t/good.out" \
            && { printf '  \033[31mFAIL\033[0m the deny survived the patch\n'; bad_out=1; }
        grep -qF -e 'deny @{PROC}/sysrq-trigger rwklx,' "$t/good.out" \
            || { printf '  \033[31mFAIL\033[0m an unrelated upstream rule was lost in the render\n'; bad_out=1; }
        if [ "$bad_out" -eq 0 ]; then
            printf '  \033[32mok\033[0m   the output carries mount and pivot_root, drops the deny, keeps the rest\n'
        else fails=$((fails+1)); fi
    fi

    # A patch that no-ops against changed upstream is the failure check-config.sh already names for
    # the seccomp profile: a policy that parses, applies, and leaves the sandbox unable to start.
    expect "a template with no \`deny mount,\` is refused" nodeny refuse "found 0"
    expect "a template with two of them is refused" twice refuse "found 2"
    expect "upstream naming pivot_root itself is refused, not doubled" pivot refuse "re-derive the patch"
    expect "an unsupported action is refused, not rendered as nothing" action refuse "does not implement"
    expect "a field this script supplies no value for is refused" field refuse "supplies no value"

    if [ "$fails" -eq 0 ]; then printf '\033[32mgenerate-apparmor self-test passed\033[0m\n'; exit 0; fi
    printf '\033[31mgenerate-apparmor self-test: %s failed\033[0m\n' "$fails"; exit 1
fi

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
curl -fsSL -o "$tmp" "$url"
grep -q 'baseTemplate' "$tmp" || {
    echo "upstream file has no baseTemplate (moved again?): $url" >&2; exit 1; }

render_profile "$tmp" "$out" "$url"
