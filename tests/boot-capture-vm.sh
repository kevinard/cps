#!/usr/bin/env bash
# Capture a running Btrfs CPS root and boot its staged upgrade on ext4.
set -euo pipefail
umask 022

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
source "$project/tests/systemd-boot-guest.sh"
cps=$(realpath -- "${CPS_BIN:-$project/target/debug/cps}")
qemu=${CPS_VM_QEMU:-qemu-system-x86_64}
kernel=${CPS_VM_KERNEL:-/boot/vmlinuz-linux}
busybox=${CPS_VM_BUSYBOX:-/usr/lib/initcpio/busybox}
btrfs=$(realpath -- "${CPS_VM_BTRFS:?Set CPS_VM_BTRFS to a trusted btrfs executable}")
formatter=${CPS_VM_MKFS_BTRFS:?Set CPS_VM_MKFS_BTRFS to a trusted mkfs.btrfs executable}
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd "$work"

mkdir -p proto/etc
openssl genpkey -algorithm ED25519 -outform DER -out private.der
openssl pkey -inform DER -in private.der -pubout -outform DER -out public.der
publish() {
    printf '%s\n' "$1" > proto/etc/cps-capture-version
    "$cps" manifest generate --proto proto --publisher test --version "$1" \
        --uid "$(id -u)" --gid "$(id -g)" test/live-root > manifest.kdl
    "$cps" publish --repo repo --proto proto --signing-key private.der manifest.kdl
}
publish 1.0.0-0
mkdir files
root="$work/files/@"
"$cps" image-create --root "$root" --repo "$work/repo" --publisher test --trust-key public.der
"$cps" plan --root "$root" --output install.json install test/live-root
"$cps" apply --root "$root" install.json
publish 2.0.0-0
mkdir -p "$root"/{bin,usr/bin,proc,sys,dev,tmp,captures,scratch,repository} files/@captures
cp -a repo/. "$root/repository/"
cp "$busybox" "$root/bin/busybox"
ln -s busybox "$root/bin/sh"
copy_runtime_deps "$root" "$busybox"
strip --strip-debug -o "$root/usr/bin/cps" "$cps"
copy_runtime_deps "$root" "$cps"
cp "$btrfs" "$root/usr/bin/btrfs"
copy_runtime_deps "$root" "$btrfs"
for tool in cp mke2fs e2fsck mount umount find sha256sum; do
    binary=$(command -v "$tool")
    cp "$binary" "$root/usr/bin/$tool"
    copy_runtime_deps "$root" "$binary"
done
cat > "$root/init" <<'GUEST'
#!/bin/sh
set -eu
bb=/bin/busybox
trap '$bb poweroff -f' EXIT
export PATH=/usr/bin:/bin
mount -t devtmpfs devtmpfs /dev
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t tmpfs tmpfs /tmp
mount -t btrfs -o subvol=@captures /dev/vda /captures
mount -t ext4 /dev/vdb /scratch
cps publisher set-repository --root / --repo test=/repository
cps plan --root / --output /captures/upgrade.json update
printf 'captured local state\n' > /etc/cps-capture-local
before=$(sha256sum /.cps/state.sqlite)
reject() {
    expected=$1
    shift
    if cps boot-env capture "$@" > /tmp/rejected 2>&1; then
        echo 'Capture unexpectedly succeeded'
        exit 1
    fi
    $bb grep -F "$expected" /tmp/rejected
}
reject 'requires the root of a Btrfs subvolume' --source /etc --output /captures/wrong-source
reject 'separate Btrfs subvolume' --source / --output /bad-capture
btrfs subvolume create /nested
reject 'capture omits nested Btrfs subvolume' --source / --output /captures/incomplete
test ! -e /captures/incomplete
test -z "$(find /captures -maxdepth 1 -name '.cps-capture-*')"
btrfs subvolume delete /nested
$bb cp /etc/cps-capture-version /tmp/version
printf 'corrupt package file\n' > /etc/cps-capture-version
reject 'image verification failed' --source / --output /captures/corrupt
test ! -e /captures/corrupt
$bb cat /tmp/version > /etc/cps-capture-version
test -z "$(find /captures -maxdepth 1 -name '.cps-capture-*')"
cps boot-env capture --source / --output /captures/race > /tmp/first 2>&1 &
first=$!
cps boot-env capture --source / --output /captures/race > /tmp/second 2>&1 &
second=$!
successes=0
if wait "$first"; then successes=$((successes + 1)); fi
if wait "$second"; then successes=$((successes + 1)); fi
test "$successes" = 1
test -z "$(find /captures -maxdepth 1 -name '.cps-capture-*')"
btrfs subvolume delete /captures/race/root
$bb rmdir /captures/race
result=$(cps boot-env capture --source / --output /captures/base)
echo "$result"
btrfs property get /captures/base/root ro | $bb grep -Fx 'ro=true'
if $bb touch /captures/base/root/changed 2>/tmp/rejected; then exit 1; fi
reject 'destination already exists' --source / --output /captures/base
printf 'running local state\n' > /etc/cps-capture-local
test "$($bb cat /captures/base/root/etc/cps-capture-local)" = 'captured local state'
cps verify --root /captures/base/root
cps boot-env stage --source /captures/base/root --root /scratch/staged /captures/upgrade.json
test "$($bb cat /scratch/staged/etc/cps-capture-version)" = 2.0.0-0
test "$($bb cat /etc/cps-capture-version)" = 1.0.0-0
test "$(sha256sum /.cps/state.sqlite)" = "$before"
cps boot-env export --root /scratch/staged --output /scratch/root.ext4 --size-mib 256 \
    --receipt > /scratch/root.kdl
