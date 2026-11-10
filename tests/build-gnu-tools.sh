#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
umask 022

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-$project/target/debug/cps}")
output=$(realpath -m -- "${1:?usage: build-gnu-tools.sh OUTPUT-DIRECTORY USR-BASE-FIXTURE}")
base=$(realpath -- "${2:?usage: build-gnu-tools.sh OUTPUT-DIRECTORY USR-BASE-FIXTURE}")
sources=$(realpath -- "${CPS_GNU_SOURCE_DIR:?Set CPS_GNU_SOURCE_DIR to the verified GNU archive directory}")
[[ $(uname -m) == x86_64 ]] || { echo 'The GNU tools fixture needs an x86-64 worker' >&2; exit 1; }
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
bash "$project/tests/seed-native-c.sh" "$work/native-gnu" --util-linux
"$cps" manifest generate --proto "$work/native-gnu" --publisher seed --version 1.0.0-0 \
    build/native-gnu > native-gnu.kdl
"$cps" publish --repo toolchain --proto "$work/native-gnu" --signing-key "$work/toolchain.der" native-gnu.kdl
mkdir repo
cp builder-public.der repo/
printf 'schema-version 1 kind="publish-policy"\npublisher "canopa-utilities" {\n    builder-key "builder-public.der"\n}\n' > repo/publish-policy.kdl
for utility in coreutils grep; do
    recipe="$project/packaging/gnu-tools"
    sed "s/uid=0 gid=0/uid=$(id -u) gid=$(id -g)/g" "$recipe/$utility.kdl" > "$utility.kdl"
    sed -e "s/@CATALOG_SHA256@/$(cat toolchain/current)/g" \
        -e "s/@KEY_SHA256@/$(tail -c 32 -- toolchain-public.der | sha256sum | cut -d' ' -f1)/g" \
        -e "s/@MANIFEST_SHA256@/$(sha "$utility.kdl")/g" "$recipe/$utility.build.kdl.in" > "$utility.build.kdl"
    "$cps" source import --from "$sources" --cache sources "$utility.build.kdl"
    "$cps" build --repo toolchain --publisher seed --trust-key toolchain-public.der \
        --source-cache sources --output "$utility.bundle" --check-reproducible \
        --builder-signing-key "$work/builder.der" "$utility.build.kdl"
    version=$(sed -n -e 's/^package .* version="\([^"]*\)".*/\1/p' \
        -e 's/^    version "\([^"]*\)".*/\1/p' "$utility.kdl" "$utility.build.kdl")
    reviewed=$(sha "$utility.build.kdl")
    sed -i "/^}$/i\\    approve \"system/$utility\" version=\"$version\" sha256=\"$reviewed\"" repo/publish-policy.kdl
    "$cps" publish-bundle --repo repo --signing-key "$work/release.der" --builder-key builder-public.der \
        --recipe-sha256 "$reviewed" "$utility.bundle"
    "$cps" repo attestation --repo repo --publisher canopa-utilities --trust-key release-public.der \
        "cps://canopa-utilities/system/$utility@$version" > "$utility-attestation.json"
    (cat "$utility.bundle/attestation.json"; printf '\n') | cmp - "$utility-attestation.json"
done
"$cps" image-create --root image --repo repo --publisher canopa-utilities --trust-key release-public.der
"$cps" publisher add --root image --repo "$base/repo" --trust-key "$base/release-public.der" canopa-usr-base
"$cps" plan --root image --output install.json install system/coreutils system/grep
"$cps" apply --root image install.json
"$cps" verify --root image --pristine
loader=(env -i "$PWD/image/usr/lib/ld-linux-x86-64.so.2" --inhibit-cache --library-path "$PWD/image/usr/lib")
for program in coreutils grep; do
    cmp "$program.bundle/proto/usr/bin/$program" "image/usr/bin/$program"
    links=$("${loader[@]}" --list "image/usr/bin/$program")
    awk -v prefix="$PWD/image/" '$2 == "=>" && index($3, prefix) != 1 {print; exit 1}' <<< "$links"
done
printf 'kernel payload\n' > input
"${loader[@]}" image/usr/bin/install -Dm640 input installed/kernel
cmp input installed/kernel
test "$("${loader[@]}" image/usr/bin/stat -c '%a' installed/kernel)" = 640
printf '# ignored\nroot=PARTUUID=fixture\n' > cmdline
"${loader[@]}" image/usr/bin/grep -Gv '^\s*#' cmdline > filtered
test "$(cat filtered)" = 'root=PARTUUID=fixture'
echo 'Verified reproducible GNU tools, signed provenance, installation, and kernel-install utility semantics'
