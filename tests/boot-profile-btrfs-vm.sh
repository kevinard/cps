#!/usr/bin/env bash
# Boot a signed release profile on Btrfs, update its private snapshot, and roll back.
set -euo pipefail
umask 022

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_BIN:-$project/target/debug/cps}")
cps_fixture=$(realpath -- "${CPS_VM_CPS_FIXTURE:?Set CPS_VM_CPS_FIXTURE}")
kernel_fixture=$(realpath -- "${CPS_VM_KERNEL_FIXTURE:?Set CPS_VM_KERNEL_FIXTURE}")
systemd_fixture=$(realpath -- "${CPS_VM_SYSTEMD_FIXTURE:?Set CPS_VM_SYSTEMD_FIXTURE}")
usr_base_fixture=$(realpath -- "${CPS_VM_USR_BASE_FIXTURE:?Set CPS_VM_USR_BASE_FIXTURE}")
util_linux_fixture=$(realpath -- "${CPS_VM_UTIL_LINUX_FIXTURE:?Set CPS_VM_UTIL_LINUX_FIXTURE}")
busybox_fixture=$(realpath -- "${CPS_VM_BUSYBOX_FIXTURE:?Set CPS_VM_BUSYBOX_FIXTURE}")
formatter=${CPS_VM_MKFS_BTRFS:?Set CPS_VM_MKFS_BTRFS to a trusted mkfs.btrfs executable}
qemu=${CPS_VM_QEMU:-qemu-system-x86_64}
code=${CPS_VM_OVMF_CODE:?Set CPS_VM_OVMF_CODE}
vars=${CPS_VM_OVMF_VARS:?Set CPS_VM_OVMF_VARS}
release=$(cat "$kernel_fixture/kernel-release")
kernel="$kernel_fixture/image/usr/lib/modules/$release/vmlinuz"
grep -Fx 'CONFIG_BTRFS_FS=y' "$kernel_fixture/image/usr/lib/modules/$release/config"
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd -- "$work"

