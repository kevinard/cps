#!/usr/bin/env bash
# Run with a writable temporary directory and local, non-Secure-Boot OVMF firmware.
set -euo pipefail
umask 022

mode=${1:-fallback}
case $mode in
    fallback|bless|health-fail|kernel-upgrade|rollback|capabilities) ;;
    *) echo "Usage: $0 [fallback|bless|health-fail|kernel-upgrade|rollback|capabilities]" >&2; exit 2 ;;
esac
recycle=${CPS_VM_RECYCLE_SLOT:-0}
device_activation=${CPS_VM_DEVICE_ACTIVATION:-0}
installed_stage=${CPS_VM_INSTALLED_STAGE:-0}
reprovision=${CPS_VM_REPROVISION:-0}
if [[ $recycle == 1 && $mode != rollback ]]; then
    echo 'CPS_VM_RECYCLE_SLOT requires the rollback scenario' >&2
    exit 2
fi

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
source "$project/tests/systemd-boot-guest.sh"
cps=${CPS_BIN:-$project/target/debug/cps}
cps_fixture=${CPS_VM_CPS_FIXTURE:-}
cps_version=${CPS_VM_CPS_VERSION:-0.1.0-4}
kernel_fixture=${CPS_VM_KERNEL_FIXTURE:-}
if [[ $device_activation == 1 && ( $mode != rollback || -z $kernel_fixture ) ]]; then
    echo 'CPS_VM_DEVICE_ACTIVATION requires rollback and a signed kernel fixture' >&2
    exit 2
fi
if [[ $installed_stage == 1 && ( $device_activation != 1 || $recycle != 1 || ${CPS_VM_RELEASE_PROFILE:-0} != 1 ) ]]; then
    echo 'CPS_VM_INSTALLED_STAGE requires device activation, slot reuse, and a signed release profile' >&2
    exit 2
fi
if [[ $reprovision == 1 && ( $device_activation == 1 || ${CPS_VM_RELEASE_PROFILE:-0} != 1 || -z ${SOURCE_DATE_EPOCH:-} ) ]]; then
    echo 'CPS_VM_REPROVISION requires offline disk activation, a signed release profile, and SOURCE_DATE_EPOCH' >&2
    exit 2
fi
kernel_package=cps://canopa-kernel/system/kernel-qemu@6.12.115-0
systemd_fixture=${CPS_VM_SYSTEMD_FIXTURE:-}
usr_base_fixture=${CPS_VM_USR_BASE_FIXTURE:-}
util_linux_fixture=${CPS_VM_UTIL_LINUX_FIXTURE:-}
systemd_version=262.1.0-3
util_linux_package=cps://canopa-util-linux/system/util-linux@2.42.4-0
systemd_efi=usr/lib/systemd/boot/efi/systemd-bootx64.efi
profiled=false
signed_cps_builds=false
qemu=${CPS_VM_QEMU:-qemu-system-x86_64}
efi=${CPS_VM_SYSTEMD_BOOT:-/usr/lib/systemd/boot/efi/systemd-bootx64.efi}
if [[ -n $systemd_fixture ]]; then
    systemd_fixture=$(realpath -- "$systemd_fixture")
    efi="$systemd_fixture/image/$systemd_efi"
fi
kernel=${CPS_VM_KERNEL:-/boot/vmlinuz-linux}
busybox=${CPS_VM_BUSYBOX:-/usr/lib/initcpio/busybox}
busybox_repo=${CPS_VM_BUSYBOX_REPO:-}
busybox_key=${CPS_VM_BUSYBOX_TRUST_KEY:-}
busybox_revision=0
if [[ -n $usr_base_fixture ]]; then
    busybox_revision=1
fi
busybox_version=${CPS_VM_BUSYBOX_VERSION:-1.38.0-$busybox_revision}
busybox_package=cps://canopa-bootstrap/system/busybox@$busybox_version
signed_insmod=false
code=${CPS_VM_OVMF_CODE:?Set CPS_VM_OVMF_CODE to x86-64 OVMF_CODE}
vars=${CPS_VM_OVMF_VARS:?Set CPS_VM_OVMF_VARS to the matching OVMF_VARS template}
qemu_data=${CPS_VM_QEMU_DATA:-}

for command in "$qemu" mkfs.fat fsck.fat mcopy mdir mmd mke2fs e2fsck sfdisk blkid openssl timeout; do
    command -v "$command" >/dev/null || { echo "Missing command: $command" >&2; exit 1; }
done
if [[ $recycle == 1 ]]; then
    for command in mdel mrd; do command -v "$command" >/dev/null; done
fi
if [[ -n $busybox_repo || -n $busybox_key ]]; then
    busybox_repo=$(realpath -- "${busybox_repo:?Set CPS_VM_BUSYBOX_REPO}")
    busybox_key=$(realpath -- "${busybox_key:?Set CPS_VM_BUSYBOX_TRUST_KEY}")
    case $busybox_version in
        "1.38.0-$busybox_revision") signed_insmod=true ;;
        "1.37.0-$busybox_revision") ;;
        *) echo "Unsupported BusyBox version for this layout: $busybox_version" >&2; exit 2 ;;
    esac
    "$cps" repo attestation --repo "$busybox_repo" --publisher canopa-bootstrap \
        --trust-key "$busybox_key" "$busybox_package" >/dev/null
else
    test -r "$busybox" || { echo "Missing input: $busybox" >&2; exit 1; }
fi
if [[ $mode == health-fail ]]; then command -v debugfs >/dev/null; fi
for file in "$cps" "$efi" "$kernel" "$code" "$vars"; do
    test -r "$file" || { echo "Missing input: $file" >&2; exit 1; }
done

systemd_guest=false
if [[ $mode == bless || $mode == health-fail || $mode == rollback || $mode == capabilities ]]; then
    systemd_guest=true
fi
fixture_publisher=canopa
if [[ -n $cps_fixture ]]; then
    $systemd_guest || { echo 'CPS_VM_CPS_FIXTURE requires a systemd guest' >&2; exit 2; }
    cps_fixture=$(realpath -- "$cps_fixture")
    fixture_publisher=vm-test
    case $cps_version in
        0.1.0-1) sqlite_version=3.54.1-0; seccomp_version=2.6.1-0; systemd_version=262.1.0-0 ;;
        0.1.0-2|0.1.0-3|0.1.0-4) sqlite_version=3.54.1-1; seccomp_version=2.6.1-1 ;;
        *) echo "Unsupported CPS fixture version: $cps_version" >&2; exit 2 ;;
    esac
    cps_packages=("system/cps@$cps_version" "system/sqlite@$sqlite_version" "system/libseccomp@$seccomp_version")
    for package in "${cps_packages[@]}"; do
        "$cps" repo attestation --repo "$cps_fixture/repo" --publisher canopa \
            --trust-key "$cps_fixture/release-public.der" "cps://canopa/$package" >/dev/null
    done
    elf_runtime_paths=(lib64/ld-linux-x86-64.so.2 usr/lib/{libc.so.6,libm.so.6,libgcc_s.so.1})
    elf_runtime=$cps_fixture
    cps_library_paths=(usr/lib/{libsqlite3.so.0,libseccomp.so.2})
fi
if [[ ${CPS_VM_RELEASE_PROFILE:-0} == 1 ]]; then
    [[ $mode == rollback && -n $cps_fixture && -n $busybox_repo ]] || {
        echo 'CPS_VM_RELEASE_PROFILE requires rollback and signed CPS and BusyBox fixtures' >&2
        exit 2
    }
    profiled=true
    case $cps_version in
        0.1.0-3|0.1.0-4) signed_cps_builds=true ;;
    esac
