#!/usr/bin/env bash
# Exercise the installed systemd tools on a cloned, disposable root.
set -euo pipefail

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=${CPS_BIN:-$project/target/debug/cps}
for command in openssl systemd-sysusers systemd-tmpfiles systemctl; do
    command -v "$command" >/dev/null || { echo "Missing command: $command" >&2; exit 1; }
done
work=$(mktemp -d /tmp/cps-systemd-actions.XXXXXX)
trap 'rm -rf -- "$work"' EXIT
cd -- "$work"

scope="cps-actions-$$"
user="cps-test-$$"
enabled="$scope.service"
masked="$scope-masked.service"
denied="$scope-denied.service"
existing="$scope-existing.service"
host_tmpfiles="$work/host-tmpfiles"
host_started="$work/host-started"
host_accounts=$(sha256sum /etc/passwd /etc/group)
uid=$(id -u)
gid=$(id -g)
account_uid=$uid
account_gid=$gid
if [[ $uid == 0 ]]; then
    account_uid=912
    account_gid=912
fi

mkdir -p proto/etc proto/usr/lib/{sysusers.d,tmpfiles.d,systemd/system,systemd/system-preset}
openssl genpkey -algorithm ED25519 -outform DER -out private.der
openssl pkey -inform DER -in private.der -pubout -outform DER -out public.der

write_unit() {
    cat > "proto/usr/lib/systemd/system/$1" <<UNIT
[Unit]
Description=$2

[Service]
Type=oneshot
ExecStart=/usr/bin/touch $host_started

[Install]
WantedBy=multi-user.target
UNIT
}

write_unit "$existing" 'Existing disabled service'
printf '[Unit]\nDescription=Fixture target\n' > proto/usr/lib/systemd/system/multi-user.target
cat > package.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "test/systemd-actions" publisher="canopa" version="1.0.0-0" {
    file "usr/lib/systemd/system/$existing" mode="0644" uid=$uid gid=$gid
    file "usr/lib/systemd/system/multi-user.target" mode="0644" uid=$uid gid=$gid
}
MANIFEST

publish() {
    "$cps" publish --repo repo --proto proto --signing-key private.der package.kdl
}
publish
"$cps" image-create --root base --repo repo --publisher canopa --trust-key public.der
"$cps" plan --root base --output install.json install test/systemd-actions
"$cps" apply --root base install.json

mkdir -p base/etc/{systemd/system,systemd/system-preset,tmpfiles.d}
ln -s /dev/null "base/etc/systemd/system/$masked"
printf 'disable %s\n' "$denied" > base/etc/systemd/system-preset/00-local.preset
cat > "base/etc/tmpfiles.d/$scope.conf" <<RULES
d /var/lib/$scope-local 0750 $user $user -
d $host_tmpfiles 0750 $user $user -
d /run/$scope 0755 - - -
RULES

printf 'g %s %s\nu %s %s:%s\n' "$user" "$account_gid" "$user" "$account_uid" "$account_gid" > "proto/usr/lib/sysusers.d/$scope.conf"
printf 'd /var/lib/%s-vendor 0755 - - -\n' "$scope" > "proto/usr/lib/tmpfiles.d/$scope.conf"
printf 'd /var/lib/%s-unrelated 0755 - - -\n' "$scope" > "proto/usr/lib/tmpfiles.d/$scope-unrelated.conf"
printf 'enable %s*\ndisable *\n' "$scope" > proto/usr/lib/systemd/system-preset/99-fixture.preset
for unit in "$existing" "$enabled" "$masked" "$denied"; do write_unit "$unit" "Version 2 of $unit"; done
: > proto/etc/passwd
cat > package.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "test/systemd-actions" publisher="canopa" version="2.0.0-0" {
    file "etc/passwd" mode="0644" uid=$uid gid=$gid preserve=#true
    file "usr/lib/sysusers.d/$scope.conf" mode="0644" uid=$uid gid=$gid
    sysusers "$scope.conf"
    file "usr/lib/tmpfiles.d/$scope.conf" mode="0644" uid=$uid gid=$gid
    tmpfiles "$scope.conf"
    file "usr/lib/tmpfiles.d/$scope-unrelated.conf" mode="0644" uid=$uid gid=$gid
    file "usr/lib/systemd/system-preset/99-fixture.preset" mode="0644" uid=$uid gid=$gid
    file "usr/lib/systemd/system/multi-user.target" mode="0644" uid=$uid gid=$gid
    file "usr/lib/systemd/system/$existing" mode="0644" uid=$uid gid=$gid
    preset "$existing"
    file "usr/lib/systemd/system/$enabled" mode="0644" uid=$uid gid=$gid
    preset "$enabled"
    file "usr/lib/systemd/system/$masked" mode="0644" uid=$uid gid=$gid
    preset "$masked"
    file "usr/lib/systemd/system/$denied" mode="0644" uid=$uid gid=$gid
    preset "$denied"
}
MANIFEST
publish
"$cps" plan --root base --output upgrade.json update | tee preview.log
grep -q "Offline systemd sysusers $scope.conf" preview.log
grep -q "Offline systemd tmpfiles $scope.conf" preview.log
grep -q "Offline systemd preset $enabled" preview.log
if grep -q "Offline systemd preset $existing" preview.log; then
    echo 'Planned a preset for an existing unit' >&2
    exit 1
