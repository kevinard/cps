#!/usr/bin/env bash
# Exercise the operator-facing freeze workflow against a signed local repository.
set -euo pipefail
umask 022
project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-$project/target/debug/cps}")
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd "$work"
mkdir -p proto/usr/share/cps-test
openssl genpkey -algorithm ED25519 -outform DER -out private.der
"$cps" repo trust init --repo repo --publisher canopa --key private.der >/dev/null
publish() {
    printf '%s\n' "$1" > proto/usr/share/cps-test/version
    cat > manifest.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "test/tool" publisher="canopa" version="$1" {
    file "usr/share/cps-test/version" mode="0644" uid=$(id -u) gid=$(id -g)
}
MANIFEST
    "$cps" publish manifests --repo repo --proto proto --key private.der manifest.kdl
}
publish 1.0.0-0
"$cps" image create --root image --repo repo --trust-root repo/trust/1.json
"$cps" install --root image -y test/tool
mv repo offline-repo
"$cps" freeze --root image -y --reason 'Validate tool compatibility' test/tool
"$cps" list --root image --frozen | tee frozen.txt
grep -F $'test/tool\t1.0.0-0\t"Validate tool compatibility"' frozen.txt
mv offline-repo repo
publish 2.0.0-0
"$cps" upgrade --root image -y
test "$(cat image/usr/share/cps-test/version)" = 1.0.0-0
if "$cps" install --root image -n --save-plan rejected.json test/tool@2.0.0-0 > rejected.log 2>&1; then
    echo 'An explicit version overrode the freeze' >&2
    exit 1
fi
grep -F 'Validate tool compatibility' rejected.log
test ! -e rejected.json
"$cps" freeze --root image -n --save-plan keep.json --reason 'Validate tool compatibility' test/tool
cp -a --reflink=auto -- image staged
"$cps" apply --root staged keep.json
cmp frozen.txt <("$cps" list --root staged --frozen)
mv repo offline-repo
"$cps" unfreeze --root staged -y test/tool
mv offline-repo repo
"$cps" upgrade --root staged -n --save-plan upgrade.json
"$cps" fetch --root staged upgrade.json
"$cps" apply --root staged -n upgrade.json
"$cps" apply --root staged upgrade.json
test "$(cat staged/usr/share/cps-test/version)" = 2.0.0-0
test "$(cat image/usr/share/cps-test/version)" = 1.0.0-0
cmp frozen.txt <("$cps" list --root image --frozen)
test "$("$cps" list --root staged --frozen)" = $'PACKAGE\tVERSION\tREASON'
"$cps" verify --root image --pristine
"$cps" verify --root staged --pristine
echo 'Verified offline freeze, version constraints, staged unfreezing, and independent upgrade'
