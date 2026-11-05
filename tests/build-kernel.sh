#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
umask 022

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-"$project/target/debug/cps"}")
output=$(realpath -m -- "${1:?usage: build-kernel.sh OUTPUT-DIRECTORY}")
sources=$(realpath -- "${CPS_KERNEL_SOURCE_DIR:?Set CPS_KERNEL_SOURCE_DIR to the verified Linux source directory}")
[[ $(uname -m) == x86_64 ]] || { echo 'The QEMU kernel fixture needs an x86-64 worker' >&2; exit 1; }
if [[ -n ${CPS_KERNEL_BC:-} ]]; then
    CPS_KERNEL_BC=$(realpath -- "$CPS_KERNEL_BC")
    export CPS_KERNEL_BC
fi
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
bash "$project/tests/seed-native-c.sh" "$work/native-kernel" --kernel
"$cps" manifest generate --proto "$work/native-kernel" --publisher seed --version 1.0.0-0 \
    build/native-kernel > native-kernel.kdl
"$cps" publish --repo toolchain --proto "$work/native-kernel" --signing-key "$work/toolchain.der" native-kernel.kdl

recipe="$project/packaging/kernel-qemu"
mkdir "$work/inputs"
cp --reflink=auto -- "$sources/linux-6.12.115.tar.xz" "$recipe/vm.config" "$work/inputs/"
sed "s/uid=0 gid=0/uid=$(id -u) gid=$(id -g)/g" "$recipe/kernel.kdl" > kernel.kdl
kernel_version=$(awk -F 'version="' '/^package / {split($2, version, "\""); print version[1]}' kernel.kdl)
test -n "$kernel_version"
sed -e "s/@CATALOG_SHA256@/$(cat toolchain/current)/g" \
    -e "s/@MANIFEST_SHA256@/$(sha kernel.kdl)/g" \
    -e "s/@CONFIG_SHA256@/$(sha "$recipe/vm.config")/g" \
    -e "s/@CONFIG_SIZE@/$(wc -c < "$recipe/vm.config")/g" \
    "$recipe/build.kdl.in" > build.kdl
"$cps" source import --from "$work/inputs" --cache sources build.kdl
"$cps" build --repo toolchain --publisher seed --trust-key toolchain-public.der \
    --source-cache sources --output bundle --check-reproducible \
    --builder-signing-key "$work/builder.der" build.kdl

mkdir repo
cp builder-public.der repo/
reviewed=$(sha build.kdl)
cat > repo/publish-policy.kdl <<POLICY
schema-version 1
publisher "canopa-kernel" {
    builder-key "builder-public.der"
    approve "system/kernel-qemu" version="$kernel_version" sha256="$reviewed"
}
POLICY
"$cps" publish-bundle --repo repo --signing-key "$work/release.der" --builder-key builder-public.der \
    --recipe-sha256 "$reviewed" bundle
package=cps://canopa-kernel/system/kernel-qemu@$kernel_version
"$cps" repo attestation --repo repo --publisher canopa-kernel --trust-key release-public.der \
    "$package" > attestation.json
(cat bundle/attestation.json; printf '\n') | cmp - attestation.json
"$cps" image-create --root image --repo repo --publisher canopa-kernel --trust-key release-public.der
"$cps" plan --root image --output install.json install "$package"
"$cps" apply --root image install.json
"$cps" verify --root image --pristine
release=6.12.115-cps-vm
cmp "bundle/proto/usr/lib/modules/$release/vmlinuz" "image/usr/lib/modules/$release/vmlinuz"
printf '%s\n' "$release" > kernel-release
printf '%s\n' "$package" > kernel-package
printf 'Verified reproducible kernel build, signed provenance, and installation of Linux %s\n' "$release"