fi
if [[ -n $kernel_fixture ]]; then
    $profiled || { echo 'CPS_VM_KERNEL_FIXTURE requires CPS_VM_RELEASE_PROFILE=1' >&2; exit 2; }
    kernel_fixture=$(realpath -- "$kernel_fixture")
    trial_release=$(cat "$kernel_fixture/kernel-release")
    test "$trial_release" = 6.12.115-cps-vm
    trial_kernel="$kernel_fixture/image/usr/lib/modules/$trial_release/vmlinuz"
    "$cps" repo attestation --repo "$kernel_fixture/repo" --publisher canopa-kernel \
        --trust-key "$kernel_fixture/release-public.der" "$kernel_package" >/dev/null
fi
if [[ -n $util_linux_fixture ]]; then
    [[ -n $usr_base_fixture ]] || { echo 'CPS_VM_UTIL_LINUX_FIXTURE requires the merged /usr fixture' >&2; exit 2; }
    util_linux_fixture=$(realpath -- "$util_linux_fixture")
    if [[ $cps_version == 0.1.0-1 ]]; then
        systemd_version=262.1.0-2
    else
        systemd_version=262.1.0-4
    fi
    "$cps" repo attestation --repo "$util_linux_fixture/repo" --publisher canopa-util-linux \
        --trust-key "$util_linux_fixture/release-public.der" "$util_linux_package" >/dev/null
fi
if [[ -n $systemd_fixture ]]; then
    $profiled || { echo 'CPS_VM_SYSTEMD_FIXTURE requires CPS_VM_RELEASE_PROFILE=1' >&2; exit 2; }
    systemd_version=${CPS_VM_SYSTEMD_VERSION:-$systemd_version}
    "$cps" repo attestation --repo "$systemd_fixture/repo" --publisher canopa-systemd \
        --trust-key "$systemd_fixture/release-public.der" "cps://canopa-systemd/system/systemd-vm@$systemd_version" >/dev/null
fi
if [[ -n $usr_base_fixture ]]; then
    [[ -n $systemd_fixture ]] || { echo 'CPS_VM_USR_BASE_FIXTURE requires the signed systemd fixture' >&2; exit 2; }
    usr_base_fixture=$(realpath -- "$usr_base_fixture")
    elf_runtime=$usr_base_fixture
    elf_runtime_paths[0]=usr/lib/ld-linux-x86-64.so.2
fi
if [[ $mode == kernel-upgrade || $mode == rollback ]]; then
    base_release=${CPS_VM_KERNEL_RELEASE:-$(uname -r)}
    if [[ -z $kernel_fixture ]]; then
        trial_kernel=${CPS_VM_TRIAL_KERNEL:?Set CPS_VM_TRIAL_KERNEL to the second kernel}
        trial_release=${CPS_VM_TRIAL_KERNEL_RELEASE:?Set CPS_VM_TRIAL_KERNEL_RELEASE to its uname -r value}
    fi
    kernel=$(realpath -- "$kernel")
    trial_kernel=$(realpath -- "$trial_kernel")
    test "$base_release" != "$trial_release"
    ! cmp -s "$kernel" "$trial_kernel"
    command -v cpio >/dev/null
    command -v kernel-install >/dev/null
fi

work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd -- "$work"

base_uuid=7adff456-af50-498f-b39a-dcf87cb4a7c1
trial_uuid=b52060cf-81ab-4b8a-ad84-3aa69000ed49
esp_uuid=9a138d0b-f9aa-4a2a-b527-93392ad7e812
options='ro console=ttyS0 loglevel=3 init=/init'
root_mib=64
if $systemd_guest; then
    options='rw console=ttyS0 loglevel=3 init=/init systemd.log_target=console'
    root_mib=320
elif [[ $mode == kernel-upgrade ]]; then
    root_mib=128
fi
# Use regular files so the fixture needs neither loop devices nor mounts.
base_mib=97
trial_mib=$((base_mib + root_mib))
truncate -s "$((trial_mib + root_mib + 1))M" template.raw
sfdisk --quiet template.raw <<PARTITIONS
label: gpt
unit: sectors

start=2048, size=196608, type=uefi, uuid=$esp_uuid
start=$((base_mib * 2048)), size=$((root_mib * 2048)), type=linux, uuid=$base_uuid
start=$((trial_mib * 2048)), size=$((root_mib * 2048)), type=linux, uuid=$trial_uuid
PARTITIONS
mkdir -p esp/EFI/BOOT proto/etc
if [[ -z $systemd_fixture ]]; then cp "$efi" esp/EFI/BOOT/BOOTX64.EFI; fi
openssl genpkey -algorithm ED25519 -outform DER -out private.der
openssl pkey -inform DER -in private.der -pubout -outform DER -out public.der

publish_state() {
    local version=$1 state=$2
    local dependency= config_action= policy_mode=0644
    if [[ $mode == rollback ]]; then
        case $state in
            previous) dependency='    require "test/legacy-marker"' ;;
            trial|recycled) dependency='    require "test/extra-marker"' ;;
        esac
    fi
    if [[ $recycle == 1 ]]; then
        printf 'vendor-%s\n' "$version" > proto/etc/cps-vm.conf
        config_action="    file \"etc/cps-vm.conf\" mode=\"0644\" uid=$(id -u) gid=$(id -g) preserve=#true"
        printf 'vendor-policy\n' > proto/etc/cps-vm-policy.conf
        if [[ $state == recycled ]]; then policy_mode=0600; fi
        config_action+=$'\n'"    file \"etc/cps-vm-policy.conf\" mode=\"$policy_mode\" uid=$(id -u) gid=$(id -g) preserve=#true"
    fi
    if [[ -n $kernel_fixture ]]; then
        case $state in
            previous) dependency+=$'\n    require "system/kernel"' ;;
            trial|recycled) dependency+=$'\n    require "system/kernel-qemu"' ;;
        esac
    fi
    printf '%s\n' "$state" > proto/etc/cps-vm-state
    cat > manifest.kdl <<MANIFEST
schema-version 1
package "test/boot-state" publisher="$fixture_publisher" version="$version" {
    file "etc/cps-vm-state" mode="0644" uid=$(id -u) gid=$(id -g)
    hardlink "etc/cps-vm-boot-state" target="etc/cps-vm-state"
$config_action
$dependency
}
MANIFEST
    "$cps" publish --repo repo --proto proto --signing-key private.der manifest.kdl
}

publish_kernel() {
    local version=$1 release=$2 binary=$3
    local modules="usr/lib/modules/$release"
    mkdir -p "proto/$modules" initrd-tree
    cp "$binary" "proto/$modules/vmlinuz"
    printf '%s\n' "$release" > initrd-tree/cps-initrd-release
    (cd initrd-tree && printf 'cps-initrd-release\n' | cpio --quiet -o -H newc) > "proto/$modules/initrd"
    cat > kernel.kdl <<MANIFEST
schema-version 1
package "system/kernel" publisher="$fixture_publisher" version="$version" {
    file "$modules/vmlinuz" mode="0644" uid=$(id -u) gid=$(id -g)
    file "$modules/initrd" mode="0644" uid=$(id -u) gid=$(id -g)
    kernel-install "$release" initrd=#true
}
MANIFEST
    "$cps" publish --repo repo --proto proto --signing-key private.der kernel.kdl
}

