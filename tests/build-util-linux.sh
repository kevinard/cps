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

# KEY [REPOSITORY PUBLISHER] creates a key pair. With a repository, the key
# also becomes the repository's 1-of-1 trust root, copied to KEY.trust.json.
keypair() {
    openssl genpkey -algorithm ED25519 -outform DER -out "$work/$1.der"
    openssl pkey -inform DER -in "$work/$1.der" -pubout -outform DER -out "$1-public.der"
    if [[ $# == 3 ]]; then
        "$cps" repo trust init --repo "$2" --publisher "$3" --key "$work/$1.der" >/dev/null
        cp -- "$2/trust/1.json" "$1.trust.json"
    fi
}
sha() { sha256sum -- "$1" | cut -d' ' -f1; }

keypair toolchain toolchain seed
keypair builder
keypair release repo canopa-util-linux
bash "$project/tests/seed-native-c.sh" "$work/native-util-linux" --util-linux
"$cps" manifest generate --proto "$work/native-util-linux" --publisher seed --version 1.0.0-0 \
    build/native-util-linux > native-util-linux.kdl
"$cps" publish manifests --repo toolchain --proto "$work/native-util-linux" \
    --key "$work/toolchain.der" native-util-linux.kdl

recipe="$project/packaging/util-linux"
sed "s/uid=0 gid=0/uid=$(id -u) gid=$(id -g)/g" "$recipe/util-linux.kdl" > util-linux.kdl
sed -e "s/@CATALOG_SHA256@/$(cat toolchain/current)/g" \
    -e "s/@KEY_SHA256@/$(sha256sum < toolchain.trust.json | cut -d' ' -f1)/g" \
    -e "s/@MANIFEST_SHA256@/$(sha util-linux.kdl)/g" "$recipe/build.kdl.in" > build.kdl
"$cps" source import --from "$sources" --cache sources build.kdl
"$cps" build --repo toolchain --trust-root toolchain.trust.json \
    --source-cache sources --output bundle --check-reproducible \
    --key "$work/builder.der" build.kdl

mkdir -p repo
cp builder-public.der repo/
reviewed=$(sha build.kdl)
cat > repo/publish-policy.kdl <<POLICY
schema-version 1 kind="publish-policy"
publisher "canopa-util-linux" {
    builder-key "builder-public.der"
    approve "system/util-linux" version="2.42.4-1" sha256="$reviewed"
}
POLICY
"$cps" publish bundle --repo repo --key "$work/release.der" --builder-key builder-public.der \
    --recipe-sha256 "$reviewed" bundle
package=cps://canopa-util-linux/system/util-linux@2.42.4-1
"$cps" repo attestation --repo repo --trust-root release.trust.json \
    "$package" > attestation.json
(cat bundle/attestation.json; printf '\n') | cmp - attestation.json
"$cps" image create --root image --repo repo --trust-root release.trust.json
"$cps" publisher add --root image --repo "$base/repo" --trust-root "$base/release.trust.json" canopa-usr-base
"$cps" install --root image -y "$package"
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
