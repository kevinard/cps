#!/usr/bin/env bash
set -euo pipefail
umask 022
project=$(cd -- "$(dirname -- "$0")/.." && pwd)
source "$project/tests/systemd-boot-guest.sh"
cps=$(realpath -- "${CPS_BIN:-$project/target/debug/cps}")
qemu=${CPS_VM_QEMU:-qemu-system-x86_64}
kernel=${CPS_VM_KERNEL:-/boot/vmlinuz-linux}
busybox=${CPS_VM_BUSYBOX:-/usr/lib/initcpio/busybox}
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd -- "$work"
mkdir -p root/{bin,etc,usr/bin,usr/local/libexec,dev,proc,sys,tmp,var/tmp} proto/bin proto/tmp
cc -O2 -Wall -Wextra -Werror -static "$project/tests/support/mode-probe.c" -o proto/bin/helper
cp proto/bin/helper root/usr/local/libexec/mode-probe
cp "$busybox" root/bin/busybox
ln -s busybox root/bin/sh
copy_runtime_deps root "$busybox"
strip --strip-debug -o root/usr/bin/cps "$cps"
copy_runtime_deps root "$cps"
for program in cp chmod mount; do
    binary=$(command -v "$program")
    cp "$binary" "root/usr/bin/$program"
    copy_runtime_deps root "$binary"
done
cp "$project/tests/file-modes-guest.sh" root/init
chmod +x root/init
printf 'root:x:0:0:root:/root:/bin/sh\n' > root/etc/passwd
printf 'root:x:0:\n' > root/etc/group
openssl genpkey -algorithm ED25519 -outform DER -out private.der
openssl pkey -inform DER -in private.der -pubout -outform DER -out root/public.der
printf 'protected\n' > proto/tmp/root-owned
for version in 1 2 3 4; do
    case $version in 1) mode=6755 ;; 2) mode=4755 ;; 3) mode=2755 ;; 4) mode=0755 ;; esac
    cat > modes.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "test/modes" publisher="modetest" version="$version.0.0-0" {
    file "bin/helper" mode="$mode" uid=0 gid=0
    hardlink "bin/helper-link" target="bin/helper"
    dir "tmp" mode="1777" uid=0 gid=0
    file "tmp/root-owned" mode="0644" uid=0 gid=0
    dir "var/shared" mode="2777" uid=0 gid=0
}
MANIFEST
    "$cps" publish --repo root/repository --proto proto --signing-key private.der modes.kdl
done
cat > groups.kdl <<'MANIFEST'
schema-version 1 kind="package-manifest"
package "test/groups" publisher="modetest" version="1.0.0-0" {
    dir "var/shared" mode="2777" uid=0 gid=0
}
MANIFEST
"$cps" publish --repo root/repository --proto proto --signing-key private.der groups.kdl
truncate -s 256M root.ext4
mke2fs -q -F -t ext4 -d root root.ext4
data_args=()
if [[ -n ${CPS_VM_QEMU_DATA:-} ]]; then data_args=(-L "$CPS_VM_QEMU_DATA"); fi
result=0
timeout "${CPS_VM_TIMEOUT:-180}" "$qemu" "${data_args[@]}" \
    -machine q35,accel=tcg -m 512 -smp 2 -nic none -display none -monitor none \
    -serial file:serial.log -kernel "$kernel" \
    -append 'root=/dev/vda rootfstype=ext4 rootwait rw console=ttyS0 loglevel=3 init=/init' \
    -drive if=virtio,format=raw,file=root.ext4 || result=$?
tr -d '\r' < serial.log > serial.clean
tail -40 serial.clean
test "$result" = 0
grep -Fx 'CPS_VM_MODES: verified privileged execution, sticky deletion, group inheritance, offline recovery, repair, clone isolation, and privilege removal' serial.clean
