#!/usr/bin/env bash
# Capture a running Btrfs root and boot the staged subvolume and ext4 export.
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
native_boot=${CPS_VM_BTRFS_BOOT:-0}
health_fail=${CPS_VM_BTRFS_HEALTH_FAIL:-0}
destroy=${CPS_VM_BTRFS_DESTROY:-0}
destroy_failed=${CPS_VM_BTRFS_DESTROY_FAILED:-0}
staged_transactions=${CPS_VM_STAGED_TRANSACTIONS:-0}
system_commands=${CPS_VM_SYSTEM_COMMANDS:-0}
if [[ $health_fail != 0 && $health_fail != 1 && $health_fail != readiness ]]; then
    echo 'CPS_VM_BTRFS_HEALTH_FAIL must be 0, 1, or readiness' >&2
    exit 2
fi
if [[ $system_commands == 1 && ( $native_boot != 1 || $health_fail != 0 || $destroy != 0 || $destroy_failed != 0 || $staged_transactions != 0 ) ]]; then
    echo 'CPS_VM_SYSTEM_COMMANDS requires a successful native trial' >&2
    exit 2
fi
if [[ $staged_transactions == 1 && ( $native_boot != 1 || $health_fail != 0 || $destroy != 0 || $destroy_failed != 0 ) ]]; then
    echo 'CPS_VM_STAGED_TRANSACTIONS requires a successful native trial without destruction' >&2
    exit 2
fi
if [[ $destroy == 1 && ( $native_boot != 1 || $health_fail != 0 ) ]]; then
    echo 'CPS_VM_BTRFS_DESTROY requires native boot without health failure' >&2
    exit 2
fi
if [[ $destroy_failed == 1 && ( $native_boot != 1 || $health_fail != 1 || $destroy != 0 ) ]]; then
    echo 'CPS_VM_BTRFS_DESTROY_FAILED requires a failed native trial without CPS_VM_BTRFS_DESTROY' >&2
    exit 2
fi
kernel_fixture=${CPS_VM_KERNEL_FIXTURE:-}
kernel_package=
kernel_name=
trial_release=
if [[ -n $kernel_fixture ]]; then
    [[ $native_boot == 1 ]] || { echo 'CPS_VM_KERNEL_FIXTURE requires CPS_VM_BTRFS_BOOT=1' >&2; exit 2; }
    kernel_fixture=$(realpath -- "$kernel_fixture")
    kernel_package=$(cat "$kernel_fixture/kernel-package")
    kernel_name=${kernel_package%@*}
    kernel_name=${kernel_name#cps://canopa-kernel/}
    trial_release=$(cat "$kernel_fixture/kernel-release")
    grep -Fx 'CONFIG_BTRFS_FS=y' "$kernel_fixture/image/usr/lib/modules/$trial_release/config"
    "$cps" repo attestation --repo "$kernel_fixture/repo" --publisher canopa-kernel \
        --trust-key "$kernel_fixture/release-public.der" "$kernel_package" >/dev/null
    test "$trial_release" != "${CPS_VM_KERNEL_RELEASE:-$(uname -r)}"
fi
root_device=/dev/vda
root_uuid=7adff456-af50-498f-b39a-dcf87cb4a7c1
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
    if [[ $system_commands == 1 && $1 != 1.0.0-0 ]]; then
        sed -i '/^package /s/ {$/ requires-reboot=#true {/' manifest.kdl
    fi
    if [[ $1 != 1.0.0-0 && -n $kernel_package ]]; then
        sed '$d' manifest.kdl > linked.kdl
        printf '    require "%s" version="=%s"\n}\n' "$kernel_name" "${kernel_package##*@}" >> linked.kdl
        mv linked.kdl manifest.kdl
    fi
    "$cps" publish --repo repo --proto proto --signing-key private.der manifest.kdl
}
publish 1.0.0-0
mkdir files
root="$work/files/@"
"$cps" image-create --root "$root" --repo "$work/repo" --publisher test --trust-key public.der
"$cps" plan --root "$root" --output install.json install test/live-root
"$cps" apply --root "$root" install.json
if [[ -n $kernel_fixture ]]; then
    cat > kernel-builders.kdl <<POLICY
