#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
umask 022

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-"$project/target/debug/cps"}")
output=$(realpath -m -- "${1:?usage: build-util-linux.sh OUTPUT-DIRECTORY USR-BASE-FIXTURE}")
base=$(realpath -- "${2:?usage: build-util-linux.sh OUTPUT-DIRECTORY USR-BASE-FIXTURE}")
sources=$(realpath -- "${CPS_UTIL_LINUX_SOURCE_DIR:?Set CPS_UTIL_LINUX_SOURCE_DIR to the verified util-linux archive directory}")
[[ $(uname -m) == x86_64 ]] || { echo 'The util-linux fixture needs an x86-64 worker' >&2; exit 1; }
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
bash "$project/tests/seed-native-c.sh" "$work/native-util-linux" --util-linux
"$cps" manifest generate --proto "$work/native-util-linux" --publisher seed --version 1.0.0-0 \
    build/native-util-linux > native-util-linux.kdl
"$cps" publish --repo toolchain --proto "$work/native-util-linux" \
    --signing-key "$work/toolchain.der" native-util-linux.kdl

recipe="$project/packaging/util-linux"
sed "s/uid=0 gid=0/uid=$(id -u) gid=$(id -g)/g" "$recipe/util-linux.kdl" > util-linux.kdl
sed -e "s/@CATALOG_SHA256@/$(cat toolchain/current)/g" \
    -e "s/@KEY_SHA256@/$(tail -c 32 -- toolchain-public.der | sha256sum | cut -d' ' -f1)/g" \
    -e "s/@MANIFEST_SHA256@/$(sha util-linux.kdl)/g" "$recipe/build.kdl.in" > build.kdl
"$cps" source import --from "$sources" --cache sources build.kdl
"$cps" build --repo toolchain --publisher seed --trust-key toolchain-public.der \
    --source-cache sources --output bundle --check-reproducible \
    --builder-signing-key "$work/builder.der" build.kdl

mkdir repo
cp builder-public.der repo/
reviewed=$(sha build.kdl)
cat > repo/publish-policy.kdl <<POLICY
schema-version 1 kind="publish-policy"
publisher "canopa-util-linux" {
    builder-key "builder-public.der"
    approve "system/util-linux" version="2.42.4-1" sha256="$reviewed"
}
POLICY
"$cps" publish-bundle --repo repo --signing-key "$work/release.der" --builder-key builder-public.der \
    --recipe-sha256 "$reviewed" bundle
package=cps://canopa-util-linux/system/util-linux@2.42.4-1
"$cps" repo attestation --repo repo --publisher canopa-util-linux --trust-key release-public.der \
    "$package" > attestation.json
(cat bundle/attestation.json; printf '\n') | cmp - attestation.json
"$cps" image-create --root image --repo repo --publisher canopa-util-linux --trust-key release-public.der
"$cps" publisher add --root image --repo "$base/repo" --trust-key "$base/release-public.der" canopa-usr-base
"$cps" plan --root image --output install.json install "$package"
"$cps" apply --root image install.json
"$cps" verify --root image --pristine
loader=(env -i "$PWD/image/usr/lib/ld-linux-x86-64.so.2" --inhibit-cache --library-path "$PWD/image/usr/lib")
for program in blkid mount umount sfdisk; do
    cmp "bundle/proto/usr/bin/$program" "image/usr/bin/$program"
    links=$("${loader[@]}" --list "image/usr/bin/$program")
    awk -v prefix="$PWD/image/" '$2 == "=>" && index($3, prefix) != 1 {print; exit 1}' <<< "$links"
    version=$("${loader[@]}" "image/usr/bin/$program" --version)
    [[ $version == "$program from util-linux 2.42.4"* ]]
done
echo 'Verified reproducible util-linux build, signed provenance, and installation'
