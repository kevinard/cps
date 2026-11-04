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
runtime_sources=$(realpath -- "${CPS_RUNTIME_SOURCE_DIR:?Set CPS_RUNTIME_SOURCE_DIR to the directory containing the pinned SQLite and libseccomp archives}")
arch=$(uname -m)
[[ $arch == x86_64 ]] || { echo 'The ELF runtime seed requires an x86-64 worker' >&2; exit 1; }
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
keypair runtime-seed
bash "$project/tests/seed-native-c.sh" "$work/native-rust" --rust
"$cps" manifest generate --proto "$work/native-rust" --publisher seed --version 1.0.0-0 \
    build/native-rust > native-rust.kdl
"$cps" publish --repo toolchain --proto "$work/native-rust" --signing-key "$work/toolchain.der" native-rust.kdl

fixture_manifest() {
    sed "s/uid=0 gid=0/uid=$(id -u) gid=$(id -g)/g" "$1"
}
build_bundle() {
    "$cps" build --repo toolchain --publisher seed --trust-key toolchain-public.der \
        --source-cache sources --output "$2" --check-reproducible \
        --builder-signing-key "$work/builder.der" "$1"
}
for library in sqlite libseccomp; do
    recipe="$project/packaging/runtime/$library"
    fixture_manifest "$recipe.kdl" > "$library.kdl"
    sed -e "s/@ARCH@/$arch/g" -e "s/@CATALOG_SHA256@/$(cat toolchain/current)/g" \
        -e "s/@MANIFEST_SHA256@/$(sha "$library.kdl")/g" \
        "$recipe.build.kdl.in" > "$library.build.kdl"
    "$cps" source import --from "$runtime_sources" --cache sources "$library.build.kdl"
    build_bundle "$library.build.kdl" "$library.bundle"
done

linker="$work/cps-libraries/usr/lib"
mkdir -p "$linker/pkgconfig"
cp sqlite.bundle/proto/usr/lib/libsqlite3.so.0 libseccomp.bundle/proto/usr/lib/libseccomp.so.2 "$linker/"
ln -s libsqlite3.so.0 "$linker/libsqlite3.so"
ln -s libseccomp.so.2 "$linker/libseccomp.so"
cat > "$linker/pkgconfig/sqlite3.pc" <<'PC'
Name: SQLite
Description: SQLite library built by CPS
Version: 3.54.1
Libs: -L/usr/lib -lsqlite3
PC
"$cps" manifest generate --proto "$work/cps-libraries" --publisher seed --version 1.0.0-0 \
    build/cps-libraries > cps-libraries.kdl
"$cps" publish --repo toolchain --proto "$work/cps-libraries" --signing-key "$work/toolchain.der" cps-libraries.kdl

fixture_manifest "$project/packaging/cps.kdl" > cps.kdl
sed -e "s/@ARCH@/$arch/g" -e "s/@EPOCH@/$epoch/g" \
    -e "s/@CATALOG_SHA256@/$(cat toolchain/current)/g" \
    -e "s/@MANIFEST_SHA256@/$(sha cps.kdl)/g" \
    -e "s/@SOURCE_SIZE@/$(wc -c < "$work/sources/cps.tar.gz")/g" \
    -e "s/@SOURCE_SHA256@/$(sha "$work/sources/cps.tar.gz")/g" \
    -e "s/@VENDOR_SIZE@/$(wc -c < "$work/sources/vendor.tar.gz")/g" \
    -e "s/@VENDOR_SHA256@/$(sha "$work/sources/vendor.tar.gz")/g" \
    "$project/packaging/cps-build.kdl.in" > build.kdl

"$cps" source import --from "$work/sources" --cache sources build.kdl
build_bundle build.kdl bundle

# Capture the matching loader and C/GCC ABI for this bootstrap fixture.
for path in lib64/ld-linux-x86-64.so.2 usr/lib/{libc.so.6,libm.so.6,libgcc_s.so.1}; do
    install -Dm755 "/$path" "$work/elf-runtime/$path"
done
"$cps" manifest generate --proto "$work/elf-runtime" --publisher canopa-runtime-seed --version 1.0.0-0 \
    --uid "$(id -u)" --gid "$(id -g)" system/elf-runtime > elf-runtime.kdl
"$cps" publish --repo runtime-seed --proto "$work/elf-runtime" --signing-key "$work/runtime-seed.der" elf-runtime.kdl

mkdir repo
cp builder-public.der repo/
reviewed=$(sha build.kdl)
cat > repo/publish-policy.kdl <<POLICY
schema-version 1
publisher "canopa" {
    builder-key "builder-public.der"
    approve "system/cps" version="0.1.0-1" sha256="$reviewed"
    approve "system/sqlite" version="3.54.1-0" sha256="$(sha sqlite.build.kdl)"
    approve "system/libseccomp" version="2.6.1-0" sha256="$(sha libseccomp.build.kdl)"
}
POLICY
for library in sqlite libseccomp; do
    "$cps" publish-bundle --repo repo --signing-key "$work/release.der" --builder-key builder-public.der \
        --recipe-sha256 "$(sha "$library.build.kdl")" "$library.bundle"
done
"$cps" publish-bundle --repo repo --signing-key "$work/release.der" --builder-key builder-public.der \
    --recipe-sha256 "$reviewed" bundle
"$cps" repo attestation --repo repo --publisher canopa --trust-key release-public.der \
    cps://canopa/system/cps@0.1.0-1 > attestation.json
(cat bundle/attestation.json; printf '\n') | cmp - attestation.json
"$cps" image-create --root image --repo repo --publisher canopa --trust-key release-public.der
"$cps" publisher add --root image --repo runtime-seed --trust-key runtime-seed-public.der canopa-runtime-seed
"$cps" plan --root image --output install.json install cps://canopa/system/cps@0.1.0-1
"$cps" apply --root image install.json
cmp bundle/proto/usr/bin/cps image/usr/bin/cps
loader=(env -i "$PWD/image/lib64/ld-linux-x86-64.so.2" --inhibit-cache --library-path "$PWD/image/usr/lib")
libraries=$("${loader[@]}" --list image/usr/bin/cps)
for library in libsqlite3.so.0 libseccomp.so.2 libc.so.6 libm.so.6 libgcc_s.so.1; do
    grep -F "$library => $PWD/image/usr/lib/$library (" <<< "$libraries"
done
"${loader[@]}" image/usr/bin/cps verify --root image --pristine
printf 'Verified reproducible CPS build, signed provenance, and installation from Git %s\n' "$revision"
