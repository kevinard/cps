#!/usr/bin/env bash
set -euo pipefail
umask 022
project=$(cd -- "$(dirname -- "$0")/.." && pwd)
source "$project/tests/systemd-boot-guest.sh"
cps=$(realpath -- "${CPS_BIN:-$project/target/debug/cps}")
kernel=$(realpath -- "${CPS_VM_KERNEL:?Set CPS_VM_KERNEL to the Livepatch and LUO-enabled source kernel}")
next_kernel=$(realpath -- "${CPS_VM_HANDOVER_KERNEL:?Set CPS_VM_HANDOVER_KERNEL to a different LUO-enabled kernel}")
patch=$(realpath -- "${CPS_VM_LIVEPATCH_MODULE:?Set CPS_VM_LIVEPATCH_MODULE to the signed livepatch-sample.ko for the source kernel}")
build_id=${CPS_VM_KERNEL_BUILD_ID:?Set CPS_VM_KERNEL_BUILD_ID to the GNU build ID of the source vmlinux}
busybox=$(realpath -- "${CPS_VM_BUSYBOX:-/usr/lib/initcpio/busybox}")
package_repo=${CPS_VM_LIVEPATCH_REPO:-}
if [[ -n $package_repo ]]; then
    package_repo=$(realpath -- "$package_repo")
    package_trust=$(realpath -- "${CPS_VM_LIVEPATCH_TRUST_ROOT:?Set CPS_VM_LIVEPATCH_TRUST_ROOT}")
    package=${CPS_VM_LIVEPATCH_PACKAGE:?Set CPS_VM_LIVEPATCH_PACKAGE to cps://PUBLISHER/PACKAGE}
    [[ $package == cps://*/* && $package != *@* ]] || { echo 'Expected cps://PUBLISHER/PACKAGE' >&2; exit 2; }
fi
mount=$(command -v mount)
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd -- "$work"

mkdir -p root/{bin,dev,proc,run,sys,usr/bin}
cp "$busybox" root/bin/busybox
ln -s busybox root/bin/sh
cp "$mount" root/bin/mount
strip --strip-all -o root/usr/bin/cps "$cps"
for binary in "$cps" "$mount" "$busybox"; do
    copy_runtime_deps root "$binary"
done
cc -O2 -Wall -Wextra -Werror -static "$project/tests/support/kernel-handover.c" -o root/bin/workload
install -m 755 "$project/tests/kernel-live-guest.sh" root/init

make_initramfs() {
    (cd root && find . -print0 | cpio --quiet --null -o -H newc --owner=0:0) | gzip -1 > "$1"
}

# Only the source archive contains the target artifacts or livepatch module.
# In particular, no archive contains the runtime workload state.
touch root/receiving
make_initramfs next.cpio.gz
rm root/receiving
cp next.cpio.gz root/next-initrd
cp "$next_kernel" root/next-kernel
cp "$patch" root/patch.ko
printf '%s\n' "$build_id" > root/patch-build-id
if [[ -n $package_repo ]]; then
    cp -a "$package_repo" root/livepatch-repository
    cp "$package_trust" root/livepatch-trust.json
    printf '%s\n' "${package#cps://}" > root/livepatch-package
fi
make_initramfs start.cpio.gz

result=0
timeout "${CPS_VM_TIMEOUT:-180}" "${CPS_VM_QEMU:-qemu-system-x86_64}" \
    -machine q35,accel=tcg -cpu max -m 1536 -smp 2 -nic none \
    -display none -monitor none -serial file:serial.log -no-reboot \
    -kernel "$kernel" -initrd start.cpio.gz \
    -append 'console=ttyS0 loglevel=4 rdinit=/init kho=on liveupdate=on kho_scratch=128M,256M,128M nokaslr panic=1' || result=$?
tr -d '\r' < serial.log > serial.clean
tail -60 serial.clean
test "$result" = 0
grep -Fx 'CPS_VM_LIVEPATCH: workload ran through enable and disable' serial.clean
if [[ -n $package_repo ]]; then
    grep -Fx 'CPS_VM_LIVEPATCH: applied the signed installed package' serial.clean
fi
awk '
    /^CPS_VM_HANDOVER: before / { nonce = $3; release = $4; boot = $5; count = $6; before++ }
    /^CPS_VM_HANDOVER: after / {
        after++
        if (NF == 6 && $3 == nonce && $4 != release && $5 != boot && $6 == count + 1) verified++
    }
    END { exit !(before == 1 && after == 1 && verified == 1) }
' serial.clean
