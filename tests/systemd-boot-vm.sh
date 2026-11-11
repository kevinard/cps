#!/usr/bin/env bash
# Run with a writable temporary directory and local, non-Secure-Boot OVMF firmware.
set -euo pipefail
umask 022

mode=${1:-fallback}
health_failure=false
case $mode in
    health-fail|catalog-health-fail) health_failure=true ;;
    fallback|bless|kernel-upgrade|rollback|capabilities|facet-change) ;;
    *) echo "Usage: $0 [fallback|bless|health-fail|catalog-health-fail|kernel-upgrade|rollback|capabilities|facet-change]" >&2; exit 2 ;;
esac
reprovision=${CPS_VM_REPROVISION:-0}
tuf_mirrors=${CPS_VM_TUF_MIRRORS:-0}
staged_transactions=${CPS_VM_STAGED_TRANSACTIONS:-0}

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
source "$project/tests/systemd-boot-guest.sh"
cps=${CPS_BIN:-$project/target/debug/cps}
cps_fixture=${CPS_VM_CPS_FIXTURE:-}
cps_version=${CPS_VM_CPS_VERSION:-0.1.0-40}
kernel_fixture=${CPS_VM_KERNEL_FIXTURE:-}
if [[ $reprovision == 1 && ( ${CPS_VM_RELEASE_PROFILE:-0} != 1 || -z ${SOURCE_DATE_EPOCH:-} ) ]]; then
    echo 'CPS_VM_REPROVISION requires a signed release profile and SOURCE_DATE_EPOCH' >&2
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
protected_profiles=false
package_intent=false
package_freezes=false
base_revision=1
qemu=${CPS_VM_QEMU:-qemu-system-x86_64}
efi=${CPS_VM_SYSTEMD_BOOT:-/usr/lib/systemd/boot/efi/systemd-bootx64.efi}
if [[ -n $systemd_fixture ]]; then
    systemd_fixture=$(realpath -- "$systemd_fixture")
    efi="$systemd_fixture/image/$systemd_efi"
fi
kernel=${CPS_VM_KERNEL:-/boot/vmlinuz-linux}
busybox=${CPS_VM_BUSYBOX:-/usr/lib/initcpio/busybox}
busybox_repo=${CPS_VM_BUSYBOX_REPO:-}
busybox_key=${CPS_VM_BUSYBOX_TRUST_ROOT:-}
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

mkfs_btrfs=${CPS_VM_MKFS_BTRFS:-mkfs.btrfs}
for command in "$qemu" "$mkfs_btrfs" mkfs.fat fsck.fat mcopy mdir mmd sfdisk openssl strip timeout; do
    command -v "$command" >/dev/null || { echo "Missing command: $command" >&2; exit 1; }
done
if [[ -n $busybox_repo || -n $busybox_key ]]; then
    busybox_repo=$(realpath -- "${busybox_repo:?Set CPS_VM_BUSYBOX_REPO}")
    busybox_key=$(realpath -- "${busybox_key:?Set CPS_VM_BUSYBOX_TRUST_ROOT}")
    case $busybox_version in
        "1.38.0-$busybox_revision") signed_insmod=true ;;
        "1.37.0-$busybox_revision") ;;
        *) echo "Unsupported BusyBox version for this layout: $busybox_version" >&2; exit 2 ;;
    esac
    "$cps" repo attestation --repo "$busybox_repo" \
        --trust-root "$busybox_key" "$busybox_package" >/dev/null
else
    test -r "$busybox" || { echo "Missing input: $busybox" >&2; exit 1; }
fi
if [[ $mode == catalog-health-fail ]]; then command -v sqlite3 >/dev/null; fi
for file in "$cps" "$efi" "$kernel" "$code" "$vars"; do
    test -r "$file" || { echo "Missing input: $file" >&2; exit 1; }
done

systemd_guest=false
if $health_failure || [[ $mode == bless || $mode == rollback || $mode == capabilities || $mode == facet-change ]]; then
    systemd_guest=true
fi
fixture_publisher=canopa
boot_trust_dependency=
boot_git_build_dependency=
if [[ -n $cps_fixture ]]; then
    $systemd_guest || { echo 'CPS_VM_CPS_FIXTURE requires a systemd guest' >&2; exit 2; }
    cps_fixture=$(realpath -- "$cps_fixture")
    fixture_publisher=vm-test
    sqlite_version=3.54.1-1
    seccomp_version=2.6.1-1
    cps_packages=("system/cps@$cps_version" "system/sqlite@$sqlite_version" "system/libseccomp@$seccomp_version")
    if [[ -f "$cps_fixture/canopa-trust.kdl" ]]; then
        trust_version=$(sed -n 's/^package "system\/canopa-trust" .* version="\([^"]*\)".*/\1/p' \
            "$cps_fixture/canopa-trust.kdl")
        [[ $trust_version =~ ^1\.0\.0-[1-9][0-9]{0,14}$ ]] || {
            echo 'Could not determine the Canopa trust package version' >&2
            exit 2
        }
        cps_packages+=("system/canopa-trust@$trust_version")
        boot_trust_dependency="    require \"system/canopa-trust\" version=\"=$trust_version\""
    fi
    if [[ -f "$cps_fixture/git-build.kdl" ]]; then
        git_build_version=$(sed -n 's/^package "system\/git-build" .* version="\([^"]*\)".*/\1/p' \
            "$cps_fixture/git-build.kdl")
        [[ $git_build_version =~ ^[0-9]+\.[0-9]+\.[0-9]+-[0-9]+$ ]] || {
            echo 'Could not determine the Git build package version' >&2
            exit 2
        }
        cps_packages+=("system/git-build@$git_build_version")
        boot_git_build_dependency="    require \"system/git-build\" version=\"=$git_build_version\""
    fi
    for package in "${cps_packages[@]}"; do
        "$cps" repo attestation --repo "$cps_fixture/repo" \
            --trust-root "$cps_fixture/release.trust.json" "cps://canopa/$package" >/dev/null
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
    signed_cps_builds=true
    protected_profiles=true
    package_intent=true
    package_freezes=true
    base_revision=2
fi
if [[ $staged_transactions == 1 ]] && ! $profiled; then
    echo 'CPS_VM_STAGED_TRANSACTIONS requires a current signed profile' >&2
    exit 2
fi
if [[ $tuf_mirrors == 1 ]]; then
    if ! $profiled; then
        echo 'CPS_VM_TUF_MIRRORS requires a signed release profile' >&2
        exit 2
    fi
    tuf_fixture=$(realpath -- "${CPS_VM_TUF_FIXTURE:-$project/target/debug/examples/tuf-fixture}")
    test -x "$tuf_fixture" || { echo "Missing TUF fixture signer: $tuf_fixture" >&2; exit 1; }
fi
if [[ -n $kernel_fixture ]]; then
    $profiled || { echo 'CPS_VM_KERNEL_FIXTURE requires CPS_VM_RELEASE_PROFILE=1' >&2; exit 2; }
    kernel_fixture=$(realpath -- "$kernel_fixture")
    if [[ -f "$kernel_fixture/kernel-package" ]]; then kernel_package=$(cat "$kernel_fixture/kernel-package"); fi
    trial_release=$(cat "$kernel_fixture/kernel-release")
    test "$trial_release" = 6.12.115-cps-vm
    trial_kernel="$kernel_fixture/image/usr/lib/modules/$trial_release/vmlinuz"
    "$cps" repo attestation --repo "$kernel_fixture/repo" \
        --trust-root "$kernel_fixture/release.trust.json" "$kernel_package" >/dev/null
