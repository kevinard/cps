#!/usr/bin/env bash
set -euo pipefail

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-"$project/target/debug/cps"}")
output=$(realpath -m -- "${1:?usage: build-busybox.sh OUTPUT-DIRECTORY}")
source_dir=${CPS_BUSYBOX_SOURCE_DIR:-}
if [[ -n $source_dir ]]; then source_dir=$(realpath -- "$source_dir"); fi
manifest=busybox.kdl
binary=bin/busybox
version=1.38.0-0
if [[ ${CPS_BUSYBOX_MERGED_USR:-0} == 1 ]]; then
    manifest=busybox-usr.kdl
    binary=usr/bin/busybox
    version=1.38.0-1
fi
package="cps://canopa-bootstrap/system/busybox@$version"
mkdir -m 700 -- "$output"
work=$(mktemp -d "$output/.private.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
cd -- "$output"

keypair() {
    openssl genpkey -algorithm ED25519 -outform DER -out "$work/$1.der"
    openssl pkey -inform DER -in "$work/$1.der" -pubout -outform DER -out "$1-public.der"
}
sha() { sha256sum -- "$1" | cut -d' ' -f1; }

keypair toolchain
keypair builder
keypair release
bash "$project/tests/seed-native-c.sh" "$work/native-c" --busybox
"$cps" manifest generate --proto "$work/native-c" --publisher seed --version 1.0.0-0 \
    build/native-c > native-c.kdl
"$cps" publish --repo toolchain --proto "$work/native-c" --signing-key "$work/toolchain.der" native-c.kdl

sed "s/uid=0 gid=0/uid=$(id -u) gid=$(id -g)/g" "$project/packaging/busybox/$manifest" > busybox.kdl
sed -e "s/@ARCH@/$(uname -m)/g" -e "s/@CATALOG_SHA256@/$(cat toolchain/current)/g" \
    -e "s/@KEY_SHA256@/$(tail -c 32 -- toolchain-public.der | sha256sum | cut -d' ' -f1)/g" \
    -e "s/@MANIFEST_SHA256@/$(sha busybox.kdl)/g" \
    -e "s|@BUSYBOX_PATH@|$binary|g" \
    "$project/packaging/busybox/build.kdl.in" > build.kdl

mkdir "$work/inputs"
cp -- "$project/packaging/busybox/"CVE-2026-*.patch "$work/inputs/"
if [[ -n $source_dir ]]; then
    cp --reflink=auto -- "$source_dir/busybox-1.38.0.tar.bz2" "$work/inputs/"
    "$cps" source import --from "$work/inputs" --cache sources build.kdl
else
    "$cps" source fetch --from "$work/inputs" --cache sources build.kdl
fi
"$cps" build --repo toolchain --publisher seed --trust-key toolchain-public.der \
    --source-cache sources --output bundle --check-reproducible \
    --builder-signing-key "$work/builder.der" build.kdl

mkdir repo
cp builder-public.der repo/
recipe_sha256=$(sha build.kdl)
cat > repo/publish-policy.kdl <<POLICY
schema-version 1 kind="publish-policy"
publisher "canopa-bootstrap" {
    builder-key "builder-public.der"
    approve "system/busybox" version="$version" sha256="$recipe_sha256"
}
POLICY
"$cps" publish-bundle --repo repo --signing-key "$work/release.der" --builder-key builder-public.der \
    --recipe-sha256 "$recipe_sha256" bundle
"$cps" repo attestation --repo repo --publisher canopa-bootstrap --trust-key release-public.der \
    "$package" > attestation.json
(cat bundle/attestation.json; printf '\n') | cmp - attestation.json
"$cps" image-create --root image --repo repo --publisher canopa-bootstrap --trust-key release-public.der
"$cps" plan --root image --output install.json install "$package"
"$cps" apply --root image install.json
"$cps" verify --root image --pristine
cmp "bundle/proto/$binary" "image/$binary"
if [[ $manifest == busybox-usr.kdl ]]; then
    test "$(readlink image/usr/bin/sh)" = busybox
    image/usr/bin/sh -ec 'test "$1" = merged' sh merged
fi
test "$(readlink "image/${binary%/*}/insmod")" = busybox
"image/${binary%/*}/insmod" --help 2>&1 | grep -F 'Usage: insmod FILE'
if error=$(ulimit -c 0; "image/$binary" ash -c 'recurse() { recurse; }; recurse' 2>&1); then
    echo 'Expected the ash recursion guard to reject unbounded recursion' >&2
    exit 1
fi
grep -F 'function recursion limit exceeded' <<< "$error"
applets=$("image/$binary" --list)
if [[ $applets != $'ash\ncat\ninsmod\nmkdir\npoweroff\nreboot\nsh\nsync\nuname' ]]; then
    printf 'Unexpected BusyBox applets\n%s\n' "$applets" >&2
    exit 1
fi
headers=$(readelf -l "image/$binary")
if [[ $headers == *INTERP* ]]; then
    echo 'Expected a static BusyBox executable' >&2
    exit 1
fi
printf 'Verified pinned BusyBox source, reproducible binary, signed provenance, and installation\n'
