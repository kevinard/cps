#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
umask 022

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-"$project/target/debug/cps"}")
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd -- "$work"

mkdir -p proto/bin
printf 'managed data\n' > proto/bin/tool
chmod 0755 proto/bin/tool
ln proto/bin/tool proto/bin/alias
ln -s tool proto/bin/link
openssl genpkey -algorithm ED25519 -outform DER -out private.der
openssl pkey -inform DER -in private.der -pubout -outform DER -out public.der
"$cps" manifest generate --proto proto --publisher test --version 1.0.0-0 \
    --uid "$(id -u)" --gid "$(id -g)" test/payload > manifest.kdl
"$cps" publish --repo repo --proto proto --signing-key private.der manifest.kdl
created=$("$cps" image-create --root image --repo repo --publisher test --trust-key public.der)
id=${created#Created image }
id=${id%% at *}
"$cps" plan --root image --output install.json install test/payload
"$cps" apply --root image install.json
chmod 0750 image
setfattr -n user.cps-root -v root-data image
printf 'local data\n' > image/local-state
setfattr -n user.cps -v exported image/local-state
source_state=$(sha256sum image/.cps/state.sqlite)

export_root() {
    "$cps" boot-env export --root image --output "$1" --size-mib "${2:-64}"
}
result=$(export_root root.ext4)
test "$result" = "Exported cps-$id-1 to root.ext4"
test "$(stat -c '%s:%a' root.ext4)" = '67108864:600'
test "$(debugfs -R 'cat /bin/tool' root.ext4 2>/dev/null)" = 'managed data'
test "$(debugfs -R 'cat /local-state' root.ext4 2>/dev/null)" = 'local data'
debugfs -R 'stat /' root.ext4 2>/dev/null | grep -E 'Mode: +0750'
debugfs -R 'ea_list /' root.ext4 2>/dev/null | grep -F 'user.cps-root (9) = "root-data"'
debugfs -R 'stat /bin/link' root.ext4 2>/dev/null | grep -Fx 'Fast link dest: "tool"'
debugfs -R 'ea_list /local-state' root.ext4 2>/dev/null | grep -F 'user.cps (8) = "exported"'
inode() { debugfs -R "stat /bin/$1" root.ext4 2>/dev/null | awk '/^Inode:/ {print $2}'; }
test -n "$(inode tool)"
test "$(inode tool)" = "$(inode alias)"
dumpe2fs -h root.ext4 2>/dev/null | grep -E "^Filesystem UUID: +$id$"
test "$(sha256sum image/.cps/state.sqlite)" = "$source_state"

epoch=1735689600
printf 'invalid mke2fs config\n' > caller-mke2fs.conf
repro_export() {
    SOURCE_DATE_EPOCH="$1" MKE2FS_CONFIG="$work/caller-mke2fs.conf" \
        "$cps" boot-env export --root image --output "$2" --size-mib 64 --receipt
}
repro_export "$epoch" repro-first.ext4 > repro-first.kdl
sleep 1
repro_export "0$epoch" repro-second.ext4 > repro-second.kdl
cmp repro-first.ext4 repro-second.ext4
cmp repro-first.kdl repro-second.kdl
TZ=UTC dumpe2fs -h repro-first.ext4 2>/dev/null | grep -E 'Filesystem created: +Wed Jan  1 00:00:00 2025$'
debugfs -R 'ea_list /' repro-first.ext4 2>/dev/null | grep -F 'user.cps-root (9) = "root-data"'
debugfs -R 'ea_list /local-state' repro-first.ext4 2>/dev/null | grep -F 'user.cps (8) = "exported"'
test "$(sha256sum image/.cps/state.sqlite)" = "$source_state"
for invalid in '' '2025-01-01' -1 4294967296; do
    if repro_export "$invalid" invalid-epoch.ext4 > rejected.log 2>&1; then
        echo 'Export accepted an invalid SOURCE_DATE_EPOCH' >&2
        exit 1
    fi
    grep -F 'SOURCE_DATE_EPOCH must be a decimal timestamp' rejected.log
    test ! -e invalid-epoch.ext4
done

reject() {
    local message=$1 output=$2 size=${3:-64}
    if export_root "$output" "$size" > rejected.log 2>&1; then
        echo "Unexpected success exporting to $output" >&2
        exit 1
    fi
    grep -F "$message" rejected.log
}
before=$(sha256sum root.ext4)
reject 'destination already exists' root.ext4
test "$(sha256sum root.ext4)" = "$before"
export_root raced.ext4 > race-first.log 2>&1 &
first=$!
export_root raced.ext4 > race-second.log 2>&1 &
second=$!
successes=0
if wait "$first"; then successes=$((successes + 1)); fi
if wait "$second"; then successes=$((successes + 1)); fi
test "$successes" = 1
grep -E 'destination already exists|publishing the ext4 image' race-{first,second}.log
e2fsck -fn raced.ext4 >/dev/null
ln -s missing dangling.ext4
reject 'destination already exists' dangling.ext4
test "$(readlink dangling.ext4)" = missing
test ! -e missing
reject 'destination must be outside the image' image/inner.ext4
test ! -e image/inner.ext4
mkfifo image/socket-input
reject 'cannot export special file "socket-input"' special.ext4
test ! -e special.ext4
rm image/socket-input
if "$cps" boot-env export --root / --output active.ext4 --size-mib 64 > rejected.log 2>&1; then
    echo 'Export accepted the running root' >&2
    exit 1
fi
grep -F 'export requires an inactive image root' rejected.log
test ! -e active.ext4
dd if=/dev/urandom of=image/large-file bs=1M count=20 status=none
reject 'mke2fs failed' too-small.ext4 16
test ! -e too-small.ext4
rm image/large-file
printf 'corrupt data\n' > image/bin/tool
reject 'image verification failed' corrupt.ext4
test ! -e corrupt.ext4
test -z "$(find . -maxdepth 1 -name '.cps-export-*' -print)"

echo 'Verified atomic and reproducible ext4 export, metadata preservation, and failure cleanup'
