#!/usr/bin/env bash
set -euo pipefail

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-"$project/target/debug/cps"}")
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd -- "$work"

openssl genpkey -algorithm ED25519 -outform DER -out private.der
openssl pkey -inform DER -in private.der -pubout -outform DER -out public.der
mkdir -p proto/usr/share/imports
while read -r name upstream selector native; do
    path="usr/share/imports/$name"
    printf '%s\n' "$upstream" > "proto/$path"
    if "$cps" manifest generate --proto proto --publisher imports --version "$upstream" \
        "imports/$name" > generated.kdl 2> version.log; then
        test "$native" = yes
    else
        test "$native" = no
        grep -E 'expected major.minor.patch-revision|invalid numeric version component' version.log
    fi
    cat > package.kdl <<MANIFEST
schema-version 1
package "imports/$name" publisher="imports" version="$selector" upstream-version="$upstream" {
    file "$path" mode="0644" uid=$(id -u) gid=$(id -g)
}
MANIFEST
    "$cps" publish --repo repo --proto proto --signing-key private.der package.kdl
done <<'FIXTURES'
sqlite 3.54.1-1 3.54.1-1 yes
python 3.14.9-1 3.14.9-1 yes
curl 8.23.0-1 8.23.0-1 yes
linux 7.2.14.arch1-1 7.2.14-1 no
glibc 2.44+r50+g1848099f063e-1 2.44.0-1 no
zlib 1:1.3.2-4 1.3.2-4 no
systemd 262.1-1 262.1.0-1 no
release-candidate 3.0.0~rc1-1 2.99.0-1 no
FIXTURES

cat > architecture.kdl <<'MANIFEST'
schema-version 1
package "imports/arch" publisher="imports" version="1.0.0-0" architecture="x86_64"
MANIFEST
"$cps" publish --repo repo --proto proto --signing-key private.der architecture.kdl
sed -i 's/x86_64/aarch64/' architecture.kdl
if "$cps" publish --repo repo --proto proto --signing-key private.der architecture.kdl > conflict.log 2>&1; then
    echo 'expected a second architecture to conflict with an existing package ID' >&2
    exit 1
fi
grep -F 'was already published with different content' conflict.log

for architecture in x86_64 aarch64; do
    "$cps" image-create --root "$architecture" --repo repo --publisher imports \
        --trust-key public.der --arch "$architecture"
done
"$cps" search --root x86_64 imports/arch | grep -F 'imports/arch@1.0.0-0'
test "$("$cps" search --root aarch64 imports/arch)" = $'PACKAGE\tSTATE\tSUMMARY'
"$cps" info --root aarch64 --available imports/zlib@1.3.2-4 | grep -F 'upstream-version="1:1.3.2-4"'
"$cps" info --root x86_64 --available imports/glibc@2.44.0-1 | grep -F 'upstream-version="2.44+r50+g1848099f063e-1"'
"$cps" plan --root x86_64 --output install.json install imports/zlib@1.3.2-4
"$cps" apply --root x86_64 install.json
"$cps" info --root x86_64 imports/zlib | grep -F 'upstream-version="1:1.3.2-4"'
"$cps" verify --root x86_64
