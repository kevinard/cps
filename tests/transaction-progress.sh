#!/usr/bin/env bash
set -euo pipefail

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-"$project/target/debug/cps"}")
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd -- "$work"

openssl genpkey -algorithm ED25519 -outform DER -out private.der
openssl pkey -inform DER -in private.der -pubout -outform DER -out public.der
mkdir proto
for name in one two; do
    echo "$name" > "proto/$name"
    cat > package.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "test/$name" publisher="test" version="1.0.0-0" {
    file "$name" mode="0644" uid=$(id -u) gid=$(id -g)
}
MANIFEST
    "$cps" publish --repo repo --proto proto --signing-key private.der package.kdl
done
"$cps" image-create --root image --repo repo --publisher test --trust-key public.der
"$cps" image-create --root other --repo repo --publisher test --trust-key public.der
"$cps" plan --root image --output one.json install test/one
"$cps" plan --root image --output two.json install test/two

progress() {
    "$cps" transaction progress --root image "$@"
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
if "$cps" transaction progress --root other one.json; then
    echo 'a plan from an unrelated image must fail' >&2
    exit 1
fi
