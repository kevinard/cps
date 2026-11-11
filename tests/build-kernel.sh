#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
umask 022

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-"$project/target/debug/cps"}")
output=$(realpath -m -- "${1:?usage: build-kernel.sh OUTPUT-DIRECTORY}")
sources=$(realpath -- "${CPS_KERNEL_SOURCE_DIR:?Set CPS_KERNEL_SOURCE_DIR to the verified Linux source directory}")
manifest=kernel.kdl
release=6.12.115-cps-vm
localversion=-cps-vm
case ${CPS_KERNEL_VARIANT:-base} in
    base) ;;
    next) manifest=kernel-next.kdl; release=6.12.115-cps-vm-next; localversion=-cps-vm-next ;;
    *) echo 'CPS_KERNEL_VARIANT must be base or next' >&2; exit 2 ;;
esac
[[ $(uname -m) == x86_64 ]] || { echo 'The QEMU kernel fixture needs an x86-64 worker' >&2; exit 1; }
if [[ -n ${CPS_KERNEL_BC:-} ]]; then
    CPS_KERNEL_BC=$(realpath -- "$CPS_KERNEL_BC")
    export CPS_KERNEL_BC
fi
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
keypair release repo canopa-kernel
bash "$project/tests/seed-native-c.sh" "$work/native-kernel" --kernel
"$cps" manifest generate --proto "$work/native-kernel" --publisher seed --version 1.0.0-0 \
    build/native-kernel > native-kernel.kdl
"$cps" publish manifests --repo toolchain --proto "$work/native-kernel" --key "$work/toolchain.der" native-kernel.kdl

recipe="$project/packaging/kernel-qemu"
mkdir "$work/inputs"
cp --reflink=auto -- "$sources/linux-6.12.115.tar.xz" "$recipe/vm.config" "$work/inputs/"
if [[ ${CPS_KERNEL_VARIANT:-base} == next ]]; then
    printf 'CONFIG_HZ_1000=y\n' >> "$work/inputs/vm.config"
fi
sed "s/uid=0 gid=0/uid=$(id -u) gid=$(id -g)/g" "$recipe/$manifest" > kernel.kdl
kernel_version=$(awk -F 'version="' '/^package / {split($2, version, "\""); print version[1]}' kernel.kdl)
test -n "$kernel_version"
sed -e "s/@CATALOG_SHA256@/$(cat toolchain/current)/g" \
    -e "s/@KEY_SHA256@/$(sha256sum < toolchain.trust.json | cut -d' ' -f1)/g" \
    -e "s/@MANIFEST_SHA256@/$(sha kernel.kdl)/g" \
    -e "s/@CONFIG_SHA256@/$(sha "$work/inputs/vm.config")/g" \
    -e "s/@CONFIG_SIZE@/$(wc -c < "$work/inputs/vm.config")/g" \
    -e "s/@LOCALVERSION@/$localversion/g" -e "s/@KERNEL_RELEASE@/$release/g" \
    "$recipe/build.kdl.in" > build.kdl
"$cps" source import --from "$work/inputs" --cache sources build.kdl
"$cps" build --repo toolchain --trust-root toolchain.trust.json \
    --source-cache sources --output bundle --check-reproducible \
    --key "$work/builder.der" build.kdl

mkdir -p repo
cp builder-public.der repo/
reviewed=$(sha build.kdl)
cat > repo/publish-policy.kdl <<POLICY
schema-version 1 kind="publish-policy"
publisher "canopa-kernel" {
    builder-key "builder-public.der"
    approve "system/kernel-qemu" version="$kernel_version" sha256="$reviewed"
}
POLICY
"$cps" publish bundle --repo repo --key "$work/release.der" --builder-key builder-public.der \
    --recipe-sha256 "$reviewed" bundle
package=cps://canopa-kernel/system/kernel-qemu@$kernel_version
"$cps" repo attestation --repo repo --trust-root release.trust.json \
    "$package" > attestation.json
(cat bundle/attestation.json; printf '\n') | cmp - attestation.json
"$cps" image create --root image --repo repo --trust-root release.trust.json
"$cps" install --root image -y "$package"
"$cps" verify --root image --pristine
cmp "bundle/proto/usr/lib/modules/$release/vmlinuz" "image/usr/lib/modules/$release/vmlinuz"
printf '%s\n' "$release" > kernel-release
printf '%s\n' "$package" > kernel-package
printf 'Verified reproducible kernel build, signed provenance, and installation of Linux %s\n' "$release"
