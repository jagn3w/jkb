#!/usr/bin/env bash
# Fetch the VS Code extensions container.json declares, as .vsix files, AT IMAGE BUILD TIME.
#
#   .container/fetch-extensions.sh [dest-dir]
#   .container/fetch-extensions.sh --self-test
#
# WHY THIS EXISTS. Left to VS Code, extensions are downloaded when you connect — which is after
# postCreate has raised the egress firewall. Measured on a real "Reopen in Container" run: both
# declared extensions failed with ECONNREFUSED to *.gallery.vsassets.io, and the container came up
# with neither installed. It is not a race we can win by reordering, either: the firewall is
# re-raised on every container start, so a later install hits the same wall.
#
# A build runs BEFORE any of that. The firewall lives inside the running container, so `docker
# build` has ordinary egress — which is why fetching here needs no change to the posture's
# `allowedDomains`. The alternative was to allowlist Microsoft's marketplace CDN, and that is
# worse than it sounds: the firewall pins names to IPs at raise time and CANNOT pin a wildcard
# (it says so on every raise), so `*.vsassets.io` would not help and each extension PUBLISHER
# would need its own concrete host, pinned to CDN addresses that rotate.
#
# setup.sh installs what this fetches; verify.sh asserts the result. The list itself is read from
# container.json through lib.sh, never restated here.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"

# The marketplace's own asset URL, in the publisher-scoped form. Taken from the failing requests
# in a real VS Code connect log rather than from memory, so it is the URL VS Code itself uses.
ext_url() { # ext_url <publisher.name> <version> <targetPlatform|"">
    local id="$1" version="$2" plat="${3:-}" publisher name
    publisher="${id%%.*}"
    name="${id#*.}"
    printf 'https://%s.gallery.vsassets.io/_apis/public/gallery/publisher/%s/extension/%s/%s/assetbyname/Microsoft.VisualStudio.Services.VSIXPackage' \
        "$publisher" "$publisher" "$name" "$version"
    [ -n "$plat" ] && printf '?targetPlatform=%s' "$plat"
    printf '\n'
}

# dpkg's architecture name is asked of the machine being built for, so this is correct under
# emulation and on both hosts this repo is used from. TARGETARCH would do the same job but only
# under BuildKit, and a build arg that is silently empty would map to an empty platform.
#
# An unknown architecture REFUSES rather than guessing or falling back to a universal download:
# these extensions are platform-specific, and a .vsix for the wrong platform installs and then
# fails at runtime, which is a worse failure than not building.
plat_for() { # plat_for <dpkg arch>
    case "$1" in
        arm64) echo linux-arm64 ;;
        amd64) echo linux-x64 ;;
        *)     return 1 ;;
    esac
}

if [ "${1:-}" = --self-test ]; then
    fails=0
    check() { # check <label> <got> <want>
        if [ "$2" = "$3" ]; then printf '  \033[32mok\033[0m   %s\n' "$1"
        else printf '  \033[31mFAIL\033[0m %s\n         got:  %s\n         want: %s\n' "$1" "$2" "$3"; fails=$((fails+1)); fi
    }
    # shellcheck source=/dev/null
    . "$here/lib.sh"
    echo "==> fetch-extensions self-test"

    check "the asset URL is the one VS Code itself requests" \
        "$(ext_url anthropic.claude-code 2.1.250 linux-arm64)" \
        'https://anthropic.gallery.vsassets.io/_apis/public/gallery/publisher/anthropic/extension/claude-code/2.1.250/assetbyname/Microsoft.VisualStudio.Services.VSIXPackage?targetPlatform=linux-arm64'
    # The publisher/name split is at the FIRST dot: `rust-lang.rust-analyzer` is publisher
    # `rust-lang`, and splitting at the last would ask for publisher `rust-lang.rust`.
    check "publisher and name split at the first dot" \
        "$(ext_url rust-lang.rust-analyzer 0.3.3025 '')" \
        'https://rust-lang.gallery.vsassets.io/_apis/public/gallery/publisher/rust-lang/extension/rust-analyzer/0.3.3025/assetbyname/Microsoft.VisualStudio.Services.VSIXPackage'
    check "arm64 maps to VS Code's platform name"  "$(plat_for arm64)" linux-arm64
    check "amd64 maps to linux-x64, not linux-amd64" "$(plat_for amd64)" linux-x64
    check "an unknown architecture is refused"     "$(plat_for riscv64 || echo REFUSED)" REFUSED

    check "a pinned entry splits into id and version" \
        "$(dc_extension_split anthropic.claude-code@2.1.250)" \
        "$(printf 'anthropic.claude-code\t2.1.250')"
    check "an UNPINNED entry is refused, never defaulted" \
        "$(dc_extension_split anthropic.claude-code || echo REFUSED)" REFUSED

    # The list really is read out of container.json, and really is pinned. This is the half
    # that would go quiet if the jq path were wrong: an empty list downloads nothing, installs
    # nothing, and every assertion downstream is vacuously satisfied.
    n=0
    while read -r ext; do
        [ -n "$ext" ] || continue
        n=$((n+1))
        check "declared entry $ext is pinned" "$(dc_extension_split "$ext" >/dev/null && echo yes || echo no)" yes
    done <<<"$(dc_extensions "$here/container.json")"
    check "container.json declares at least one extension" "$([ "$n" -gt 0 ] && echo yes || echo no)" yes

    echo
    [ "$fails" -eq 0 ] || { printf '\033[31m%d failed\033[0m\n' "$fails"; exit 1; }
    printf '\033[32mfetch-extensions self-test passed\033[0m\n'
    exit 0
fi

dest="${1:-$HOME/.vsix}"
# shellcheck source=/dev/null
. "$here/lib.sh"

arch="$(dpkg --print-architecture)"
plat="$(plat_for "$arch")" || {
    echo "fetch-extensions.sh: unknown architecture '$arch' — add it to plat_for" >&2
    exit 1
}

mkdir -p "$dest"
count=0
while read -r ext; do
    [ -n "$ext" ] || continue
    split="$(dc_extension_split "$ext")" || {
        echo "fetch-extensions.sh: '$ext' is not version-pinned." >&2
        echo "  Write it as publisher.name@version in container.json. Unpinned, VS Code" >&2
        echo "  resolves 'latest' over the network at connect time, which the egress firewall" >&2
        echo "  refuses — and refuses non-fatally, so the container comes up without it." >&2
        exit 1
    }
    id="${split%%$'\t'*}"; version="${split##*$'\t'}"
    out="$dest/$id-$version.vsix"

    # Platform-specific first, universal second. Most extensions are universal and answer 404 to
    # a targetPlatform they do not publish; both of the ones declared here are platform-specific,
    # and a universal download for them would install a package with no native bits.
    if curl -fsSL -o "$out" "$(ext_url "$id" "$version" "$plat")" \
    || curl -fsSL -o "$out" "$(ext_url "$id" "$version" '')"; then
        count=$((count+1))
        echo "fetched $id@$version ($plat)"
    else
        rm -f "$out"
        echo "fetch-extensions.sh: could not download $id@$version for $plat" >&2
        echo "  Check the version exists in the marketplace. A build that cannot fetch an" >&2
        echo "  extension must FAIL here: the container would otherwise come up without it and" >&2
        echo "  say nothing, which is the exact failure this script was written for." >&2
        exit 1
    fi
done <<<"$(dc_extensions "$here/container.json")"

echo "fetch-extensions.sh: $count extension(s) staged in $dest"