fi
if [[ -n $util_linux_fixture ]]; then
    [[ -n $usr_base_fixture ]] || { echo 'CPS_VM_UTIL_LINUX_FIXTURE requires the merged /usr fixture' >&2; exit 2; }
    util_linux_fixture=$(realpath -- "$util_linux_fixture")
    util_linux_version=$(sed -n 's/^package .* version="\([^"]*\)".*/\1/p' "$util_linux_fixture/util-linux.kdl")
    case $util_linux_version in 2.42.4-0|2.42.4-1) ;; *) echo 'Unsupported util-linux fixture' >&2; exit 2 ;; esac
    util_linux_package=cps://canopa-util-linux/system/util-linux@$util_linux_version
    systemd_version=262.1.0-4
    if [[ $util_linux_version == 2.42.4-1 ]]; then systemd_version=262.1.0-5; fi
    "$cps" repo attestation --repo "$util_linux_fixture/repo" \
        --trust-root "$util_linux_fixture/release.trust.json" "$util_linux_package" >/dev/null
fi
if [[ -n $systemd_fixture ]]; then
    $profiled || { echo 'CPS_VM_SYSTEMD_FIXTURE requires CPS_VM_RELEASE_PROFILE=1' >&2; exit 2; }
    systemd_version=${CPS_VM_SYSTEMD_VERSION:-$systemd_version}
    "$cps" repo attestation --repo "$systemd_fixture/repo" \
        --trust-root "$systemd_fixture/release.trust.json" "cps://canopa-systemd/system/systemd-vm@$systemd_version" >/dev/null
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

root_uuid=7adff456-af50-498f-b39a-dcf87cb4a7c1
esp_uuid=9a138d0b-f9aa-4a2a-b527-93392ad7e812
options='ro console=ttyS0 loglevel=3 init=/init'
root_mib=256
if $systemd_guest; then
    options='rw console=ttyS0 loglevel=3 init=/init systemd.log_target=console'
    root_mib=1024
elif [[ $mode == kernel-upgrade ]]; then
    root_mib=384
fi
# Use regular files so the fixture needs neither loop devices nor mounts.
# One Btrfs partition holds the base, its capture, and the staged trial.
truncate -s "$((97 + root_mib + 1))M" template.raw
sfdisk --quiet template.raw <<PARTITIONS
label: gpt
unit: sectors

start=2048, size=196608, type=uefi, uuid=$esp_uuid
start=$((97 * 2048)), size=$((root_mib * 2048)), type=linux, uuid=$root_uuid
PARTITIONS
mkdir -p esp/EFI/BOOT proto/etc
if [[ -z $systemd_fixture ]]; then cp "$efi" esp/EFI/BOOT/BOOTX64.EFI; fi
openssl genpkey -algorithm ED25519 -outform DER -out private.der
"$cps" repo trust init --repo repo --publisher "$fixture_publisher" --key private.der >/dev/null
cp repo/trust/1.json public.trust.json

publish_state() {
    local version=$1 state=$2
    local dependency= facet_action=
    if [[ $mode == facet-change ]]; then
        mkdir -p proto/usr/share/man/man1
        printf 'trial\n' > proto/usr/share/man/man1/cps-vm.1
        facet_action="    file \"usr/share/man/man1/cps-vm.1\" mode=\"0644\" uid=$(id -u) gid=$(id -g) facet=\"doc.man\""
        printf 'vendor settings\n' > proto/etc/cps-vm-optional.conf
        facet_action+=$'\n'"    file \"etc/cps-vm-optional.conf\" mode=\"0644\" uid=$(id -u) gid=$(id -g) preserve=#true facet=\"optional\""
    fi
    if [[ $mode == rollback ]]; then
        case $state in
            previous) dependency='    require "test/legacy-marker"' ;;
            trial) dependency='    require "test/extra-marker"' ;;
        esac
    fi
    if [[ -n $kernel_fixture ]]; then
        case $state in
            previous) dependency+=$'\n    require "system/kernel"' ;;
            trial) dependency+=$'\n    require "system/kernel-qemu"' ;;
        esac
    fi
    printf '%s\n' "$state" > proto/etc/cps-vm-state
    cat > manifest.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "test/boot-state" publisher="$fixture_publisher" version="$version" {
    file "etc/cps-vm-state" mode="0644" uid=$(id -u) gid=$(id -g)
    hardlink "etc/cps-vm-boot-state" target="etc/cps-vm-state"
$facet_action
$dependency
}
MANIFEST
    "$cps" publish manifests --repo repo --proto proto --key private.der manifest.kdl
}

publish_kernel() {
    local version=$1 release=$2 binary=$3
    local modules="usr/lib/modules/$release"
    mkdir -p "proto/$modules" initrd-tree
    cp "$binary" "proto/$modules/vmlinuz"
    printf '%s\n' "$release" > initrd-tree/cps-initrd-release
    (cd initrd-tree && printf 'cps-initrd-release\n' | cpio --quiet -o -H newc) > "proto/$modules/initrd"
    cat > kernel.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "system/kernel" publisher="$fixture_publisher" version="$version" {
    file "$modules/vmlinuz" mode="0644" uid=$(id -u) gid=$(id -g)
    file "$modules/initrd" mode="0644" uid=$(id -u) gid=$(id -g)
    kernel-install "$release" initrd=#true
}
MANIFEST
    "$cps" publish manifests --repo repo --proto proto --key private.der kernel.kdl
}

publish_extra_marker() {
    openssl genpkey -algorithm ED25519 -outform DER -out extras-private.der
    "$cps" repo trust init --repo extras --publisher extras --key extras-private.der >/dev/null
    cp extras/trust/1.json extras.trust.json
    printf 'extras\n' > proto/etc/cps-vm-extra-marker
    cat > extra-marker.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "test/extra-marker" publisher="extras" version="1.0.0-0" {
    file "etc/cps-vm-extra-marker" mode="0644" uid=$(id -u) gid=$(id -g)
}
MANIFEST
    "$cps" publish manifests --repo extras --proto proto --key extras-private.der extra-marker.kdl
}

profile_companions() {
    printf '%s\n' "$fixture_publisher repo public.trust.json" 'canopa signed-cps cps.trust.json'
    if [[ -n $usr_base_fixture ]]; then
        printf '%s\n' 'canopa-usr-base signed-seed seed.trust.json'
    else
        printf '%s\n' 'canopa-runtime-seed signed-seed seed.trust.json'
    fi
    printf '%s\n' 'canopa-bootstrap signed-busybox busybox.trust.json' \
        'extras extras extras.trust.json'
    if [[ -n $kernel_fixture ]]; then
        printf '%s\n' 'canopa-kernel signed-kernel kernel.trust.json'
    fi
    if [[ -n $systemd_fixture ]]; then
        printf '%s\n' 'canopa-systemd signed-systemd systemd.trust.json'
        if [[ -n $util_linux_fixture ]]; then
            printf '%s\n' 'canopa-util-linux signed-util-linux util-linux.trust.json'
        else
            printf '%s\n' 'canopa-systemd-seed signed-systemd-seed systemd-seed.trust.json'
        fi
    fi
}

profile_repositories() {
    printf '%s\n' 'vm-profile profile-repo profile.trust.json'
    profile_companions
}

publish_vm_mirror() {
    local name=$1 repository=$2 key=$3 version=$4
    shift 4
    local mirror="mirrors/$name" export="mirrors/$name/export-$version"
    mkdir -p -- "$mirror"
    "$cps" repo export --repo "$repository" --trust-root "$key" \
        --output "$export" "$@" >/dev/null
    (umask 077; "$tuf_fixture" "$export" "$mirror" "$version" > "$mirror/url")
    rm -r -- "$export"
}

