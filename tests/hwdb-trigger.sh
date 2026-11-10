#!/usr/bin/env bash
# The hardware database rebuild is an ordinary trigger declared by the
# package that ships systemd-hwdb, as packaging/systemd/vm.kdl does.
set -euo pipefail
project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=${CPS_BIN:-$project/target/debug/cps}
# shellcheck source=tests/support/runtime.sh
source "$project/tests/support/runtime.sh"
tool=$(command -v systemd-hwdb)
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd "$work"
owner="uid=$(id -u) gid=$(id -g)"
mkdir -p proto/usr/lib/udev/hwdb.d proto/usr/libexec
openssl genpkey -algorithm ED25519 -outform DER -out private.der
"$cps" repo trust init --repo repo --publisher canopa --key private.der >/dev/null
publish() {
    printf 'schema-version 1 kind="package-manifest"\npackage "%s" publisher="canopa" version="1.0.0-0" {\n%s}\n' \
        "$1" "$2" > manifest.kdl
    "$cps" publish --repo repo --proto proto --signing-key private.der manifest.kdl
}

# The fixture handler records each run in a declared output and fails while
# the image contains a marker.
cat > proto/usr/libexec/hwdb-update <<'HANDLER'
#!/usr/bin/bash
set -eu
[[ ! -e /etc/udev/hwdb-fail ]] || exit 42
echo update >> /etc/udev/hwdb.runs
exec /usr/bin/systemd-hwdb --strict update
HANDLER
chmod 0755 proto/usr/libexec/hwdb-update
publish test/systemd "$(ship_runtime proto /usr/bin/bash "$tool")
    file \"usr/libexec/hwdb-update\" mode=\"0755\" $owner
    dir \"etc/udev\" mode=\"0755\" $owner
    trigger \"hwdb\" {
        run \"usr/libexec/hwdb-update\"
        watch \"usr/lib/udev/hwdb.d/*.hwdb\"
        output \"etc/udev/hwdb.bin\"
        output \"etc/udev/hwdb.runs\"
    }
"
for name in first second; do
    path="usr/lib/udev/hwdb.d/$name.hwdb"
    printf 'test:cps:%s*\n CPS_TEST=%s\n' "$name" "$name" > "proto/$path"
    publish "test/$name" "    require \"test/systemd\"
    file \"$path\" mode=\"0644\" $owner
"
done
runs() { wc -l < image/etc/udev/hwdb.runs; }

"$cps" image-create --root image --repo repo --publisher canopa --trust-root repo/trust/1.json
"$cps" plan --root image --output install.json install test/first test/second > plan.log
rg -Fx 'Run trigger test/systemd:hwdb' plan.log
"$cps" apply --root image install.json
test "$(runs)" -eq 1
"$tool" --root=image query test:cps:first-device | rg -Fx 'CPS_TEST=first'
"$tool" --root=image query test:cps:second-device | rg -Fx 'CPS_TEST=second'
"$cps" verify --root image --pristine
"$cps" plan --root image --output remove.json remove test/first test/second
touch image/etc/udev/hwdb-fail
if "$cps" apply --root image remove.json; then
    echo 'expected the hardware database helper to fail' >&2
    exit 1
fi
test "$(runs)" -eq 1
test ! -e image/usr/lib/udev/hwdb.d/first.hwdb
test ! -e image/usr/lib/udev/hwdb.d/second.hwdb
"$cps" transaction status --root image | rg -Fx 'Offline actions pending'
rm image/etc/udev/hwdb-fail
mv repo offline
"$cps" transaction resume --root image
test "$(runs)" -eq 2
test ! -e image/etc/udev/hwdb.bin
"$cps" verify --root image --pristine

# Aborting rebuilds the database from the restored files, and an abort that
# the helper interrupts finishes on retry.
mv offline repo
"$cps" plan --root image --output install.json install test/first test/second > /dev/null
"$cps" apply --root image install.json
test "$(runs)" -eq 3
"$cps" plan --root image --output remove.json remove test/second
touch image/etc/udev/hwdb-fail
if "$cps" apply --root image remove.json; then
    echo 'expected the hardware database helper to fail' >&2
    exit 1
fi
if "$cps" transaction abort --root image; then
    echo 'expected the hardware database helper to fail during the abort' >&2
    exit 1
fi
"$cps" transaction status --root image | rg -Fx 'Abort in progress'
rm image/etc/udev/hwdb-fail
"$cps" transaction abort --root image
test "$(runs)" -eq 4
"$tool" --root=image query test:cps:second-device | rg -Fx 'CPS_TEST=second'
"$cps" verify --root image --pristine
echo 'Verified coalesced hardware database rebuilds, removal, offline journal retry, and abort'