schema-version 1
build-trust {
    builder-key "$kernel_fixture/builder-public.der"
}
POLICY
    "$cps" publisher add --root "$root" --repo "$kernel_fixture/repo" \
        --trust-key "$kernel_fixture/release-public.der" --build-trust kernel-builders.kdl canopa-kernel
fi
if [[ $system_commands == 1 ]]; then
    mkdir -p app/usr/bin service/usr/{bin,lib/systemd/system}
    cat > service/usr/lib/systemd/system/cps-demo.service <<'UNIT'
[Service]
ExecStart=/usr/bin/cps-demo-service
UNIT
    for version in 1.0.0-0 2.0.0-0; do
        printf '#!/bin/sh\nprintf "%%s\\n" %s\n' "$version" > app/usr/bin/cps-demo
        printf '#!/bin/sh\nprintf "%%s\\n" %s > /run/cps-demo-version\nexec /bin/busybox sleep 600\n' \
            "$version" > service/usr/bin/cps-demo-service
        chmod 755 app/usr/bin/cps-demo service/usr/bin/cps-demo-service
        "$cps" manifest generate --proto service --publisher test --version "$version" \
            --uid "$(id -u)" --gid "$(id -g)" test/cps-demo-service > service.kdl
        "$cps" publish --repo repo --proto service --signing-key private.der service.kdl
        "$cps" manifest generate --proto app --publisher test --version "$version" \
            --uid "$(id -u)" --gid "$(id -g)" test/cps-demo > app.kdl
        sed -i '$i\    require "test/cps-demo-service" version="='"$version"'"' app.kdl
        "$cps" publish --repo repo --proto app --signing-key private.der app.kdl
        cp repo/current "catalog-$version"
    done
fi
publish 2.0.0-0
mkdir -p "$root"/{bin,usr/bin,proc,sys,dev,tmp,captures,scratch,repository} files/@captures
cp -a repo/. "$root/repository/"
printf '2.0.0-0\n' > "$root/etc/cps-btrfs-trial-version"
if [[ $staged_transactions == 1 ]]; then
    cp repo/current before-review-catalog
    publish 3.0.0-0
    cp -a repo/. "$root/repository/"
    mv "$root/repository/current" "$root/repository/review-catalog"
    cp before-review-catalog "$root/repository/current"
    touch "$root/etc/cps-btrfs-staged-transactions"
    printf '3.0.0-0\n' > "$root/etc/cps-btrfs-trial-version"
fi
cp "$busybox" "$root/bin/busybox"
ln -s busybox "$root/bin/sh"
copy_runtime_deps "$root" "$busybox"
strip --strip-debug -o "$root/usr/bin/cps" "$cps"
copy_runtime_deps "$root" "$cps"
cp "$btrfs" "$root/usr/bin/btrfs"
copy_runtime_deps "$root" "$btrfs"
for tool in cp mke2fs e2fsck mount umount find sha256sum sfdisk flock; do
    binary=$(command -v "$tool")
    cp "$binary" "$root/usr/bin/$tool"
    copy_runtime_deps "$root" "$binary"
done
if [[ -n $kernel_fixture ]]; then
    cp -a "$kernel_fixture/repo" "$root/kernel-repository"
    mkdir -p "$root"/usr/lib/kernel/install.d "$root/vm-modules/$trial_release"
    : > "$root/vm-modules/$trial_release/load"
    cp /usr/lib/kernel/install.d/90-loaderentry.install "$root/usr/lib/kernel/install.d/"
    for tool in kernel-install install chown mkdir grep tr stat rm cmp; do
        binary=$(command -v "$tool")
        cp "$binary" "$root/usr/bin/$tool"
        copy_runtime_deps "$root" "$binary"
    done
    printf '%s\n' "${CPS_VM_KERNEL_RELEASE:-$(uname -r)}" > "$root/etc/cps-btrfs-base-kernel"
    printf '%s\n' "$trial_release" > "$root/etc/cps-btrfs-trial-kernel"