refresh_vm_profile() {
    local name repository key
    {
        printf 'schema-version 1 kind="publish-policy"\npublisher "vm-profile" {\n    builder-key "unused.der"\n'
        printf '    profile "boot" architecture="%s" {\n        package "test/boot-stack"\n' "$(uname -m)"
        if [[ ${1:-} == base ]]; then printf '        package "test/previous-default"\n'; fi
        printf '        facet "devel" #false\n'
        while read -r name repository key; do
            printf '        publisher "%s" catalog="%s" {\n' "$name" "$(cat "$repository/current")"
            printf '            repository "../%s"\n            trust-root "../%s"\n' "$repository" "$key"
            if [[ $name == canopa ]] && $signed_cps_builds; then
                printf '            build-trust { builder-key "../cps-builder-public.der"; }\n'
            fi
            printf '        }\n'
        done < <(profile_companions)
        printf '    }\n}\n'
    } > profile-repo/publish-policy.kdl
    printf 'schema-version 1 kind="publish-release"\nrelease "vm-profile" base="%s" refresh-profiles=#true\n' \
        "$(cat profile-repo/current)" > profile-release.kdl
    "$cps" publish release --repo profile-repo --key profile-private.der profile-release.kdl
}

if [[ $mode == rollback ]]; then
    printf 'legacy\n' > proto/etc/cps-vm-legacy-marker
    cat > legacy-marker.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "test/legacy-marker" publisher="$fixture_publisher" version="1.0.0-0" {
    file "etc/cps-vm-legacy-marker" mode="0644" uid=$(id -u) gid=$(id -g)
}
MANIFEST
    "$cps" publish manifests --repo repo --proto proto --key private.der legacy-marker.kdl
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
    cp "$project/packaging/systemd/"{cps-boot-health.service,cps-uefi-bless.service,cps-finalize-trial.path,cps-finalize-trial.service} proto/usr/lib/systemd/system/
    install -Dm644 "$project/LICENSE" proto/usr/share/licenses/cps/LICENSE
    sed -e '/^    require /d' -e "s/uid=0 gid=0/uid=$(id -u) gid=$(id -g)/g" \
        "$project/packaging/cps.kdl" > cps.kdl
    "$cps" publish manifests --repo repo --proto proto --key private.der cps.kdl
    packages+=(system/cps)
fi
mkdir -p runtime/etc
if [[ $mode == facet-change ]]; then touch runtime/etc/cps-vm-assert-facet; fi
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
        if $protected_profiles; then
            touch runtime/etc/cps-vm-assert-profile-protection
        fi
        if $package_intent; then touch runtime/etc/cps-vm-assert-package-intent; fi
        if $package_freezes; then
            printf '%s\n' "$sqlite_version" > runtime/etc/cps-vm-frozen-sqlite
        fi
        if [[ $staged_transactions == 1 ]]; then touch runtime/etc/cps-vm-staged-transactions; fi
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
        if $profiled; then touch runtime/etc/cps-vm-assert-profile-reconcile; fi
    fi
else
    mkdir runtime/dev
    # Every environment mounts the same /var subvolume, as an installed system does.
    printf 'PARTUUID=%s /var btrfs subvol=/@var 0 0\n' "$root_uuid" > runtime/etc/fstab
    # The guest BusyBox has no mount applet.
    mkdir -p runtime/usr/libexec
    env -u LD_LIBRARY_PATH -u LD_PRELOAD cc -O2 -Wall -Wextra -Werror -static -x c \
        -o runtime/usr/libexec/cps-vm-mount-var - <<'C'
#include <stdio.h>
#include <sys/mount.h>

int main(void)
{
    if (mount("/dev/vda2", "/var", "btrfs", 0, "subvol=/@var") != 0) {
        perror("mounting /var");
        return 1;
    }
    return 0;
}
C
    cat > runtime/init <<'GUEST'
#!/bin/busybox sh
set -e
[ /etc/cps-vm-state -ef /etc/cps-vm-boot-state ]
state=$(/bin/busybox cat /etc/cps-vm-boot-state)
printf 'CPS_VM_BOOT: %s\n' "$state"
/usr/libexec/cps-vm-mount-var
printf '%s\n' "$state" >> /var/cps-vm-boots
/bin/busybox sync
printf 'CPS_VM_VAR: %s\n' "$(/bin/busybox paste -s -d ' ' /var/cps-vm-boots)"
/bin/busybox umount /var
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
"$cps" publish manifests --repo repo --proto runtime --key private.der runtime.kdl
packages+=(test/boot-runtime)
if $profiled; then
    publish_extra_marker
    cp -a -- "$cps_fixture/repo" signed-cps
    cp -a -- "$busybox_repo" signed-busybox
    cp -- "$cps_fixture/release.trust.json" cps.trust.json
    if $signed_cps_builds; then
        cp -- "$cps_fixture/builder-public.der" cps-builder-public.der
    fi
    if [[ -n $usr_base_fixture ]]; then
        cp -a -- "$usr_base_fixture/repo" signed-seed
        cp -- "$usr_base_fixture/release.trust.json" seed.trust.json
    else
        cp -a -- "$cps_fixture/runtime-seed" signed-seed
        cp -- "$cps_fixture/runtime-seed.trust.json" seed.trust.json
    fi
    cp -- "$busybox_key" busybox.trust.json
    boot_kernel_dependency='    require "system/kernel"'
    if [[ -n $kernel_fixture ]]; then
        cp -a -- "$kernel_fixture/repo" signed-kernel
        cp -- "$kernel_fixture/release.trust.json" kernel.trust.json
        boot_kernel_dependency=
    fi
    boot_systemd_dependency=
    if [[ -n $systemd_fixture ]]; then
        cp -a -- "$systemd_fixture/repo" signed-systemd
        cp -- "$systemd_fixture/release.trust.json" systemd.trust.json
        if [[ -n $util_linux_fixture ]]; then
            cp -a -- "$util_linux_fixture/repo" signed-util-linux
            cp -- "$util_linux_fixture/release.trust.json" util-linux.trust.json
        else
            cp -a -- "$systemd_fixture/runtime-seed" signed-systemd-seed
            cp -- "$systemd_fixture/runtime-seed.trust.json" systemd-seed.trust.json
        fi
        boot_systemd_dependency="    require \"system/systemd-vm\" version=\"=$systemd_version\""
    fi
    openssl genpkey -algorithm ED25519 -outform DER -out profile-private.der
    "$cps" repo trust init --repo profile-repo --publisher vm-profile --key profile-private.der >/dev/null
    cp profile-repo/trust/1.json profile.trust.json
    cat > boot-stack.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "test/boot-stack" publisher="vm-profile" version="1.0.0-0" {
    require "test/boot-state"
    require "test/boot-runtime"
$boot_kernel_dependency
$boot_systemd_dependency
    require "system/cps" version="=$cps_version"
$boot_trust_dependency
$boot_git_build_dependency
    require "system/sqlite" version="=$sqlite_version"
    require "system/libseccomp" version="=$seccomp_version"
    require "system/busybox" version="=$busybox_version"
}
MANIFEST
    "$cps" publish manifests --repo profile-repo --proto proto --key profile-private.der boot-stack.kdl
    printf 'previous default\n' > proto/etc/cps-vm-previous-default
    cat > previous-default.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "test/previous-default" publisher="vm-profile" version="1.0.0-0" {
    file "etc/cps-vm-previous-default" mode="0644" uid=$(id -u) gid=$(id -g)
}
MANIFEST
    "$cps" publish manifests --repo profile-repo --proto proto --key profile-private.der previous-default.kdl
    base_test_catalog=$(cat repo/current)
    refresh_vm_profile base
    if [[ $tuf_mirrors == 1 ]]; then
        while read -r name repository key; do
            publish_vm_mirror "$name" "$repository" "$key" 1
        done < <(profile_repositories)
        cp mirrors/vm-profile/metadata/timestamp.json base-tuf-timestamp.json
    fi
    {
        printf 'schema-version 1 kind="image-config"\nfacet "devel" #false\n'
        if [[ $reprovision == 1 ]]; then
            printf 'image-id "A47593C2-E985-42D2-92B6-44C7057E1E96"\n'
            printf 'release-profile "boot" bootstrap-catalog="%s"\n' "$(cat profile-repo/current)"
        else
            printf 'release-profile "boot"\n'
        fi
        while read -r name repository key; do
            if [[ $tuf_mirrors == 1 ]]; then
                repository="mirror-cache/$name"
                mirror_url=$(cat "mirrors/$name/url")
                printf 'mirror "%s" root="mirrors/%s/root.json" metadata="%smetadata/" targets="%stargets/"\n' \
                    "$name" "$name" "$mirror_url" "$mirror_url"
            fi
            printf 'publisher "%s" {\n    repository "%s"\n    trust-root "%s"\n}\n' "$name" "$repository" "$key"
        done < <(profile_repositories)
    } > image.kdl
