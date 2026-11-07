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
while read -r name upstream; do
    path="usr/share/imports/$name"
    printf '%s\n' "$upstream" > "proto/$path"
    "$cps" manifest generate --proto proto --publisher imports --version "$upstream" \
        "imports/$name" > generated.kdl
    grep -F "version=\"$upstream\"" generated.kdl
    cat > package.kdl <<MANIFEST
schema-version 1
package "imports/$name" publisher="imports" version="$upstream" upstream-version="$upstream" {
    file "$path" mode="0644" uid=$(id -u) gid=$(id -g)
}
MANIFEST
    "$cps" publish --repo repo --proto proto --signing-key private.der package.kdl
done <<'FIXTURES'
sqlite 3.54.1-1
python 3.14.9-1
curl 8.23.0-1
linux 7.2.14.arch1-1
glibc 2.44+r50+g1848099f063e-1
zlib 1:1.3.2-4
systemd 262.1-1
release-candidate 3.0.0~rc1-1
FIXTURES

for version in '3.0.0~rc2-1' '3.0.0~rc10-1' '3.0.0-1'; do
    cat > package.kdl <<MANIFEST
schema-version 1
package "imports/release-candidate" publisher="imports" version="$version" {
    file "usr/share/imports/release-candidate" mode="0644" uid=$(id -u) gid=$(id -g)
}
MANIFEST
    "$cps" publish --repo repo --proto proto --signing-key private.der package.kdl
done

cat > consumer.kdl <<'MANIFEST'
schema-version 1
package "imports/consumer" publisher="imports" version="1.0" {
    require "imports/release-candidate" version=">=3.0.0~rc1-1, <3.0.0-1"
}
MANIFEST
"$cps" publish --repo repo --proto proto --signing-key private.der consumer.kdl

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
"$cps" info --root aarch64 --available imports/zlib@1:1.3.2-4 | grep -F 'upstream-version="1:1.3.2-4"'
"$cps" info --root x86_64 --available imports/glibc@2.44+r50+g1848099f063e-1 | grep -F 'upstream-version="2.44+r50+g1848099f063e-1"'
"$cps" plan --root x86_64 --output install.json install cps://imports/imports/zlib@1:1.3.2-4 imports/consumer
"$cps" apply --root x86_64 install.json
"$cps" info --root x86_64 imports/zlib | grep -F 'upstream-version="1:1.3.2-4"'
"$cps" info --root x86_64 imports/release-candidate | grep -F 'version="3.0.0~rc10-1"'
"$cps" verify --root x86_64