publish_extra_marker() {
    openssl genpkey -algorithm ED25519 -outform DER -out extras-private.der
    openssl pkey -inform DER -in extras-private.der -pubout -outform DER -out extras-public.der
    printf 'extras\n' > proto/etc/cps-vm-extra-marker
    cat > extra-marker.kdl <<MANIFEST
schema-version 1
package "test/extra-marker" publisher="extras" version="1.0.0-0" {
    file "etc/cps-vm-extra-marker" mode="0644" uid=$(id -u) gid=$(id -g)
}
MANIFEST
    "$cps" publish --repo extras --proto proto --signing-key extras-private.der extra-marker.kdl
}

profile_companions() {
    printf '%s\n' "$fixture_publisher repo public.der" 'canopa signed-cps cps-public.der'
    if [[ -n $usr_base_fixture ]]; then
        printf '%s\n' 'canopa-usr-base signed-seed seed-public.der'
    else
        printf '%s\n' 'canopa-runtime-seed signed-seed seed-public.der'
    fi
    printf '%s\n' 'canopa-bootstrap signed-busybox busybox-public.der' \
        'extras extras extras-public.der'
    if [[ -n $kernel_fixture ]]; then
        printf '%s\n' 'canopa-kernel signed-kernel kernel-public.der'
    fi
    if [[ -n $systemd_fixture ]]; then
        printf '%s\n' 'canopa-systemd signed-systemd systemd-public.der'
        if [[ -n $util_linux_fixture ]]; then
            printf '%s\n' 'canopa-util-linux signed-util-linux util-linux-public.der'
        else
            printf '%s\n' 'canopa-systemd-seed signed-systemd-seed systemd-seed-public.der'
        fi
    fi
}

refresh_vm_profile() {
    local name repository key
    {
        printf 'schema-version 1\npublisher "vm-profile" {\n    builder-key "unused.der"\n'
        printf '    profile "boot" architecture="%s" {\n        package "test/boot-stack"\n' "$(uname -m)"
        printf '        facet "devel" #false\n'
        while read -r name repository key; do
            printf '        publisher "%s" catalog="%s" {\n' "$name" "$(cat "$repository/current")"
            printf '            repository "../%s"\n            trust-key "../%s"\n' "$repository" "$key"
            if [[ $name == canopa ]] && $signed_cps_builds; then
                printf '            build-trust { builder-key "../cps-builder-public.der"; }\n'
            fi
            printf '        }\n'
        done < <(profile_companions)
        printf '    }\n}\n'
    } > profile-repo/publish-policy.kdl
    printf 'schema-version 1\nrelease "vm-profile" base="%s" refresh-profiles=#true\n' \
        "$(cat profile-repo/current)" > profile-release.kdl
    "$cps" repo publish-release --repo profile-repo --signing-key profile-private.der profile-release.kdl
}

if [[ $mode == rollback ]]; then
    printf 'legacy\n' > proto/etc/cps-vm-legacy-marker
    cat > legacy-marker.kdl <<MANIFEST
schema-version 1
package "test/legacy-marker" publisher="$fixture_publisher" version="1.0.0-0" {
    file "etc/cps-vm-legacy-marker" mode="0644" uid=$(id -u) gid=$(id -g)
}
MANIFEST
    "$cps" publish --repo repo --proto proto --signing-key private.der legacy-marker.kdl
fi
publish_state 1.0.0-0 previous
packages=(test/boot-state)
if [[ $mode == kernel-upgrade || $mode == rollback ]]; then
    publish_kernel 1.0.0-0 "$base_release" "$kernel"
    packages+=(system/kernel)
fi
if [[ -n $cps_fixture ]]; then
    for package in "${cps_packages[@]}"; do packages+=("cps://canopa/$package"); done
elif $systemd_guest; then
    mkdir -p proto/usr/{bin,lib/systemd/system}
    sections=$(readelf --wide --section-headers "$cps")
    if [[ $sections =~ [[:space:]]\.z?debug_ ]]; then
        strip --strip-debug -o proto/usr/bin/cps "$cps"
    else
        cp -- "$cps" proto/usr/bin/cps
    fi
    cp "$project/packaging/systemd/cps-boot-health.service" proto/usr/lib/systemd/system/
    sed -e '/^    require /d' -e "s/uid=0 gid=0/uid=$(id -u) gid=$(id -g)/g" \
        "$project/packaging/cps.kdl" > cps.kdl
    "$cps" publish --repo repo --proto proto --signing-key private.der cps.kdl
    packages+=(system/cps)
fi
mkdir -p runtime/etc
if [[ $mode == kernel-upgrade || $mode == rollback ]]; then
    touch runtime/etc/cps-vm-assert-kernel
else
    mkdir -p runtime/boot
    cp "$kernel" runtime/boot/vmlinuz
fi
if [[ -n $busybox_repo ]]; then
    packages+=("$busybox_package")
    if $systemd_guest; then touch runtime/etc/cps-vm-assert-built-busybox; fi
else
    mkdir -p runtime/bin
    cp -L "$busybox" runtime/bin/busybox
    copy_runtime_deps runtime "$busybox"
fi
if $systemd_guest; then
    setup_systemd_guest runtime "$systemd_fixture" "$usr_base_fixture" "$util_linux_fixture" "$signed_insmod"
    if [[ $mode == capabilities ]]; then
        setup_capability_guest runtime "$cps" "$project/tests/file-capabilities-guest.sh"
    fi
    if [[ -n $cps_fixture ]]; then
        # Leave runtime ownership to the packages selected from the signed repositories.
        for path in "${elf_runtime_paths[@]}"; do
            if [[ -f runtime/$path ]]; then cmp "runtime/$path" "$elf_runtime/image/$path"; fi
            rm -f -- "runtime/$path"
        done
        for path in "${cps_library_paths[@]}"; do rm -f -- "runtime/$path"; done
        touch runtime/etc/cps-vm-assert-cps-runtime
        if $signed_cps_builds; then touch runtime/etc/cps-vm-assert-cps-builds; fi
    else
        copy_runtime_deps runtime proto/usr/bin/cps
    fi
    if [[ -n $systemd_fixture ]]; then
        if [[ -z $util_linux_fixture ]]; then
            for library in libblkid.so.1 libmount.so.1; do
                cmp "runtime/usr/lib/$library" "$systemd_fixture/image/usr/lib/$library"
                rm -- "runtime/usr/lib/$library"
            done
        fi
        rm -f -- runtime/usr/lib/libsystemd.so.0
    fi
    if [[ -n $usr_base_fixture ]]; then
        touch runtime/etc/cps-vm-assert-merged-usr
    fi
    if [[ $mode == rollback ]]; then
        if [[ -n $kernel_fixture ]]; then
            CPS_VM_FAT_BUILTIN=1 prepare_fat_modules runtime "$trial_release" /
        else
            prepare_fat_modules runtime "$trial_release" "${CPS_VM_TRIAL_MODULES_ROOT:-/}"
        fi
        touch runtime/etc/cps-vm-assert-extra-publisher
    fi
    if [[ $installed_stage == 1 ]]; then touch runtime/etc/cps-vm-assert-installed-stage; fi
else
    mkdir runtime/dev
    cat > runtime/init <<'GUEST'
