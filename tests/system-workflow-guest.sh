#!/bin/sh
# Invoked by boot-capture-vm.sh from the running systemd guest.
set -eu
bb=/bin/busybox
export PATH=/usr/bin:/bin
trap 'echo "CPS_VM_SYSTEM: failed"; $bb poweroff -f' EXIT

# Boot environments without the backups that each live change adds.
environments() {
    cps boot-env list --esp /efi | $bb grep -vF 'Canopa backup'
}

newest_backup() {
    cps boot-env list --esp /efi | $bb awk -F '\t' '$5 ~ /^Canopa backup/ { print $3; exit }'
}

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
    # The shutdown merge carried edits made after staging into the trial.
    test "$($bb cat /etc/cps-vm-after-staging)" = 'edited after staging'
    $bb grep -Fx 'alice:x:1000:1000::/home/alice:/bin/sh' /etc/passwd
    test ! -e /run/cps/finalize-trial
    echo 'CPS_VM_SYSTEM: /etc edits made after staging reached the trial'
    progress=$(cps status --plan /volume/@captures/upgrade.json)
    printf '%s\n' "$progress" | $bb grep -F '"phase": "booted_known_good"'
    printf '%s\n' "$progress" | $bb grep -F "$($bb cat /volume/@captures/upgrade-plan-id)"
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
if [ -f /volume/@captures/cli-upgrade ] && [ -f /volume/@captures/backup-rollback ] &&
    [ ! -f /volume/@captures/backup-booted ]; then
    # Running the backup taken before the live removal.
    test "$($bb cat /run/cps-vm-id)" = "$($bb cat /volume/@captures/backup-rollback)"
    test "$(cps-demo)" = 2.0.0-0
    btrfs property get / ro | $bb grep -Fx 'ro=false'
    cps verify --root /
    cps status | $bb grep -Fx 'No reboot pending.'
    echo 'CPS_VM_SYSTEM: booted the backup taken before a live removal'
    : > /volume/@captures/backup-booted
    cps boot-env rollback --esp /efi --btrfs-volume /volume "$($bb cat /volume/@captures/main-id)"
    $bb sync
    exec $bb reboot -f
fi
if [ -f /volume/@captures/backup-booted ]; then
    test "$($bb cat /run/cps-vm-id)" = "$($bb cat /volume/@captures/main-id)"
    test ! -e /usr/bin/cps-demo
    test -e /usr/bin/cps-demo-service
    $bb mv /repository/current /repository/offline-current
    printf '%s\n' 'mirror "test" root="missing-tuf-root.json" metadata="file:///missing/metadata/" targets="file:///missing/targets/"' >> /etc/cps/image.kdl
    before=$(environments)
    printf '%s\n' 'stage-package "test/cps-demo-service"' >> /etc/cps/image.kdl
    cps autoremove --dry-run | $bb grep -Fx 'A reboot will be required because test/cps-demo-service is configured for staged changes.'
    test "$(environments)" = "$before"
    cps autoremove --yes
    test -e /usr/bin/cps-demo-service
    cps status | $bb grep -F 'Reboot pending into '
    : > /volume/@captures/cli-autoremove
    $bb sync
    exec $bb reboot -f
fi
if [ -f /volume/@captures/cli-upgrade ]; then
    test "$(cps-demo)" = 2.0.0-0
    cps verify --root /
    cps status | $bb grep -Fx 'No reboot pending.'
    cps status --plan /volume/@captures/upgrade.json | $bb grep -F '"phase": "inactive_boot_environment"'
    echo 'CPS_VM_SYSTEM: rolled back to the base with the live package'

    # Removals must use installed state even when a configured mirror is unavailable.
    $bb cp /etc/cps/image.kdl /run/cps-image.kdl
    $bb mv /repository/current /repository/offline-current
    printf '%s\n' 'mirror "test" root="missing-tuf-root.json" metadata="file:///missing/metadata/" targets="file:///missing/targets/"' >> /etc/cps/image.kdl
    before=$(environments)
    test "$(cps autoremove --dry-run)" = 'Nothing to do.'
    if cps remove --dry-run test/cps-demo-service > /run/rejected 2>&1; then exit 1; fi
    $bb grep -F 'removal would break installed dependencies' /run/rejected
    demo_package=test/cps-demo
    if [ -f /etc/cps-btrfs-trial-kernel ]; then demo_package=cps://test/test/cps-demo; fi
    cps remove --dry-run test/cps-demo | $bb grep -Fx "Remove $demo_package 2.0.0-0"
    if cps remove test/cps-demo </dev/null > /run/rejected 2>&1; then exit 1; fi
    $bb grep -F 'use --yes to confirm' /run/rejected
    test "$(cps-demo)" = 2.0.0-0
    cps remove --yes test/cps-demo
    test ! -e /usr/bin/cps-demo
    test -e /usr/bin/cps-demo-service
    test "$(environments)" = "$before"

    # The live removal left a backup with the package. Boot it, then return.
    backup=$(newest_backup)
    test -n "$backup"
    found=false
    for directory in /volume/@captures/backup-*/root; do
        btrfs property get "$directory" ro | $bb grep -Fx 'ro=true'
        found=true
    done
    test "$found" = true
    cps boot-env list --esp /efi | $bb grep -F "$backup" | $bb grep -F 'known-good'
    if cps boot-env list --esp /efi | $bb grep -F "$backup" | $bb grep -q '^\*'; then exit 1; fi
    $bb cat /run/cps-vm-id > /volume/@captures/main-id
    printf '%s\n' "$backup" > /volume/@captures/backup-rollback
    # Undo the configuration edits on this root; the autoremove phase repeats them.
    $bb cp /run/cps-image.kdl /etc/cps/image.kdl
    $bb mv /repository/offline-current /repository/current
    cps boot-env rollback --esp /efi --btrfs-volume /volume "$backup"
    echo 'CPS_VM_SYSTEM: removed a package live after saving a backup environment'
    $bb sync
    exec $bb reboot -f
