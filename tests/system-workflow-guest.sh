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
    loader_before=$($bb sha256sum /efi/loader/loader.conf)
    cps boot-env rollback --esp /efi --btrfs-volume /volume --dry-run > /run/cps-rollback-preview
    $bb grep -F 'Would select cps-' /run/cps-rollback-preview
    $bb grep -F 'test/live-root:' /run/cps-rollback-preview
    test "$($bb sha256sum /efi/loader/loader.conf)" = "$loader_before"
    cps boot-env rollback --esp /efi --btrfs-volume /volume
    $bb sync
    exec $bb reboot -f
fi
test "$version" = 1.0.0-0
if [ -f /volume/@captures/cli-autoremove ]; then
    test ! -e /usr/bin/cps-demo
    test ! -e /usr/bin/cps-demo-service
    test -e /volume/@/usr/bin/cps-demo-service
    cps verify --root /
    systemctl is-active --quiet systemd-bless-boot.service
    echo 'CPS_VM_SYSTEM: staged autoremove booted with the dependency removed'
    $bb cp /repository/catalog-2.0.0-0 /repository/current
    $bb sed -i '/^stage-package "test\/cps-demo-service"$/d' /etc/cps/image.kdl
    cps install --no-refresh --dry-run test/cps-demo | $bb grep -Fx 'Changes will take effect on the running system.'
    cps install --no-refresh --stage --yes test/cps-demo
    test ! -e /usr/bin/cps-demo
    test ! -e /usr/bin/cps-demo-service
    cps status | $bb grep -F 'Reboot pending into '
    echo 'CPS_VM_SYSTEM: forced install staged without changing the running root'
    $bb sync
    exec $bb poweroff -f
fi
if [ -f /etc/cps-btrfs-base-kernel ]; then
    test "$($bb uname -r)" = "$($bb cat /etc/cps-btrfs-base-kernel)"
    echo "CPS_VM_BTRFS_KERNEL: previous $($bb uname -r)"
fi
if [ -f /volume/@captures/cli-upgrade ]; then
    test "$(cps-demo)" = 2.0.0-0
    cps verify --root /
    cps status | $bb grep -Fx 'No reboot pending.'
    echo 'CPS_VM_SYSTEM: rolled back to the base with the live package'

    # Removals must use installed state even when a configured mirror is unavailable.
    $bb mv /repository/current /repository/offline-current
    printf '%s\n' 'mirror "test" root="missing-tuf-root.json" metadata="file:///missing/metadata/" targets="file:///missing/targets/"' >> /etc/cps/image.kdl
    before=$(cps boot-env list --esp /efi)
    test "$(cps autoremove --dry-run)" = 'Nothing to do.'
    if cps remove --dry-run test/cps-demo-service > /run/rejected 2>&1; then exit 1; fi
    $bb grep -F 'removal would break installed dependencies' /run/rejected
    cps remove --dry-run test/cps-demo | $bb grep -Fx 'Remove cps://test/test/cps-demo 2.0.0-0'
    if cps remove test/cps-demo </dev/null > /run/rejected 2>&1; then exit 1; fi
    $bb grep -F 'use --yes to confirm' /run/rejected
    test "$(cps-demo)" = 2.0.0-0
    cps remove --yes test/cps-demo
    test ! -e /usr/bin/cps-demo
    test -e /usr/bin/cps-demo-service
    test "$(cps boot-env list --esp /efi)" = "$before"

    printf '%s\n' 'stage-package "test/cps-demo-service"' >> /etc/cps/image.kdl
    cps autoremove --dry-run | $bb grep -Fx 'A reboot will be required because test/cps-demo-service is configured for staged changes.'
    test "$(cps boot-env list --esp /efi)" = "$before"
    cps autoremove --yes
    test -e /usr/bin/cps-demo-service
    cps status | $bb grep -F 'Reboot pending into '
    : > /volume/@captures/cli-autoremove
    $bb sync
    exec $bb reboot -f
fi

$bb cp /repository/catalog-1.0.0-0 /repository/current
cps search cps-demo | $bb grep -F 'test/cps-demo@'
before=$(cps boot-env list --esp /efi)
cps install --dry-run test/cps-demo
cps install --stage --dry-run test/cps-demo | $bb grep -Fx 'A reboot will be required because --stage was requested.'
test "$(cps boot-env list --esp /efi)" = "$before"
test ! -e /usr/bin/cps-demo
cps install --yes test/cps-demo
test "$(cps-demo)" = 1.0.0-0
test "$(cps boot-env list --esp /efi)" = "$before"
systemctl start cps-demo.service
verify_service 1.0.0-0
report=$(cps check-updates --no-refresh --json)
printf '%s\n' "$report" | $bb grep -F '"status": "up_to_date"'
printf '%s\n' "$report" | $bb grep -F '"reboot_required": false'
test "$(cps upgrade --stage --dry-run)" = 'Nothing to do.'

# An upgrade staged before the live install must not lose the new package.
. /etc/cps-btrfs-boot
if cps boot-env activate-btrfs --root /volume/@captures/trial/root --btrfs-volume /volume \
    --esp /efi --root-partuuid "$root_uuid" > /run/rejected 2>&1; then exit 1; fi
$bb grep -F 'base changed after staging' /run/rejected

$bb cp /repository/catalog-2.0.0-0 /repository/current
report=$(cps check-updates --json)
printf '%s\n' "$report" | $bb grep -F '"status": "updates_available"'
printf '%s\n' "$report" | $bb grep -F '"name": "test/cps-demo"'
printf '%s\n' "$report" | $bb grep -F '"reboot_required": false'
cps upgrade --stage --dry-run | $bb grep -Fx 'A reboot will be required because --stage was requested.'
test "$(cps boot-env list --esp /efi)" = "$before"
test "$(cps-demo)" = 1.0.0-0
cps upgrade --yes
test "$(cps-demo)" = 2.0.0-0
verify_service 2.0.0-0
test "$(cps boot-env list --esp /efi)" = "$before"

$bb cp /repository/catalog-base-upgrade /repository/current
report=$(cps check-updates --json)
printf '%s\n' "$report" | $bb grep -F '"reboot_required": true'
test "$(cps boot-env list --esp /efi)" = "$before"
if [ -f /etc/cps-btrfs-trial-kernel ]; then
    cps upgrade --dry-run | $bb grep -F 'changes the boot kernel or initramfs'
else
    cps upgrade --dry-run | $bb grep -F 'test/live-root declares requires-reboot in its manifest'
fi
cps upgrade --yes
test "$($bb cat /etc/cps-capture-version)" = 1.0.0-0
cps status | $bb grep -F 'Reboot pending into '
code=0
report=$(cps check-updates --json) || code=$?
test "$code" = 1
printf '%s\n' "$report" | $bb grep -F '"status": "blocked"'
printf '%s\n' "$report" | $bb grep -F 'a trial is pending'
if cps install --yes test/cps-demo > /run/rejected 2>&1; then exit 1; fi
$bb grep -F 'a trial is pending' /run/rejected
: > /volume/@captures/cli-upgrade
echo 'CPS_VM_SYSTEM: installed and upgraded in place, then staged the base upgrade'
$bb sync
exec $bb reboot -f