fi
if [[ $native_boot == 1 ]]; then
    setup_systemd_guest "$root"
    if [[ $system_commands == 1 ]]; then
        mkdir -p "$root/etc/cps"
        cp public.der "$root/etc/cps/test.der"
        cp catalog-{1.0.0-0,2.0.0-0} "$root/repository/"
        cp repo/current "$root/repository/catalog-base-upgrade"
        install -m755 "$project/tests/system-workflow-guest.sh" "$root/usr/local/libexec/cps-system-workflow"
        cat > "$root/etc/cps/image.kdl" <<CONFIG
schema-version 1
publisher "test" {
    repository "/repository"
    trust-key "test.der"
}
boot-btrfs volume="/volume" environments="/volume/@captures" esp="/efi" root-partuuid="$root_uuid" options="rw console=ttyS0 loglevel=3 init=/init-systemd"
CONFIG
    fi
    if [[ $destroy == 1 ]]; then
        touch "$root/etc/cps-btrfs-destroy"
        install -m755 "$project/tests/boot-destroy-guest.sh" "$root/usr/local/libexec/cps-vm-destroy"
        cc -O2 -Wall -Wextra "$project/tests/support/deny-unlink.c" -o "$root/usr/local/libexec/cps-vm-deny-unlink"
        copy_runtime_deps "$root" "$root/usr/local/libexec/cps-vm-deny-unlink"
    fi
    if [[ $destroy_failed == 1 ]]; then touch "$root/etc/cps-btrfs-destroy-failed"; fi
    mkdir -p "$root"/{volume,efi,boot,run,var/tmp}
    cp "$kernel" "$root/boot/vmlinuz"
    cp "$project/packaging/systemd/cps-boot-health.service" "$root/usr/lib/systemd/system/"
    mkdir -p "$root/usr/lib/systemd/system/boot-complete.target.requires"
    ln -s ../cps-boot-health.service "$root/usr/lib/systemd/system/boot-complete.target.requires/"
    cat > "$root/etc/cps-btrfs-boot" <<GUEST
root_device=/dev/vda2
root_uuid=$root_uuid
GUEST
    root_device=/dev/vda2
    if [[ $health_fail != 0 ]]; then
        printf '%s\n' "$health_fail" > "$root/etc/cps-btrfs-fail-health"
    fi
    if [[ $health_fail == readiness ]]; then
        mkdir -p "$root/etc/systemd/system/cps-boot-health.service.d"
        cat > "$root/etc/systemd/system/cps-boot-health.service.d/readiness.conf" <<'UNIT'
[Service]
ExecStartPost=/usr/local/libexec/cps-test-readiness
UNIT
        cat > "$root/usr/local/libexec/cps-test-readiness" <<'GUEST'
#!/bin/sh
bb=/bin/busybox
test "$($bb cat /etc/cps-capture-version)" != "$($bb cat /etc/cps-btrfs-trial-version)"
GUEST
        chmod 0755 "$root/usr/local/libexec/cps-test-readiness"
    fi
    cat > "$root/init-systemd" <<'GUEST'
#!/bin/sh
set -eu
bb=/bin/busybox
export PATH=/usr/bin:/bin
mount -t devtmpfs devtmpfs /dev
exec </dev/console >/dev/console 2>&1
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t cgroup2 cgroup2 /sys/fs/cgroup
mount -t efivarfs efivarfs /sys/firmware/efi/efivars
mount -t tmpfs tmpfs /run
mount -t tmpfs tmpfs /tmp
while read -r module name; do insmod "$module"; done < "/vm-modules/$($bb uname -r)/load"
mount -t vfat /dev/vda1 /efi
mount -t btrfs -o subvolid=5 /dev/vda2 /volume
if [ -f /etc/cps-btrfs-fail-health ] && [ "$($bb cat /etc/cps-btrfs-fail-health)" = 1 ] &&
    [ "$($bb cat /etc/cps-capture-version)" = "$($bb cat /etc/cps-btrfs-trial-version)" ]; then
    $bb chmod 0600 /volume/@captures/trial/root/etc/cps-capture-version
fi
flags=
for arg in $($bb cat /proc/cmdline); do
    case "$arg" in rootflags=*) flags=$arg ;; esac
done
test -n "$flags"
for entry in /efi/loader/entries/cps-*.conf; do
    test -f "$entry" || continue
    if $bb grep -F " $flags " "$entry"; then
        name=${entry##*/}
        id=${name%%+*}
        echo "${id%.conf}" > /run/cps-vm-id
        case "$name" in *+*) : > /run/cps-vm-counted ;; esac
    fi