#!/bin/busybox sh
set -e
[ /etc/cps-vm-state -ef /etc/cps-vm-boot-state ]
state=$(/bin/busybox cat /etc/cps-vm-boot-state)
printf 'CPS_VM_BOOT: %s\n' "$state"
if [ -f /etc/cps-vm-assert-kernel ]; then
    printf 'CPS_VM_KERNEL: %s %s\n' "$state" "$(/bin/busybox uname -r)"
fi
if [ "$state" = trial ]; then
    /bin/busybox reboot -f
elif [ "$state" = previous ]; then
    /bin/busybox poweroff -f
fi
GUEST
    chmod 0755 runtime/init
fi
"$cps" manifest generate --proto runtime --publisher "$fixture_publisher" --version 1.0.0-0 \
    --uid "$(id -u)" --gid "$(id -g)" test/boot-runtime > runtime.kdl
"$cps" publish --repo repo --proto runtime --signing-key private.der runtime.kdl
packages+=(test/boot-runtime)
if $profiled; then
    publish_extra_marker
    cp -a -- "$cps_fixture/repo" signed-cps
    cp -a -- "$busybox_repo" signed-busybox
    cp -- "$cps_fixture/release-public.der" cps-public.der
    if $signed_cps_builds; then
        cp -- "$cps_fixture/builder-public.der" cps-builder-public.der
    fi
    if [[ -n $usr_base_fixture ]]; then
        cp -a -- "$usr_base_fixture/repo" signed-seed
        cp -- "$usr_base_fixture/release-public.der" seed-public.der
    else
        cp -a -- "$cps_fixture/runtime-seed" signed-seed
        cp -- "$cps_fixture/runtime-seed-public.der" seed-public.der
    fi
    cp -- "$busybox_key" busybox-public.der
    boot_kernel_dependency='    require "system/kernel"'
    if [[ -n $kernel_fixture ]]; then
        cp -a -- "$kernel_fixture/repo" signed-kernel
        cp -- "$kernel_fixture/release-public.der" kernel-public.der
        boot_kernel_dependency=
    fi
    boot_systemd_dependency=
    if [[ -n $systemd_fixture ]]; then
        cp -a -- "$systemd_fixture/repo" signed-systemd
        cp -- "$systemd_fixture/release-public.der" systemd-public.der
        if [[ -n $util_linux_fixture ]]; then
            cp -a -- "$util_linux_fixture/repo" signed-util-linux
            cp -- "$util_linux_fixture/release-public.der" util-linux-public.der
        else
            cp -a -- "$systemd_fixture/runtime-seed" signed-systemd-seed
            cp -- "$systemd_fixture/runtime-seed-public.der" systemd-seed-public.der
        fi
        boot_systemd_dependency="    require \"system/systemd-vm\" version=\"=$systemd_version\""
    fi
    openssl genpkey -algorithm ED25519 -outform DER -out profile-private.der
    openssl pkey -inform DER -in profile-private.der -pubout -outform DER -out profile-public.der
    cat > boot-stack.kdl <<MANIFEST
schema-version 1
package "test/boot-stack" publisher="vm-profile" version="1.0.0-0" {
    require "test/boot-state"
    require "test/boot-runtime"
$boot_kernel_dependency
$boot_systemd_dependency
    require "system/cps" version="=$cps_version"
    require "system/sqlite" version="=$sqlite_version"
    require "system/libseccomp" version="=$seccomp_version"
    require "system/busybox" version="=$busybox_version"
}
MANIFEST
    "$cps" publish --repo profile-repo --proto proto --signing-key profile-private.der boot-stack.kdl
    base_test_catalog=$(cat repo/current)
    refresh_vm_profile
    {
        cat <<'CONFIG'
schema-version 1
facet "devel" #false
publisher "vm-profile" {
    repository "profile-repo"
    trust-key "profile-public.der"
}
CONFIG
        if [[ $reprovision == 1 ]]; then
            printf 'image-id "A47593C2-E985-42D2-92B6-44C7057E1E96"\n'
            printf 'release-profile "boot" bootstrap-catalog="%s"\n' "$(cat profile-repo/current)"
        else
            printf 'release-profile "boot"\n'
        fi
        while read -r name repository key; do
            printf 'publisher "%s" {\n    repository "%s"\n    trust-key "%s"\n}\n' "$name" "$repository" "$key"
        done < <(profile_companions)
    } > image.kdl
else
    cat > image.kdl <<CONFIG
schema-version 1
facet "devel" #false
publisher "$fixture_publisher" {
    repository "repo"
    trust-key "public.der"
}
CONFIG
fi
"$cps" image-create --root base --config image.kdl
if ! $profiled && [[ -n $cps_fixture ]]; then
    "$cps" publisher add --root base --repo "$cps_fixture/repo" \
        --trust-key "$cps_fixture/release-public.der" canopa
    "$cps" publisher add --root base --repo "$cps_fixture/runtime-seed" \
        --trust-key "$cps_fixture/runtime-seed-public.der" canopa-runtime-seed
fi
if ! $profiled && [[ -n $busybox_repo ]]; then
    "$cps" publisher add --root base --repo "$busybox_repo" --trust-key "$busybox_key" canopa-bootstrap
fi
if $profiled; then
    "$cps" plan --root base --output install.json install-profile
else
    "$cps" plan --root base --output install.json install "${packages[@]}"
fi
"$cps" apply --root base install.json
"$cps" verify --root base --pristine
if [[ $reprovision == 1 ]]; then
    "$cps" image-create --root repro-base --config image.kdl
    "$cps" plan --root repro-base --output repro-install.json install-profile
    cmp install.json repro-install.json
    "$cps" apply --root repro-base repro-install.json
    "$cps" verify --root repro-base --pristine
fi
check_runtime_packages() {
    local root=$1 path
    if [[ -n $cps_fixture ]]; then
        for path in usr/include/{sqlite3,sqlite3ext,seccomp,seccomp-syscalls}.h \
            usr/lib/pkgconfig/{sqlite3,libseccomp}.pc usr/lib/{libsqlite3,libseccomp}.so; do
            test ! -e "$root/$path"
            test ! -L "$root/$path"
        done
    fi
    if [[ -z $util_linux_fixture ]]; then return; fi
    for path in usr/bin/{blkid,mount,umount} usr/lib/{libblkid,libmount}.so.1; do
        cmp "$root/$path" "$util_linux_fixture/image/$path"
    done
    test ! -e "$root/usr/include/libmount/libmount.h"
    test ! -L "$root/usr/lib/libmount.so"
}
check_runtime_packages base
if [[ -n $usr_base_fixture ]]; then
    test "$(readlink base/bin)" = usr/bin
    test "$(readlink base/lib64)" = usr/lib
fi
if [[ -n $systemd_fixture ]]; then
    cmp "base/$systemd_efi" "$efi"
    cmp base/usr/lib/systemd/systemd "$systemd_fixture/image/usr/lib/systemd/systemd"
    cp -- "base/$systemd_efi" esp/EFI/BOOT/BOOTX64.EFI
fi
if [[ -n $cps_fixture ]]; then
    for path in usr/bin/cps "${cps_library_paths[@]}"; do
        cmp "base/$path" "$cps_fixture/image/$path"
    done
    for path in "${elf_runtime_paths[@]}"; do
        cmp "base/$path" "$elf_runtime/image/$path"
    done
fi
if $profiled; then
    env -i base/lib64/ld-linux-x86-64.so.2 --inhibit-cache --library-path "$PWD/base/usr/lib" \
        base/usr/bin/cps publisher list --root base >/dev/null
