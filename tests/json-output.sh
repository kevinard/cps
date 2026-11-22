#!/usr/bin/env bash
# Check the shape of each --json document against tests/golden/json. A key
# list there changes only with a documented change to the JSON contract
# (docs/reference/json.md). Run with --update to rewrite the key lists.
set -euo pipefail
umask 022
project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-$project/target/debug/cps}")
golden=$project/tests/golden/json
update=${1:-}
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd "$work"

# The sorted paths of every scalar in the document, with array indices as [].
shape() {
    jq -r '[paths(type != "object" and type != "array") | map(if type == "number" then "[]" else . end) | join(".")] | unique | .[]'
}
check() {
    local name=$1 status=0
    shift
    "$@" > "$name.json" || status=$?
    # Status 3 reports a finding, which some documents describe.
    [[ $status == 0 || $status == 3 ]] || { echo "$name: status $status" >&2; exit 1; }
    shape < "$name.json" > "$name.keys"
    if [[ $update == --update ]]; then
        cp "$name.keys" "$golden/$name.keys"
    elif ! diff -u "$golden/$name.keys" "$name.keys"; then
        echo "The JSON shape of $name changed; see docs/reference/json.md" >&2
        exit 1
    fi
}

mkdir -p proto/usr/share/cps-test proto/etc
openssl genpkey -algorithm ED25519 -outform DER -out private.der
"$cps" repo trust init --repo repo --publisher canopa --key private.der > /dev/null
publish() {
    printf '%s\n' "$1" > proto/usr/share/cps-test/version
    printf 'setting=%s\n' "$1" > proto/etc/cps-test.conf
    cat > tool.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "test/tool" publisher="canopa" version="$1" summary="JSON fixture" {
    require "test/lib"
    file "usr/share/cps-test/version" mode="0644" uid=$(id -u) gid=$(id -g)
    file "etc/cps-test.conf" mode="0644" preserve=#true uid=$(id -u) gid=$(id -g)
}
MANIFEST
    cat > lib.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "test/lib" publisher="canopa" version="1.0.0-0" summary="JSON fixture library" {
}
MANIFEST
    "$cps" publish manifests --repo repo --proto proto --key private.der lib.kdl tool.kdl > /dev/null
}
publish 1.0.0-0
"$cps" image create --root image --repo repo --trust-root repo/trust/1.json > /dev/null
"$cps" install --root image -y test/tool > /dev/null
printf 'setting=local\n' > image/etc/cps-test.conf
cp -a --reflink=auto -- image before
publish 2.0.0-0
"$cps" upgrade --root image -y > /dev/null

check list "$cps" list --root image --json
check search "$cps" search --root image --json fixture
check info "$cps" info --root image --json test/tool
check policy "$cps" policy --root image --json test/tool
check owner "$cps" owner --root image --json /usr/share/cps-test/version
check contents "$cps" contents --root image --json test/tool
check deps "$cps" deps --root image --json test/tool
check rdeps "$cps" rdeps --root image --json test/lib
check why "$cps" why --root image --json test/lib
check verify "$cps" verify --root image --json
check history "$cps" history --root image --json
check status "$cps" status --root image --json
check config-list "$cps" config list --root image --json
check publisher-list "$cps" publisher list --root image --json
check image-diff "$cps" image diff --json before image
check obsoletions "$cps" repo obsoletions --repo repo --trust-root repo/trust/1.json --json
"$cps" repo withdraw --repo repo --key private.der cps://canopa/test/tool@1.0.0-0 > /dev/null
check withdrawals "$cps" repo withdrawals --repo repo --trust-root repo/trust/1.json --json
echo 'Verified the shape of every --json document'