else
    cat > image.kdl <<CONFIG
schema-version 1 kind="image-config"
facet "devel" #false
publisher "$fixture_publisher" {
    repository "repo"
    trust-root "public.trust.json"
}
CONFIG
    if [[ $mode == facet-change ]]; then printf 'facet "doc.man" #false\n' >> image.kdl; fi
fi
if [[ $tuf_mirrors == 1 ]]; then
    "$cps" repo sync-profile --config image.kdl --index-only
    test -z "$(find mirror-cache -path '*/blobs/*' -type f -print -quit)"
fi
"$cps" image create --root base --config image.kdl
state_dir=usr/lib/sysimage/cps
if ! $profiled && [[ -n $cps_fixture ]]; then
    "$cps" publisher add --root base --repo "$cps_fixture/repo" \
        --trust-root "$cps_fixture/release.trust.json" canopa
    "$cps" publisher add --root base --repo "$cps_fixture/runtime-seed" \
        --trust-root "$cps_fixture/runtime-seed.trust.json" canopa-runtime-seed
fi
if ! $profiled && [[ -n $busybox_repo ]]; then
    "$cps" publisher add --root base --repo "$busybox_repo" --trust-root "$busybox_key" canopa-bootstrap
fi
if $profiled; then
    "$cps" install --root base --profile -n --save-plan install.json
else
    "$cps" install --root base -n --save-plan install.json "${packages[@]}"
fi
if [[ $tuf_mirrors == 1 ]]; then
    "$cps" fetch --root base install.json
    echo 'Verified initial profile payloads were fetched into an index-only TUF cache'
fi
"$cps" apply --root base install.json
freeze_runtime() {
    local root=$1
    "$cps" freeze --root "$root" -n --save-plan "$root-freeze.json" \
        --reason 'VM runtime compatibility' system/sqlite
    "$cps" apply --root "$root" "$root-freeze.json"
    "$cps" list --root "$root" --frozen | grep -F $'system/sqlite\t'"$sqlite_version"$'\t"VM runtime compatibility"'
}
if $package_freezes; then freeze_runtime base; fi
"$cps" verify --root base --pristine
if [[ $mode == facet-change ]]; then printf 'local settings\n' > base/etc/cps-vm-optional.conf; fi
if [[ $reprovision == 1 ]]; then
    "$cps" image create --root repro-base --config image.kdl
    "$cps" install --root repro-base --profile -n --save-plan repro-install.json
    cmp install.json repro-install.json
    "$cps" apply --root repro-base repro-install.json
    if $package_freezes; then freeze_runtime repro-base; cmp base-freeze.json repro-base-freeze.json; fi
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
    if [[ -n $boot_trust_dependency ]]; then
        cmp base/etc/cps/tuf-root.json "$cps_fixture/image/etc/cps/tuf-root.json"
    fi
    if [[ -n $boot_git_build_dependency ]]; then
        diff -r "$cps_fixture/image/usr/share/cps/git-build" base/usr/share/cps/git-build
    fi
    for path in "${elf_runtime_paths[@]}"; do
        cmp "base/$path" "$elf_runtime/image/$path"
    done
fi
if $profiled; then
    env -i base/lib64/ld-linux-x86-64.so.2 --inhibit-cache --library-path "$PWD/base/usr/lib" \
        base/usr/bin/cps publisher list --root base >/dev/null
fi

truncate -s 96M esp.fat
mkfs.fat -F 32 esp.fat >/dev/null
mcopy -s -i esp.fat esp/EFI ::/
dd if=esp.fat of=template.raw bs=1M seek=1 conv=notrunc,sparse status=none
rm -r esp
# Write a root as the @ subvolume of a disk. Its volume directory holds only
# the root and the environments directory while the image is written.
image_disk() {
    local root=$1 output=$2 status=0 var=()
    mkdir -- "$root.volume" "$root.volume/@environments"
    # A root whose fstab mounts the shared /var gets its subvolume beside it.
    if [[ -f $root/etc/fstab ]]; then
        mkdir -- "$root.volume/@var"
        var=(--var "$root.volume/@var")
    fi
    mv -- "$root" "$root.volume/@"
    "$cps" image disk --btrfs-volume "$root.volume" --root "$root.volume/@" \
        --environments "$root.volume/@environments" "${var[@]}" --disk template.raw \
        --root-partuuid "$root_uuid" --output "$output" --options "$options" || status=$?
    mv -- "$root.volume/@" "$root"
    rm -r -- "$root.volume"
    return "$status"
}
if [[ $reprovision == 1 ]]; then
    # btrfs-progs gives new subvolumes random UUIDs and creation times. Compare the rest.
    masked_disk() {
        dd if="$1" of="$1.btrfs" bs=1M skip=97 count="$root_mib" status=none
        btrfs inspect-internal dump-tree -t root "$1.btrfs" |
            sed -E '/^[[:space:]]*(uuid|parent_uuid|ctime|otime) /d'
        for tree in fs 256 257 extent chunk dev csum free-space; do
            btrfs inspect-internal dump-tree -t "$tree" "$1.btrfs"
        done
        mcopy -i "$1@@1048576" ::/loader/loader.conf -
        mdir -b -i "$1@@1048576" ::/loader/entries | while read -r entry; do
            mcopy -i "$1@@1048576" "$entry" -
        done | grep -v '^# cps-btrfs-uuid '
        mdir -a -s -i "$1@@1048576" ::/
        rm -- "$1.btrfs"
    }
    first=$(image_disk base reprovision-first.raw)
    test "$first" = "Wrote cps-a47593c2-e985-42d2-92b6-44c7057e1e96-$base_revision to reprovision-first.raw"
    image_disk repro-base reprovision-second.raw >/dev/null
    cmp <(masked_disk reprovision-first.raw) <(masked_disk reprovision-second.raw)
    rm -- reprovision-first.raw reprovision-second.raw
    echo 'Verified independent signed-profile installations produced matching boot disks'
fi
if [[ $mode == rollback ]]; then
    primary_pointer=repo/current
    if $profiled; then primary_pointer=profile-repo/current; fi
    if [[ $tuf_mirrors == 1 ]]; then primary_pointer=mirror-cache/vm-profile/current; fi
    cp "$primary_pointer" base-catalog
    if ! $profiled; then
        publish_extra_marker
        "$cps" publisher add --root base --repo extras --trust-root extras.trust.json extras
    fi