fi

"$cps" boot-env export --receipt --root base --output base.ext4 --size-mib "$root_mib" > base.kdl
base_payload=$(sha256sum base.ext4)
truncate -s 96M esp.fat
mkfs.fat -F 32 esp.fat >/dev/null
mcopy -s -i esp.fat esp/EFI ::/
dd if=esp.fat of=template.raw bs=1M seek=1 conv=notrunc,sparse status=none
base_disk=bootable-base.raw
if [[ $device_activation == 1 ]]; then
    run_device_guest bootstrap template.raw "$base_disk" base
    base_id=$(sed -n 's/^CPS_VM_BOOTSTRAP: //p' device-bootstrap-activation.log)
    test -n "$base_id"
    cmp -n 1048576 template.raw "$base_disk"
    cmp -i "$((base_mib * 1024 * 1024)):0" -n "$((root_mib * 1024 * 1024))" "$base_disk" base.ext4
    cmp -i "$((trial_mib * 1024 * 1024))" template.raw "$base_disk"
else
    "$cps" boot-env deploy --disk template.raw --root-image base.ext4 \
        --root-partuuid "$base_uuid" --output base.raw
    base_id=$("$cps" boot-env activate --known-good --root base \
        --disk base.raw --receipt base.kdl --esp-partuuid "$esp_uuid" --output-disk "$base_disk" \
        --root-partuuid "$base_uuid" --options "$options")
    base_id=${base_id#Activated }
fi
if [[ $reprovision == 1 ]]; then
    test "$base_id" = cps-a47593c2-e985-42d2-92b6-44c7057e1e96-1
    "$cps" boot-env export --receipt --root repro-base --output repro-base.ext4 \
        --size-mib "$root_mib" > repro-base.kdl
    cmp base.ext4 repro-base.ext4
    cmp base.kdl repro-base.kdl
    "$cps" boot-env activate --known-good --root repro-base \
        --disk template.raw --receipt repro-base.kdl --esp-partuuid "$esp_uuid" \
        --output-disk repro-base.raw --root-partuuid "$base_uuid" \
        --options "$options" --root-image repro-base.ext4
    cmp "$base_disk" repro-base.raw
    echo 'Verified independent signed-profile installations produced byte-identical boot disks'
fi
if [[ $mode == rollback ]]; then
    # A new upgrade should still be testable from an explicitly selected base.
    primary_pointer=repo/current
    if $profiled; then primary_pointer=profile-repo/current; fi
    cp "$primary_pointer" base-catalog
    "$cps" boot-env rollback --disk "$base_disk" --esp-partuuid "$esp_uuid" \
        --output-disk pinned-base.raw "$base_id"
    base_disk=pinned-base.raw
    if ! $profiled; then
        publish_extra_marker
        "$cps" publisher add --root base --repo extras --trust-key extras-public.der extras
    fi
fi
mcopy -s -i "$base_disk@@1048576" ::/loader esp/
mcopy -s -i "$base_disk@@1048576" ::/EFI/cps esp/EFI/
test -f "esp/loader/entries/$base_id.conf"

publish_state 2.0.0-0 trial
if [[ -z $kernel_fixture && ( $mode == kernel-upgrade || $mode == rollback ) ]]; then
    publish_kernel 2.0.0-0 "$trial_release" "$trial_kernel"
fi
if [[ $mode == rollback ]]; then
    retire_kernel=
    if [[ -n $kernel_fixture ]]; then
        retire_kernel="    obsolete \"cps://$fixture_publisher/system/kernel@2.0.0-0\""
    fi
    cat > retirement.kdl <<RELEASE
schema-version 1
release "$fixture_publisher" base="$(cat repo/current)" {
    obsolete "cps://$fixture_publisher/test/legacy-marker@2.0.0-0"
$retire_kernel
}
RELEASE
    "$cps" repo publish-release --repo repo --signing-key private.der retirement.kdl
fi
if $profiled; then
    "$cps" plan --root base --output before-profile.json update > before-profile.log
    grep -Fq "\"$base_test_catalog\"" before-profile.json
    if grep -Eq '(Update|Install|Remove) .*system/kernel' before-profile.log; then
        echo 'The companion update escaped its signed profile pin' >&2
        exit 1
    fi
    echo 'Verified the signed profile kept the previous kernel after the companion advanced'
    refresh_vm_profile
fi
"$cps" plan --root base --output update.json update | tee update.log
cp -a esp esp-before
trial=$("$cps" boot-env stage --source base --root staged update.json)
trial_id=${trial#Staged }
trial_id=${trial_id%% at *}
diff -r esp-before esp
test "$(cat base/etc/cps-vm-state)" = previous
test "$(cat staged/etc/cps-vm-state)" = trial
grep -Fq 'Link etc/cps-vm-boot-state to etc/cps-vm-state' update.log
"$cps" verify --root base --pristine
"$cps" verify --root staged --pristine
check_runtime_packages staged

if [[ $mode == rollback ]]; then
    cp "$primary_pointer" trial-catalog
    cp base-catalog "$primary_pointer"
    "$cps" plan --root base --output base-catalog-plan.json update >/dev/null
    if "$cps" plan --root staged --output replay.json update > replay.log 2>&1; then
        echo 'The staged environment accepted a replayed primary catalog' >&2
        exit 1
    fi
    grep -F 'publisher catalog rollback or fork' replay.log
    mv trial-catalog "$primary_pointer"
fi

if [[ $mode == kernel-upgrade || $mode == rollback ]]; then
    grep -Fq "Prepare Linux $trial_release with kernel-install" update.log
    cmp "$kernel" "base/usr/lib/modules/$base_release/vmlinuz"
    test ! -e "staged/usr/lib/modules/$base_release/vmlinuz"
    cmp "$trial_kernel" "staged/usr/lib/modules/$trial_release/vmlinuz"
    prepared=
    for file in staged/.cps/kernel-*/linux; do
        if cmp -s "$file" "$trial_kernel"; then prepared=$file; break; fi
    done
    test -n "$prepared"
    if [[ -n $kernel_fixture ]]; then
        grep -Fq 'Install cps://canopa-kernel/system/kernel-qemu 6.12.115-0' update.log
        cmp "$kernel_fixture/image/usr/lib/modules/$trial_release/config" "staged/usr/lib/modules/$trial_release/config"
        test ! -e "${prepared%/*}/initrd"
    else
        cmp "staged/usr/lib/modules/$trial_release/initrd" "${prepared%/*}/initrd"
    fi
    printf 'corrupt prepared kernel' > "$prepared"
    if "$cps" boot-env export --root staged --output rejected.ext4 --size-mib "$root_mib" 2> rejected.log; then
        echo "Export accepted a corrupted prepared kernel" >&2
        exit 1
    fi
    grep -q 'kernel artifact digest mismatch' rejected.log
    test ! -e rejected.ext4
    if "$cps" boot-env activate --root staged --esp esp --root-partuuid "$trial_uuid" \
        --options "$options" 2> rejected.log; then
        echo "Activation accepted a corrupted prepared kernel" >&2
        exit 1
    fi
    grep -q 'kernel artifact digest mismatch' rejected.log
    cp "$trial_kernel" "$prepared"
    diff -r esp-before esp
fi

if [[ $recycle == 1 ]]; then
    printf 'administrator-setting\n' > staged/etc/cps-vm.conf
    printf 'administrator-policy\n' > staged/etc/cps-vm-policy.conf
fi
"$cps" boot-env export --receipt --root staged --output staged.ext4 --size-mib "$root_mib" > staged.kdl
test "$(sha256sum base.ext4)" = "$base_payload"
diff -r esp-before esp
if [[ $device_activation != 1 ]]; then
    "$cps" boot-env deploy --disk "$base_disk" --root-image staged.ext4 \
        --root-partuuid "$trial_uuid" --output deployed.raw
    cmp -n "$((trial_mib * 1024 * 1024))" "$base_disk" deployed.raw
fi

mv "esp/EFI/cps/$base_id/vmlinuz.efi" saved-kernel
if "$cps" boot-env activate --root staged --esp esp --root-partuuid "$trial_uuid" \
    --options "$options" 2> rejected.log; then
    echo "Activation accepted a missing fallback kernel" >&2
    exit 1
fi
grep -q 'known-good fallback kernel' rejected.log
mv saved-kernel "esp/EFI/cps/$base_id/vmlinuz.efi"

if "$cps" boot-env activate --root staged --esp esp --root-partuuid "$base_uuid" \
    --options "$options" 2> rejected.log; then
    echo "Activation accepted the fallback's root partition" >&2
    exit 1
fi
grep -q 'distinct root PARTUUID' rejected.log
diff -r esp-before esp

if [[ $device_activation == 1 ]]; then
    run_device_guest initial "$base_disk" disk.raw staged
    cmp -n 1048576 "$base_disk" disk.raw
    cmp -i "$((base_mib * 1024 * 1024))" -n "$((root_mib * 1024 * 1024))" "$base_disk" disk.raw
    cmp -i "$((trial_mib * 1024 * 1024)):0" -n "$((root_mib * 1024 * 1024))" disk.raw staged.ext4
    cmp -i "$(((trial_mib + root_mib) * 1024 * 1024))" "$base_disk" disk.raw
else
    "$cps" boot-env activate --root staged --root-partuuid "$trial_uuid" \
        --disk deployed.raw --receipt staged.kdl --esp-partuuid "$esp_uuid" --output-disk disk.raw \
        --options "$options" --tries 2
    cmp -n 1048576 deployed.raw disk.raw
    cmp -i "$((base_mib * 1024 * 1024))" deployed.raw disk.raw
fi
mkdir esp-activated
mcopy -s -i disk.raw@@1048576 ::/EFI ::/loader esp-activated/
diff -r esp-before/EFI/BOOT esp-activated/EFI/BOOT
mv esp esp-detached
mv esp-activated esp
"$cps" boot-env activate --root staged --esp esp --root-partuuid "$trial_uuid" \
    --options "$options" --tries 9
test -f "esp/loader/entries/$trial_id+2.conf"
test ! -e "esp/loader/entries/$trial_id+9.conf"
cmp "esp-before/loader/entries/$base_id.conf" "esp/loader/entries/$base_id.conf"

if [[ $mode == kernel-upgrade || $mode == rollback ]]; then
    cmp "$kernel" "esp/EFI/cps/$base_id/vmlinuz.efi"
    cmp "$trial_kernel" "esp/EFI/cps/$trial_id/vmlinuz.efi"
    cmp "base/usr/lib/modules/$base_release/initrd" "esp/EFI/cps/$base_id/initramfs.img"
    if [[ -n $kernel_fixture ]]; then
        test ! -e "esp/EFI/cps/$trial_id/initramfs.img"
    else
        cmp "staged/usr/lib/modules/$trial_release/initrd" "esp/EFI/cps/$trial_id/initramfs.img"
    fi
fi

if [[ $mode == rollback ]]; then
    grep -Fq 'Install cps://extras/test/extra-marker 1.0.0-0' update.log
    grep -Fq "Remove cps://$fixture_publisher/test/legacy-marker 1.0.0-0" update.log
    test ! -e base/etc/cps-vm-extra-marker
    test "$(cat staged/etc/cps-vm-extra-marker)" = extras
    test "$(cat base/etc/cps-vm-legacy-marker)" = legacy
    test ! -e staged/etc/cps-vm-legacy-marker
    cp esp/loader/loader.conf config-before-rejected-rollback
    if "$cps" boot-env rollback --esp esp "$trial_id" 2> rejected.log; then
        echo 'Rollback accepted an unblessed trial' >&2
        exit 1
    fi
    grep -q 'is not known-good' rejected.log
    cmp config-before-rejected-rollback esp/loader/loader.conf
fi

if [[ $mode == health-fail ]]; then
    # Simulate an offline modification after checked activation.
    debugfs -w -R 'set_inode_field /etc/cps-vm-state mode 0100600' staged.ext4 >/dev/null
    debugfs -R 'stat /etc/cps-vm-state' staged.ext4 2>/dev/null | grep -Eq 'Mode: +0600'
    dd if=staged.ext4 of=disk.raw bs=1M seek="$trial_mib" conv=notrunc status=none
fi

cp "$vars" vars.fd

args=()
if [[ -n $qemu_data ]]; then args+=(-L "$qemu_data"); fi
run_vm() {
    local log=$1
    local disk=${2:-disk.raw}
    if ! timeout "${CPS_VM_TIMEOUT:-180}" "$qemu" "${args[@]}" \
        -machine q35,accel=tcg -m 768 -smp 2 -nic none -display none -monitor none \
        -serial "file:$log" \
        -drive "if=pflash,format=raw,unit=0,readonly=on,file=$code" \
        -drive if=pflash,format=raw,unit=1,file=vars.fd \
        -drive "if=virtio,format=raw,file=$disk"; then
        test ! -f "$log" || tail -80 "$log" >&2
        return 1
    fi
}
check_busybox() {
    if [[ -n $busybox_repo ]] && $systemd_guest; then
        local log=$1
        tr -d '\r' < "$log" | awk -v version="v${busybox_version%-*}" -v signed="$signed_insmod" '
            /^CPS_VM_BOOT: / { boots++ }
            /^CPS_VM_BUSYBOX: / {
                print
                if ($2 != version) invalid = 1
                versions++
            }
            /^CPS_VM_INSMOD: / { print; module_checks++ }
            END {
                exit (invalid || boots == 0 || boots != versions ||
                      (signed == "true" && boots != module_checks))
            }
        '
    fi
}
run_vm serial.log
check_busybox serial.log

case $mode in
    bless|rollback|capabilities) expected=$'CPS_VM_BOOT: trial blessed\nCPS_VM_BOOT: trial default' ;;
    health-fail) expected=$'CPS_VM_BOOT: trial health-failed\nCPS_VM_BOOT: trial health-failed\nCPS_VM_BOOT: previous' ;;
    fallback|kernel-upgrade) expected=$'CPS_VM_BOOT: trial\nCPS_VM_BOOT: trial\nCPS_VM_BOOT: previous' ;;
