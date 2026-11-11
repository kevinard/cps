#!/usr/bin/env bash
set -euo pipefail
umask 022

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-"$project/target/debug/cps"}")
output=$(realpath -m -- "${1:?usage: build-usr-base.sh OUTPUT-DIRECTORY CPS-FIXTURE}")
runtime=$(realpath -- "${2:?usage: build-usr-base.sh OUTPUT-DIRECTORY CPS-FIXTURE}")
[[ $(uname -m) == x86_64 ]] || { echo 'The merged /usr fixture needs an x86-64 runtime' >&2; exit 1; }
mkdir -m 700 -- "$output"
work=$(mktemp -d "$output/.private.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
cd -- "$output"

original="$work/original"
"$cps" image create --root "$original" --repo "$runtime/runtime-seed" \
    --trust-root "$runtime/runtime-seed.trust.json"
"$cps" install --root "$original" -y \
    cps://canopa-runtime-seed/system/elf-runtime
"$cps" verify --root "$original" --pristine

openssl genpkey -algorithm ED25519 -outform DER -out "$work/release.der"
openssl pkey -inform DER -in "$work/release.der" -pubout -outform DER -out release-public.der
"$cps" repo trust init --repo repo --publisher canopa-usr-base --key "$work/release.der" >/dev/null
cp -- repo/trust/1.json release.trust.json
mkdir "$work/proto"
uid=$(id -u)
gid=$(id -g)
for package in filesystem elf-runtime; do
    sed "s/uid=0 gid=0/uid=$uid gid=$gid/g" "$project/packaging/filesystem/$package.kdl" > "$package.kdl"
done
for path in lib64/ld-linux-x86-64.so.2 usr/lib/{libc.so.6,libm.so.6,libgcc_s.so.1}; do
    install -Dm755 "$original/$path" "$work/proto/usr/lib/${path##*/}"
done
for package in filesystem elf-runtime; do
    "$cps" publish manifests --repo repo --proto "$work/proto" --key "$work/release.der" "$package.kdl"
done
"$cps" image create --root image --repo repo --trust-root release.trust.json
"$cps" install --root image -y cps://canopa-usr-base/system/elf-runtime@1.0.0-1
"$cps" verify --root image --pristine
test "$(readlink image/lib64)" = usr/lib
cmp "$original/lib64/ld-linux-x86-64.so.2" image/usr/lib/ld-linux-x86-64.so.2
echo 'Verified the signed merged /usr layout and canonical ELF runtime'
