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
printf 'test initramfs\n' > base/boot/initrd
printf 'local configuration\n' > base/etc/local.conf
chmod 0644 base/etc/local.conf
ln base/etc/local.conf base/etc/local-alias
ln -s local.conf base/etc/local-link
setfattr -n user.cps -v local base/etc/local.conf

export_with_receipt() {
    "$cps" boot-env export --root "$1" --output "$1.ext4" --size-mib 16 --receipt > "$1.kdl"
}
activate_checked() {
    local root=$1 disk=$2 partuuid=$3 receipt=$4
    shift 4
    "$cps" boot-env activate --root "$root" --esp esp --root-partuuid "$partuuid" \
        --disk "$disk" --receipt "$receipt" --initrd boot/initrd "$@"
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
sed -E 's/tree-sha256="[0-9a-f]+"/tree-sha256="broken"/' base.kdl > malformed-tree.kdl
reject_activation 'invalid SHA-256 digest "broken"' \
    base base.raw "$base" malformed-tree.kdl --known-good
truncate -s 4097 oversized.kdl
reject_activation 'root export receipt is too large' \
    base base.raw "$base" oversized.kdl --known-good
oversized_options=$(python3 -c 'print("a" * 65536)')
reject_activation 'boot entry exceeds 64 KiB' \
    base base.raw "$base" base.kdl --known-good --options "$oversized_options"
test -z "$(ls -A esp)"
base_id=$(activate_checked base base.raw "$base" base.kdl --known-good)
base_id=${base_id#Activated }
test -f "esp/loader/entries/$base_id.conf"
kernel_hash=$(sha256sum base/boot/vmlinuz)
initrd_hash=$(sha256sum base/boot/initrd)
grep -Fx "# cps-linux-sha256 ${kernel_hash%% *}" "esp/loader/entries/$base_id.conf"
grep -Fx "# cps-initrd-sha256 ${initrd_hash%% *}" "esp/loader/entries/$base_id.conf"

cp -a esp legacy-esp
sed -i '/^# cps-.*-sha256 /d' "legacy-esp/loader/entries/$base_id.conf"
sed -E 's/ tree-sha256="[0-9a-f]+"$//' base.kdl > legacy.kdl
cp -a legacy-esp legacy-before
"$cps" boot-env activate --root base --esp legacy-esp --root-partuuid "$base" \
    --disk base.raw --receipt legacy.kdl --known-good --initrd boot/initrd
diff -r legacy-before legacy-esp

"$cps" boot-env stage --source base --root staged update.json
export_with_receipt staged
deploy base.raw staged.ext4 "$trial" staged.raw
deploy template.raw staged.ext4 "$trial" without-base.raw
deploy different-revision.raw staged.ext4 "$trial" wrong-base-revision.raw
cp -a esp esp-before
for change in kernel config mode hardlink symlink xattr root-xattr; do
    cp -a staged "changed-$change"
    changed="changed-$change"
    case "$change" in
        kernel) printf 'replaced kernel\n' > "$changed/boot/vmlinuz" ;;
        config) printf 'edited configuration\n' > "$changed/etc/local.conf" ;;
        mode) chmod 0600 "$changed/etc/local.conf" ;;
        hardlink)
            rm -- "$changed/etc/local-alias"
            cp -a -- "$changed/etc/local.conf" "$changed/etc/local-alias"
            ;;
        symlink) ln -sfn local-alias "$changed/etc/local-link" ;;
        xattr) setfattr -n user.cps -v changed "$changed/etc/local.conf" ;;
        root-xattr) setfattr -n user.cps -v changed "$changed" ;;
    esac
    cmp staged/.cps/state.sqlite "$changed/.cps/state.sqlite"
    reject_activation 'root tree differs from the root export receipt' "$changed" staged.raw "$trial" staged.kdl
    diff -r esp-before esp
done
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
for file in vmlinuz.efi initramfs.img; do
    artifact="esp/EFI/cps/$base_id/$file"
    printf '!' | dd of="$artifact" bs=1 count=1 conv=notrunc status=none
    reject_activation 'boot artifact digest mismatch' staged staged.raw "$trial" staged.kdl
    cp -- "esp-before/EFI/cps/$base_id/$file" "$artifact"
    diff -r esp-before esp
