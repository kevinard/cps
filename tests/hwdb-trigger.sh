#!/usr/bin/env bash
set -euo pipefail
project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=${CPS_BIN:-$project/target/debug/cps}
tool=$(command -v systemd-hwdb)
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd "$work"
mkdir -p bin proto/usr/lib/udev/hwdb.d
printf '#!/usr/bin/env bash\necho update >> %q\n[[ ! -e %q ]] || exit 42\nexec %q "$@"\n' \
    "$work/actions" "$work/fail" "$tool" > bin/systemd-hwdb
chmod +x bin/systemd-hwdb
export PATH="$work/bin:$PATH"
openssl genpkey -algorithm ED25519 -outform DER -out private.der
openssl pkey -inform DER -in private.der -pubout -outform DER -out public.der

for name in first second; do
    path="usr/lib/udev/hwdb.d/$name.hwdb"
    printf 'test:cps:%s*\n CPS_TEST=%s\n' "$name" "$name" > "proto/$path"
    cat > manifest.kdl <<MANIFEST
schema-version 1
package "test/$name" publisher="canopa" version="1.0.0-0" {
    file "$path" mode="0644" uid=$(id -u) gid=$(id -g)
    hwdb "$name.hwdb"
}
MANIFEST
    "$cps" publish --repo repo --proto proto --signing-key private.der manifest.kdl
done
"$cps" image-create --root image --repo repo --publisher canopa --trust-key public.der
"$cps" plan --root image --output install.json install test/first test/second
"$cps" apply --root image install.json
test "$(wc -l < actions)" -eq 1
"$tool" --root=image query test:cps:first-device | rg -Fx 'CPS_TEST=first'
"$tool" --root=image query test:cps:second-device | rg -Fx 'CPS_TEST=second'
"$cps" plan --root image --output remove.json remove test/first test/second
touch fail
if "$cps" apply --root image remove.json; then
    echo 'expected the hardware database helper to fail' >&2
    exit 1
fi
test "$(wc -l < actions)" -eq 2
test ! -e image/usr/lib/udev/hwdb.d/first.hwdb
test ! -e image/usr/lib/udev/hwdb.d/second.hwdb
"$cps" transaction status --root image | rg -Fx 'Offline actions pending'
rm fail
mv repo offline
"$cps" transaction resume --root image
test "$(wc -l < actions)" -eq 3
test ! -e image/etc/udev/hwdb.bin
"$cps" verify --root image
echo 'Verified coalesced hardware database rebuilds, removal, and offline journal retry'
