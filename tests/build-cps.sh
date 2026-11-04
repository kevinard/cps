#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
umask 022

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
source "$project/tests/build-toolchain.sh"
cps=$(realpath -- "${CPS_TEST_BINARY:-"$project/target/debug/cps"}")
output=$(realpath -m -- "${1:?usage: build-cps.sh OUTPUT-DIRECTORY [GIT-REVISION]}")
revision=$(git -C "$project" rev-parse --verify --end-of-options "${2:-HEAD}^{commit}")
epoch=$(git -C "$project" show -s --format=%ct "$revision")
registry=$(realpath -- "${CARGO_HOME:-$HOME/.cargo}/registry")
runtime_sources=$(realpath -- "${CPS_RUNTIME_SOURCE_DIR:?Set CPS_RUNTIME_SOURCE_DIR to the directory containing the pinned SQLite and libseccomp archives}")
arch=$(uname -m)
cps_version=0.1.0-3
sqlite_version=3.54.1-1
seccomp_version=2.6.1-1
runtime_version=1.0.0-1
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
# Preserve the worker's matched loader and C/GCC ABI as a separate signed
# package that the sandbox and the installed CPS image both consume.
for path in lib64/ld-linux-x86-64.so.2 usr/lib/{libc.so.6,libm.so.6,libgcc_s.so.1}; do
    install -Dm755 "$work/native-rust/$path" "$work/elf-runtime/$path"
    rm -- "$work/native-rust/$path"
done
# glibc's own executable entry point can name the canonical loader path.
ln "$work/elf-runtime/lib64/ld-linux-x86-64.so.2" "$work/elf-runtime/usr/lib/ld-linux-x86-64.so.2"
rm -f -- "$work/native-rust/usr/lib/ld-linux-x86-64.so.2"
"$cps" manifest generate --proto "$work/native-rust" --publisher seed --version 1.0.0-0 \
    --uid "$(id -u)" --gid "$(id -g)" build/native-rust > native-rust.kdl
"$cps" publish --repo toolchain --proto "$work/native-rust" --signing-key "$work/toolchain.der" native-rust.kdl
"$cps" manifest generate --proto "$work/elf-runtime" --publisher canopa-runtime-seed --version "$runtime_version" \
    --uid "$(id -u)" --gid "$(id -g)" system/elf-runtime > elf-runtime.kdl
"$cps" publish --repo runtime-seed --proto "$work/elf-runtime" --signing-key "$work/runtime-seed.der" elf-runtime.kdl

printf 'schema-version 1\n' > bootstrap-toolchain.kdl
pins="$work/publisher-pins"
append_toolchain_publisher "$pins" bootstrap-toolchain.kdl seed toolchain toolchain-public.der
append_toolchain_publisher "$pins" bootstrap-toolchain.kdl canopa-runtime-seed runtime-seed runtime-seed-public.der
toolchain_recipe="$work/toolchain-recipe"
write_toolchain_recipe() {
    {
        printf '    toolchain solver="sat-v1" {\n'
        cat "$pins"
        printf '        package "%s"\n' \
            cps://seed/build/native-rust@1.0.0-0 \
            "cps://canopa-runtime-seed/system/elf-runtime@$runtime_version" "$@"
        printf '    }\n'
    } > "$toolchain_recipe"
}
write_toolchain_recipe

fixture_manifest() {
    sed "s/uid=0 gid=0/uid=$(id -u) gid=$(id -g)/g" "$1"
}
build_bundle() {
    "$cps" build --toolchain-config "$1" --source-cache sources --output "$3" \
        --check-reproducible --builder-signing-key "$work/builder.der" "$2"
}
for library in sqlite libseccomp; do
    recipe="$project/packaging/runtime/$library"
    fixture_manifest "$recipe.kdl" > "$library.kdl"
    sed -e "s/@ARCH@/$arch/g" -e "/^@TOOLCHAIN@\$/ {
r $toolchain_recipe
d
}" \
        -e "s/@MANIFEST_SHA256@/$(sha "$library.kdl")/g" \
        "$recipe.build.kdl.in" > "$library.build.kdl"
    "$cps" source import --from "$runtime_sources" --cache sources "$library.build.kdl"
    build_bundle bootstrap-toolchain.kdl "$library.build.kdl" "$library.bundle"
done

mkdir repo
cp builder-public.der repo/
sqlite_recipe=$(sha sqlite.build.kdl)
seccomp_recipe=$(sha libseccomp.build.kdl)
write_profile() {
    local name=$1
    shift
    printf '    profile "%s" architecture="%s" {\n' "$name" "$arch"
    if [[ $name == cps-runtime ]]; then printf '        facet "devel" #false\n'; fi
    printf '        package "%s"\n' "$@"
    cat <<PROFILE
        elf-check {
            library-dir "/usr/lib"
            library-dir "/lib64"
        }
        publisher "canopa-runtime-seed" catalog="$(cat runtime-seed/current)" {
            repository "../runtime-seed"
            trust-key "../runtime-seed-public.der"
        }
    }