done
base_entry="loader/entries/$base_id.conf"
printf '\tlinux\t/other.efi\n' >> "esp/$base_entry"
reject_activation 'duplicate fallback kernel path' staged staged.raw "$trial" staged.kdl
cp -- "esp-before/$base_entry" "esp/$base_entry"
diff -r esp-before esp

cp -a staged staged-copy
touch -d @1 staged-copy/etc/local.conf
trial_id=$(activate_checked staged-copy staged.raw "$trial" staged.kdl)
trial_id=${trial_id#Activated }
test -f "esp/loader/entries/$trial_id+2.conf"
activate_checked staged staged.raw "$trial" staged.kdl --tries 9
test ! -e "esp/loader/entries/$trial_id+9.conf"
cmp "esp-before/loader/entries/$base_id.conf" "esp/loader/entries/$base_id.conf"

echo 'Verified checked activation rejects stale exports, modified partitions, and missing fallbacks'

if [[ ${1:-} != --esp ]]; then exit 0; fi

mkdir -p seed/EFI/{BOOT,vendor} seed/loader
printf 'firmware entry\n' > seed/EFI/BOOT/BOOTX64.EFI
printf 'vendor data\n' > seed/EFI/vendor/keep.efi
printf 'systemd random seed\n' > seed/loader/random-seed
truncate -s 2M seed.fat
mkfs.fat -F 12 seed.fat >/dev/null
mcopy -s -i seed.fat seed/EFI seed/loader ::/
cp --sparse=always template.raw with-esp.raw
dd if=seed.fat of=with-esp.raw bs=1M seek=1 conv=notrunc status=none
raw_activate() {
    local root=$1 disk=$2 partuuid=$3 output=$4
    shift 4
    "$cps" boot-env activate --root "$root" --root-partuuid "$partuuid" \
        --receipt "$root.kdl" --disk "$disk" --esp-partuuid "$esp" --output-disk "$output" \
        --initrd boot/initrd "$@"
}
raw_list() {
    "$cps" boot-env list --disk "$1" --esp-partuuid "$esp"
}
raw_rollback() {
    "$cps" boot-env rollback --disk "$1" --esp-partuuid "$esp" --output-disk "$2"
}
reject_raw() {
    local message=$1
    shift
    if "$@" > rejected.log 2>&1; then
        echo 'Unexpected success using the offline EFI partition' >&2
        exit 1
    fi
    grep -F "$message" rejected.log
}
assert_default() {
    raw_list "$1" | awk -F '\t' -v id="$2" -v status="$3" \
        '$1 == "*" && $2 == status && $3 == id {found=1} END {exit !found}'
}
assert_retired() {
    local listing
    listing=$(raw_list "$1")
    if grep -Fq "$2" <<< "$listing"; then
        echo "Retired boot environment $2 is still registered" >&2
        exit 1
    fi
}

reject_raw 'not a GPT EFI System partition' "$cps" boot-env list --disk with-esp.raw --esp-partuuid "$base"
reject_raw 'fsck.fat rejected the EFI partition' raw_activate base base.raw "$base" rejected.raw --known-good
test ! -e rejected.raw
test "$(raw_list with-esp.raw)" = 'No boot environments registered'
original=$(sha256sum with-esp.raw)
raw_activate base with-esp.raw "$base" bootable-base.raw --known-good --root-image base.ext4
test "$(sha256sum with-esp.raw)" = "$original"
assert_default bootable-base.raw "$base_id" known-good

SOURCE_DATE_EPOCH=0 TZ=Pacific/Honolulu \
    raw_activate base with-esp.raw "$base" reproducible-base.raw --known-good --root-image base.ext4
sleep 2
SOURCE_DATE_EPOCH=315532800 TZ=Asia/Tokyo \
    raw_activate base with-esp.raw "$base" repeated-base.raw --known-good --root-image base.ext4
cmp reproducible-base.raw repeated-base.raw
TZ=UTC mdir -i reproducible-base.raw@@1048576 ::/loader/entries | grep -F '1980-01-01'
SOURCE_DATE_EPOCH=invalid reject_raw 'SOURCE_DATE_EPOCH must be a decimal timestamp' \
    raw_activate base with-esp.raw "$base" rejected.raw --known-good --root-image base.ext4
test ! -e rejected.raw

deploy bootable-base.raw staged.ext4 "$trial" deployed-trial.raw
cp --sparse=always bootable-base.raw full-esp.raw
free=$(mdir -i full-esp.raw@@1048576 ::/ | awk '/bytes free/ {sub(/bytes free/, ""); gsub(/[^0-9]/, ""); print}')
test -n "$free"
truncate -s "$free" fill
mcopy -i full-esp.raw@@1048576 fill ::/fill
full_before=$(sha256sum full-esp.raw)
reject_raw 'could not create EFI directory' raw_activate staged full-esp.raw "$trial" rejected.raw --root-image staged.ext4
test "$(sha256sum full-esp.raw)" = "$full_before"
test ! -e rejected.raw
raw_activate staged deployed-trial.raw "$trial" bootable-trial.raw
cmp -n 1048576 deployed-trial.raw bootable-trial.raw
cmp -i "$((3 * 1024 * 1024))" deployed-trial.raw bootable-trial.raw
assert_default bootable-trial.raw "$trial_id" 'trial (2 left)'
raw_activate staged bootable-trial.raw "$trial" retry.raw --tries 9
cmp bootable-trial.raw retry.raw

# A pinned base can coexist with a live trial. Only its exhausted entry can
# be recycled without blessing that trial.
"$cps" boot-env stage --source base --root retry-root update.json
export_with_receipt retry-root
"$cps" boot-env rollback --disk bootable-trial.raw --esp-partuuid "$esp" \
    --output-disk pinned-base.raw "$base_id"
reject_raw 'while its trial has attempts left' \
    raw_activate retry-root pinned-base.raw "$trial" rejected.raw --root-image retry-root.ext4 --replace "$trial_id"
mren -i pinned-base.raw@@1048576 \
    "::/loader/entries/$trial_id+2.conf" "::/loader/entries/$trial_id+0-2.conf"
retry_id=$(raw_activate retry-root pinned-base.raw "$trial" retried.raw \
    --root-image retry-root.ext4 --replace "$trial_id")
retry_id=${retry_id#Activated }
assert_default retried.raw "$retry_id" 'trial (2 left)'
assert_retired retried.raw "$trial_id"

# Simulate the guest blessing the counted entry on the source disk.
mren -i bootable-trial.raw@@1048576 \
    "::/loader/entries/$trial_id+2.conf" "::/loader/entries/$trial_id.conf"
assert_default bootable-trial.raw "$trial_id" known-good
before_rollback=$(sha256sum bootable-trial.raw)
SOURCE_DATE_EPOCH=1735689601 raw_rollback bootable-trial.raw rolled-back.raw
SOURCE_DATE_EPOCH=1735689600 raw_rollback bootable-trial.raw repeated-rollback.raw
cmp rolled-back.raw repeated-rollback.raw
test "$(sha256sum bootable-trial.raw)" = "$before_rollback"
assert_default rolled-back.raw "$base_id" known-good
raw_list rolled-back.raw | grep -F "$trial_id"
cmp -n 1048576 bootable-trial.raw rolled-back.raw
cmp -i "$((3 * 1024 * 1024))" bootable-trial.raw rolled-back.raw
for path in EFI/BOOT/BOOTX64.EFI EFI/vendor/keep.efi loader/random-seed; do
    mcopy -i rolled-back.raw@@1048576 "::/$path" - | cmp "seed/$path" -
done

deploy bootable-trial.raw different-revision.ext4 "$base" unusable-fallback.raw
reject_raw 'checking the rollback root partition' raw_rollback unusable-fallback.raw rejected.raw
reject_raw 'destination already exists' raw_rollback bootable-trial.raw rolled-back.raw
test ! -e rejected.raw

publish test repo 3.0.0-0
"$cps" plan --root staged --output next.json update
"$cps" boot-env stage --source staged --root recycled next.json
export_with_receipt recycled
before_recycle=$(sha256sum bootable-trial.raw)
reject_raw 'distinct root PARTUUID' \
    raw_activate recycled bootable-trial.raw "$base" rejected.raw --root-image recycled.ext4
reject_raw 'cannot replace the known-good base' \
    raw_activate recycled bootable-trial.raw "$trial" rejected.raw --root-image recycled.ext4 --replace "$trial_id"
reject_raw 'unknown replacement boot environment' \
    raw_activate recycled bootable-trial.raw "$base" rejected.raw --root-image recycled.ext4 --replace "$disk_id"
reject_raw 'checking the occupied root partition' \
    raw_activate recycled unusable-fallback.raw "$base" rejected.raw --root-image recycled.ext4 --replace "$base_id"
reject_raw 'root payload does not match the root export receipt' \
    raw_activate recycled bootable-trial.raw "$base" rejected.raw --root-image base.ext4 --replace "$base_id"
test ! -e rejected.raw
test "$(sha256sum bootable-trial.raw)" = "$before_recycle"

printf 'foreign file in retired directory\n' > unrelated
mcopy -i bootable-trial.raw@@1048576 unrelated "::/EFI/cps/$base_id/keep"
before_recycle=$(sha256sum bootable-trial.raw)
recycled_id=$(SOURCE_DATE_EPOCH=1735689601 raw_activate recycled bootable-trial.raw "$base" recycled.raw \
    --root-image recycled.ext4 --replace "$base_id")
recycled_id=${recycled_id#Activated }
SOURCE_DATE_EPOCH=1735689600 raw_activate recycled bootable-trial.raw "$base" repeated-recycle.raw \
    --root-image recycled.ext4 --replace "$base_id"
cmp recycled.raw repeated-recycle.raw
test "$(sha256sum bootable-trial.raw)" = "$before_recycle"
cmp -n 1048576 bootable-trial.raw recycled.raw
cmp -i "$((3 * 1024 * 1024)):0" -n "$((16 * 1024 * 1024))" recycled.raw recycled.ext4
cmp -i "$((19 * 1024 * 1024))" bootable-trial.raw recycled.raw
mcopy -i recycled.raw@@1048576 "::/EFI/cps/$base_id/keep" - | cmp unrelated -
if mdir -i recycled.raw@@1048576 "::/EFI/cps/$base_id/vmlinuz.efi" >/dev/null 2>&1; then
    echo 'Retired kernel still exists on the ESP' >&2
    exit 1
fi
assert_default recycled.raw "$recycled_id" 'trial (2 left)'
assert_retired recycled.raw "$base_id"
raw_rollback recycled.raw recycled-rollback.raw
assert_default recycled-rollback.raw "$trial_id" known-good
reject_raw 'the recorded parent is unavailable' raw_rollback recycled-rollback.raw rejected.raw
test ! -e rejected.raw

# Simulate the dirty flag left in the primary boot sector after guest shutdown.
truncate -s 66M dirty.raw
printf 'label: gpt\nstart=2048,size=131072,type=uefi,uuid=%s\n' "$esp" | sfdisk --quiet dirty.raw
truncate -s 64M fat32
mkfs.fat -F 32 -b 6 fat32 >/dev/null
printf '\001' | dd of=fat32 bs=1 seek=65 conv=notrunc status=none
dd if=fat32 of=dirty.raw bs=1M seek=1 conv=notrunc,sparse status=none
dirty_hash=$(sha256sum dirty.raw)
test "$(raw_list dirty.raw)" = 'No boot environments registered'
test "$(sha256sum dirty.raw)" = "$dirty_hash"
printf 'X' | dd of=dirty.raw bs=1 seek="$((1048576 + 6 * 512 + 3))" conv=notrunc status=none
reject_raw 'fsck.fat rejected the EFI partition' raw_list dirty.raw

test -z "$(find . -maxdepth 1 -name '.cps-deploy-*' -print)"
echo 'Verified activation and rollback update the same disk and preserve unrelated EFI files'
echo 'Verified reproducible disk activation, rollback, and slot reuse'