fi

if [[ $mode != facet-change ]]; then publish_state 2.0.0-0 trial; fi
if [[ -z $kernel_fixture && ( $mode == kernel-upgrade || $mode == rollback ) ]]; then
    publish_kernel 2.0.0-0 "$trial_release" "$trial_kernel"
fi
if [[ $mode == rollback ]]; then
    retire_kernel=
    if [[ -n $kernel_fixture ]]; then
        retire_kernel="    obsolete \"cps://$fixture_publisher/system/kernel@2.0.0-0\""
    fi
    cat > retirement.kdl <<RELEASE
schema-version 1 kind="publish-release"
release "$fixture_publisher" base="$(cat repo/current)" {
    obsolete "cps://$fixture_publisher/test/legacy-marker@2.0.0-0"
$retire_kernel
}
RELEASE
    "$cps" publish release --repo repo --key private.der retirement.kdl
fi
if $profiled; then
    if [[ $tuf_mirrors == 1 ]]; then
        publish_vm_mirror "$fixture_publisher" repo public.trust.json 2 --retain-catalog "$base_test_catalog"
        "$cps" repo sync-profile --config image.kdl --index-only --image base
        cmp repo/current "mirror-cache/$fixture_publisher/current"
        publish_vm_mirror "$fixture_publisher" repo public.trust.json 3
        mirror_url=$(cat "mirrors/$fixture_publisher/url")
        companion_cache="mirror-cache/$fixture_publisher"
        "$cps" repo sync --repo "$companion_cache" --tuf-root "mirrors/$fixture_publisher/root.json" \
            --metadata "${mirror_url}metadata/" --targets "${mirror_url}targets/" \
            --trust-root public.trust.json --index-only
        test ! -s "$companion_cache/.tuf/retention/$(cat repo/current)"
        "$cps" repo prune-cache --repo "$companion_cache" --trust-root public.trust.json
        old_kernel_hash=$(sha256sum "$kernel" | cut -d' ' -f1)
        cmp "$kernel" "$companion_cache/blobs/$old_kernel_hash"
        echo 'Verified pruning kept the visible profile kernel after the companion mirror dropped its pin'

        "$cps" repo export --repo repo --trust-root public.trust.json \
            --archive-catalog "$base_test_catalog" --output fallback-archive-targets
        archive_url=$(umask 077; "$tuf_fixture" fallback-archive-targets fallback-archive-mirror 1)
        "$cps" repo sync --repo fallback-archive --tuf-root fallback-archive-mirror/root.json \
            --metadata "${archive_url}metadata/" --targets "${archive_url}targets/" \
            --trust-root public.trust.json --index-only \
            --archive-catalog "$base_test_catalog"
        cp -a --reflink=auto base repair-probe
        repaired_kernel="repair-probe/usr/lib/modules/$base_release/vmlinuz"
        rm -- "$repaired_kernel" "$companion_cache/blobs/$old_kernel_hash"
        "$cps" repair --root repair-probe -n --save-plan fallback-repair.json system/kernel
        if "$cps" fetch --root repair-probe fallback-repair.json > archive-repair.log 2>&1; then
            echo 'The live mirror still served the withdrawn fallback kernel' >&2
            exit 1
        fi
        grep -F "checking TUF target blobs/$old_kernel_hash" archive-repair.log || { cat archive-repair.log >&2; exit 1; }
        "$cps" fetch --root repair-probe --archive-cache "$fixture_publisher=fallback-archive" \
            fallback-repair.json
        "$cps" apply --root repair-probe fallback-repair.json
        "$cps" verify --root repair-probe --pristine
        cmp "$kernel" "$repaired_kernel"
        cmp base-catalog "$primary_pointer"
        cmp repo/current "$companion_cache/current"
        echo 'Verified repair restored the withdrawn fallback kernel from a signed archive'

        "$cps" image create --root replay-base --config image.kdl
        "$cps" install --root replay-base --profile -n --save-plan replay-install.json
        rm -- "$companion_cache/catalogs/$base_test_catalog"
        if "$cps" fetch --root replay-base replay-install.json > archive-replay.log 2>&1; then
            echo 'Fetch replayed a saved plan with missing catalog metadata' >&2
            exit 1
        fi
        grep -F "reading signed catalog for $fixture_publisher" archive-replay.log \
            || { cat archive-replay.log >&2; exit 1; }
        "$cps" fetch --root replay-base --archive-cache "$fixture_publisher=fallback-archive" \
            replay-install.json
        "$cps" apply --root replay-base replay-install.json
        "$cps" verify --root replay-base --pristine
        cmp "$kernel" "replay-base/usr/lib/modules/$base_release/vmlinuz"
        cmp base-catalog "$primary_pointer"
        cmp repo/current "$companion_cache/current"
        cmp repo/current "$companion_cache/.tuf/catalog"
        echo 'Verified a saved profile plan replayed after its catalog was restored from a signed archive'
        publish_vm_mirror "$fixture_publisher" repo public.trust.json 4 --retain-catalog "$base_test_catalog"
    fi
    "$cps" upgrade --root base -n --save-plan before-profile.json > before-profile.log
    grep -Fq "\"$base_test_catalog\"" before-profile.json
    if grep -Eq '(Update|Install|Remove) .*system/kernel' before-profile.log; then
        echo 'The companion update escaped its signed profile pin' >&2
        exit 1
    fi
    echo 'Verified the signed profile kept the previous kernel after the companion advanced'
    refresh_vm_profile
    if [[ $tuf_mirrors == 1 ]]; then
        publish_vm_mirror vm-profile profile-repo profile.trust.json 2 --retain-catalog "$(cat base-catalog)"
        trial_payload=$(sha256sum proto/etc/cps-vm-state | cut -d' ' -f1)
        cached_payload="mirror-cache/$fixture_publisher/blobs/$trial_payload"
        target="mirrors/$fixture_publisher/targets/$trial_payload.blobs-$trial_payload"
        test ! -e "$cached_payload"
        prefetch=(repo sync-profile --config image.kdl --profile-payloads --image base)
        mv -- "$target" unavailable-target
        if "$cps" "${prefetch[@]}" > missing-target.log 2>&1; then
            echo 'Profile sync promoted a release with a missing payload' >&2
            exit 1
        fi
        grep -F "reading target blobs/$trial_payload" missing-target.log || { cat missing-target.log >&2; exit 1; }
        cmp base-catalog "$primary_pointer"
        cp -- unavailable-target "$target"
        printf 'x' | dd of="$target" bs=1 conv=notrunc status=none
        if "$cps" "${prefetch[@]}" > corrupt-target.log 2>&1; then
            echo 'Profile sync promoted a release with a corrupted payload' >&2
            exit 1
        fi
        grep -F 'Hash mismatch' corrupt-target.log || { cat corrupt-target.log >&2; exit 1; }
        cmp base-catalog "$primary_pointer"
        test ! -e "$cached_payload"
        mv -- unavailable-target "$target"
        "$cps" "${prefetch[@]}"
        cmp profile-repo/current "$primary_pointer"
        cmp proto/etc/cps-vm-state "$cached_payload"
        echo 'Verified profile promotion waited for the selected payloads'
        timestamp=mirrors/vm-profile/metadata/timestamp.json
        mv -- "$timestamp" trial-tuf-timestamp.json
        cp -- base-tuf-timestamp.json "$timestamp"
        if "$cps" repo sync-profile --config image.kdl --index-only --image base > tuf-replay.log 2>&1; then
            echo 'TUF accepted the previous primary timestamp' >&2
            exit 1
        fi
        grep -F 'Found version 1 of timestamp metadata when we had previously fetched version 2' \
            tuf-replay.log || { cat tuf-replay.log >&2; exit 1; }
        cmp profile-repo/current "$primary_pointer"
        mv -- trial-tuf-timestamp.json "$timestamp"
        echo 'Verified TUF rejected replay of the previous primary mirror timestamp'
    else
        mkdir relocated-repositories
        mv profile-repo relocated-repositories/
        primary_pointer=relocated-repositories/profile-repo/current
        replacements=(--repo vm-profile=relocated-repositories/profile-repo)
        while read -r name repository _; do
            mv -- "$repository" relocated-repositories/
            replacements+=(--repo "$name=relocated-repositories/$repository")
        done < <(profile_companions)
        "$cps" publisher set-repository --root base "${replacements[@]}"
        if "$cps" apply --root base before-profile.json > relocation.log 2>&1; then
            echo 'Relocation accepted a plan from before the repository change' >&2
            exit 1
        fi
        grep -Fq 'publisher configuration changed' relocation.log
        echo 'Verified atomic relocation of the signed profile repositories'
    fi
