#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
umask 022

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-"$project/target/debug/cps"}")
output=$(realpath -m -- "${1:?usage: build-systemd.sh OUTPUT-DIRECTORY}")
sources=$(realpath -- "${CPS_SYSTEMD_SOURCE_DIR:?Set CPS_SYSTEMD_SOURCE_DIR to the verified systemd archive directory}")
runtime=$(realpath -- "${CPS_SYSTEMD_CPS_FIXTURE:?Set CPS_SYSTEMD_CPS_FIXTURE to the source-built CPS fixture}")
CPS_SYSTEMD_TOOLS_ROOT=$(realpath -- "${CPS_SYSTEMD_TOOLS_ROOT:-/}")
export CPS_SYSTEMD_TOOLS_ROOT
[[ $(uname -m) == x86_64 ]] || { echo 'The systemd VM fixture needs an x86-64 worker' >&2; exit 1; }
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
keypair runtime-seed
"$cps" repo attestation --repo "$runtime/repo" --publisher canopa --trust-key "$runtime/release-public.der" \
    cps://canopa/system/libseccomp@2.6.1-0 >/dev/null
"$cps" verify --root "$runtime/image" --pristine
bash "$project/tests/seed-native-c.sh" "$work/native-systemd" --systemd
install -Dm755 "$runtime/image/usr/lib/libseccomp.so.2" "$work/native-systemd/usr/lib/libseccomp.so.2"
ln -s libseccomp.so.2 "$work/native-systemd/usr/lib/libseccomp.so"
"$cps" manifest generate --proto "$work/native-systemd" --publisher seed --version 1.0.0-0 \
    build/native-systemd > native-systemd.kdl
"$cps" publish --repo toolchain --proto "$work/native-systemd" --signing-key "$work/toolchain.der" native-systemd.kdl

recipe="$project/packaging/systemd"
sed "s/uid=0 gid=0/uid=$(id -u) gid=$(id -g)/g" "$recipe/vm.kdl" > systemd.kdl
sed -e "s/@CATALOG_SHA256@/$(cat toolchain/current)/g" \
    -e "s/@MANIFEST_SHA256@/$(sha systemd.kdl)/g" "$recipe/vm-build.kdl.in" > build.kdl
"$cps" source import --from "$sources" --cache sources build.kdl
"$cps" build --repo toolchain --publisher seed --trust-key toolchain-public.der \
    --source-cache sources --output bundle --check-reproducible \
    --builder-signing-key "$work/builder.der" build.kdl

for library in libblkid.so.1 libmount.so.1; do
    install -Dm755 "$work/native-systemd/usr/lib/$library" "$work/runtime-seed/usr/lib/$library"
done
"$cps" manifest generate --proto "$work/runtime-seed" --publisher canopa-systemd-seed --version 1.0.0-0 \
    --uid "$(id -u)" --gid "$(id -g)" system/systemd-vm-libs > runtime-seed.kdl
"$cps" publish --repo runtime-seed --proto "$work/runtime-seed" --signing-key "$work/runtime-seed.der" runtime-seed.kdl

mkdir repo
cp builder-public.der repo/
reviewed=$(sha build.kdl)
cat > repo/publish-policy.kdl <<POLICY
schema-version 1
publisher "canopa-systemd" {
    builder-key "builder-public.der"
    approve "system/systemd-vm" version="262.1.0-0" sha256="$reviewed"
}
POLICY
"$cps" publish-bundle --repo repo --signing-key "$work/release.der" --builder-key builder-public.der \
    --recipe-sha256 "$reviewed" bundle
package=cps://canopa-systemd/system/systemd-vm@262.1.0-0
"$cps" repo attestation --repo repo --publisher canopa-systemd --trust-key release-public.der "$package" > attestation.json
(cat bundle/attestation.json; printf '\n') | cmp - attestation.json
"$cps" image-create --root image --repo repo --publisher canopa-systemd --trust-key release-public.der
"$cps" publisher add --root image --repo runtime-seed --trust-key runtime-seed-public.der canopa-systemd-seed
"$cps" publisher add --root image --repo "$runtime/repo" --trust-key "$runtime/release-public.der" canopa
"$cps" publisher add --root image --repo "$runtime/runtime-seed" \
    --trust-key "$runtime/runtime-seed-public.der" canopa-runtime-seed
"$cps" plan --root image --output install.json install "$package"
"$cps" apply --root image install.json
"$cps" verify --root image --pristine
cmp bundle/proto/usr/lib/systemd/systemd image/usr/lib/systemd/systemd
cmp bundle/proto/usr/lib/systemd/boot/efi/systemd-bootx64.efi image/usr/lib/systemd/boot/efi/systemd-bootx64.efi
loader=(env -i "$PWD/image/lib64/ld-linux-x86-64.so.2" --inhibit-cache --library-path "$PWD/image/usr/lib:$PWD/image/usr/lib/systemd")
for program in usr/lib/systemd/systemd usr/lib/systemd/systemd-executor usr/lib/systemd/systemd-bless-boot usr/bin/systemctl; do
    links=$("${loader[@]}" --list "image/$program")
    awk -v prefix="$PWD/image/" '$2 == "=>" && index($3, prefix) != 1 {print; exit 1}' <<< "$links"
done
version=$("${loader[@]}" image/usr/lib/systemd/systemd --version)
grep -Fx 'systemd 262 (v262.1)' <<< "$version"
printf 'Verified reproducible systemd and systemd-boot build, signed provenance, and installation\n'
