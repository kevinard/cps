#!/usr/bin/env bash
set -euo pipefail

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-"$project/target/debug/cps"}")
output=$(realpath -m -- "${1:?usage: build-busybox.sh OUTPUT-DIRECTORY}")
source_dir=${CPS_BUSYBOX_SOURCE_DIR:-}
if [[ -n $source_dir ]]; then source_dir=$(realpath -- "$source_dir"); fi
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
bash "$project/tests/seed-native-c.sh" "$work/native-c"
"$cps" manifest generate --proto "$work/native-c" --publisher seed --version 1.0.0-0 \
    build/native-c > native-c.kdl
"$cps" publish --repo toolchain --proto "$work/native-c" --signing-key "$work/toolchain.der" native-c.kdl

sed "s/uid=0 gid=0/uid=$(id -u) gid=$(id -g)/g" "$project/packaging/busybox/busybox.kdl" > busybox.kdl
sed -e "s/@ARCH@/$(uname -m)/g" -e "s/@CATALOG_SHA256@/$(cat toolchain/current)/g" \
    -e "s/@MANIFEST_SHA256@/$(sha busybox.kdl)/g" \
    "$project/packaging/busybox/build.kdl.in" > build.kdl

if [[ -n $source_dir ]]; then
    "$cps" source import --from "$source_dir" --cache sources build.kdl
else
    "$cps" source fetch --cache sources build.kdl
fi
"$cps" build --repo toolchain --publisher seed --trust-key toolchain-public.der \
    --source-cache sources --output bundle --check-reproducible \
    --builder-signing-key "$work/builder.der" build.kdl

mkdir repo
cp builder-public.der repo/
recipe_sha256=$(sha build.kdl)
cat > repo/publish-policy.kdl <<POLICY
schema-version 1
publisher "canopa-bootstrap" {
    builder-key "builder-public.der"
    approve "system/busybox" version="1.37.0-0" sha256="$recipe_sha256"
}
POLICY
"$cps" publish-bundle --repo repo --signing-key "$work/release.der" --builder-key builder-public.der \
    --recipe-sha256 "$recipe_sha256" bundle
"$cps" repo attestation --repo repo --publisher canopa-bootstrap --trust-key release-public.der \
    cps://canopa-bootstrap/system/busybox@1.37.0-0 > attestation.json
(cat bundle/attestation.json; printf '\n') | cmp - attestation.json
"$cps" image-create --root image --repo repo --publisher canopa-bootstrap --trust-key release-public.der
"$cps" plan --root image --output install.json install cps://canopa-bootstrap/system/busybox@1.37.0-0
"$cps" apply --root image install.json
"$cps" verify --root image --pristine
cmp bundle/proto/bin/busybox image/bin/busybox
applets=$(image/bin/busybox --list)
if [[ $applets != $'ash\ncat\nmkdir\npoweroff\nreboot\nsh\nsync\nuname' ]]; then
    printf 'Unexpected BusyBox applets\n%s\n' "$applets" >&2
    exit 1
fi
headers=$(readelf -l image/bin/busybox)
if [[ $headers == *INTERP* ]]; then
    echo 'Expected a static BusyBox executable' >&2
    exit 1
fi
printf 'Verified pinned BusyBox source, reproducible binary, signed provenance, and installation\n'
