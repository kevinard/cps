#!/usr/bin/env bash
# Exercise the operator-facing freeze workflow against a signed local repository.
set -euo pipefail
umask 022
project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-$project/target/debug/cps}")
previous=${CPS_TEST_PREVIOUS_BINARY:-}
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd "$work"
mkdir -p proto/usr/share/cps-test
openssl genpkey -algorithm ED25519 -outform DER -out private.der
openssl pkey -inform DER -in private.der -pubout -outform DER -out public.der
publish() {
    printf '%s\n' "$1" > proto/usr/share/cps-test/version
    cat > manifest.kdl <<MANIFEST
schema-version 1
package "test/tool" publisher="canopa" version="$1" {
    file "usr/share/cps-test/version" mode="0644" uid=$(id -u) gid=$(id -g)
}
MANIFEST
    "$cps" publish --repo repo --proto proto --signing-key private.der manifest.kdl
}
publish 1.0.0-0
"$cps" image-create --root image --repo repo --publisher canopa --trust-key public.der
"$cps" plan --root image --output install.json install test/tool
"$cps" apply --root image install.json
mv repo offline-repo
"$cps" plan --root image --output freeze.json freeze --reason 'Validate tool compatibility' test/tool
"$cps" apply --root image freeze.json
"$cps" freeze --root image | tee frozen.txt
grep -F $'test/tool\t1.0.0-0\t"Validate tool compatibility"' frozen.txt
if [[ -n $previous ]]; then
    if "$previous" verify --root image > previous.log 2>&1; then
        echo 'An earlier client accepted an image with unenforced freezes' >&2
        exit 1
    fi
    grep -F 'unsupported image format' previous.log
fi
mv offline-repo repo
publish 2.0.0-0
"$cps" plan --root image --output held-update.json update
"$cps" apply --root image held-update.json
test "$(cat image/usr/share/cps-test/version)" = 1.0.0-0
if "$cps" plan --root image --output rejected.json install test/tool@2.0.0-0 > rejected.log 2>&1; then
    echo 'An explicit version overrode the freeze' >&2
    exit 1
fi
grep -F 'Validate tool compatibility' rejected.log
test ! -e rejected.json
"$cps" plan --root image --output keep.json freeze --reason 'Validate tool compatibility' test/tool
"$cps" boot-env stage --source image --root staged keep.json
cmp frozen.txt <("$cps" freeze --root staged)
mv repo offline-repo
"$cps" plan --root staged --output unfreeze.json unfreeze test/tool
"$cps" apply --root staged unfreeze.json
mv offline-repo repo
"$cps" plan --root staged --output upgrade.json update
"$cps" apply --root staged upgrade.json
test "$(cat staged/usr/share/cps-test/version)" = 2.0.0-0
test "$(cat image/usr/share/cps-test/version)" = 1.0.0-0
cmp frozen.txt <("$cps" freeze --root image)
test "$("$cps" freeze --root staged)" = $'PACKAGE\tVERSION\tREASON'
"$cps" verify --root image --pristine
"$cps" verify --root staged --pristine
echo 'Verified offline freeze, version constraints, staged unfreezing, and independent upgrade'
