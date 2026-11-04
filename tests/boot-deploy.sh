#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-"$project/target/debug/cps"}")
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd -- "$work"

esp=9a138d0b-f9aa-4a2a-b527-93392ad7e812
base=7adff456-af50-498f-b39a-dcf87cb4a7c1
trial=b52060cf-81ab-4b8a-ad84-3aa69000ed49
disk_id=195c8dfa-636d-432b-9e78-79ad62b7d4a6
make_disk() {
    truncate -s 40M "$1"
    sfdisk --quiet "$1" <<PARTITIONS
label: gpt
label-id: $disk_id
unit: sectors

start=2048, size=4096, type=uefi, uuid=$esp
start=6144, size=32768, type=linux, uuid=$base
start=$2, size=32768, type=linux, uuid=$trial
PARTITIONS
}
make_disk template.raw 38912
# Put sentinel data in the ESP, known-good slot, target slot, and trailing gap.
for offset in 1 3 19 36; do
    printf 'fixture marker %s' "$offset" | dd of=template.raw bs=1M seek="$offset" conv=notrunc status=none
done
truncate -s 16M root.ext4
mke2fs -q -t ext4 -F root.ext4
input_hashes=$(sha256sum template.raw root.ext4)

deploy() {
    "$cps" boot-env deploy --disk "$1" --root-image "$2" --root-partuuid "$3" --output "$4"
}
hash=$(sha256sum root.ext4)
result=$(deploy template.raw root.ext4 "${trial^^}" next.raw)
test "$result" = "Deployed root to ${trial^^} in next.raw (SHA-256 ${hash%% *})"
test "$(stat -c '%s:%a' next.raw)" = '41943040:600'
cmp -n "$((19 * 1024 * 1024))" template.raw next.raw
cmp -i "$((19 * 1024 * 1024)):0" -n "$((16 * 1024 * 1024))" next.raw root.ext4
cmp -i "$((35 * 1024 * 1024))" template.raw next.raw

reject() {
    local message=$1
    shift
    if deploy "$@" > rejected.log 2>&1; then
        echo "Unexpected deployment success to $4" >&2
        exit 1
    fi
    grep -F "$message" rejected.log
}
deployed_hash=$(sha256sum next.raw)
reject 'destination already exists' template.raw root.ext4 "$trial" next.raw
test "$(sha256sum next.raw)" = "$deployed_hash"
deploy template.raw root.ext4 "$trial" raced.raw > race-first.log 2>&1 &
first=$!
deploy template.raw root.ext4 "$trial" raced.raw > race-second.log 2>&1 &
second=$!
successes=0
if wait "$first"; then successes=$((successes + 1)); fi
if wait "$second"; then successes=$((successes + 1)); fi
test "$successes" = 1
grep -E 'destination already exists|publishing the deployed disk image' race-{first,second}.log
cmp next.raw raced.raw

ln -s missing link.raw
reject 'destination already exists' template.raw root.ext4 "$trial" link.raw
reject 'must be an offline regular file' link.raw root.ext4 "$trial" rejected.raw
mkfifo pipe.ext4
reject 'must be an offline regular file' template.raw pipe.ext4 "$trial" rejected.raw
reject 'was not found' template.raw root.ext4 "$disk_id" rejected.raw
reject 'is not a GPT Linux filesystem partition' template.raw root.ext4 "$esp" rejected.raw
truncate -s 15M too-small.ext4
reject 'root payload size must match' template.raw too-small.ext4 "$trial" rejected.raw
truncate -s 16M invalid.ext4
reject 'e2fsck failed' template.raw invalid.ext4 "$trial" rejected.raw

cp template.raw damaged.raw
dd if=/dev/zero of=damaged.raw bs=512 seek=1 count=1 conv=notrunc status=none
reject 'sfdisk --json rejected the disk' damaged.raw root.ext4 "$trial" rejected.raw
cp template.raw duplicate.raw
sfdisk --quiet --part-uuid duplicate.raw 2 "$trial"
reject 'duplicate partition UUID' duplicate.raw root.ext4 "$trial" rejected.raw
make_disk conflicting.raw 40960
cp template.raw mismatch.raw
backup_lba=$((40 * 2048 - 33))
dd if=conflicting.raw of=mismatch.raw bs=512 skip="$backup_lba" seek="$backup_lba" conv=notrunc status=none
reject 'primary and backup GPT headers disagree' mismatch.raw root.ext4 "$trial" rejected.raw

test "$(sha256sum template.raw root.ext4)" = "$input_hashes"
test ! -e rejected.raw
test -L link.raw
test ! -e missing
test -z "$(find . -maxdepth 1 -name '.cps-deploy-*' -print)"

echo 'Verified atomic GPT deployment and byte-for-byte preservation outside the target slot'
