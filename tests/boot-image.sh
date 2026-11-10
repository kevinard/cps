#!/usr/bin/env bash
# Build offline Btrfs disk images with boot-env image, without root or a VM.
set -euo pipefail
export LC_ALL=C
umask 022

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-"$project/target/debug/cps"}")
efi=$(realpath -- "${CPS_VM_SYSTEMD_BOOT:-/usr/lib/systemd/boot/efi/systemd-bootx64.efi}")
for command in mkfs.btrfs btrfs mkfs.fat fsck.fat mcopy mdir mmd sfdisk openssl setfattr getfattr; do
    command -v "$command" >/dev/null || { echo "Missing command: $command" >&2; exit 1; }
done
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd -- "$work"

mkdir -p proto/bin proto/boot proto/tmp proto/srv/shared
chmod 1777 proto/tmp
chmod 2775 proto/srv/shared
printf 'managed data\n' > proto/bin/tool
chmod 0755 proto/bin/tool
ln proto/bin/tool proto/bin/alias
ln -s tool proto/bin/link
printf 'fixture kernel\n' > proto/boot/vmlinuz
openssl genpkey -algorithm ED25519 -outform DER -out private.der
"$cps" repo trust init --repo repo --publisher test --key private.der >/dev/null
"$cps" manifest generate --proto proto --publisher test --version 1.0.0-0 \
    --uid "$(id -u)" --gid "$(id -g)" test/payload > manifest.kdl
