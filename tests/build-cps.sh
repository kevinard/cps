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

mkdir "$work/sources"
git -C "$project" archive --format=tar --prefix=cps/ "$revision" | gzip -n > "$work/sources/cps.tar.gz"
tar -xzf "$work/sources/cps.tar.gz" -C "$work"
source_tree="$work/cps"
source "$source_tree/tests/build-toolchain.sh"
# A recipe's version node, or the package node's version in a manifest.
manifest_version() {
    local version
    version=$(sed -n -e 's/^package .* version="\([^"]*\)".*/\1/p' \
        -e 's/^    version "\([^"]*\)".*/\1/p' "$1")
    [[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+-[0-9]+$ ]] || {
        echo "Could not determine the package version in $1" >&2
        return 1
    }
    printf '%s\n' "$version"
}
cps_manifest="$source_tree/packaging/cps.kdl"
cps_version=$(manifest_version "$cps_manifest")
trust_manifest="$source_tree/packaging/canopa-trust.kdl.in"
git_build_template="$source_tree/packaging/git-build.build.kdl.in"
git_build_version=
trust_version=
if [[ -f $trust_manifest ]]; then
    root=${CPS_MIRROR_TUF_ROOT:?Set CPS_MIRROR_TUF_ROOT to the reviewed public TUF root JSON}
    [[ -f $root ]] || { echo 'Expected a public TUF root file' >&2; exit 1; }
    trust_root="$work/sources/tuf-root.json"
    head -c 1048577 -- "$root" > "$trust_root"
    root_size=$(wc -c < "$trust_root")
    (( root_size <= 1048576 )) || { echo 'TUF root exceeds the CPS size limit' >&2; exit 1; }
    root_version=$(jq -ser '
        select(length == 1) | .[0]
        | select(.signed._type == "root" and .signed.consistent_snapshot == true
                 and (.signatures | type == "array" and length > 0))
        | .signed.version | select(type == "number")
    ' "$trust_root") && [[ $root_version =~ ^[1-9][0-9]{0,14}$ ]] || {
        echo 'Expected signed TUF root metadata with a valid version and consistent snapshots' >&2
        exit 1
    }
    trust_version="1.0.0-$root_version"
fi
cd -- "$output"
printf '%s\n' "$revision" > source-revision
sqlite_version=$(manifest_version "$source_tree/packaging/runtime/sqlite.build.kdl.in")
seccomp_version=$(manifest_version "$source_tree/packaging/runtime/libseccomp.kdl")
seed_version=1.0.0-0
runtime_version=1.0.0-1
seed_arch=()
if grep -q '^package .* architecture=' "$cps_manifest"; then
    seed_version=1.0.0-1
    runtime_version=1.0.0-2
    seed_arch=(--arch "$arch")
fi
python3 "$source_tree/tests/seed-cargo-cache.py" "$source_tree/Cargo.lock" "$registry" "$work/cargo-home"
(
    cd "$source_tree"
    CARGO_HOME="$work/cargo-home" CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse \
        cargo vendor --quiet --frozen --versioned-dirs "$work/vendor" > /dev/null
)
tar --sort=name --format=gnu --mtime="@$epoch" --owner=0 --group=0 --numeric-owner \
    -C "$work" -cf - vendor | gzip -n > "$work/sources/vendor.tar.gz"

keypair toolchain toolchain seed
keypair builder
keypair release repo canopa
keypair runtime-seed runtime-seed canopa-runtime-seed
bash "$source_tree/tests/seed-native-c.sh" "$work/native-rust" --rust
# Preserve the worker's matched loader and C/GCC ABI as a separate signed
# package that the sandbox and the installed CPS image both consume.
for path in lib64/ld-linux-x86-64.so.2 usr/lib/{libc.so.6,libm.so.6,libgcc_s.so.1}; do
    install -Dm755 "$work/native-rust/$path" "$work/elf-runtime/$path"
    rm -- "$work/native-rust/$path"
done
# glibc's own executable entry point can name the canonical loader path.
ln "$work/elf-runtime/lib64/ld-linux-x86-64.so.2" "$work/elf-runtime/usr/lib/ld-linux-x86-64.so.2"
rm -f -- "$work/native-rust/usr/lib/ld-linux-x86-64.so.2"
"$cps" manifest generate --proto "$work/native-rust" --publisher seed --version "$seed_version" "${seed_arch[@]}" \
    --uid "$(id -u)" --gid "$(id -g)" build/native-rust > native-rust.kdl
"$cps" publish manifests --repo toolchain --proto "$work/native-rust" --key "$work/toolchain.der" native-rust.kdl
"$cps" manifest generate --proto "$work/elf-runtime" --publisher canopa-runtime-seed --version "$runtime_version" "${seed_arch[@]}" \
    --uid "$(id -u)" --gid "$(id -g)" system/elf-runtime > elf-runtime.kdl
"$cps" publish manifests --repo runtime-seed --proto "$work/elf-runtime" --key "$work/runtime-seed.der" elf-runtime.kdl

