#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
umask 022

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-"$project/target/debug/cps"}")
output=$(realpath -m -- "${1:?usage: build-cps.sh OUTPUT-DIRECTORY [GIT-REVISION]}")
revision=$(git -C "$project" rev-parse --verify --end-of-options "${2:-HEAD}^{commit}")
epoch=$(git -C "$project" show -s --format=%ct "$revision")
registry=$(realpath -- "${CARGO_HOME:-$HOME/.cargo}/registry")
mkdir -m 700 -- "$output"
work=$(mktemp -d "$output/.private.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
cd -- "$output"

keypair() {
    openssl genpkey -algorithm ED25519 -outform DER -out "$work/$1.der"
    openssl pkey -inform DER -in "$work/$1.der" -pubout -outform DER -out "$1-public.der"
}
sha() { sha256sum -- "$1" | cut -d' ' -f1; }

mkdir "$work/sources"
git -C "$project" archive --format=tar --prefix=cps/ "$revision" | gzip -n > "$work/sources/cps.tar.gz"
tar -xzf "$work/sources/cps.tar.gz" -C "$work"
printf '%s\n' "$revision" > source-revision
python3 "$project/tests/seed-cargo-cache.py" "$work/cps/Cargo.lock" "$registry" "$work/cargo-home"
(
    cd "$work/cps"
    CARGO_HOME="$work/cargo-home" CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse \
        cargo vendor --quiet --frozen --versioned-dirs "$work/vendor" > /dev/null
)
tar --sort=name --format=gnu --mtime="@$epoch" --owner=0 --group=0 --numeric-owner \
    -C "$work" -cf - vendor | gzip -n > "$work/sources/vendor.tar.gz"

keypair toolchain
keypair builder
keypair release
bash "$project/tests/seed-native-c.sh" "$work/native-rust" --rust
"$cps" manifest generate --proto "$work/native-rust" --publisher seed --version 1.0.0-0 \
    build/native-rust > native-rust.kdl
"$cps" publish --repo toolchain --proto "$work/native-rust" --signing-key "$work/toolchain.der" native-rust.kdl

sed "s/uid=0 gid=0/uid=$(id -u) gid=$(id -g)/g" "$work/cps/packaging/cps.kdl" > cps.kdl
sed -e "s/@ARCH@/$(uname -m)/g" -e "s/@EPOCH@/$epoch/g" \
    -e "s/@CATALOG_SHA256@/$(cat toolchain/current)/g" \
    -e "s/@MANIFEST_SHA256@/$(sha cps.kdl)/g" \
    -e "s/@SOURCE_SIZE@/$(wc -c < "$work/sources/cps.tar.gz")/g" \
    -e "s/@SOURCE_SHA256@/$(sha "$work/sources/cps.tar.gz")/g" \
    -e "s/@VENDOR_SIZE@/$(wc -c < "$work/sources/vendor.tar.gz")/g" \
    -e "s/@VENDOR_SHA256@/$(sha "$work/sources/vendor.tar.gz")/g" \
    "$project/packaging/cps-build.kdl.in" > build.kdl

"$cps" source import --from "$work/sources" --cache sources build.kdl
"$cps" build --repo toolchain --publisher seed --trust-key toolchain-public.der \
    --source-cache sources --output bundle --check-reproducible \
    --builder-signing-key "$work/builder.der" build.kdl

mkdir repo
cp builder-public.der repo/
reviewed=$(sha build.kdl)
cat > repo/publish-policy.kdl <<POLICY
schema-version 1
publisher "canopa" {
    builder-key "builder-public.der"
    approve "system/cps" version="0.1.0-0" sha256="$reviewed"
}
POLICY
"$cps" publish-bundle --repo repo --signing-key "$work/release.der" --builder-key builder-public.der \
    --recipe-sha256 "$reviewed" bundle
"$cps" repo attestation --repo repo --publisher canopa --trust-key release-public.der \
    cps://canopa/system/cps@0.1.0-0 > attestation.json
(cat bundle/attestation.json; printf '\n') | cmp - attestation.json
"$cps" image-create --root image --repo repo --publisher canopa --trust-key release-public.der
"$cps" plan --root image --output install.json install cps://canopa/system/cps@0.1.0-0
"$cps" apply --root image install.json
cmp bundle/proto/usr/bin/cps image/usr/bin/cps
image/usr/bin/cps verify --root image --pristine
printf 'Verified reproducible CPS build, signed provenance, and installation from Git %s\n' "$revision"