fi
request=(upgrade)
request_args=()
if $package_freezes; then
    cp -a --reflink=auto base freeze-probe
    "$cps" freeze --root freeze-probe -n --save-plan freeze-state.json \
        --reason 'VM boot-state compatibility' test/boot-state
    "$cps" apply --root freeze-probe freeze-state.json
    if "$cps" upgrade --root freeze-probe -n --save-plan blocked.json > freeze-rejected.log 2>&1; then
        echo 'The signed profile update ignored a package freeze' >&2
        exit 1
    fi
    grep -F 'VM boot-state compatibility' freeze-rejected.log
    test ! -e blocked.json
    "$cps" unfreeze --root freeze-probe -n --save-plan unfreeze-state.json test/boot-state
    "$cps" apply --root freeze-probe unfreeze-state.json
    "$cps" upgrade --root freeze-probe -n --save-plan unfrozen.json >/dev/null
    echo 'Verified the signed profile update required explicit unfreezing'
fi
if [[ $mode == facet-change ]]; then request=(facet set); request_args=(doc.man=default optional=false); fi
"$cps" "${request[@]}" --root base -n --save-plan update.json "${request_args[@]}" | tee update.log
# The rescue system stages the trial as a Btrfs snapshot on the disk. A plain
# copy of the base with the same plan applied previews it for host checks.
if [[ $tuf_mirrors == 1 ]]; then
    mv -- "$cached_payload" saved-payload
    cp -a --reflink=auto base unfetched
    if "$cps" apply --root unfetched update.json > unfetched.log 2>&1; then
        echo 'Staging accepted an unfetched upgrade' >&2
        exit 1
    fi
    grep -F "opening repository payload $trial_payload" unfetched.log || { cat unfetched.log >&2; exit 1; }
    rm -rf -- unfetched
    mv -- saved-payload "$cached_payload"
    mv -- mirrors offline-mirrors
fi
staged_root=staged-preview
cp -a --reflink=auto base "$staged_root"
"$cps" apply --root "$staged_root" update.json
test "$(cat base/etc/cps-vm-state)" = previous
if [[ $mode == facet-change ]]; then
    test ! -e base/usr/share/man/man1/cps-vm.1
    test "$(cat "$staged_root/usr/share/man/man1/cps-vm.1")" = trial
    cmp base/etc/cps-vm-state "$staged_root/etc/cps-vm-state"
    cmp <("$cps" list --root base) <("$cps" list --root "$staged_root")
    grep -Fq 'Add usr/share/man/man1/cps-vm.1' update.log
    grep -Fq 'Preserve local config etc/cps-vm-optional.conf' update.log
    cmp base/etc/cps-vm-optional.conf "$staged_root/etc/cps-vm-optional.conf"
    "$cps" verify --root base
    "$cps" verify --root "$staged_root"
else
    test "$(cat "$staged_root/etc/cps-vm-state")" = trial
    grep -Fq 'Link etc/cps-vm-boot-state to etc/cps-vm-state' update.log
    "$cps" verify --root base --pristine
    "$cps" verify --root "$staged_root" --pristine
fi
check_runtime_packages "$staged_root"
if $profiled; then
    grep -Fq 'Remove cps://vm-profile/test/previous-default 1.0.0-0' update.log
    test "$(cat base/etc/cps-vm-previous-default)" = 'previous default'
    test ! -e "$staged_root/etc/cps-vm-previous-default"
    "$cps" info --root "$staged_root" --available test/previous-default > /dev/null
    trial_plan=$("$cps" history --root "$staged_root" --limit 2 | awk -F '\t' -v revision="$((base_revision + 1))" '$1 == revision && $3 == "upgrade (profile)" {print $2}')
    "$cps" history --root "$staged_root" "$trial_plan" > trial-history.json
    grep -Fq "\"kernel_release\": \"$trial_release\"" trial-history.json
    grep -Fq "\"base_revision\": $base_revision" trial-history.json
    "$cps" history --root base --limit 2 | grep -Fq $'\tinstall --profile'
    echo 'Verified committed history in both boot environments'
fi

if [[ $mode == rollback ]]; then
    cp "$primary_pointer" trial-catalog
    cp base-catalog "$primary_pointer"
    "$cps" upgrade --root base -n --save-plan base-catalog-plan.json >/dev/null
    if "$cps" upgrade --root "$staged_root" -n --save-plan replay.json > replay.log 2>&1; then
        echo 'The staged environment accepted a replayed primary catalog' >&2
        exit 1
    fi
    grep -F 'publisher catalog rollback or fork' replay.log
    mv trial-catalog "$primary_pointer"
fi

prepared_kernel=
if [[ $mode == kernel-upgrade || $mode == rollback ]]; then
    grep -Fq "Prepare Linux $trial_release with kernel-install" update.log
    cmp "$kernel" "base/usr/lib/modules/$base_release/vmlinuz"
    test ! -e "$staged_root/usr/lib/modules/$base_release/vmlinuz"
    cmp "$trial_kernel" "$staged_root/usr/lib/modules/$trial_release/vmlinuz"
    prepared=
    for file in "$staged_root/$state_dir"/kernel-*/linux; do
        if cmp -s "$file" "$trial_kernel"; then prepared=$file; break; fi
    done
    test -n "$prepared"
    prepared_kernel=${prepared%/*}
    prepared_kernel=${prepared_kernel##*/}
    if [[ -n $kernel_fixture ]]; then
        grep -Fq "Install ${kernel_package%@*} ${kernel_package##*@}" update.log
        cmp "$kernel_fixture/image/usr/lib/modules/$trial_release/config" "$staged_root/usr/lib/modules/$trial_release/config"
        test ! -e "${prepared%/*}/initrd"
    else
        cmp "$staged_root/usr/lib/modules/$trial_release/initrd" "${prepared%/*}/initrd"
    fi
fi