mkdir repos runtime
for pair in "canopa:$cps_fixture" "canopa-kernel:$kernel_fixture" \
    "canopa-systemd:$systemd_fixture" "canopa-usr-base:$usr_base_fixture" \
    "canopa-util-linux:$util_linux_fixture" "canopa-bootstrap:$busybox_fixture"; do
    publisher=${pair%%:*}
    fixture=${pair#*:}
    cp -a -- "$fixture/repo" "repos/$publisher"
    cp -- "$fixture/release-public.der" "$publisher-public.der"
    if [[ $publisher != canopa-usr-base ]]; then
        cp -- "$fixture/builder-public.der" "$publisher-builder.der"
    fi
done
keypair() {
    openssl genpkey -algorithm ED25519 -outform DER -out "$1-private.der"
    openssl pkey -inform DER -in "$1-private.der" -pubout -outform DER -out "$1-public.der"
}
keypair vm-test
keypair vm-profile
publishers=(vm-test canopa canopa-kernel canopa-systemd canopa-usr-base canopa-util-linux canopa-bootstrap)
mkdir -p runtime/{etc/systemd/system.conf.d,etc/systemd/system,usr/local/libexec}
printf '%s\n' "${publishers[@]}" vm-profile > runtime/etc/cps-btrfs-publishers
printf '%s\n' canopa canopa-kernel canopa-systemd canopa-util-linux canopa-bootstrap > runtime/etc/cps-btrfs-build-publishers
printf '%s\n' "$release" > runtime/etc/cps-btrfs-release
printf 'root:x:0:0:root:/root:/bin/sh\n' > runtime/etc/passwd
printf 'root:x:0:\n' > runtime/etc/group
printf 'ID=canopa\nPRETTY_NAME="Canopa signed Btrfs VM"\n' > runtime/etc/os-release
printf '9b6c080d423b4a6090307e0670659a21\n' > runtime/etc/machine-id
printf '[Manager]\nDefaultStandardOutput=tty\nDefaultStandardError=inherit\n' > runtime/etc/systemd/system.conf.d/vm-console.conf
install -m755 "$project/tests/boot-profile-btrfs-guest.sh" runtime/usr/local/libexec/cps-profile-btrfs
cat > runtime/etc/systemd/system/default.target <<'UNIT'
[Unit]
Description=CPS signed Btrfs VM
DefaultDependencies=no
Requires=multi-user.target
After=multi-user.target
Wants=cps-profile-observe.service
UNIT
cat > runtime/etc/systemd/system/cps-profile-observe.service <<'UNIT'
[Unit]
Description=Verify the signed Btrfs upgrade and rollback
After=multi-user.target systemd-bless-boot.service

[Service]
Type=oneshot
ExecStart=/usr/local/libexec/cps-profile-btrfs observe
StandardOutput=tty
StandardError=inherit
UNIT
"$cps" manifest generate --proto runtime --publisher vm-test --version 1.0.0-0 \
    --uid "$(id -u)" --gid "$(id -g)" test/btrfs-harness > runtime.kdl
"$cps" publish --repo repos/vm-test --proto runtime --signing-key vm-test-private.der runtime.kdl

publish_desktop() {
    mkdir -p desktop/etc
    printf '%s\n' "$2" > desktop/etc/cps-btrfs-state
    "$cps" manifest generate --proto desktop --publisher vm-test --version "$1" \
        --uid "$(id -u)" --gid "$(id -g)" test/desktop > desktop.kdl
    "$cps" publish --repo repos/vm-test --proto desktop --signing-key vm-test-private.der desktop.kdl
}
publish_desktop 1.0.0-0 previous
cps_version=$(sed -n 's/^package .* version="\([^"]*\)".*/\1/p' "$cps_fixture/cps.kdl")
systemd_version=$(sed -n 's/^package .* version="\([^"]*\)".*/\1/p' "$systemd_fixture/systemd.kdl")
kernel_package=$(cat "$kernel_fixture/kernel-package")
publish_stack() {
    cat > stack.kdl <<MANIFEST
schema-version 1
package "test/btrfs-stack" publisher="vm-profile" version="$1" {
    require "test/desktop" version="=$1"
    require "test/btrfs-harness"
    require "system/cps" version="=$cps_version"
    require "system/systemd-vm" version="=$systemd_version"
    require "system/busybox" version="=1.38.0-1"
    require "system/kernel-qemu" version="=${kernel_package##*@}"
}
MANIFEST
    "$cps" publish --repo repos/vm-profile --proto desktop --signing-key vm-profile-private.der stack.kdl
}
publish_stack 1.0.0-0
publish_stack 2.0.0-0
refresh_profile() {
    {
        printf 'schema-version 1\npublisher "vm-profile" {\n    builder-key "unused.der"\n'
        printf '    profile "boot" architecture="%s" {\n        package "test/btrfs-stack@%s"\n        facet "devel" #false\n' "$(uname -m)" "$1"
        for publisher in "${publishers[@]}"; do
            printf '        publisher "%s" catalog="%s" {\n' "$publisher" "$(cat "repos/$publisher/current")"
            printf '            repository "../%s"\n            trust-key "../../%s-public.der"\n' "$publisher" "$publisher"
            if [[ -f $publisher-builder.der ]]; then
                printf '            build-trust { builder-key "../../%s-builder.der"; }\n' "$publisher"
            fi
            printf '        }\n'
        done
        printf '    }\n}\n'
    } > repos/vm-profile/publish-policy.kdl
    printf 'schema-version 1\nrelease "vm-profile" base="%s" refresh-profiles=#true\n' \
        "$(cat repos/vm-profile/current)" > release.kdl
    "$cps" repo publish-release --repo repos/vm-profile --signing-key vm-profile-private.der release.kdl
}
refresh_profile 1.0.0-0
{
    printf 'schema-version 1\nfacet "devel" #false\nrelease-profile "boot"\n'
    for publisher in vm-profile "${publishers[@]}"; do
        printf 'publisher "%s" { repository "repos/%s"; trust-key "%s-public.der"; }\n' "$publisher" "$publisher" "$publisher"
    done
} > image.kdl
mkdir files
"$cps" image-create --root files/@ --config image.kdl
"$cps" plan --root files/@ --output install.json install-profile
"$cps" apply --root files/@ install.json
"$cps" plan --root files/@ --output freeze.json freeze --reason 'Review before upgrade' test/desktop
"$cps" apply --root files/@ freeze.json
"$cps" verify --root files/@ --pristine
for fixture_path in "canopa:$cps_fixture:usr/bin/cps" \
    "canopa-systemd:$systemd_fixture:usr/lib/systemd/systemd" \
    "canopa-util-linux:$util_linux_fixture:usr/bin/mount" \
    "canopa-bootstrap:$busybox_fixture:usr/bin/busybox" \
    "canopa-kernel:$kernel_fixture:usr/lib/modules/$release/vmlinuz"; do
    rest=${fixture_path#*:}; fixture=${rest%%:*}; path=${rest#*:}
    cmp "files/@/$path" "$fixture/image/$path"
done
cmp files/@/usr/bin/sfdisk "$util_linux_fixture/image/usr/bin/sfdisk"
publish_desktop 2.0.0-0 trial
refresh_profile 2.0.0-0
# Only immutable signed objects go to the guest; publication policy contains
# host-side paths, and private signing keys always stay outside these disks.
for publisher in vm-profile "${publishers[@]}"; do
    rm -f -- "repos/$publisher/publish-policy.kdl"
done
mkdir files/@environments
truncate -s 1024M source.btrfs
"$formatter" -q -f --rootdir files --subvol default:@ --subvol rw:@environments source.btrfs
truncate -s 1152M disk.raw
sfdisk --quiet disk.raw <<'PARTITIONS'
label: gpt
unit: sectors
start=2048,size=196608,type=uefi,uuid=9a138d0b-f9aa-4a2a-b527-93392ad7e812
start=198656,size=2097152,type=linux,uuid=7adff456-af50-498f-b39a-dcf87cb4a7c1
PARTITIONS
mkdir -p esp/EFI/BOOT
cp files/@/usr/lib/systemd/boot/efi/systemd-bootx64.efi esp/EFI/BOOT/BOOTX64.EFI
truncate -s 96M esp.fat
mkfs.fat -F 32 esp.fat >/dev/null
mcopy -s -i esp.fat esp/EFI ::/
dd if=esp.fat of=disk.raw bs=1M seek=1 conv=notrunc,sparse status=none
dd if=source.btrfs of=disk.raw bs=1M seek=97 conv=notrunc,sparse status=none
truncate -s 768M repositories.ext4
mke2fs -q -F -t ext4 -d repos repositories.ext4
data_args=()
if [[ -n ${CPS_VM_QEMU_DATA:-} ]]; then data_args=(-L "$CPS_VM_QEMU_DATA"); fi
run() {
    local log=$1 result=0
    shift
    timeout "${CPS_VM_TIMEOUT:-240}" "$qemu" "${data_args[@]}" \
        -machine q35,accel=tcg -m 768 -smp 2 -nic none -display none -monitor none \
        -serial "file:$log" -drive if=virtio,format=raw,file=disk.raw \
        -drive if=virtio,format=raw,readonly=on,file=repositories.ext4 "$@" || result=$?
    tr -d '\r' < "$log" > "$log.clean"
    tail -30 "$log.clean"
    test "$result" = 0
    ! grep -F 'CPS_VM_SIGNED_BTRFS: failed' "$log.clean"
}
run stage.log -kernel "$kernel" \
    -append 'root=/dev/vda2 rootfstype=btrfs rootflags=subvol=@ rootwait rw console=ttyS0 loglevel=3 init=/usr/local/libexec/cps-profile-btrfs cps.test.stage=1'
grep -Fx 'CPS_VM_SIGNED_BTRFS: staged unfreeze and profile upgrade' stage.log.clean
cp -- "$vars" OVMF_VARS.fd
run boot.log -drive if=pflash,format=raw,readonly=on,file="$code" \
    -drive if=pflash,format=raw,file=OVMF_VARS.fd
markers=$(grep '^CPS_VM_SIGNED_BTRFS: ' boot.log.clean)
test "$markers" = $'CPS_VM_SIGNED_BTRFS: trial signed stack and kernel verified\nCPS_VM_SIGNED_BTRFS: trial blessed\nCPS_VM_SIGNED_BTRFS: trial signed stack and kernel verified\nCPS_VM_SIGNED_BTRFS: trial default after reboot\nCPS_VM_SIGNED_BTRFS: previous signed stack and kernel verified\nCPS_VM_SIGNED_BTRFS: previous root restored with its freeze and kernel'
printf '%s\n' "$markers"
echo 'Verified a signed system stack through native Btrfs staging, blessing, reboot, and rollback'