printf 'schema-version 1 kind="toolchain-config"\n' > bootstrap-toolchain.kdl
pins="$work/publisher-pins"
append_toolchain_publisher "$pins" bootstrap-toolchain.kdl seed toolchain toolchain.trust.json
append_toolchain_publisher "$pins" bootstrap-toolchain.kdl canopa-runtime-seed runtime-seed runtime-seed.trust.json
toolchain_recipe="$work/toolchain-recipe"
write_toolchain_recipe() {
    {
        printf '    toolchain solver="sat-v2" {\n'
        cat "$pins"
        printf '        package "%s"\n' \
            "cps://seed/build/native-rust@$seed_version" \
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
        --check-reproducible --key "$work/builder.der" "$2"
}
if [[ -n $trust_version ]]; then
    fixture_manifest "$trust_manifest" | sed "s/@ROOT_VERSION@/$root_version/" > canopa-trust.kdl
    sed -e "s/@ARCH@/$arch/g" -e "/^@TOOLCHAIN@\$/ {
r $toolchain_recipe
d
}" \
        -e "s/@MANIFEST_SHA256@/$(sha canopa-trust.kdl)/g" \
        -e "s/@ROOT_SIZE@/$root_size/g" -e "s/@ROOT_SHA256@/$(sha "$trust_root")/g" \
        "$source_tree/packaging/canopa-trust.build.kdl.in" > canopa-trust.build.kdl
    "$cps" source import --from "$work/sources" --cache sources canopa-trust.build.kdl
    build_bundle bootstrap-toolchain.kdl canopa-trust.build.kdl canopa-trust.bundle
    trust_recipe=$(sha canopa-trust.build.kdl)
fi
if [[ -f $git_build_template ]]; then
    git_build_version=1.0.0-0
    keypair git-build
    git_build_proto="$work/git-build-proto"
    bash "$source_tree/tests/seed-git-build.sh" "$cps" "$git_build_proto" \
        "$work/git-build.der"
    "$cps" manifest generate --proto "$git_build_proto" --publisher canopa --version "$git_build_version" \
        --arch "$arch" --uid "$(id -u)" --gid "$(id -g)" system/git-build > git-build.kdl
    git_build_archive="$work/sources/git-build.tar.gz"
    tar --sort=name --format=gnu --mtime=@0 --owner=0 --group=0 --numeric-owner \
        -C "$git_build_proto" -cf - . | gzip -n > "$git_build_archive"
    sed -e "s/@ARCH@/$arch/g" -e "/^@TOOLCHAIN@\$/ {
r $toolchain_recipe
d
}" \
        -e "s/@MANIFEST_SHA256@/$(sha git-build.kdl)/g" \
        -e "s/@SOURCE_SIZE@/$(wc -c < "$git_build_archive")/g" \
        -e "s/@SOURCE_SHA256@/$(sha "$git_build_archive")/g" \
        "$git_build_template" > git-build.build.kdl
    "$cps" source import --from "$work/sources" --cache sources git-build.build.kdl
    build_bundle bootstrap-toolchain.kdl git-build.build.kdl git-build.bundle
    git_build_recipe=$(sha git-build.build.kdl)
fi
for library in sqlite libseccomp; do
    recipe="$source_tree/packaging/runtime/$library"
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

mkdir -p repo
cp builder-public.der repo/
sqlite_recipe=$(sha sqlite.build.kdl)
seccomp_recipe=$(sha libseccomp.build.kdl)
review_bootstrap=' toolchain-config="../bootstrap-toolchain.kdl"'
review_cps=' toolchain-config="../toolchain.kdl"'
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
            trust-root "../runtime-seed.trust.json"
        }
    }
PROFILE
}
runtime_packages=("system/cps@$cps_version")
if [[ -n $trust_version ]]; then runtime_packages+=("system/canopa-trust@$trust_version"); fi
if [[ -n $git_build_version ]]; then runtime_packages+=("system/git-build@$git_build_version"); fi
write_policy() {
    local reviewed=${1:-}
    {
        cat <<POLICY
schema-version 1 kind="publish-policy"
publisher "canopa" verify-toolchains=#true archive-build-inputs=#true require-source-archives=#true {
    builder-key "builder-public.der"
    approve "system/sqlite" version="$sqlite_version" sha256="$sqlite_recipe"$review_bootstrap
    approve "system/libseccomp" version="$seccomp_version" sha256="$seccomp_recipe"$review_bootstrap
POLICY
        write_profile cps-build-libs "system/sqlite@$sqlite_version" "system/libseccomp@$seccomp_version"
        if [[ -n $reviewed ]]; then
            printf '    approve "system/cps" version="%s" sha256="%s"%s\n' "$cps_version" "$reviewed" "$review_cps"
            if [[ -n $trust_version ]]; then
                printf '    approve "system/canopa-trust" version="%s" sha256="%s"%s\n' \
                    "$trust_version" "$trust_recipe" "$review_bootstrap"
            fi
            if [[ -n $git_build_version ]]; then
                printf '    approve "system/git-build" version="%s" sha256="%s"%s\n' \
                    "$git_build_version" "$git_build_recipe" "$review_bootstrap"
            fi
            write_profile cps-runtime "${runtime_packages[@]}"
        fi
        printf '}\n'
    } > repo/publish-policy.kdl
}
write_policy
cat > bootstrap-release.kdl <<RELEASE
schema-version 1 kind="publish-release"
release "canopa" base="empty" {
    bundle "cps://canopa/system/sqlite@$sqlite_version" path="sqlite.bundle" builder-key="builder-public.der" recipe-sha256="$sqlite_recipe"
    bundle "cps://canopa/system/libseccomp@$seccomp_version" path="libseccomp.bundle" builder-key="builder-public.der" recipe-sha256="$seccomp_recipe"
}
RELEASE
"$cps" publish release --repo repo --key "$work/release.der" \
    --source-cache sources bootstrap-release.kdl
