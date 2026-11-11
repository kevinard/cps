#!/usr/bin/env bash
# Assemble the local, signed sandbox toolchain shipped in system/git-build.
set -euo pipefail
export LC_ALL=C
umask 022

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
source "$project/tests/build-toolchain.sh"
source "$project/tests/systemd-boot-guest.sh"
cps=$1 prototype=$2 signing_key=$3
busybox=${CPS_GIT_BUILD_BUSYBOX:-/usr/lib/initcpio/busybox}
arch=$(uname -m)
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT

seed="$work/base-tools"
mkdir -p "$seed/bin"
install -m755 -- "$busybox" "$seed/bin/busybox"
copy_runtime_deps "$seed" "$busybox"
applets=$("$busybox" --list)
for applet in sh awk basename cat chmod cp cut dirname echo env grep gzip head install ln ls mkdir mktemp mv printf pwd readlink rm sed sha256sum sort stat tail tar test touch uname wc; do
    grep -Fxq "$applet" <<< "$applets" || { echo "BusyBox is missing the $applet applet" >&2; exit 1; }
    ln -s busybox "$seed/bin/$applet"
done
"$cps" manifest generate --proto "$seed" --publisher canopa-git-tools \
    --version 1.0.0-0 --arch "$arch" build/base-tools > "$work/base-tools.kdl"

destination="$prototype/usr/share/cps/git-build"
mkdir -p "$destination"
(
    cd -- "$destination"
    "$cps" repo trust init --repo repo --publisher canopa-git-tools --key "$signing_key" >/dev/null
    "$cps" publish manifests --repo repo --proto "$seed" --key "$signing_key" "$work/base-tools.kdl"
    rm -- repo/publish.lock
    # Keep the sandbox ELF files opaque to the host image's runtime closure check.
    for blob in repo/blobs/*; do
        zstd --quiet --zstd=wlog=21 --rm -o "repo/blobs-${blob##*/}.zst" -- "$blob"
    done
    printf 'schema-version 1 kind="toolchain-config"\n' > toolchain.kdl
    append_toolchain_publisher "$work/pins" toolchain.kdl canopa-git-tools repo repo/trust/1.json
    {
        printf 'schema-version 1 kind="git-toolchain"\ntoolchain solver="sat-v2" {\n'
        cat "$work/pins"
        printf '    package "cps://canopa-git-tools/build/base-tools@1.0.0-0"\n}\n'
    } > toolchain.default.kdl
    find repo -type d -exec chmod 755 {} +
    find repo -type f -exec chmod 644 {} +
)
