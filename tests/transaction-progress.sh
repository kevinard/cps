#!/usr/bin/env bash
set -euo pipefail

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-"$project/target/debug/cps"}")
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd -- "$work"

openssl genpkey -algorithm ED25519 -outform DER -out private.der
"$cps" repo trust init --repo repo --publisher test --key private.der >/dev/null
mkdir proto
for name in one two; do
    echo "$name" > "proto/$name"
    cat > package.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "test/$name" publisher="test" version="1.0.0-0" {
    file "$name" mode="0644" uid=$(id -u) gid=$(id -g)
}
MANIFEST
    "$cps" publish manifests --repo repo --proto proto --key private.der package.kdl
done
"$cps" image create --root image --repo repo --trust-root repo/trust/1.json
"$cps" image create --root other --repo repo --trust-root repo/trust/1.json
"$cps" install --root image -n --save-plan one.json test/one
"$cps" install --root image -n --save-plan two.json test/two

progress() {
    "$cps" status --root image "${@:1:$#-1}" --plan "${@: -1}"
}
planned=$(progress one.json)
plan_id=$(jq -er '.plan_id | select(test("^[0-9a-f]{64}$"))' <<< "$planned")
jq -e '.phase == "planned" and .revision == 0' <<< "$planned"
mv repo/blobs repo/hidden
if progress --validate one.json; then
    echo 'missing payloads must fail validation' >&2
    exit 1
fi
mv repo/hidden repo/blobs
progress --validate one.json | jq -e --arg id "$plan_id" \
    '.plan_id == $id and .phase == "payload_validated" and .revision == 0'
test ! -e image/one
"$cps" apply --root image one.json
progress one.json | jq -e --arg id "$plan_id" \
    '.plan_id == $id and .phase == "applied" and .revision == 1'
progress two.json | jq -e '.phase == "superseded" and .revision == 1'
if "$cps" status --root other --plan one.json; then
    echo 'a plan from an unrelated image must fail' >&2
    exit 1
fi

# An apply that stops after recording its journal can be aborted.
"$cps" install --root image -n --save-plan three.json test/two
chmod a-w image
if "$cps" apply --root image three.json 2>/dev/null; then
    echo 'apply must fail on a read-only image root' >&2
    exit 1
fi
chmod u+w image
"$cps" status --root image | grep -qx 'File writes pending'
progress three.json | jq -e '.phase == "applying"'
"$cps" abort --root image | grep -qx 'Aborted the pending transaction'
"$cps" status --root image | grep -qx 'No pending transaction'
test ! -e image/two
progress three.json | jq -e '.phase == "planned" and .revision == 1'
"$cps" abort --root image | grep -qx 'No pending transaction'
"$cps" apply --root image three.json
progress three.json | jq -e '.phase == "applied" and .revision == 2'