bootstrap_catalog=$(cat repo/current)
cp bootstrap-toolchain.kdl toolchain.kdl
append_toolchain_publisher "$pins" toolchain.kdl canopa repo release.trust.json
write_toolchain_recipe "cps://canopa/system/sqlite@$sqlite_version" "cps://canopa/system/libseccomp@$seccomp_version"

fixture_manifest "$cps_manifest" > cps.kdl
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
    "$source_tree/packaging/cps-build.kdl.in" > build.kdl

"$cps" source import --from "$work/sources" --cache sources build.kdl
build_bundle toolchain.kdl build.kdl bundle
reviewed=$(sha build.kdl)
write_policy "$reviewed"
cps_package=cps://canopa/system/cps@$cps_version
final_packages=("$cps_package")
{
    cat <<RELEASE
schema-version 1 kind="publish-release"
release "canopa" base="$bootstrap_catalog" {
    bundle "$cps_package" path="bundle" builder-key="builder-public.der" recipe-sha256="$reviewed"
RELEASE
    if [[ -n $trust_version ]]; then
        trust_package="cps://canopa/system/canopa-trust@$trust_version"
        printf '    bundle "%s" path="canopa-trust.bundle" builder-key="builder-public.der" recipe-sha256="%s"\n' \
            "$trust_package" "$trust_recipe"
        final_packages+=("$trust_package")
    fi
    if [[ -n $git_build_version ]]; then
        git_build_package="cps://canopa/system/git-build@$git_build_version"
        printf '    bundle "%s" path="git-build.bundle" builder-key="builder-public.der" recipe-sha256="%s"\n' \
            "$git_build_package" "$git_build_recipe"
        final_packages+=("$git_build_package")
    fi
    printf '}\n'
} > release.kdl
"$cps" publish release --repo repo --key "$work/release.der" \
    --source-cache sources release.kdl
"$cps" repo attestation --repo repo --trust-root release.trust.json \
    "$cps_package" > attestation.json
(cat bundle/attestation.json; printf '\n') | cmp - attestation.json
if [[ -n $archive_build_inputs ]]; then
    recovered=$("$cps" repo rebuild-inputs --repo repo --trust-root release.trust.json \
        --output reviewed-cps "$cps_package")
    cmp build.kdl "$recovered"
    cmp cps.kdl "$(dirname -- "$recovered")/cps.kdl"
    cmp bundle/attestation.json reviewed-cps/attestation.json
fi
cat > image.kdl <<CONFIG
schema-version 1 kind="image-config"
release-profile "cps-runtime"
facet "devel" #false
publisher "canopa" {
    repository "repo"
    trust-root "release.trust.json"
}
publisher "canopa-runtime-seed" {
    repository "runtime-seed"
    trust-root "runtime-seed.trust.json"
}
CONFIG
"$cps" image create --root image --config image.kdl
if [[ -n $trust_version || -n $git_build_version ]]; then
    "$cps" install --root image --profile -n --save-plan install.json
else
    "$cps" install --root image -n --save-plan install.json "$cps_package"
fi
"$cps" apply --root image install.json
cmp bundle/proto/usr/bin/cps image/usr/bin/cps
if [[ -n $trust_version ]]; then
    cmp "$trust_root" image/etc/cps/tuf-root.json
    test "$(stat -c %a image/etc/cps/tuf-root.json)" = 644
fi
if [[ -n $git_build_version ]]; then
    diff -r "$git_build_proto/usr/share/cps/git-build" image/usr/share/cps/git-build
fi
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
"${loader[@]}" image/usr/bin/cps repo verify-rebuild --repo repo \
    --trust-root release.trust.json --bundle bundle "$cps_package"
for package in "${final_packages[@]}" "cps://canopa/system/sqlite@$sqlite_version" "cps://canopa/system/libseccomp@$seccomp_version"; do
    "${loader[@]}" image/usr/bin/cps source fetch-archived --repo repo \
        --trust-root release.trust.json --cache recovered-sources "$package"
done
diff -r sources/sha256 recovered-sources/sha256
"${loader[@]}" image/usr/bin/cps recipe fetch-toolchain --toolchain-config toolchain.kdl build.kdl
printf 'Verified reproducible CPS build, signed provenance, and installation from Git %s\n' "$revision"