result=$(image_disk base bootable-base.raw)
base_id=${result#Wrote }
base_id=${base_id%% to *}
rescue_kernel=$kernel
prepare_rescue repo extras profile-repo signed-cps signed-busybox signed-seed signed-kernel \
    signed-systemd signed-util-linux signed-systemd-seed relocated-repositories mirror-cache \
    fallback-archive update.json ./*.trust.json
cp --sparse=always bootable-base.raw disk.raw
{
    printf 'root_uuid=%q\noptions=%q\nbase_id=%q\nstate_dir=%q\nplan=%q\nmode=%q\n' \
        "$root_uuid" "$options" "$base_id" "$state_dir" "$PWD/update.json" "$mode"
    printf 'staged_transactions=%q\nprepared_kernel=%q\n' "$staged_transactions" "$prepared_kernel"
    cat <<'GUEST'
trial=/volume/@environments/trial/root
activate() {
    cps boot activate --root "$trial" --btrfs-volume /volume --esp /efi \
        --root-partuuid "$root_uuid" --options "$options" "$@"
}
reject() {
    expected=$1
    shift
    if "$@" > /run/rejected 2>&1; then echo "CPS_VM_RESCUE: unexpected success"; exit 1; fi
    $bb cat /run/rejected
    $bb grep -Fq "$expected" /run/rejected
}
$bb chown 0:0 /volume/@environments
$bb chmod 0755 /volume/@environments
cps boot list --esp /efi
if [ "$mode" = rollback ]; then
    # Pin the base, so the trial is selected with `preferred`.
    cps boot rollback --esp /efi --btrfs-volume /volume "$base_id"
fi
cps boot capture --source /volume/@ --output /volume/@environments/base
cps boot stage --source /volume/@environments/base/root \
    --output /volume/@environments/trial "$plan"
if [ "$staged_transactions" = 1 ]; then
    cps freeze --root "$trial" -n --save-plan /run/review.json --reason 'VM staged review' test/boot-state
    cps apply --root "$trial" /run/review.json
    echo 'CPS_VM_STAGED_TRANSACTIONS: applied a reviewed freeze to the private root'
fi
if [ -n "$prepared_kernel" ]; then
    prepared="$trial/$state_dir/$prepared_kernel/linux"
    $bb cp "$prepared" /run/prepared
    printf 'corrupt prepared kernel' > "$prepared"
    reject 'kernel artifact digest mismatch' activate
    $bb cat /run/prepared > "$prepared"
fi
fallback="/efi/EFI/cps/$base_id/vmlinuz.efi"
$bb mv "$fallback" /run/fallback-kernel
reject 'known-good fallback kernel' activate
$bb mv /run/fallback-kernel "$fallback"
result=$(activate --tries 2)
echo "$result"
id=${result#Activated }
[ -f "/efi/loader/entries/$id+2.conf" ]
[ "$(activate --tries 9)" = "$result" ]
[ ! -e "/efi/loader/entries/$id+9.conf" ]
if [ "$mode" = rollback ]; then
    $bb grep -Fx "preferred $id+2.conf" /efi/loader/loader.conf || $bb grep -Fx "preferred $id.conf" /efi/loader/loader.conf
    reject 'is not known-good' cps boot rollback --esp /efi --btrfs-volume /volume "$id"
fi
# Simulate an offline change after checked activation.
case "$mode" in
    health-fail) $bb chmod 0600 "$trial/etc/cps-vm-state" ;;
    catalog-health-fail)
        changed=$(sqlite3 -batch -bail "$trial/$state_dir/state.sqlite" \
            "UPDATE catalog_archives SET catalog=x'00'; SELECT changes();")
        [ "$changed" -gt 0 ]
        ;;
esac
cps boot list --esp /efi
echo "CPS_VM_TRIAL: $id"
GUEST
} | run_rescue stage disk.raw
cat rescue-stage.log
trial_id=$(sed -n 's/^CPS_VM_TRIAL: //p' rescue-stage.log)
test -n "$trial_id"
if [[ $staged_transactions == 1 ]]; then
    test "$trial_id" = "${trial_id%-*}-$((base_revision + 2))"
    grep -Fx 'CPS_VM_STAGED_TRANSACTIONS: applied a reviewed freeze to the private root' rescue-stage.log
    echo 'Verified an additional reviewed transaction kept the original boot parent'
fi
if [[ $tuf_mirrors == 1 ]]; then
    echo 'Verified the rescue system staged the prefetched upgrade with the mirrors offline'
fi
cmp -n 1048576 bootable-base.raw disk.raw
mkdir -p esp/EFI
mcopy -s -i disk.raw@@1048576 ::/loader esp/
mcopy -s -i disk.raw@@1048576 ::/EFI/cps esp/EFI/
test -f "esp/loader/entries/$base_id.conf"
test -f "esp/loader/entries/$trial_id+2.conf"
mdir -i disk.raw@@1048576 ::/EFI/BOOT/BOOTX64.EFI >/dev/null

if [[ $mode == kernel-upgrade || $mode == rollback ]]; then
    cmp "$kernel" "esp/EFI/cps/$base_id/vmlinuz.efi"
    cmp "$trial_kernel" "esp/EFI/cps/$trial_id/vmlinuz.efi"
    cmp "base/usr/lib/modules/$base_release/initrd" "esp/EFI/cps/$base_id/initramfs.img"
    if [[ -n $kernel_fixture ]]; then
        test ! -e "esp/EFI/cps/$trial_id/initramfs.img"
    else
        cmp "$staged_root/usr/lib/modules/$trial_release/initrd" "esp/EFI/cps/$trial_id/initramfs.img"
    fi
fi

if [[ $mode == rollback ]]; then
    grep -Fq 'Install cps://extras/test/extra-marker 1.0.0-0' update.log
    grep -Fq "Remove cps://$fixture_publisher/test/legacy-marker 1.0.0-0" update.log
    test ! -e base/etc/cps-vm-extra-marker
    test "$(cat "$staged_root/etc/cps-vm-extra-marker")" = extras
    test "$(cat base/etc/cps-vm-legacy-marker)" = legacy
    test ! -e "$staged_root/etc/cps-vm-legacy-marker"
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
    bless|rollback|capabilities|facet-change) expected=$'CPS_VM_BOOT: trial blessed\nCPS_VM_BOOT: trial default' ;;
    health-fail|catalog-health-fail) expected=$'CPS_VM_BOOT: trial health-failed\nCPS_VM_BOOT: trial health-failed\nCPS_VM_BOOT: previous' ;;
    fallback|kernel-upgrade) expected=$'CPS_VM_BOOT: trial\nCPS_VM_BOOT: trial\nCPS_VM_BOOT: previous' ;;
esac
boots=$(tr -d '\r' < serial.log | grep '^CPS_VM_BOOT: ' || :)
if [[ $boots != "$expected" ]]; then
    printf 'Unexpected boot order:\n%s\n' "$boots" >&2
    tail -80 serial.log >&2
    exit 1
fi
if [[ $mode == fallback || $mode == kernel-upgrade ]]; then
    # The fallback still sees what the failed trial's boots wrote to /var.
    shared=$(tr -d '\r' < serial.log | grep '^CPS_VM_VAR: ' | tail -1)
    if [[ $shared != 'CPS_VM_VAR: trial trial previous' ]]; then
        printf 'Expected the trial and its fallback to share /var, got: %s\n' "$shared" >&2
        exit 1
    fi
    echo 'Verified the trial and its fallback share one /var subvolume'
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
if [[ $mode == bless || $mode == rollback || $mode == capabilities || $mode == facet-change ]]; then
    mcopy -i disk.raw@@1048576 "::/loader/entries/$trial_id.conf" blessed.conf
    cmp "esp/loader/entries/$trial_id+2.conf" blessed.conf
    if [[ $mode == facet-change ]]; then
        facets=$(tr -d '\r' < serial.log | grep '^CPS_VM_FACET: ')
        test "$facets" = $'CPS_VM_FACET: trial docs present\nCPS_VM_FACET: trial docs present'
        printf '%s\n' "$facets"
        cp --sparse=always disk.raw rollback.raw
        run_rescue facet-rollback rollback.raw <<< 'cps boot rollback --esp /efi --btrfs-volume /volume'
        run_vm rollback.log rollback.raw
        tr -d '\r' < rollback.log | grep -Fx 'CPS_VM_FACET: previous docs absent'
        tr -d '\r' < rollback.log | grep -Fx 'CPS_VM_BOOT: previous'
    fi
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
        if $package_freezes; then
            freezes=$(tr -d '\r' < serial.log | grep '^CPS_VM_FREEZE: ')
            test "$freezes" = $'CPS_VM_FREEZE: trial verified\nCPS_VM_FREEZE: trial verified'
            printf '%s\n' "$freezes"
        fi
        if [[ $staged_transactions == 1 ]]; then
            reviews=$(tr -d '\r' < serial.log | grep '^CPS_VM_STAGED_TRANSACTIONS: ')
            test "$reviews" = $'CPS_VM_STAGED_TRANSACTIONS: trial reviewed\nCPS_VM_STAGED_TRANSACTIONS: trial reviewed'
            printf '%s\n' "$reviews"
        fi
        if $protected_profiles; then
            protections=$(tr -d '\r' < serial.log | grep '^CPS_VM_PROFILE_PROTECTION: ')
            test "$protections" = $'CPS_VM_PROFILE_PROTECTION: trial verified\nCPS_VM_PROFILE_PROTECTION: trial verified'
            printf '%s\n' "$protections"
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
            modules=$(awk '{printf " %s", $2}' "$staged_root/vm-modules/$trial_release/load")
            loaded=$(tr -d '\r' < serial.log | grep '^CPS_VM_INSMOD: ')
            test "$loaded" = "$(printf 'CPS_VM_INSMOD: trial%s\nCPS_VM_INSMOD: trial%s' "$modules" "$modules")"
        fi
        publishers=$(tr -d '\r' < serial.log | grep '^CPS_VM_PUBLISHER: ' || :)
        test "$publishers" = $'CPS_VM_PUBLISHER: trial extras\nCPS_VM_PUBLISHER: trial extras'
        obsoletions=$(tr -d '\r' < serial.log | grep '^CPS_VM_OBSOLETION: ' || :)
        test "$obsoletions" = $'CPS_VM_OBSOLETION: trial removed\nCPS_VM_OBSOLETION: trial removed'
        if $profiled; then
            reconciled=$(tr -d '\r' < serial.log | grep '^CPS_VM_PROFILE_RECONCILE: ')
            test "$reconciled" = $'CPS_VM_PROFILE_RECONCILE: trial removed\nCPS_VM_PROFILE_RECONCILE: trial removed'
            printf '%s\n' "$reconciled"
        fi
        if $package_intent; then
            intent_checks=$(tr -d '\r' < serial.log | grep '^CPS_VM_PACKAGE_INTENT: ')
            test "$intent_checks" = $'CPS_VM_PACKAGE_INTENT: trial verified\nCPS_VM_PACKAGE_INTENT: trial verified'
            printf '%s\n' "$intent_checks"
        fi
        assert_default() {
            awk -F '\t' -v id="$1" -v part="$2" -v release="$3" \
                '$1 == "*" && $2 == "known-good" && $3 == id && $4 == part && index($5, release) {found=1} END {exit !found}' "$4"
        }
        cp --sparse=always disk.raw corrupted-fallback.raw
        cp -- "$kernel" corrupted-kernel
        printf '!' | dd of=corrupted-kernel bs=1 count=1 conv=notrunc status=none
        if cmp -s "$kernel" corrupted-kernel; then exit 1; fi
        mcopy -o -i corrupted-fallback.raw@@1048576 corrupted-kernel "::/EFI/cps/$base_id/vmlinuz.efi"
        run_rescue rejected-rollback corrupted-fallback.raw <<'GUEST'
before=$($bb sha256sum /efi/loader/loader.conf)
if cps boot rollback --esp /efi --btrfs-volume /volume > /run/rejected 2>&1; then exit 1; fi
$bb cat /run/rejected
$bb grep -Fq 'boot artifact digest mismatch' /run/rejected
[ "$($bb sha256sum /efi/loader/loader.conf)" = "$before" ]
GUEST
        rm -- corrupted-fallback.raw corrupted-kernel
        cp --sparse=always disk.raw rollback.raw
        run_rescue rollback rollback.raw <<'GUEST'
cps boot list --esp /efi | $bb sed 's/^/BEFORE /'
cps boot rollback --esp /efi --btrfs-volume /volume
cps boot list --esp /efi | $bb sed 's/^/AFTER /'
GUEST
        sed -n 's/^BEFORE //p' rescue-rollback.log | tee before-rollback.log
        sed -n 's/^AFTER //p' rescue-rollback.log | tee after-rollback.log
        grep -Fx "Selected $base_id for the next boot" rescue-rollback.log
        assert_default "$trial_id" "$root_uuid" "$trial_release" before-rollback.log
        assert_default "$base_id" "$root_uuid" "$base_release" after-rollback.log
        grep -Fq "$trial_id" after-rollback.log
        mcopy -i rollback.raw@@1048576 "::/loader/entries/$trial_id.conf" rollback-trial.conf
        mcopy -i rollback.raw@@1048576 "::/loader/entries/$base_id.conf" rollback-base.conf
        cmp blessed.conf rollback-trial.conf
        cmp previous.conf rollback-base.conf
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
        if $profiled; then
            tr -d '\r' < rollback.log | grep -Fx 'CPS_VM_PROFILE_RECONCILE: previous retained'
        fi
        if [[ -n $cps_fixture ]]; then
            tr -d '\r' < rollback.log | grep -Fx 'CPS_VM_CPS_RUNTIME: previous verified'
        fi
        if $signed_cps_builds; then
            tr -d '\r' < rollback.log | grep -Fx 'CPS_VM_CPS_BUILDS: previous verified'
        fi
        if $protected_profiles; then
            tr -d '\r' < rollback.log | grep -Fx 'CPS_VM_PROFILE_PROTECTION: previous verified'
        fi
        if $package_intent; then
            tr -d '\r' < rollback.log | grep -Fx 'CPS_VM_PACKAGE_INTENT: previous verified'
        fi
        if $package_freezes; then
            tr -d '\r' < rollback.log | grep -Fx 'CPS_VM_FREEZE: previous verified'
        fi
        if [[ $staged_transactions == 1 ]]; then
            tr -d '\r' < rollback.log | grep -Fx 'CPS_VM_STAGED_TRANSACTIONS: previous unchanged'
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
    fi
    exit 0
fi

mcopy -i disk.raw@@1048576 "::/loader/entries/$trial_id+0-2.conf" exhausted.conf
if $health_failure; then
    case $mode in
        health-fail) expected='etc/cps-vm-state: content, type, or metadata differs' ;;
        catalog-health-fail) expected='verifying the archived catalog for' ;;
    esac
    grep -q "$expected" serial.log || { tr -d '\r' < serial.log | grep -i 'cps\|verif' >&2; exit 1; }
    printf '%s\nVerified the health gate blocked blessing and systemd-boot fell back\n' "$boots"
    exit 0
fi
# Another activation of the trial cannot replenish its exhausted counter.
cp --sparse=always disk.raw exhausted.raw
{
    printf 'root_uuid=%q\noptions=%q\n' "$root_uuid" "$options"
    cat <<'GUEST'
if cps boot activate --root /volume/@environments/trial/root --btrfs-volume /volume \
    --esp /efi --root-partuuid "$root_uuid" --options "$options" > /run/rejected 2>&1; then
    exit 1
fi
$bb cat /run/rejected
$bb grep -Fq 'boot attempts are exhausted' /run/rejected
GUEST
} | run_rescue exhausted exhausted.raw
printf '%s\nVerified systemd-boot counter %s+0-2.conf\n' "$boots" "$trial_id"