done
exec /usr/lib/systemd/systemd
GUEST
    cat > "$root/usr/local/libexec/cps-vm-observe" <<'GUEST'
#!/bin/sh
set -eu
bb=/bin/busybox
export PATH=/usr/bin:/bin
if [ -x /usr/local/libexec/cps-system-workflow ]; then
    exec /usr/local/libexec/cps-system-workflow
fi
version=$($bb cat /etc/cps-capture-version)
trial_version=$($bb cat /etc/cps-btrfs-trial-version)
if [ -f /etc/cps-btrfs-trial-kernel ]; then
    if [ "$version" = "$trial_version" ]; then
        test "$($bb uname -r)" = "$($bb cat /etc/cps-btrfs-trial-kernel)"
        audit=$(cps publisher audit-builds --root / canopa-kernel)
        echo "$audit"
        test "$audit" = 'Verified build evidence for 1 installed package(s) from canopa-kernel'
        echo "CPS_VM_BTRFS_KERNEL: trial $($bb uname -r)"
    else
        test "$($bb uname -r)" = "$($bb cat /etc/cps-btrfs-base-kernel)"
        echo "CPS_VM_BTRFS_KERNEL: previous $($bb uname -r)"
    fi
fi
if [ "$version" = "$trial_version" ] && [ -f /etc/cps-btrfs-fail-health ]; then
    if [ "$($bb cat /etc/cps-btrfs-fail-health)" = readiness ]; then
        cps verify --root /
        echo 'CPS_VM_BTRFS_BOOT: image verified but readiness failed'
    else
        if cps verify --root / > /run/rejected 2>&1; then exit 1; fi
        $bb grep -F 'etc/cps-capture-version' /run/rejected
    fi
    systemctl is-failed --quiet cps-boot-health.service
    if systemctl is-active --quiet systemd-bless-boot.service; then exit 1; fi
    echo 'CPS_VM_BTRFS_BOOT: trial health failed without blessing'
    $bb sync
    exec $bb reboot -f
fi
cps verify --root /
if [ -f /etc/cps-btrfs-destroy ] && [ ! -f /run/cps-vm-counted ]; then
    exec /usr/local/libexec/cps-vm-destroy
fi
if [ "$version" = "$trial_version" ]; then
    test "$($bb cat /etc/cps-capture-local)" = 'captured local state'
    if [ -f /run/cps-vm-counted ]; then
        systemctl is-active --quiet cps-boot-health.service
        systemctl is-active --quiet systemd-bless-boot.service
        echo 'CPS_VM_BTRFS_BOOT: trial blessed'
    else
        echo 'CPS_VM_BTRFS_BOOT: trial default after reboot'
        cps boot-env list --esp /efi
        before=$(sha256sum /efi/loader/loader.conf)
        if cps boot-env rollback --esp /efi > /run/rejected 2>&1; then exit 1; fi
        $bb grep -F 'use --btrfs-volume' /run/rejected
        test "$(sha256sum /efi/loader/loader.conf)" = "$before"
        parent=$($bb cat /volume/@captures/base-id)
        entry="/efi/loader/entries/$parent.conf"
        $bb cp "$entry" /run/base-entry
        $bb sed 's/^# cps-btrfs-uuid .*/# cps-btrfs-uuid 11111111-1111-4111-8111-111111111111/' /run/base-entry > "$entry"
        if cps boot-env rollback --esp /efi --btrfs-volume /volume > /run/rejected 2>&1; then exit 1; fi
        $bb grep -F 'Btrfs subvolume identity differs' /run/rejected
        test "$(sha256sum /efi/loader/loader.conf)" = "$before"
        $bb cat /run/base-entry > "$entry"
        cps boot-env rollback --esp /efi --btrfs-volume /volume
    fi
    $bb sync
    exec $bb reboot -f
fi
test "$version" = 1.0.0-0
test "$($bb cat /etc/cps-capture-local)" = 'running local state'
if [ -f /etc/cps-btrfs-fail-health ]; then
    echo 'CPS_VM_BTRFS_BOOT: previous root after exhausted trial'
