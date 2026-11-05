#!/bin/sh
# Invoked by boot-capture-vm.sh from the running systemd guest.
set -eu
bb=/bin/busybox
export PATH=/usr/bin:/bin
trap 'echo "CPS_VM_SYSTEM: failed"; $bb poweroff -f' EXIT

verify_service() {
    attempts=0
    until [ -f /run/cps-demo-version ] && [ "$($bb cat /run/cps-demo-version)" = "$1" ]; do
        attempts=$((attempts + 1))
        test "$attempts" -lt 100
        $bb sleep 0.1
    done
    systemctl is-active --quiet cps-demo.service
}

version=$($bb cat /etc/cps-capture-version)
if [ "$version" = 2.0.0-0 ]; then
    if [ -f /etc/cps-btrfs-trial-kernel ]; then
        test "$($bb uname -r)" = "$($bb cat /etc/cps-btrfs-trial-kernel)"
        echo "CPS_VM_BTRFS_KERNEL: trial $($bb uname -r)"
    fi
    test "$(cps-demo)" = 2.0.0-0
    cps verify --root /
    systemctl is-active --quiet cps-boot-health.service
    systemctl is-active --quiet systemd-bless-boot.service
    echo 'CPS_VM_SYSTEM: staged upgrade booted with the live package'
    cps boot-env rollback --esp /efi --btrfs-volume /volume
    $bb sync
    exec $bb reboot -f
fi
test "$version" = 1.0.0-0
if [ -f /etc/cps-btrfs-base-kernel ]; then
    test "$($bb uname -r)" = "$($bb cat /etc/cps-btrfs-base-kernel)"
    echo "CPS_VM_BTRFS_KERNEL: previous $($bb uname -r)"
fi
if [ -f /volume/@captures/cli-upgrade ]; then
    test "$(cps-demo)" = 2.0.0-0
    cps verify --root /
    cps status | $bb grep -Fx 'No reboot pending.'
    echo 'CPS_VM_SYSTEM: rolled back to the base with the live package'
    $bb sync
    exec $bb poweroff -f
fi

$bb cp /repository/catalog-1.0.0-0 /repository/current
cps search cps-demo | $bb grep -F 'test/cps-demo@'
before=$(cps boot-env list --esp /efi)
cps install --dry-run test/cps-demo
test ! -e /usr/bin/cps-demo
cps install --yes test/cps-demo
test "$(cps-demo)" = 1.0.0-0
test "$(cps boot-env list --esp /efi)" = "$before"
systemctl start cps-demo.service
verify_service 1.0.0-0

# An upgrade staged before the live install must not lose the new package.
. /etc/cps-btrfs-boot
if cps boot-env activate-btrfs --root /volume/@captures/trial/root --btrfs-volume /volume \
    --esp /efi --root-partuuid "$root_uuid" > /run/rejected 2>&1; then exit 1; fi
$bb grep -F 'base changed after staging' /run/rejected

$bb cp /repository/catalog-2.0.0-0 /repository/current
cps upgrade --yes
test "$(cps-demo)" = 2.0.0-0
verify_service 2.0.0-0
test "$(cps boot-env list --esp /efi)" = "$before"

$bb cp /repository/catalog-base-upgrade /repository/current
if [ -f /etc/cps-btrfs-trial-kernel ]; then
    cps upgrade --dry-run | $bb grep -F 'changes the boot kernel or initramfs'
else
    cps upgrade --dry-run | $bb grep -F 'test/live-root declares requires-reboot in its manifest'
fi
cps upgrade --yes
test "$($bb cat /etc/cps-capture-version)" = 1.0.0-0
cps status | $bb grep -F 'Reboot pending into '
if cps install --yes test/cps-demo > /run/rejected 2>&1; then exit 1; fi
$bb grep -F 'a trial is pending' /run/rejected
: > /volume/@captures/cli-upgrade
echo 'CPS_VM_SYSTEM: installed and upgraded in place, then staged the base upgrade'
$bb sync
exec $bb reboot -f