test "$($bb cat /scratch/staged/etc/cps-capture-local)" = 'captured local state'
echo 'CPS_VM_CAPTURE: immutable live snapshot staged without changing the running root'
umount /scratch
umount /captures
GUEST
cat > "$root/init-trial" <<'GUEST'
#!/bin/sh
set -eu
bb=/bin/busybox
trap '$bb poweroff -f' EXIT
export PATH=/usr/bin:/bin
mount -t devtmpfs devtmpfs /dev
mount -t proc proc /proc
mount -t sysfs sysfs /sys
test "$($bb cat /etc/cps-capture-version)" = 2.0.0-0
test "$($bb cat /etc/cps-capture-local)" = 'captured local state'
cps verify --root /
echo 'CPS_VM_CAPTURE_BOOT: booted the upgrade prepared from the running Btrfs root'
GUEST
chmod +x "$root/init" "$root/init-trial"
truncate -s 512M source.btrfs
"$formatter" -q -f --rootdir files --subvol default:@ --subvol rw:@captures source.btrfs
truncate -s 768M scratch.ext4
mke2fs -q -F -t ext4 scratch.ext4
data_args=()
if [[ -n ${CPS_VM_QEMU_DATA:-} ]]; then data_args=(-L "$CPS_VM_QEMU_DATA"); fi
run() {
    local log=$1 root_image=$2 arguments=$3
    shift 3
    timeout "${CPS_VM_TIMEOUT:-180}" "$qemu" "${data_args[@]}" \
        -machine q35,accel=tcg -m 768 -smp 2 -nic none -display none -monitor none \
        -serial "file:$log" -kernel "$kernel" -append "$arguments console=ttyS0 loglevel=3" \
        -drive "if=virtio,format=raw,file=$root_image" "$@" || { cat "$log"; return 1; }
    tr -d '\r' < "$log" > "$log.clean"
    cat "$log.clean"
}
run capture.log source.btrfs 'root=/dev/vda rootfstype=btrfs rootflags=subvol=@ rootwait rw init=/init' \
    -drive if=virtio,format=raw,file=scratch.ext4
grep -Fx 'CPS_VM_CAPTURE: immutable live snapshot staged without changing the running root' capture.log.clean
debugfs -R "dump /root.ext4 $work/staged.ext4" scratch.ext4
test -f staged.ext4
run boot.log staged.ext4 'root=/dev/vda rootfstype=ext4 rootwait ro init=/init-trial'
grep -Fx 'CPS_VM_CAPTURE_BOOT: booted the upgrade prepared from the running Btrfs root' boot.log.clean
echo 'Verified read-only live root capture, nested subvolume rejection, staging, export, and upgrade boot'