fi

# An unrelated damaged file must stop validation and apply before journaling.
unchanged=usr/lib/systemd/system/multi-user.target
cp -a base drifted
printf 'unrelated drift\n' > "drifted/$unchanged"
for command in validate apply; do
    if "$cps" "$command" --root drifted upgrade.json > drift.log 2>&1; then
        echo "Expected $command to reject unrelated drift" >&2
        exit 1
    fi
    grep -Fq "pre-existing drift at $unchanged" drift.log
done
test ! -e "drifted/usr/lib/systemd/system/$enabled"
test ! -e drifted/etc/passwd
"$cps" transaction status --root drifted | grep -Fx 'No pending transaction'

"$cps" boot-env stage --source base --root staged upgrade.json
"$cps" verify --root staged
grep -q "^$user:x:$account_uid:$account_gid:" staged/etc/passwd
grep -q "^$user:x:$account_gid:" staged/etc/group
test "$(stat -c '%a %u:%g' "staged/var/lib/$scope-local")" = "750 $account_uid:$account_gid"
test -d "staged$host_tmpfiles"
test ! -e "staged/var/lib/$scope-vendor"
test ! -e "staged/var/lib/$scope-unrelated"
test ! -e "staged/run/$scope"
wants=etc/systemd/system/multi-user.target.wants
test "$(readlink "staged/$wants/$enabled")" = "/usr/lib/systemd/system/$enabled"
test "$(readlink "staged/etc/systemd/system/$masked")" = /dev/null
for unit in "$existing" "$masked" "$denied"; do test ! -L "staged/$wants/$unit"; done

# Fail the native tmpfiles step after sysusers has changed the managed
# passwd file. The same plan must resume offline without replaying file writes.
cp -a base recover
printf 'd /var/lib/%s-retry 0750 %s-missing - -\n' "$scope" "$user" > "recover/etc/tmpfiles.d/$scope.conf"
if "$cps" apply --root recover upgrade.json > retry.log 2>&1; then
    echo 'Expected tmpfiles to fail on an unknown account' >&2
    exit 1
fi
grep -q 'offline systemd tmpfiles failed' retry.log
grep -q "^$user:x:$account_uid:$account_gid:" recover/etc/passwd
if "$cps" verify --root recover > retry.log 2>&1; then
    echo 'Verified an incomplete transaction' >&2
    exit 1
fi
grep -q 'incomplete transaction' retry.log
cp "base/etc/tmpfiles.d/$scope.conf" recover/etc/tmpfiles.d/
"$cps" transaction status --root recover > recovery-status.log
grep -Fq 'Offline actions pending' recovery-status.log
grep -Fq 'Recover with transaction resume' recovery-status.log
# A change after the file phase can still leave plain resume stuck. Explicit
# repair uses the original journal and verifies the exact recovery payload.
cp -a base repair-pending
printf 'd /var/lib/%s-retry 0750 %s-missing - -\n' "$scope" "$user" > "repair-pending/etc/tmpfiles.d/$scope.conf"
if "$cps" apply --root repair-pending upgrade.json > retry.log 2>&1; then
    echo 'Expected tmpfiles to leave a pending transaction' >&2
    exit 1
