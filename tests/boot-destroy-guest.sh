#!/bin/sh
# Invoked by the native Btrfs VM observer after a successful default boot.
set -eu
bb=/bin/busybox
export PATH=/usr/bin:/bin
. /etc/cps-btrfs-boot
captures=/volume/@captures
current=$($bb cat /run/cps-vm-id)
phase=
if [ -f "$captures/destroy-phase" ]; then phase=$($bb cat "$captures/destroy-phase"); fi

if [ -z "$phase" ]; then
    echo "$current" > "$captures/retained-id"
    cps plan --root / --output "$captures/cleanup-plan.json" mark explicit test/live-root
    cps boot-env capture --source / --output "$captures/cleanup-base"
    cps boot-env stage-btrfs --source "$captures/cleanup-base/root" \
        --output "$captures/cleanup-trial" "$captures/cleanup-plan.json"
    next=$(cps boot-env activate-btrfs --root "$captures/cleanup-trial/root" \
        --btrfs-volume /volume --esp /efi --root-partuuid "$root_uuid" \
        --options 'console=ttyS0 loglevel=3 init=/init-systemd')
    next=${next#Activated }
    echo "$next" > "$captures/cleanup-id"
    echo prepared > "$captures/destroy-phase"
    echo 'CPS_VM_BTRFS_DESTROY: staged another trial from the blessed root'
    $bb sync
    exec $bb reboot -f
fi
base=$($bb cat "$captures/base-id")
retained=$($bb cat "$captures/retained-id")
next=$($bb cat "$captures/cleanup-id")
if [ "$phase" = done ]; then
    test "$current" = "$retained"
    test ! -e /volume/@
    test ! -e "/efi/EFI/cps/$base"
    cps verify --root /
    echo 'CPS_VM_BTRFS_DESTROY: previous root booted after cleanup'
    $bb sync
    exec $bb poweroff -f
fi
test "$phase" = prepared
test "$current" = "$next"
test -f "/efi/loader/entries/$current.conf"
echo 'CPS_VM_BTRFS_DESTROY: checking protected and mounted roots'

boot_state() { find /efi/loader /efi/EFI/cps -type f -exec sha256sum '{}' +; }
reject() {
    expected=$1
    id=$2
    before=$(boot_state)
    if cps boot-env destroy-btrfs --esp /efi --btrfs-volume /volume "$id" > /run/rejected 2>&1; then exit 1; fi
    if ! $bb grep -F "$expected" /run/rejected; then $bb cat /run/rejected; exit 1; fi
    test "$(boot_state)" = "$before"
}
reject 'selected boot environment' "$current"
cps boot-env rollback --esp /efi --btrfs-volume /volume "$current"
reject "selected environment's fallback" "$retained"
cps boot-env rollback --esp /efi --btrfs-volume /volume "$retained"
reject 'inactive image root' "$current"
cps boot-env rollback --esp /efi --btrfs-volume /volume "$current"
cps boot-env rollback --esp /efi --btrfs-volume /volume "$base"
reject "running environment's fallback" "$retained"
cps boot-env rollback --esp /efi --btrfs-volume /volume "$current"

old_mount="/run/old root$($bb printf '\377')"
$bb mkdir "$old_mount"
mount -t btrfs -o ro,subvol=@ "$root_device" "$old_mount"
reject 'mounted Btrfs root' "$base"
umount "$old_mount"
mount --bind /volume/@/etc "$old_mount"
reject 'mounted Btrfs root' "$base"
umount "$old_mount"
btrfs subvolume create /volume/@/nested
echo 'keep shared child data' > /volume/@/nested/keep
reject 'nested' "$base"
test "$($bb cat /volume/@/nested/keep)" = 'keep shared child data'
btrfs subvolume delete /volume/@/nested

entry="/efi/loader/entries/$base.conf"
$bb cp "$entry" /run/original-entry
$bb sed 's/^# cps-btrfs-uuid .*/# cps-btrfs-uuid 11111111-1111-4111-8111-111111111111/' /run/original-entry > "$entry"
reject 'Btrfs subvolume identity differs' "$base"
$bb cat /run/original-entry > "$entry"

$bb mkfifo /run/release-destroy
state_dir=usr/lib/sysimage/cps
if [ -d /volume/@/.cps ]; then state_dir=.cps; fi
flock --shared "/volume/@/$state_dir/lock" sh -c ': > /run/destroy-locked; read released < /run/release-destroy' &
holding=$!
while [ ! -e /run/destroy-locked ]; do $bb sleep 0.01; done
reject 'busy with another CPS operation' "$base"
echo released > /run/release-destroy
wait "$holding"

before=$(boot_state)
cps boot-env destroy-btrfs --esp /efi --btrfs-volume /volume --dry-run "$base"
test "$(boot_state)" = "$before"
kept=$(sha256sum "/efi/EFI/cps/$current/vmlinuz.efi" "/efi/EFI/cps/$retained/vmlinuz.efi" /efi/EFI/BOOT/BOOTX64.EFI /efi/loader/loader.conf)
# mkfs made @ the filesystem default. The kernel refuses its destruction,
# after CPS has durably retired its boot entry. Resume after moving that default.
if cps boot-env destroy-btrfs --esp /efi --btrfs-volume /volume "$base" > /run/interrupted 2>&1; then exit 1; fi
$bb grep -F 'deleting the registered Btrfs subvolume by ID' /run/interrupted
test -d /volume/@
test ! -e "$entry"
test -f "/efi/loader/entries/$base.cps-destroy"
cps boot-env list --esp /efi > /run/retiring
$bb grep "retiring.*$base" /run/retiring
if cps boot-env activate-btrfs --known-good --root /volume/@ --btrfs-volume /volume \
    --esp /efi --root-partuuid "$root_uuid" > /run/rejected 2>&1; then exit 1; fi
$bb grep -F 'is retiring' /run/rejected
btrfs subvolume set-default 5 /volume
if /usr/local/libexec/cps-vm-deny-unlink cps boot-env destroy-btrfs \
    --esp /efi --btrfs-volume /volume "$base" > /run/cleanup-interrupted 2>&1; then exit 1; fi
$bb cat /run/cleanup-interrupted
$bb grep -F 'Input/output error' /run/cleanup-interrupted
test ! -e /volume/@
test -f "/efi/loader/entries/$base.cps-destroy"
test -f "/efi/EFI/cps/$base/vmlinuz.efi"
cps boot-env list --esp /efi > /run/retiring
$bb grep "retiring.*$base" /run/retiring
cps boot-env destroy-btrfs --esp /efi --btrfs-volume /volume "$base"
test ! -e /volume/@
test ! -e "/efi/loader/entries/$base.cps-destroy"
test ! -e "/efi/EFI/cps/$base"
test "$(sha256sum "/efi/EFI/cps/$current/vmlinuz.efi" "/efi/EFI/cps/$retained/vmlinuz.efi" /efi/EFI/BOOT/BOOTX64.EFI /efi/loader/loader.conf)" = "$kept"
test "$($bb cat "$captures/base/root/etc/cps-capture-version")" = 1.0.0-0
cps verify --root "$captures/trial/root"
cps verify --root /
echo 'CPS_VM_BTRFS_DESTROY: retired old root and artifacts after retry'
echo done > "$captures/destroy-phase"
cps boot-env rollback --esp /efi --btrfs-volume /volume
$bb sync
exec $bb reboot -f
