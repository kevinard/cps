#!/usr/bin/env bash
# Boot a signed release profile on Btrfs, update its private snapshot, and roll back.
set -euo pipefail
umask 022

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_BIN:-$project/target/debug/cps}")
cps_fixture=$(realpath -- "${CPS_VM_CPS_FIXTURE:?Set CPS_VM_CPS_FIXTURE}")
kernel_fixture=$(realpath -- "${CPS_VM_KERNEL_FIXTURE:?Set CPS_VM_KERNEL_FIXTURE}")
next_kernel_fixture=$(realpath -- "${CPS_VM_NEXT_KERNEL_FIXTURE:?Set CPS_VM_NEXT_KERNEL_FIXTURE to the next signed kernel build}")
utilities_fixture=$(realpath -- "${CPS_VM_UTILITIES_FIXTURE:?Set CPS_VM_UTILITIES_FIXTURE to the signed GNU tools build}")
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
next_release=$(cat "$next_kernel_fixture/kernel-release")
test "$release" != "$next_release"
! cmp -s "$kernel" "$next_kernel_fixture/image/usr/lib/modules/$next_release/vmlinuz"
grep -Fx 'CONFIG_BTRFS_FS=y' "$kernel_fixture/image/usr/lib/modules/$release/config"
grep -Fx 'CONFIG_BTRFS_FS=y' "$next_kernel_fixture/image/usr/lib/modules/$next_release/config"
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd -- "$work"

mkdir repos runtime
for pair in "canopa:$cps_fixture" "canopa-kernel:$kernel_fixture" \
    "canopa-systemd:$systemd_fixture" "canopa-usr-base:$usr_base_fixture" \
    "canopa-util-linux:$util_linux_fixture" "canopa-bootstrap:$busybox_fixture" "canopa-utilities:$utilities_fixture"; do
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
keypair canopa-kernel
mv repos/canopa-kernel original-kernel-repo
mkdir repos/canopa-kernel
cp "$next_kernel_fixture/builder-public.der" canopa-kernel-next-builder.der
{
    printf 'schema-version 1\npublisher "canopa-kernel" {\n'
    printf '    builder-key "../../canopa-kernel-builder.der"\n    builder-key "../../canopa-kernel-next-builder.der"\n'
    for fixture in "$kernel_fixture" "$next_kernel_fixture"; do
        package=$(cat "$fixture/kernel-package")
        "$cps" repo attestation --repo "$fixture/repo" --publisher canopa-kernel \
            --trust-key "$fixture/release-public.der" "$package" > /dev/null
        recipe=$(sha256sum "$fixture/build.kdl"); recipe=${recipe%% *}
        printf '    approve "system/kernel-qemu" version="%s" sha256="%s"\n' "${package##*@}" "$recipe"
    done
    printf '}\n'
} > repos/canopa-kernel/publish-policy.kdl
for fixture in "$kernel_fixture" "$next_kernel_fixture"; do
    recipe=$(sha256sum "$fixture/build.kdl"); recipe=${recipe%% *}
    "$cps" publish-bundle --repo repos/canopa-kernel --signing-key canopa-kernel-private.der \
        --builder-key "$fixture/builder-public.der" --recipe-sha256 "$recipe" "$fixture/bundle"
done
publishers=(vm-test canopa canopa-kernel canopa-systemd canopa-usr-base canopa-util-linux canopa-bootstrap canopa-utilities)
mkdir -p runtime/{etc/systemd/system.conf.d,etc/systemd/system,usr/local/libexec}
printf '%s\n' "${publishers[@]}" vm-profile > runtime/etc/cps-btrfs-publishers
printf '%s\n' canopa canopa-kernel canopa-systemd canopa-util-linux canopa-bootstrap canopa-utilities > runtime/etc/cps-btrfs-build-publishers
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
    printf '%s\n' "$3" > desktop/etc/cps-btrfs-release
    "$cps" manifest generate --proto desktop --publisher vm-test --version "$1" \
        --uid "$(id -u)" --gid "$(id -g)" test/desktop > desktop.kdl
    "$cps" publish --repo repos/vm-test --proto desktop --signing-key vm-test-private.der desktop.kdl
}
publish_desktop 1.0.0-0 previous "$release"
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
    require "system/coreutils" version="=9.12.0-0"
    require "system/grep" version="=3.12.0-0"
    require "system/kernel-qemu" version="=$2"
}
MANIFEST
    "$cps" publish --repo repos/vm-profile --proto desktop --signing-key vm-profile-private.der stack.kdl
}
publish_stack 1.0.0-0 "${kernel_package##*@}"
next_kernel_package=$(cat "$next_kernel_fixture/kernel-package")
publish_stack 2.0.0-0 "${next_kernel_package##*@}"
refresh_profile() {
    {
        printf 'schema-version 1\npublisher "vm-profile" {\n    builder-key "unused.der"\n'
        printf '    profile "boot" architecture="%s" {\n        package "test/btrfs-stack@%s"\n        facet "devel" #false\n' "$(uname -m)" "$1"
        for publisher in "${publishers[@]}"; do
            printf '        publisher "%s" catalog="%s" {\n' "$publisher" "$(cat "repos/$publisher/current")"
            printf '            repository "../%s"\n            trust-key "../../%s-public.der"\n' "$publisher" "$publisher"
            if [[ -f $publisher-builder.der ]]; then
                printf '            build-trust {\n                builder-key "../../%s-builder.der"\n' "$publisher"
                if [[ $publisher == canopa-kernel ]]; then
                    printf '                builder-key "../../canopa-kernel-next-builder.der"\n'
                fi
                printf '            }\n'
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
cmp files/@/usr/bin/coreutils "$utilities_fixture/image/usr/bin/coreutils"
cmp files/@/usr/bin/grep "$utilities_fixture/image/usr/bin/grep"
publish_desktop 2.0.0-0 trial "$next_release"
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
kernel_markers=$(grep '^CPS_VM_SIGNED_BTRFS_KERNEL: ' boot.log.clean)
test "$kernel_markers" = "$(printf 'CPS_VM_SIGNED_BTRFS_KERNEL: trial %s\nCPS_VM_SIGNED_BTRFS_KERNEL: trial %s\nCPS_VM_SIGNED_BTRFS_KERNEL: previous %s' "$next_release" "$next_release" "$release")"
printf '%s\n' "$kernel_markers"
echo 'Verified a signed system stack through native Btrfs staging, blessing, reboot, and rollback'