esac
boots=$(tr -d '\r' < serial.log | grep '^CPS_VM_BOOT: ' || :)
if [[ $boots != "$expected" ]]; then
    printf 'Unexpected boot order:\n%s\n' "$boots" >&2
    tail -80 serial.log >&2
    exit 1
fi
if [[ $mode == kernel-upgrade || $mode == rollback ]]; then
    kernels=$(tr -d '\r' < serial.log | grep '^CPS_VM_KERNEL: ' || :)
    expected_kernels=$(printf 'CPS_VM_KERNEL: trial %s\nCPS_VM_KERNEL: trial %s' "$trial_release" "$trial_release")
    if [[ $mode == kernel-upgrade ]]; then
        expected_kernels+=$'\n'"CPS_VM_KERNEL: previous $base_release"
    fi
    if [[ $kernels != "$expected_kernels" ]]; then
        printf 'Unexpected root and kernel pairs:\n%s\n' "$kernels" >&2
        exit 1
    fi
    printf '%s\n' "$kernels"
fi
mcopy -i disk.raw@@1048576 "::/loader/entries/$base_id.conf" previous.conf
cmp "esp/loader/entries/$base_id.conf" previous.conf
if [[ $mode == bless || $mode == rollback || $mode == capabilities ]]; then
    mcopy -i disk.raw@@1048576 "::/loader/entries/$trial_id.conf" blessed.conf
    cmp "esp/loader/entries/$trial_id+2.conf" blessed.conf
    listing=$(mdir -b -i disk.raw@@1048576 ::/loader/entries)
    if grep -Fq "$trial_id+" <<< "$listing"; then
        echo "A counted trial entry remains after blessing" >&2
        exit 1
    fi
    printf '%s\nVerified systemd-boot blessed %s.conf\n' "$boots" "$trial_id"
    if [[ $mode == capabilities ]]; then
        tr -d '\r' < serial.log | grep -Fx 'CPS_VM_CAPABILITIES: verified'
    fi
    if [[ $mode == rollback ]]; then
        if [[ -n $cps_fixture ]]; then
            runtimes=$(tr -d '\r' < serial.log | grep '^CPS_VM_CPS_RUNTIME: ' || :)
            test "$runtimes" = $'CPS_VM_CPS_RUNTIME: trial verified\nCPS_VM_CPS_RUNTIME: trial verified'
            printf '%s\n' "$runtimes"
        fi
        if $signed_cps_builds; then
            audits=$(tr -d '\r' < serial.log | grep '^CPS_VM_CPS_BUILDS: ')
            test "$audits" = $'CPS_VM_CPS_BUILDS: trial verified\nCPS_VM_CPS_BUILDS: trial verified'
            printf '%s\n' "$audits"
        fi
        if [[ -n $systemd_fixture ]]; then
            managers=$(tr -d '\r' < serial.log | grep '^CPS_VM_SYSTEMD: ' || :)
            test "$managers" = $'CPS_VM_SYSTEMD: trial v262.1\nCPS_VM_SYSTEMD: trial v262.1'
            printf '%s\n' "$managers"
        fi
        if [[ -n $usr_base_fixture ]]; then
            layouts=$(tr -d '\r' < serial.log | grep '^CPS_VM_USR: ' || :)
            test "$layouts" = $'CPS_VM_USR: trial merged\nCPS_VM_USR: trial merged'
            if grep -F 'unmerged-usr' serial.log; then exit 1; fi
            printf '%s\n' "$layouts"
        fi
        if [[ -n $util_linux_fixture ]]; then
            utilities=$(tr -d '\r' < serial.log | grep '^CPS_VM_UTIL_LINUX: ' || :)
            test "$utilities" = $'CPS_VM_UTIL_LINUX: trial 2.42.4\nCPS_VM_UTIL_LINUX: trial 2.42.4'
            printf '%s\n' "$utilities"
        fi
        if $signed_insmod; then
            modules=$(awk '{printf " %s", $2}' "staged/vm-modules/$trial_release/load")
            loaded=$(tr -d '\r' < serial.log | grep '^CPS_VM_INSMOD: ')
            test "$loaded" = "$(printf 'CPS_VM_INSMOD: trial%s\nCPS_VM_INSMOD: trial%s' "$modules" "$modules")"
        fi
        publishers=$(tr -d '\r' < serial.log | grep '^CPS_VM_PUBLISHER: ' || :)
        test "$publishers" = $'CPS_VM_PUBLISHER: trial extras\nCPS_VM_PUBLISHER: trial extras'
        obsoletions=$(tr -d '\r' < serial.log | grep '^CPS_VM_OBSOLETION: ' || :)
        test "$obsoletions" = $'CPS_VM_OBSOLETION: trial removed\nCPS_VM_OBSOLETION: trial removed'
        "$cps" boot-env list --disk disk.raw --esp-partuuid "$esp_uuid" | tee before-rollback.log
        assert_default() {
            awk -F '\t' -v id="$1" -v part="$2" -v release="$3" \
                '$1 == "*" && $2 == "known-good" && $3 == id && $4 == part && index($5, release) {found=1} END {exit !found}' "$4"
        }
        assert_default "$trial_id" "$trial_uuid" "$trial_release" before-rollback.log
        if [[ $device_activation == 1 ]]; then
            run_device_guest rollback disk.raw rollback.raw
            cmp -n 1048576 disk.raw rollback.raw
        else
            result=$("$cps" boot-env rollback --disk disk.raw --esp-partuuid "$esp_uuid" --output-disk rollback.raw)
            test "$result" = "Selected $base_id for the next boot"
        fi
        "$cps" boot-env list --disk rollback.raw --esp-partuuid "$esp_uuid" | tee after-rollback.log
        assert_default "$base_id" "$base_uuid" "$base_release" after-rollback.log
        grep -Fq "$trial_id" after-rollback.log
        mcopy -i rollback.raw@@1048576 "::/loader/entries/$trial_id.conf" rollback-trial.conf
        mcopy -i rollback.raw@@1048576 "::/loader/entries/$base_id.conf" rollback-base.conf
        cmp blessed.conf rollback-trial.conf
        cmp previous.conf rollback-base.conf
        cmp -i "$((base_mib * 1024 * 1024))" disk.raw rollback.raw
        run_vm rollback.log rollback.raw
        check_busybox rollback.log
        rollback_boots=$(tr -d '\r' < rollback.log | grep '^CPS_VM_BOOT: ' || :)
        rollback_kernels=$(tr -d '\r' < rollback.log | grep '^CPS_VM_KERNEL: ' || :)
        rollback_publishers=$(tr -d '\r' < rollback.log | grep '^CPS_VM_PUBLISHER: ' || :)
        rollback_obsoletions=$(tr -d '\r' < rollback.log | grep '^CPS_VM_OBSOLETION: ' || :)
        if [[ $rollback_boots != 'CPS_VM_BOOT: previous' || $rollback_kernels != "CPS_VM_KERNEL: previous $base_release" ]]; then
            printf 'Unexpected rollback boot:\n%s\n%s\n' "$rollback_boots" "$rollback_kernels" >&2
            tail -80 rollback.log >&2
            exit 1
        fi
        test "$rollback_publishers" = 'CPS_VM_PUBLISHER: previous absent'
        test "$rollback_obsoletions" = 'CPS_VM_OBSOLETION: previous retained'
        if [[ -n $cps_fixture ]]; then
            tr -d '\r' < rollback.log | grep -Fx 'CPS_VM_CPS_RUNTIME: previous verified'
        fi
        if $signed_cps_builds; then
            tr -d '\r' < rollback.log | grep -Fx 'CPS_VM_CPS_BUILDS: previous verified'
        fi
        if [[ -n $systemd_fixture ]]; then
            tr -d '\r' < rollback.log | grep -Fx 'CPS_VM_SYSTEMD: previous v262.1'
        fi
        if [[ -n $usr_base_fixture ]]; then
            tr -d '\r' < rollback.log | grep -Fx 'CPS_VM_USR: previous merged'
            if grep -F 'unmerged-usr' rollback.log; then exit 1; fi
        fi
        if [[ -n $util_linux_fixture ]]; then
            tr -d '\r' < rollback.log | grep -Fx 'CPS_VM_UTIL_LINUX: previous 2.42.4'
        fi
        if $signed_insmod; then
            modules=$(awk '{printf " %s", $2}' "base/vm-modules/$base_release/load")
            tr -d '\r' < rollback.log | grep -Fx "CPS_VM_INSMOD: previous$modules"
        fi
        printf '%s\n' "$obsoletions" "$rollback_boots" "$rollback_kernels" "$rollback_publishers" "$rollback_obsoletions"
        if [[ $recycle == 1 ]]; then
            # Start from the disk with the blessed upgrade, before rollback.
            configs=$(tr -d '\r' < serial.log | grep '^CPS_VM_CONFIG: ')
            test "$configs" = $'CPS_VM_CONFIG: trial administrator-setting\nCPS_VM_CONFIG: trial administrator-setting'
            publish_state 3.0.0-0 recycled
            if $profiled; then refresh_vm_profile; fi
            if [[ $installed_stage == 1 ]]; then
                run_device_guest installed disk.raw recycled.raw
                recycled_id=$(sed -n 's/^CPS_VM_INSTALLED_STAGE: //p' device-installed-activation.log)
                test -n "$recycled_id"
            else
                "$cps" plan --root staged --output recycle.json update | tee recycle-plan.log
                grep -Fq 'test/boot-state 2.0.0-0 -> 3.0.0-0' recycle-plan.log
                result=$("$cps" boot-env stage --source staged --root recycled recycle.json)
                recycled_id=${result#Staged }
                recycled_id=${recycled_id%% at *}
                test "$(cat recycled/etc/cps-vm.conf)" = administrator-setting
                test "$(cat recycled/etc/cps-vm-policy.conf)" = administrator-policy
                test "$(stat -c %a staged/etc/cps-vm-policy.conf)" = 644
                test "$(stat -c %a recycled/etc/cps-vm-policy.conf)" = 600
                "$cps" config list --root recycled | tee config-review.log
                grep -Fq '"etc/cps-vm.conf"' config-review.log
                "$cps" plan --root recycled --output config-review.json \
                    resolve-config etc/cps-vm.conf --take vendor
                "$cps" apply --root recycled config-review.json
                "$cps" plan --root recycled --output policy-review.json \
                    resolve-config etc/cps-vm-policy.conf --take local
                "$cps" apply --root recycled policy-review.json
                test "$("$cps" config list --root recycled)" = 'No pending configuration updates'
                "$cps" boot-env export --receipt --root recycled --output recycled.ext4 \
                    --size-mib "$root_mib" > recycled.kdl
                if [[ $device_activation == 1 ]]; then
                    run_device_guest replace disk.raw replaced.raw recycled
                    run_device_guest resume disk.raw recycled.raw recycled
                    cmp -i "$((base_mib * 1024 * 1024))" replaced.raw recycled.raw
                    for output in replaced.raw recycled.raw; do
                        cmp -i "$((base_mib * 1024 * 1024)):0" -n "$((root_mib * 1024 * 1024))" \
                            "$output" recycled.ext4
                        "$cps" boot-env list --disk "$output" --esp-partuuid "$esp_uuid" > "$output.list"
                    done
                    cmp replaced.raw.list recycled.raw.list
                else
                    result=$("$cps" boot-env activate --root recycled --root-image recycled.ext4 \
                        --root-partuuid "$base_uuid" --replace "$base_id" \
                        --disk disk.raw --receipt recycled.kdl --esp-partuuid "$esp_uuid" \
                        --output-disk recycled.raw --options "$options")
                    test "$result" = "Activated $recycled_id"
                fi
            fi
            cmp -n 1048576 disk.raw recycled.raw
            cmp -i "$((trial_mib * 1024 * 1024))" disk.raw recycled.raw
            "$cps" boot-env list --disk recycled.raw --esp-partuuid "$esp_uuid" | tee recycled-trial.log
            grep -Fq "$recycled_id" recycled-trial.log
            if grep -Fq "$base_id" recycled-trial.log ||
                mdir -i recycled.raw@@1048576 "::/EFI/cps/$base_id" >/dev/null 2>&1; then
                echo 'Retired environment is still present in the recycled disk' >&2
                exit 1
            fi

            cp "$vars" vars.fd
            run_vm recycled-boot.log recycled.raw
            check_busybox recycled-boot.log
            recycled_boots=$(tr -d '\r' < recycled-boot.log | grep '^CPS_VM_BOOT: ')
            test "$recycled_boots" = $'CPS_VM_BOOT: recycled blessed\nCPS_VM_BOOT: recycled default'
            configs=$(tr -d '\r' < recycled-boot.log | grep '^CPS_VM_CONFIG: ')
            test "$configs" = $'CPS_VM_CONFIG: recycled vendor-3.0.0-0\nCPS_VM_CONFIG: recycled vendor-3.0.0-0'
            policies=$(tr -d '\r' < recycled-boot.log | grep '^CPS_VM_POLICY: ')
            test "$policies" = $'CPS_VM_POLICY: recycled administrator-policy\nCPS_VM_POLICY: recycled administrator-policy'
            "$cps" boot-env list --disk recycled.raw --esp-partuuid "$esp_uuid" | tee recycled-blessed.log
            assert_default "$recycled_id" "$base_uuid" "$trial_release" recycled-blessed.log
            result=$("$cps" boot-env rollback --disk recycled.raw --esp-partuuid "$esp_uuid" \
                --output-disk recycled-rollback.raw)
            test "$result" = "Selected $trial_id for the next boot"
            run_vm recycled-rollback.log recycled-rollback.raw
            check_busybox recycled-rollback.log
            test "$(tr -d '\r' < recycled-rollback.log | grep '^CPS_VM_BOOT: ')" = 'CPS_VM_BOOT: trial default'
            tr -d '\r' < recycled-rollback.log | grep -Fx "CPS_VM_KERNEL: trial $trial_release"
            tr -d '\r' < recycled-rollback.log | grep -Fx 'CPS_VM_CONFIG: trial administrator-setting'
            tr -d '\r' < recycled-rollback.log | grep -Fx 'CPS_VM_POLICY: trial administrator-policy'
            printf '%s\nVerified the recycled slot and rolled back to the preceding root\n' "$recycled_boots"
        fi
    fi
    exit 0
fi

mcopy -i disk.raw@@1048576 "::/loader/entries/$trial_id+0-2.conf" exhausted.conf
if [[ $mode == health-fail ]]; then
    grep -q 'etc/cps-vm-state: content, type, or metadata differs' serial.log
    printf '%s\nVerified the health gate blocked blessing and systemd-boot fell back\n' "$boots"
    exit 0
fi
# Reproduce the exhausted ESP state on the host-facing directory and verify
# that another invocation of activate cannot replenish the boot counter.
mv "esp/loader/entries/$trial_id+2.conf" "esp/loader/entries/$trial_id+0-2.conf"
if "$cps" boot-env activate --root staged --esp esp --root-partuuid "$trial_uuid" \
    --options "$options" 2> rejected.log; then
    echo "Activation reset the exhausted counter" >&2
    exit 1
fi
grep -q 'boot attempts are exhausted' rejected.log
printf '%s\nVerified systemd-boot counter %s+0-2.conf\n' "$boots" "$trial_id"
