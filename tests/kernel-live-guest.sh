#!/bin/sh
set -eu

bb=/bin/busybox
cps=/usr/bin/cps
/bin/mount -n -t devtmpfs devtmpfs /dev
exec </dev/console >/dev/console 2>&1
trap 'echo CPS_VM_KERNEL: failed
for log in /run/build /run/mismatch /run/untrusted /run/untrusted-package; do
    if [ -f "$log" ]; then "$bb" cat "$log"; fi
done
"$bb" reboot -f' EXIT
/bin/mount -n -t proc proc /proc
/bin/mount -n -t sysfs sysfs /sys

if [ -f /receiving ]; then
    exec "$cps" kernel handover receive --session cps-vm -- /bin/workload receive
fi

sum() {
    line=$($bb sha256sum "$1")
    printf '%s\n' "${line%% *}"
}

baseline=$($bb cat /proc/cmdline)
boot_id=$($bb cat /proc/sys/kernel/random/boot_id)
build_id=$($bb cat /patch-build-id)
patch_sha=$(sum /patch.ko)
(
    while :; do
        "$bb" cat /proc/cmdline >> /run/workload
        "$bb" sleep 0.02
    done
) &
workload=$!
while [ ! -s /run/workload ]; do "$bb" sleep 0.02; done
if "$cps" kernel livepatch apply /patch.ko --sha256 "$patch_sha" --kernel-build-id 0 2>/run/build; then
    exit 1
fi
"$bb" grep -F 'the livepatch targets kernel build 0' /run/build
if "$cps" kernel livepatch apply /patch.ko --sha256 "$(printf '%064d' 0)" --kernel-build-id "$build_id" 2>/run/mismatch; then
    exit 1
fi
"$bb" grep -F 'SHA-256 mismatch' /run/mismatch
"$bb" cat /patch.ko > /run/untrusted.ko
# Appending a byte leaves .modinfo intact and makes the signature invalid to Linux.
printf 'x' >> /run/untrusted.ko
if "$cps" kernel livepatch apply /run/untrusted.ko --sha256 "$(sum /run/untrusted.ko)" --kernel-build-id "$build_id" 2>/run/untrusted; then
    exit 1
fi
"$bb" grep -F 'loading the signed livepatch module' /run/untrusted
if [ -f /livepatch-package ]; then
    spec=$($bb cat /livepatch-package)
    name=${spec#*/}
    image=/livepatch-image
    $bb mkdir "$image"
    "$cps" image create --root "$image" \
        --repo /livepatch-repository --trust-root /livepatch-trust.json
    "$cps" install --root "$image" --yes "$name"
    for dir in usr lib lib64; do
        if [ -d "/$dir" ]; then $bb cp -a "/$dir" "$image/"; fi
    done
    $bb mkdir -p "$image/proc" "$image/sys" "$image/run"
    /bin/mount -n --bind /proc "$image/proc"
    /bin/mount -n --bind /sys "$image/sys"
    patch="$image/usr/lib/livepatch/$build_id/livepatch_sample.ko"
    test "$(sum "$patch")" = "$patch_sha"
    printf 'x' >> "$patch"
    if $bb chroot "$image" "$cps" kernel livepatch apply-package "$name" 2>/run/untrusted-package; then
        exit 1
    fi
    $bb grep -F 'SHA-256 mismatch' /run/untrusted-package
    $bb cp /patch.ko "$patch"
    $bb chroot "$image" "$cps" kernel livepatch apply-package "$name"
    echo 'CPS_VM_LIVEPATCH: applied the signed installed package'
else
    "$cps" kernel livepatch apply /patch.ko --sha256 "$patch_sha" --kernel-build-id "$build_id"
fi
"$cps" kernel livepatch status --json | "$bb" grep -F '"enabled":true,"name":"livepatch_sample","transition":false'
while ! "$bb" grep -qx 'this has been live patched' /run/workload; do
    kill -0 "$workload"
    "$bb" sleep 0.02
done
"$cps" kernel livepatch disable livepatch_sample
lines=$($bb wc -l </run/workload)
while [ "$($bb wc -l </run/workload)" -le "$lines" ] || [ "$($bb tail -n 1 /run/workload)" != "$baseline" ]; do
    kill -0 "$workload"
    "$bb" sleep 0.02
done
test "$($bb cat /proc/cmdline)" = "$baseline"
test "$($bb cat /proc/sys/kernel/random/boot_id)" = "$boot_id"
kill -0 "$workload"
kill "$workload"
wait "$workload" || :
echo 'CPS_VM_LIVEPATCH: workload ran through enable and disable'

exec /bin/workload send "$cps" kernel handover start --session cps-vm --state-fd 3 \
    --kernel /next-kernel --kernel-sha256 "$(sum /next-kernel)" \
    --initrd /next-initrd --initrd-sha256 "$(sum /next-initrd)" \
    --cmdline 'console=ttyS0 loglevel=4 rdinit=/init kho=on liveupdate=on kho_scratch=128M,256M,128M nokaslr panic=1'