PROFILE
}
write_policy() {
    local reviewed=${1:-}
    {
        cat <<POLICY
schema-version 1
publisher "canopa" {
    builder-key "builder-public.der"
    approve "system/sqlite" version="$sqlite_version" sha256="$sqlite_recipe"
    approve "system/libseccomp" version="$seccomp_version" sha256="$seccomp_recipe"
POLICY
        write_profile cps-build-libs "system/sqlite@$sqlite_version" "system/libseccomp@$seccomp_version"
        if [[ -n $reviewed ]]; then
            printf '    approve "system/cps" version="%s" sha256="%s"\n' "$cps_version" "$reviewed"
            write_profile cps-runtime "system/cps@$cps_version"
        fi
        printf '}\n'
    } > repo/publish-policy.kdl
}
write_policy
cat > bootstrap-release.kdl <<RELEASE
schema-version 1
release "canopa" base="empty" {
    bundle "cps://canopa/system/sqlite@$sqlite_version" path="sqlite.bundle" builder-key="builder-public.der" recipe-sha256="$sqlite_recipe"
    bundle "cps://canopa/system/libseccomp@$seccomp_version" path="libseccomp.bundle" builder-key="builder-public.der" recipe-sha256="$seccomp_recipe"
}
RELEASE
"$cps" repo publish-release --repo repo --signing-key "$work/release.der" bootstrap-release.kdl
bootstrap_catalog=$(cat repo/current)
cp bootstrap-toolchain.kdl toolchain.kdl
append_toolchain_publisher "$pins" toolchain.kdl canopa repo release-public.der
write_toolchain_recipe "cps://canopa/system/sqlite@$sqlite_version" "cps://canopa/system/libseccomp@$seccomp_version"

fixture_manifest "$project/packaging/cps.kdl" > cps.kdl
sed -e "s/@ARCH@/$arch/g" -e "s/@EPOCH@/$epoch/g" \
    -e "/^@TOOLCHAIN@\$/ {
r $toolchain_recipe
d
}" \
    -e "s/@MANIFEST_SHA256@/$(sha cps.kdl)/g" \
    -e "s/@SOURCE_SIZE@/$(wc -c < "$work/sources/cps.tar.gz")/g" \
    -e "s/@SOURCE_SHA256@/$(sha "$work/sources/cps.tar.gz")/g" \
    -e "s/@VENDOR_SIZE@/$(wc -c < "$work/sources/vendor.tar.gz")/g" \
    -e "s/@VENDOR_SHA256@/$(sha "$work/sources/vendor.tar.gz")/g" \
    "$project/packaging/cps-build.kdl.in" > build.kdl

"$cps" source import --from "$work/sources" --cache sources build.kdl
build_bundle toolchain.kdl build.kdl bundle
reviewed=$(sha build.kdl)
write_policy "$reviewed"
cps_package=cps://canopa/system/cps@$cps_version
cat > release.kdl <<RELEASE
schema-version 1
release "canopa" base="$bootstrap_catalog" {
    bundle "$cps_package" path="bundle" builder-key="builder-public.der" recipe-sha256="$reviewed"
}
RELEASE
"$cps" repo publish-release --repo repo --signing-key "$work/release.der" release.kdl
"$cps" repo attestation --repo repo --publisher canopa --trust-key release-public.der \
    "$cps_package" > attestation.json
(cat bundle/attestation.json; printf '\n') | cmp - attestation.json
cat > image.kdl <<CONFIG
schema-version 1
release-profile "cps-runtime"
facet "devel" #false
publisher "canopa" {
    repository "repo"
    trust-key "release-public.der"
}
publisher "canopa-runtime-seed" {
    repository "runtime-seed"
    trust-key "runtime-seed-public.der"
}
CONFIG
"$cps" image-create --root image --config image.kdl
"$cps" plan --root image --output install.json install "$cps_package"
"$cps" apply --root image install.json
cmp bundle/proto/usr/bin/cps image/usr/bin/cps
test image/lib64/ld-linux-x86-64.so.2 -ef image/usr/lib/ld-linux-x86-64.so.2
for path in usr/include/{sqlite3,sqlite3ext,seccomp,seccomp-syscalls}.h \
    usr/lib/pkgconfig/{sqlite3,libseccomp}.pc usr/lib/{libsqlite3,libseccomp}.so; do
    test ! -e "image/$path"
    test ! -L "image/$path"
done
loader=(env -i "$PWD/image/lib64/ld-linux-x86-64.so.2" --inhibit-cache --library-path "$PWD/image/usr/lib")
libraries=$("${loader[@]}" --list image/usr/bin/cps)
for library in libsqlite3.so.0 libseccomp.so.2 libc.so.6 libm.so.6 libgcc_s.so.1; do
    grep -F "$library => $PWD/image/usr/lib/$library (" <<< "$libraries"
done
"${loader[@]}" image/usr/bin/cps verify --root image --pristine
printf 'Verified reproducible CPS build, signed provenance, and installation from Git %s\n' "$revision"