fi
grep -q 'offline systemd tmpfiles failed' retry.log
cp "base/etc/tmpfiles.d/$scope.conf" repair-pending/etc/tmpfiles.d/
printf 'drift after file writes\n' > "repair-pending/$unchanged"
if "$cps" transaction resume --root repair-pending > drift.log 2>&1; then
    echo 'Expected ordinary resume to reject the damaged image' >&2
    exit 1
fi
grep -Fq "inconsistent image: $unchanged" drift.log
hash=$(sha256sum "base/$unchanged" | cut -d' ' -f1)
mv "repo/blobs/$hash" saved-blob
printf 'corrupt payload\n' > "repo/blobs/$hash"
if "$cps" transaction resume --root repair-pending --repair > drift.log 2>&1; then
    echo 'Repair accepted a corrupt recovery payload' >&2
    exit 1
fi
grep -Fxq 'drift after file writes' "repair-pending/$unchanged"
mv -f saved-blob "repo/blobs/$hash"
"$cps" transaction resume --root repair-pending --repair
"$cps" transaction status --root repair-pending | grep -Fx 'No pending transaction'
cmp "base/$unchanged" "repair-pending/$unchanged"
grep -q "^$user:x:$account_uid:$account_gid:" repair-pending/etc/passwd
"$cps" verify --root repair-pending

rm upgrade.json
mv repo repo-offline
"$cps" transaction resume --root recover
"$cps" transaction status --root recover | grep -Fx 'No pending transaction'
plan_id=$("$cps" transaction history --root recover --limit 1 | awk -F '\t' '$1 == 2 && $3 == "update" {print $2}')
"$cps" transaction show --root recover "$plan_id" > completed.json
grep -Fq '"systemd_actions": 5' completed.json
"$cps" verify --root recover
test "$(grep -c "^$user:" recover/etc/passwd)" = 1
test -d "recover$host_tmpfiles"
test -L "recover/$wants/$enabled"

test ! -e base/etc/passwd
test ! -e base/etc/group
test ! -e "base/var/lib/$scope-local"
test ! -e "base/$wants"
test ! -e "$host_tmpfiles"
test ! -e "$host_started"
test "$(sha256sum /etc/passwd /etc/group)" = "$host_accounts"
"$cps" verify --root base
echo 'Verified sysusers, tmpfiles, and presets only changed the staged root'

mv repo-offline repo
# A retired vendor unit must lose links created by its preset.
"$cps" plan --root staged --output remove.json remove test/systemd-actions
"$cps" apply --root staged remove.json
test ! -e "staged/usr/lib/systemd/system/$enabled"
test ! -L "staged/$wants/$enabled"
test "$(readlink "staged/etc/systemd/system/$masked")" = /dev/null

facet_rule="$scope-facet.conf"
printf 'd /var/lib/%s-facet 0755 - - -\n' "$scope" > "proto/usr/lib/tmpfiles.d/$facet_rule"
cat > package.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "test/optional-tmpfiles" publisher="canopa" version="1.0.0-0" {
    file "usr/lib/tmpfiles.d/$facet_rule" mode="0644" uid=$uid gid=$gid facet="optional"
    tmpfiles "$facet_rule"
}
MANIFEST
publish
"$cps" image-create --root facet-base --repo repo --publisher canopa --trust-key public.der --facet optional=false
"$cps" plan --root facet-base --output facet-install.json install test/optional-tmpfiles
"$cps" apply --root facet-base facet-install.json
"$cps" plan --root facet-base --output facet-enable.json change-facet optional=default | tee facet-preview.log
grep -Fq "Offline systemd tmpfiles $facet_rule" facet-preview.log
"$cps" boot-env stage --source facet-base --root facet-staged facet-enable.json
test -d "facet-staged/var/lib/$scope-facet"
test ! -e "facet-base/var/lib/$scope-facet"
test ! -e "/var/lib/$scope-facet"
"$cps" verify --root facet-staged
echo 'Verified a newly selected tmpfiles declaration only ran in the staged root'
