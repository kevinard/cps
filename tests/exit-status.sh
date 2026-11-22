#!/usr/bin/env bash
# Check the documented exit statuses (docs/reference/exit-status.md).
set -euo pipefail
umask 022
project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-$project/target/debug/cps}")
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd "$work"

# Run a command and require its exit status.
expect() {
    local expected=$1 status=0
    shift
    "$@" > out.log 2>&1 || status=$?
    if [[ $status != "$expected" ]]; then
        echo "expected status $expected, got $status: $*" >&2
        cat out.log >&2
        exit 1
    fi
}

expect 0 "$cps" --help
expect 0 "$cps" search --help
expect 2 "$cps" --no-such-option
expect 2 "$cps" search
expect 1 "$cps" list --root missing-image

mkdir -p proto/usr/share/cps-test
openssl genpkey -algorithm ED25519 -outform DER -out private.der
"$cps" repo trust init --repo repo --publisher canopa --key private.der > /dev/null
publish() {
    printf '%s\n' "$1" > proto/usr/share/cps-test/version
    cat > manifest.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "test/tool" publisher="canopa" version="$1" summary="Exit status fixture" {
    file "usr/share/cps-test/version" mode="0644"
}
MANIFEST
    "$cps" publish manifests --repo repo --proto proto --key private.der manifest.kdl > /dev/null
}
publish 1.0.0-0
"$cps" image create --root image --repo repo --trust-root repo/trust/1.json
"$cps" install --root image -y test/tool > /dev/null

# Lookups that find nothing exit with 3.
expect 0 "$cps" search --root image fixture
expect 3 "$cps" search --root image no-such-package
grep -F 'no package matches' out.log
expect 3 "$cps" search --root image --json no-such-package
grep -Fx '[]' out.log
expect 0 "$cps" owner --root image /usr/share/cps-test/version
expect 3 "$cps" owner --root image /usr/share/cps-test/missing
grep -F 'no installed package owns' out.log

# Verification and comparisons that find differences exit with 3.
cp -a --reflink=auto -- image changed
expect 0 "$cps" verify --root image
printf 'local edit\n' > changed/usr/share/cps-test/version
expect 3 "$cps" verify --root changed
expect 0 "$cps" image diff image image
publish 2.0.0-0
"$cps" upgrade --root changed -y > /dev/null
expect 3 "$cps" image diff image changed

# An image change with nothing to do commits no revision.
revision=$("$cps" status --root changed | sed -n 's/^Image revision //p')
expect 0 "$cps" upgrade --root changed -y
grep -Fx 'Nothing to do.' out.log
test "$("$cps" status --root changed | sed -n 's/^Image revision //p')" = "$revision"

# A saved plan runs only where it was saved: an image plan with --root, and
# a plan saved on the running system without it.
"$cps" install --root image -n --save-plan image-plan.json test/tool > /dev/null
expect 1 "$cps" apply image-plan.json
grep -F 'saved for an image' out.log
expect 1 "$cps" fetch image-plan.json
jq -c '. + {system: "live"}' image-plan.json > system-plan.json
expect 1 "$cps" apply --root image system-plan.json
grep -F 'saved on the running system' out.log

# A declined prompt exits with 4 and changes nothing.
status=0
printf 'n\n' | script -qec "$cps upgrade --root image" /dev/null > prompt.log 2>&1 || status=$?
test "$status" = 4 || { cat prompt.log >&2; exit 1; }
grep -F 'Canceled.' prompt.log
test "$(cat image/usr/share/cps-test/version)" = 1.0.0-0
echo 'Verified the documented exit statuses'