else
    echo 'CPS_VM_BTRFS_BOOT: previous root after rollback'
fi
if [ -f /etc/cps-btrfs-destroy-failed ]; then
    found=false
    for entry in /efi/loader/entries/cps-*+0-*.conf /efi/loader/entries/cps-*+0.conf; do
        test -f "$entry" || continue
        id=${entry##*/}
        id=${id%%+*}
        cps boot-env destroy-btrfs --esp /efi --btrfs-volume /volume "$id"
        test ! -e "/efi/EFI/cps/$id"
        found=true
    done
    test "$found" = true
    test ! -e /volume/@captures/trial/root
    cps verify --root /
    echo 'CPS_VM_BTRFS_DESTROY: exhausted failed trial removed'
fi
$bb sync
exec $bb poweroff -f
GUEST
    chmod +x "$root/init-systemd"
fi
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
root_device=/dev/vda
if [ -f /etc/cps-btrfs-boot ]; then
    . /etc/cps-btrfs-boot
    while read -r module name; do insmod "$module"; done < "/vm-modules/$($bb uname -r)/load"
    mount -t vfat /dev/vda1 /efi
    mount -t btrfs -o subvolid=5 "$root_device" /volume
fi
mount -t btrfs -o subvol=@captures "$root_device" /captures
mount -t ext4 /dev/vdb /scratch
if [ -f /etc/cps-btrfs-boot ]; then
    boot_files=$(sha256sum /boot/vmlinuz /efi/EFI/BOOT/BOOTX64.EFI)
    boot_paths=$(find /boot /efi -print)
fi
if [ -f /etc/cps-btrfs-trial-kernel ]; then
    cps publisher set-repository --root / --repo test=/repository --repo canopa-kernel=/kernel-repository
else
    cps publisher set-repository --root / --repo test=/repository
fi
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
btrfs subvolume snapshot /captures/base/root /captures/writable
if cps boot-env stage-btrfs --source /captures/writable --output /captures/rejected /captures/upgrade.json > /tmp/rejected 2>&1; then exit 1; fi
$bb grep -F 'requires a read-only baseline' /tmp/rejected
btrfs subvolume delete /captures/writable
blob=$(printf '2.0.0-0\n' | sha256sum)
blob=${blob%% *}
$bb mv "/repository/blobs/$blob" /tmp/payload
if cps boot-env stage-btrfs --source /captures/base/root --output /captures/rejected /captures/upgrade.json > /tmp/rejected 2>&1; then exit 1; fi
$bb grep -F 'opening repository payload' /tmp/rejected
test ! -e /captures/rejected
test -z "$(find /captures -maxdepth 1 -name '.cps-capture-*')"
$bb mv /tmp/payload "/repository/blobs/$blob"
cps boot-env stage-btrfs --source /captures/base/root --output /captures/trial /captures/upgrade.json > /tmp/first 2>&1 &
first=$!
cps boot-env stage-btrfs --source /captures/base/root --output /captures/trial /captures/upgrade.json > /tmp/second 2>&1 &
second=$!
successes=0
if wait "$first"; then successes=$((successes + 1)); fi
if wait "$second"; then successes=$((successes + 1)); fi
test "$successes" = 1
test -z "$(find /captures -maxdepth 1 -name '.cps-capture-*')"
stage=/captures/trial/root
btrfs property get "$stage" ro | $bb grep -Fx 'ro=false'
test "$($bb stat -c %i "$stage")" = 256
test "$($bb cat "$stage/etc/cps-capture-version")" = 2.0.0-0
if [ -f /etc/cps-btrfs-staged-transactions ]; then
    cps plan --root "$stage" --output /tmp/review-freeze.json freeze --reason 'Review staged desktop' test/live-root
    cps apply --root "$stage" /tmp/review-freeze.json
    $bb cat /repository/review-catalog > /repository/current
    cps plan --root "$stage" --output /tmp/held.json update
    cps plan --root "$stage" --output /tmp/unfreeze.json unfreeze test/live-root
    cps apply --root "$stage" /tmp/unfreeze.json
    if cps apply --root "$stage" /tmp/held.json > /tmp/rejected 2>&1; then exit 1; fi
    $bb grep -F 'stale plan' /tmp/rejected
    cps plan --root "$stage" --output /tmp/review-upgrade.json update
    cps apply --root "$stage" /tmp/review-upgrade.json
    test "$($bb cat "$stage/etc/cps-capture-version")" = 3.0.0-0
    echo 'CPS_VM_STAGED_TRANSACTIONS: unfroze and upgraded the private Btrfs root'
fi
test "$($bb cat /captures/base/root/etc/cps-capture-version)" = 1.0.0-0
test "$($bb cat /etc/cps-capture-version)" = 1.0.0-0
test "$(sha256sum /.cps/state.sqlite)" = "$before"
if [ -f /etc/cps-btrfs-boot ]; then
    test "$(sha256sum /boot/vmlinuz /efi/EFI/BOOT/BOOTX64.EFI)" = "$boot_files"
    test "$(find /boot /efi -print)" = "$boot_paths"
fi
cps verify --root "$stage"
if [ -f /etc/cps-btrfs-trial-kernel ]; then
    audit=$(cps publisher audit-builds --root "$stage" canopa-kernel)
    echo "$audit"
    test "$audit" = 'Verified build evidence for 1 installed package(s) from canopa-kernel'
    release=$($bb cat /etc/cps-btrfs-trial-kernel)
    found=false
    for prepared in "$stage"/.cps/kernel-*/linux; do
        if cmp -s "$prepared" "$stage/usr/lib/modules/$release/vmlinuz"; then found=true; fi
    done
    test "$found" = true
fi
cps boot-env export --root "$stage" --output /scratch/root.ext4 --size-mib 256 \
    --receipt > /scratch/root.kdl
test "$($bb cat "$stage/etc/cps-capture-local")" = 'captured local state'
echo 'CPS_VM_CAPTURE: immutable live snapshot staged without changing the running root'
if [ -f /etc/cps-btrfs-boot ]; then
    boot_options='console=ttyS0 loglevel=3 init=/init-systemd'
    if [ -x /usr/local/libexec/cps-system-workflow ]; then boot_options="rw $boot_options"; fi
    base=$(cps boot-env activate-btrfs --known-good --root /volume/@ --btrfs-volume /volume \
        --esp /efi --root-partuuid "$root_uuid" --options "$boot_options")
    base=${base#Activated }
    echo "$base" > /captures/base-id
    if [ -x /usr/local/libexec/cps-system-workflow ]; then
        $bb chown 0:0 /captures
        $bb sync
        exit 0
    fi
    cps boot-env capture --source / --output /captures/protected
    cps boot-env stage-btrfs --source /captures/protected/root --output /captures/protected-trial /captures/upgrade.json
    test "$($bb cat /captures/protected-trial/root/etc/cps-capture-version)" = 2.0.0-0
    btrfs subvolume delete /captures/protected-trial/root /captures/protected/root
    $bb rmdir /captures/protected-trial /captures/protected
    before=$(sha256sum /efi/loader/loader.conf)
    if cps boot-env activate-btrfs --root /volume/@captures/trial/root --btrfs-volume /volume/@ \
        --esp /efi --root-partuuid "$root_uuid" > /tmp/rejected 2>&1; then exit 1; fi
    $bb grep -F 'top-level subvolume' /tmp/rejected
    test "$(sha256sum /efi/loader/loader.conf)" = "$before"
    cps plan --root /volume/@captures/trial/root --output /tmp/mark.json mark explicit test/live-root
    staged_state=$(sha256sum /volume/@captures/trial/root/.cps/state.sqlite)
    $bb mkfifo /tmp/release
    flock --shared /volume/@captures/trial/root/.cps/lock sh -c ': > /tmp/locked; read released < /tmp/release' &
    holding=$!
    while [ ! -e /tmp/locked ]; do $bb sleep 0.01; done
    cps apply --root /volume/@captures/trial/root /tmp/mark.json > /tmp/race-rejected 2>&1 &
    applying=$!
    attempts=0
    while ! $bb grep -q locks "/proc/$applying/wchan"; do
        attempts=$((attempts + 1))
        test "$attempts" -lt 1000
        $bb sleep 0.01
    done
    cps boot-env activate-btrfs --root /volume/@captures/trial/root --btrfs-volume /volume \
        --esp /efi --root-partuuid "$root_uuid" --options 'console=ttyS0 loglevel=3 init=/init-systemd'
    echo released > /tmp/release
    wait "$holding"
    if wait "$applying"; then exit 1; fi
    $bb grep -F 'registered Btrfs root' /tmp/race-rejected
    if cps apply --root /volume/@captures/trial/root /tmp/mark.json > /tmp/rejected 2>&1; then exit 1; fi
    $bb grep -F 'registered Btrfs root' /tmp/rejected
    test "$(sha256sum /volume/@captures/trial/root/.cps/state.sqlite)" = "$staged_state"
    cps boot-env list --esp /efi
    umount /efi
    umount /volume
fi
umount /scratch
umount /captures
$bb sync
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
test "$($bb cat /etc/cps-capture-version)" = "$($bb cat /etc/cps-btrfs-trial-version)"
test "$($bb cat /etc/cps-capture-local)" = 'captured local state'
cps verify --root /
echo 'CPS_VM_CAPTURE_BOOT: booted the upgrade prepared from the running Btrfs root'
GUEST
cat > "$root/init-original" <<'GUEST'
#!/bin/sh
set -eu
bb=/bin/busybox
trap '$bb poweroff -f' EXIT
export PATH=/usr/bin:/bin
mount -t devtmpfs devtmpfs /dev
mount -t proc proc /proc
mount -t sysfs sysfs /sys
test "$($bb cat /etc/cps-capture-version)" = 1.0.0-0
test "$($bb cat /etc/cps-capture-local)" = 'running local state'
cps verify --root /
echo 'CPS_VM_BTRFS_ORIGINAL: booted the previous root after staging its upgrade'
GUEST
chmod +x "$root/init" "$root/init-trial" "$root/init-original"
truncate -s 512M source.btrfs
"$formatter" -q -f --rootdir files --subvol default:@ --subvol rw:@captures source.btrfs
root_drive=source.btrfs
if [[ $native_boot == 1 ]]; then
    truncate -s 640M disk.raw
    sfdisk --quiet disk.raw <<PARTITIONS
label: gpt
unit: sectors
start=2048,size=196608,type=uefi,uuid=9a138d0b-f9aa-4a2a-b527-93392ad7e812
start=198656,size=1048576,type=linux,uuid=$root_uuid
PARTITIONS
    mkdir -p esp/EFI/BOOT
    cp "${CPS_VM_SYSTEMD_BOOT:-/usr/lib/systemd/boot/efi/systemd-bootx64.efi}" esp/EFI/BOOT/BOOTX64.EFI
    truncate -s 96M esp.fat
    mkfs.fat -F 32 esp.fat >/dev/null
    mcopy -s -i esp.fat esp/EFI ::/
    dd if=esp.fat of=disk.raw bs=1M seek=1 conv=notrunc,sparse status=none
    dd if=source.btrfs of=disk.raw bs=1M seek=97 conv=notrunc,sparse status=none
    root_drive=disk.raw
fi
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
run capture.log "$root_drive" "root=$root_device rootfstype=btrfs rootflags=subvol=@ rootwait rw init=/init" \
    -drive if=virtio,format=raw,file=scratch.ext4
grep -Fx 'CPS_VM_CAPTURE: immutable live snapshot staged without changing the running root' capture.log.clean
if [[ $native_boot == 1 ]]; then
    cp "${CPS_VM_OVMF_VARS:?Set CPS_VM_OVMF_VARS}" vars.fd
    uefi_timeout=180
    if [[ $destroy == 1 ]]; then uefi_timeout=300; fi
    timeout "${CPS_VM_TIMEOUT:-$uefi_timeout}" "$qemu" "${data_args[@]}" \
        -machine q35,accel=tcg -m 768 -smp 2 -nic none -display none -monitor none \
        -serial file:uefi.log \
        -drive "if=pflash,format=raw,readonly=on,file=${CPS_VM_OVMF_CODE:?Set CPS_VM_OVMF_CODE}" \
        -drive if=pflash,format=raw,file=vars.fd -drive if=virtio,format=raw,file=disk.raw || { cat uefi.log; exit 1; }
    tr -d '\r' < uefi.log > uefi.log.clean
    cat uefi.log.clean
    if [[ -n $kernel_fixture ]]; then
        trial_boots=2
        if [[ $destroy == 1 ]]; then trial_boots=5; fi
        if [[ $system_commands == 1 ]]; then trial_boots=1; fi
        test "$(grep -Fxc "CPS_VM_BTRFS_KERNEL: trial $trial_release" uefi.log.clean)" = "$trial_boots"
        if [[ $destroy != 1 ]]; then
            grep -Fx "CPS_VM_BTRFS_KERNEL: previous ${CPS_VM_KERNEL_RELEASE:-$(uname -r)}" uefi.log.clean
        fi
    fi
    if [[ $destroy == 1 ]]; then
        grep -Fx 'CPS_VM_BTRFS_DESTROY: staged another trial from the blessed root' uefi.log.clean
        grep -Fx 'CPS_VM_BTRFS_DESTROY: retired old root and artifacts after retry' uefi.log.clean
        grep -Fx 'CPS_VM_BTRFS_DESTROY: previous root booted after cleanup' uefi.log.clean
    elif [[ $health_fail != 0 ]]; then
        test "$(grep -Fxc 'CPS_VM_BTRFS_BOOT: trial health failed without blessing' uefi.log.clean)" = 2
        grep -Fx 'CPS_VM_BTRFS_BOOT: previous root after exhausted trial' uefi.log.clean
        if [[ $health_fail == readiness ]]; then
            test "$(grep -Fxc 'CPS_VM_BTRFS_BOOT: image verified but readiness failed' uefi.log.clean)" = 2
        fi
        if [[ $destroy_failed == 1 ]]; then
            grep -Fx 'CPS_VM_BTRFS_DESTROY: exhausted failed trial removed' uefi.log.clean
        fi
    elif [[ $system_commands == 1 ]]; then
        grep -Fx 'CPS_VM_SYSTEM: verified staged and live repair of a damaged running image' uefi.log.clean
        grep -Fx 'CPS_VM_SYSTEM: installed and upgraded in place, then staged the base upgrade' uefi.log.clean
        grep -Fx 'CPS_VM_SYSTEM: staged upgrade booted with the live package' uefi.log.clean
        grep -Fx 'CPS_VM_SYSTEM: rolled back to the base with the live package' uefi.log.clean
        grep -Fx 'CPS_VM_SYSTEM: staged autoremove booted with the dependency removed' uefi.log.clean
        grep -Fx 'CPS_VM_SYSTEM: forced install staged without changing the running root' uefi.log.clean
    else
        grep -Fx 'CPS_VM_BTRFS_BOOT: trial blessed' uefi.log.clean
        grep -Fx 'CPS_VM_BTRFS_BOOT: trial default after reboot' uefi.log.clean
        grep -Fx 'CPS_VM_BTRFS_BOOT: previous root after rollback' uefi.log.clean
    fi
    if [[ $staged_transactions == 1 ]]; then
        grep -Fx 'CPS_VM_STAGED_TRANSACTIONS: unfroze and upgraded the private Btrfs root' capture.log.clean
    fi
else
    run native.log source.btrfs 'root=/dev/vda rootfstype=btrfs rootflags=subvol=@captures/trial/root rootwait ro init=/init-trial'
    grep -Fx 'CPS_VM_CAPTURE_BOOT: booted the upgrade prepared from the running Btrfs root' native.log.clean
    run original.log source.btrfs 'root=/dev/vda rootfstype=btrfs rootflags=subvol=@ rootwait ro init=/init-original'
    grep -Fx 'CPS_VM_BTRFS_ORIGINAL: booted the previous root after staging its upgrade' original.log.clean
fi
debugfs -R "dump /root.ext4 $work/staged.ext4" scratch.ext4
test -f staged.ext4
run boot.log staged.ext4 'root=/dev/vda rootfstype=ext4 rootwait ro init=/init-trial'
grep -Fx 'CPS_VM_CAPTURE_BOOT: booted the upgrade prepared from the running Btrfs root' boot.log.clean
echo 'Verified live capture, atomic Btrfs staging, native upgrade and previous-root boots, and ext4 export'