"$cps" publish --repo repo --proto proto --signing-key private.der manifest.kdl
root=volume/@
mkdir -p volume/@environments
created=$("$cps" image-create --root "$root" --repo repo --publisher test --trust-root repo/trust/1.json)
id=${created#Created image }
id=${id%% at *}
"$cps" plan --root "$root" --output install.json install test/payload
"$cps" apply --root "$root" install.json
chmod 0750 "$root"
setfattr -n user.cps-root -v root-data "$root"
printf 'local data\n' > "$root/local-state"
setfattr -n user.cps -v kept "$root/local-state"
state_dir=usr/lib/sysimage/cps
source_state=$(sha256sum "$root/$state_dir/state.sqlite")

esp_uuid=9a138d0b-f9aa-4a2a-b527-93392ad7e812
root_uuid=7adff456-af50-498f-b39a-dcf87cb4a7c1
template() {
    local disk=$1 loader=$2
    truncate -s 400M "$disk"
    sfdisk --quiet "$disk" <<PARTITIONS
label: gpt
unit: sectors
start=2048, size=131072, type=uefi, uuid=$esp_uuid
start=133120, size=$((300 * 2048)), type=linux, uuid=$root_uuid
PARTITIONS
    rm -f esp.fat
    truncate -s 64M esp.fat
    mkfs.fat -F 32 esp.fat >/dev/null
    mmd -i esp.fat ::/EFI ::/EFI/BOOT
    mcopy -i esp.fat "$loader" ::/EFI/BOOT/BOOTX64.EFI
    dd if=esp.fat of="$disk" bs=1M seek=1 conv=notrunc status=none
}
template template.raw "$efi"
image() {
    local output=$1 disk=${2:-template.raw}
    shift $(($# < 2 ? $# : 2))
    "$cps" boot-env image --btrfs-volume volume --root "$root" --environments volume/@environments \
        --disk "$disk" --root-partuuid "$root_uuid" --output "$output" --options 'rw quiet' "$@"
}
partition() { dd if="$1" of="$2" bs=1M skip=65 count=300 status=none; }

result=$(image disk.raw)
test "$result" = "Wrote cps-$id-1 to disk.raw"
test "$(stat -c %a disk.raw)" = 600
cmp -n 1048576 template.raw disk.raw
partition disk.raw root.btrfs
btrfs check --readonly root.btrfs >/dev/null 2>&1
btrfs inspect-internal dump-super root.btrfs | grep -E "^fsid[[:space:]]+$id$"
btrfs inspect-internal dump-tree -t root root.btrfs > root-tree.txt
subvolume_id() {
    awk -v name="$1" '/key \(FS_TREE ROOT_REF/ { id = $6; sub(/\)/, "", id) }
        /root ref key dirid 256/ && $NF == name { print id }' root-tree.txt
}
root_id=$(subvolume_id @)
test -n "$root_id" -a -n "$(subvolume_id @environments)"
btrfs inspect-internal dump-tree -t "$root_id" root.btrfs > root-fs.txt
grep -A4 'key (256 XATTR_ITEM' root-fs.txt | grep -Fx "		data cps-$id-1"
# The registration exists only in the image, never on the source root.
test -z "$(getfattr --absolute-names --only-values -n user.cps.boot-environment "$root" 2>/dev/null)"
mkdir restored
btrfs restore -S -x -m -r "$root_id" root.btrfs restored >/dev/null 2>&1
diff -r "$root" restored
test "$(stat -c %a restored/tmp restored/srv/shared)" = $'1777\n2775'
test "$(getfattr --only-values -n user.cps restored/local-state)" = kept
# btrfs restore copies hard links as separate files. The tree keeps one inode.
inode_of() {
    awk -v name="$1" '/DIR_ITEM/ { getline; split($0, key, /[( ]/); inode = key[4] }
        $1 == "name:" && $2 == name { print inode; exit }' root-fs.txt
}
test -n "$(inode_of tool)"
test "$(inode_of tool)" = "$(inode_of alias)"
test "$(sha256sum "$root/$state_dir/state.sqlite")" = "$source_state"

entry=loader/entries/cps-$id-1.conf
mcopy -i disk.raw@@1048576 "::/$entry" entry.conf
mcopy -i disk.raw@@1048576 ::/loader/loader.conf loader.conf
mcopy -i disk.raw@@1048576 "::/EFI/cps/cps-$id-1/vmlinuz.efi" kernel.efi
cmp proto/boot/vmlinuz kernel.efi
grep -Fx 'default cps-*.conf' loader.conf
grep -Fx "# cps-btrfs-fsid $id" entry.conf
grep -Fx "# cps-btrfs-subvolid $root_id" entry.conf
grep -Fx '# cps-btrfs-path @' entry.conf
grep -Fx "options root=PARTUUID=$root_uuid rootwait rootfstype=btrfs rootflags=subvolid=$root_id rw quiet" entry.conf
subvolume_uuid=$(awk -v id="$root_id" '$0 ~ "key \\(" id " ROOT_ITEM" { found = 1 }
    found && $1 == "uuid" { print $2; exit }' root-tree.txt)
grep -Fx "# cps-btrfs-uuid $subvolume_uuid" entry.conf
mdir -i disk.raw@@1048576 ::/EFI/BOOT | grep -F BOOTX64
echo 'Verified the Btrfs layout, registration, metadata, and known-good entry of an offline image'

# btrfs-progs 7.3 still gives the subvolumes it creates random UUIDs and the
# current creation time. Everything else repeats.
epoch=1735689600
SOURCE_DATE_EPOCH=$epoch image repro-first.raw >/dev/null
SOURCE_DATE_EPOCH=0$epoch image repro-second.raw >/dev/null
masked() {
    partition "$1" "$1.btrfs"
    btrfs inspect-internal dump-tree -t root "$1.btrfs" |
        sed -E '/^[[:space:]]*(uuid|parent_uuid|ctime|otime) /d'
    for tree in fs 256 257 extent chunk dev csum free-space; do
        btrfs inspect-internal dump-tree -t "$tree" "$1.btrfs"
    done
    mcopy -i "$1@@1048576" "::/$entry" - | grep -v '^# cps-btrfs-uuid '
    mdir -a -i "$1@@1048576" -s ::/
}
cmp <(masked repro-first.raw) <(masked repro-second.raw)
TZ=UTC mdir -i repro-first.raw@@1048576 ::/loader | grep -F '2025-01-01'
echo 'Verified SOURCE_DATE_EPOCH repeats the image apart from btrfs-progs subvolume identities'

reject() {
    local message=$1 output=$2
    shift 2
    if "$@" > rejected.log 2>&1; then
        echo "Unexpected success writing $output" >&2
        exit 1
    fi
    grep -F "$message" rejected.log || { cat rejected.log >&2; exit 1; }
    test ! -e "$output"
    test -z "$(find . -maxdepth 1 -name '.cps-disk-*' -print)"
    test -z "$(getfattr --absolute-names --only-values -n user.cps.boot-environment "$root" 2>/dev/null)"
}
before=$(sha256sum disk.raw)
if image disk.raw > rejected.log 2>&1; then exit 1; fi
grep -F 'destination already exists' rejected.log
test "$(sha256sum disk.raw)" = "$before"
reject 'must be outside the volume directory' volume/disk.raw image volume/disk.raw
touch volume/stray
reject 'contain only the root, environments, and /var' extra.raw image extra.raw
rm volume/stray
touch volume/@environments/stray
reject 'environments directory must be empty' busy.raw image busy.raw
rm volume/@environments/stray
reject 'directly inside the volume directory' nested.raw \
    "$cps" boot-env image --btrfs-volume volume --root "$root/bin" --environments volume/@environments \
    --disk template.raw --root-partuuid "$root_uuid" --output nested.raw
reject 'rootflags and rootfstype' flags.raw \
    "$cps" boot-env image --btrfs-volume volume --root "$root" --environments volume/@environments \
    --disk template.raw --root-partuuid "$root_uuid" --output flags.raw --options rootflags=subvol=@
reject 'not a GPT Linux filesystem partition' other.raw \
    "$cps" boot-env image --btrfs-volume volume --root "$root" --environments volume/@environments \
    --disk template.raw --root-partuuid "$esp_uuid" --output other.raw
reject 'already has CPS boot files' reused.raw image reused.raw disk.raw
printf 'MZ#### LoaderInfo: systemd-boot 255.4 ####' > old-loader.efi
template old-loader.raw old-loader.efi
reject 'EFI/BOOT/BOOTX64.EFI on the ESP is systemd-boot 255.4' old.raw image old.raw old-loader.raw
mkfifo "$root/fifo"
reject 'cannot image special file "fifo"' special.raw image special.raw
rm "$root/fifo"
dd if=/dev/urandom of="$root/large-file" bs=1M count=320 status=none
reject 'does not fit in its 300.0 MiB partition' large.raw image large.raw
rm "$root/large-file"
printf 'corrupt data\n' > "$root/bin/tool"
reject 'image verification failed' corrupt.raw image corrupt.raw
printf 'managed data\n' > "$root/bin/tool"
test "$(sha256sum "$root/$state_dir/state.sqlite")" = "$source_state"

image raced.raw > race-first.log 2>&1 &
first=$!
image raced.raw > race-second.log 2>&1 &
second=$!
successes=0
if wait "$first"; then successes=$((successes + 1)); fi
if wait "$second"; then successes=$((successes + 1)); fi
test "$successes" = 1
grep -E 'destination already exists|publishing the disk image' race-{first,second}.log
test -z "$(find . -maxdepth 1 -name '.cps-disk-*' -print)"
echo 'Verified image rejections, failure cleanup, and a single winner on concurrent writes'

# The shared /var subvolume: empty, a sibling of the root, and mounted by the root's fstab.
mkdir volume/@var
var_image() { image "$1" "${2:-template.raw}" --var volume/@var; }
reject 'no /etc/fstab to mount the shared /var subvolume' no-fstab.raw var_image no-fstab.raw
mkdir -p "$root/etc"
printf 'PARTUUID=%s /var/log btrfs subvol=/@var 0 0\n' "$root_uuid" > "$root/etc/fstab"
reject 'must mount subvolume @var at /var' wrong-fstab.raw var_image wrong-fstab.raw
printf 'PARTUUID=%s / btrfs defaults 0 0\nPARTUUID=%s /var btrfs noatime,subvol=/@var 0 0\n' \
    "$root_uuid" "$root_uuid" > "$root/etc/fstab"
touch volume/@var/stray
reject 'the /var directory must be empty' busy-var.raw var_image busy-var.raw
rm volume/@var/stray
reject 'contain only the root, environments, and /var' no-var.raw image no-var.raw
reject 'must differ' same.raw image same.raw template.raw --var volume/@environments
var_image var.raw >/dev/null
partition var.raw var.btrfs
btrfs check --readonly var.btrfs >/dev/null 2>&1
btrfs inspect-internal dump-tree -t root var.btrfs > root-tree.txt
test -n "$(subvolume_id @var)" -a -n "$(subvolume_id @)" -a -n "$(subvolume_id @environments)"
mkdir restored-var
btrfs restore -S -x -m -r "$(subvolume_id @)" var.btrfs restored-var >/dev/null 2>&1
test -d restored-var/var && test -z "$(ls -A restored-var/var)"
echo 'Verified the shared /var subvolume beside the root'