fi

if [ ! -f /volume/@captures/live-upgrade.json ]; then
    $bb cp /repository/catalog-1.0.0-0 /repository/current
    cps search cps-demo | $bb grep -F 'test/cps-demo@'
    before=$(environments)
    cps install --dry-run test/cps-demo
    cps install --stage --dry-run test/cps-demo | $bb grep -Fx 'A reboot will be required because --stage was requested.'
    test "$(environments)" = "$before"
    test ! -e /usr/bin/cps-demo
    cps install --yes --save-plan /run/cps-install.json test/cps-demo
    cps status --plan /run/cps-install.json | $bb grep -F '"phase": "applied"'
    test "$(cps-demo)" = 1.0.0-0
    test "$(environments)" = "$before"
    # The live install saved the previous root without selecting it.
    test -n "$(newest_backup)"
    $bb grep -Fx "default $($bb cat /volume/@captures/base-id).conf" /efi/loader/loader.conf
    test ! -e /run/cps/finalize-trial
    echo 'CPS_VM_SYSTEM: a live install saved a backup environment'
    systemctl start cps-demo.service
    verify_service 1.0.0-0
    printf 'damaged\n' > /usr/bin/cps-demo
    cps repair --dry-run --path /usr/bin/cps-demo | $bb grep -Fx 'Repair usr/bin/cps-demo'
    test "$($bb cat /usr/bin/cps-demo)" = damaged
    recovery=$(cps repair --yes --prepare-only --save-plan /run/cps-repair.json --path /usr/bin/cps-demo)
    recovery=$(printf '%s\n' "$recovery" | $bb sed -n 's/^Prepared recovery root //p')
    test -n "$recovery"
    cps status --plan /run/cps-repair.json | $bb grep -F '"phase": "prepared_for_boot"'
    test "$($bb cat /usr/bin/cps-demo)" = damaged
    test "$("$recovery/usr/bin/cps-demo")" = 1.0.0-0
    cps verify --root "$recovery"
    btrfs subvolume delete "$recovery"
    $bb rmdir "${recovery%/root}"
    cps repair --yes --path /usr/bin/cps-demo
    test "$(cps-demo)" = 1.0.0-0
    test "$(environments)" = "$before"
    echo 'CPS_VM_SYSTEM: verified staged and live repair of a damaged running image'
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
    test "$(environments)" = "$before"
    test "$(cps-demo)" = 1.0.0-0
    cps upgrade --dry-run --save-plan /volume/@captures/live-upgrade.json
    $bb sync
    : > /run/cps-demo-fail
    if cps upgrade --yes > /run/rejected 2>&1; then exit 1; fi
    $bb grep -F 'live systemd restart failed' /run/rejected
    cps status --plan /volume/@captures/live-upgrade.json | $bb grep -F '"phase": "applying"'
    if systemctl is-active --quiet cps-demo.service; then exit 1; fi
    exec $bb reboot -nf
fi
before=$(environments)
cps resume
cps status --plan /volume/@captures/live-upgrade.json | $bb grep -F '"phase": "applied"'
echo 'CPS_VM_SYSTEM: resumed a failed live restart after reboot'
test "$(cps-demo)" = 2.0.0-0
verify_service 2.0.0-0
test "$(environments)" = "$before"

$bb cp /repository/catalog-base-upgrade /repository/current
report=$(cps check-updates --json)
printf '%s\n' "$report" | $bb grep -F '"reboot_required": true'
test "$(environments)" = "$before"
if [ -f /etc/cps-btrfs-trial-kernel ]; then
    cps upgrade --dry-run | $bb grep -F 'changes the boot kernel or initramfs'
else
    cps upgrade --dry-run | $bb grep -F 'test/live-root declares requires-reboot in its manifest'
fi
cps upgrade --dry-run --save-plan /volume/@captures/upgrade.json
cps status --plan /volume/@captures/upgrade.json | $bb grep -F '"phase": "planned"'
cps status --plan /volume/@captures/upgrade.json --validate | $bb grep -F '"phase": "payload_validated"'
cps upgrade --yes --save-plan /volume/@captures/upgrade.json
progress=$(cps status --plan /volume/@captures/upgrade.json)
printf '%s\n' "$progress" | $bb grep -F '"phase": "staged_for_boot"'
printf '%s\n' "$progress" | $bb sed -n 's/^ *"plan_id": "\([0-9a-f]*\)",$/\1/p' > /volume/@captures/upgrade-plan-id
test -s /volume/@captures/upgrade-plan-id
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
# Edits made after staging reach the trial through the merge at shutdown.
test -e /run/cps/finalize-trial
attempts=0
until systemctl is-active --quiet cps-finalize-trial.service; do
    attempts=$((attempts + 1))
    test "$attempts" -lt 100
    $bb sleep 0.1
done
printf 'edited after staging\n' > /etc/cps-vm-after-staging
printf 'alice:x:1000:1000::/home/alice:/bin/sh\n' >> /etc/passwd
$bb sync
exec systemctl reboot
