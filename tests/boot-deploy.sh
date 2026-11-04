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

mkdir -p proto/etc esp
openssl genpkey -algorithm ED25519 -outform DER -out private.der
openssl pkey -inform DER -in private.der -pubout -outform DER -out public.der
publish() {
    local publisher=$1 repo=$2 version=$3
    printf '%s\n' "$version" > proto/etc/version
    "$cps" manifest generate --proto proto --publisher "$publisher" --version "$version" \
        --uid "$(id -u)" --gid "$(id -g)" test/root > package.kdl
    "$cps" publish --repo "$repo" --proto proto --signing-key private.der package.kdl
}
publish test repo 1.0.0-0
"$cps" image-create --root base --repo repo --publisher test --trust-key public.der
"$cps" plan --root base --output install.json install test/root
"$cps" apply --root base install.json
mkdir -p base/boot
printf 'test kernel\n' > base/boot/vmlinuz

export_with_receipt() {
    "$cps" boot-env export --root "$1" --output "$1.ext4" --size-mib 16 --receipt > "$1.kdl"
}
activate_checked() {
    local root=$1 disk=$2 partuuid=$3 receipt=$4
    shift 4
    "$cps" boot-env activate --root "$root" --esp esp --root-partuuid "$partuuid" \
        --disk "$disk" --receipt "$receipt" "$@"
}
reject_activation() {
    local message=$1
    shift
    if activate_checked "$@" > rejected.log 2>&1; then
        echo 'Unexpected checked activation success' >&2
        exit 1
    fi
    grep -F "$message" rejected.log
}

export_with_receipt base
base_hash=$(sha256sum base.ext4)
grep -F "bytes=16777216 sha256=\"${base_hash%% *}\"" base.kdl
deploy template.raw base.ext4 "$base" base.raw
reject_activation 'does not match the root export receipt' \
    base template.raw "$base" base.kdl --known-good
test -z "$(ls -A esp)"

publish test repo 2.0.0-0
"$cps" plan --root base --output update.json update
cp -a base different-revision
"$cps" apply --root different-revision update.json
reject_activation 'different image or revision' \
    different-revision base.raw "$base" base.kdl --known-good
export_with_receipt different-revision
deploy template.raw different-revision.ext4 "$base" different-revision.raw

publish extra extras 1.0.0-0
cp -a base different-state
"$cps" publisher add --root different-state --repo extras --trust-key public.der extra
reject_activation 'image state differs from the root export receipt' \
    different-state base.raw "$base" base.kdl --known-good

sed 's/ bytes=/ revision=1 bytes=/' base.kdl > duplicate.kdl
reject_activation 'duplicate root-export property revision' \
    base base.raw "$base" duplicate.kdl --known-good
truncate -s 4097 oversized.kdl
reject_activation 'root export receipt is too large' \
    base base.raw "$base" oversized.kdl --known-good
test -z "$(ls -A esp)"
base_id=$(activate_checked base base.raw "$base" base.kdl --known-good)
base_id=${base_id#Activated }
test -f "esp/loader/entries/$base_id.conf"

"$cps" boot-env stage --source base --root staged update.json
export_with_receipt staged
deploy base.raw staged.ext4 "$trial" staged.raw
deploy template.raw staged.ext4 "$trial" without-base.raw
deploy different-revision.raw staged.ext4 "$trial" wrong-base-revision.raw
cp -a esp esp-before
reject_activation 'checking the known-good fallback partition' \
    staged without-base.raw "$trial" staged.kdl
reject_activation 'at revision 1' \
    staged wrong-base-revision.raw "$trial" staged.kdl
reject_activation 'does not match the root export receipt' \
    staged staged.raw "$base" staged.kdl
cp staged.raw tampered.raw
printf 'changed deployed bytes' | dd of=tampered.raw bs=1M seek=19 conv=notrunc status=none
reject_activation 'does not match the root export receipt' \
    staged tampered.raw "$trial" staged.kdl
diff -r esp-before esp

trial_id=$(activate_checked staged staged.raw "$trial" staged.kdl)
trial_id=${trial_id#Activated }
test -f "esp/loader/entries/$trial_id+2.conf"
activate_checked staged staged.raw "$trial" staged.kdl --tries 9
test ! -e "esp/loader/entries/$trial_id+9.conf"
cmp "esp-before/loader/entries/$base_id.conf" "esp/loader/entries/$base_id.conf"

echo 'Verified checked activation rejects stale exports, modified partitions, and missing fallbacks'
