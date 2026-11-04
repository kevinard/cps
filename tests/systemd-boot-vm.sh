#!/usr/bin/env bash
# Run with a writable temporary directory and local, non-Secure-Boot OVMF firmware.
set -euo pipefail
umask 022

mode=${1:-fallback}
case $mode in
    fallback|bless|health-fail|kernel-upgrade|rollback|capabilities) ;;
    *) echo "Usage: $0 [fallback|bless|health-fail|kernel-upgrade|rollback|capabilities]" >&2; exit 2 ;;
esac

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
source "$project/tests/systemd-boot-guest.sh"
cps=${CPS_BIN:-$project/target/debug/cps}
cps_fixture=${CPS_VM_CPS_FIXTURE:-}
profiled=false
qemu=${CPS_VM_QEMU:-qemu-system-x86_64}
efi=${CPS_VM_SYSTEMD_BOOT:-/usr/lib/systemd/boot/efi/systemd-bootx64.efi}
kernel=${CPS_VM_KERNEL:-/boot/vmlinuz-linux}
busybox=${CPS_VM_BUSYBOX:-/usr/lib/initcpio/busybox}
busybox_repo=${CPS_VM_BUSYBOX_REPO:-}
busybox_key=${CPS_VM_BUSYBOX_TRUST_KEY:-}
busybox_package=cps://canopa-bootstrap/system/busybox@1.37.0-0
code=${CPS_VM_OVMF_CODE:?Set CPS_VM_OVMF_CODE to x86-64 OVMF_CODE}
vars=${CPS_VM_OVMF_VARS:?Set CPS_VM_OVMF_VARS to the matching OVMF_VARS template}
qemu_data=${CPS_VM_QEMU_DATA:-}

for command in "$qemu" mkfs.fat mcopy mdir mkfs.ext4 sfdisk openssl timeout; do
    command -v "$command" >/dev/null || { echo "Missing command: $command" >&2; exit 1; }
done
if [[ -n $busybox_repo || -n $busybox_key ]]; then
    busybox_repo=$(realpath -- "${busybox_repo:?Set CPS_VM_BUSYBOX_REPO}")
    busybox_key=$(realpath -- "${busybox_key:?Set CPS_VM_BUSYBOX_TRUST_KEY}")
    "$cps" repo attestation --repo "$busybox_repo" --publisher canopa-bootstrap \
        --trust-key "$busybox_key" "$busybox_package" >/dev/null
else
    test -r "$busybox" || { echo "Missing input: $busybox" >&2; exit 1; }
fi
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
    for package in system/cps@0.1.0-1 system/sqlite@3.54.1-0 system/libseccomp@2.6.1-0; do
        "$cps" repo attestation --repo "$cps_fixture/repo" --publisher canopa \
            --trust-key "$cps_fixture/release-public.der" "cps://canopa/$package" >/dev/null
    done
    elf_runtime_paths=(lib64/ld-linux-x86-64.so.2 usr/lib/{libc.so.6,libm.so.6,libgcc_s.so.1})
    cps_library_paths=(usr/lib/{libsqlite3.so.0,libseccomp.so.2})
fi
if [[ ${CPS_VM_RELEASE_PROFILE:-0} == 1 ]]; then
    [[ $mode == rollback && -n $cps_fixture && -n $busybox_repo ]] || {
        echo 'CPS_VM_RELEASE_PROFILE requires rollback and signed CPS and BusyBox fixtures' >&2
        exit 2
    }
    profiled=true
fi
if [[ $mode == kernel-upgrade || $mode == rollback ]]; then
    base_release=${CPS_VM_KERNEL_RELEASE:-$(uname -r)}
    trial_kernel=${CPS_VM_TRIAL_KERNEL:?Set CPS_VM_TRIAL_KERNEL to the second kernel}
    trial_release=${CPS_VM_TRIAL_KERNEL_RELEASE:?Set CPS_VM_TRIAL_KERNEL_RELEASE to its uname -r value}
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
options='ro console=ttyS0 loglevel=3 init=/init'
root_mib=64
if $systemd_guest; then
    options='rw console=ttyS0 loglevel=3 init=/init systemd.log_target=console'
    root_mib=320
elif [[ $mode == kernel-upgrade ]]; then
    root_mib=128
fi
mkdir -p esp/EFI/BOOT proto/etc
cp "$efi" esp/EFI/BOOT/BOOTX64.EFI
openssl genpkey -algorithm ED25519 -outform DER -out private.der
openssl pkey -inform DER -in private.der -pubout -outform DER -out public.der

publish_state() {
    local version=$1 state=$2
    local dependency=
    if [[ $mode == rollback ]]; then
        case $state in
            previous) dependency='    require "test/legacy-marker"' ;;
            trial) dependency='    require "test/extra-marker"' ;;
        esac
    fi
    printf '%s\n' "$state" > proto/etc/cps-vm-state
    cat > manifest.kdl <<MANIFEST
schema-version 1
package "test/boot-state" publisher="$fixture_publisher" version="$version" {
    file "etc/cps-vm-state" mode="0644" uid=$(id -u) gid=$(id -g)
    hardlink "etc/cps-vm-boot-state" target="etc/cps-vm-state"
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
    printf '%s\n' "$fixture_publisher repo public.der" 'canopa signed-cps cps-public.der' \
        'canopa-runtime-seed signed-seed seed-public.der' 'canopa-bootstrap signed-busybox busybox-public.der' \
        'extras extras extras-public.der'
}

refresh_vm_profile() {
    local baseline=${1:-} baseline_test=${2:-} upgrade= companion_upgrade=
    local name repository key
    if [[ -n $baseline ]]; then upgrade=" upgrade-from=\"$baseline\""; fi
    {
        printf 'schema-version 1\npublisher "vm-profile" {\n    builder-key "unused.der"\n'
        printf '    profile "boot" architecture="%s"%s {\n        package "test/boot-stack"\n' "$(uname -m)" "$upgrade"
        while read -r name repository key; do
            companion_upgrade=
            if [[ $repository == repo && -n $baseline_test ]]; then
                companion_upgrade=" upgrade-from=\"$baseline_test\""
            fi
            printf '        publisher "%s" catalog="%s"%s {\n' "$name" "$(cat "$repository/current")" "$companion_upgrade"
            printf '            repository "../%s"\n            trust-key "../%s"\n        }\n' "$repository" "$key"
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
    packages+=(cps://canopa/system/cps@0.1.0-1)
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
mkdir -p runtime/{bin,etc}
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
    cp -L "$busybox" runtime/bin/busybox
    copy_runtime_deps runtime "$busybox"
fi
if $systemd_guest; then
    setup_systemd_guest runtime
    if [[ $mode == capabilities ]]; then
        setup_capability_guest runtime "$cps" "$project/tests/file-capabilities-guest.sh"
    fi
    if [[ -n $cps_fixture ]]; then
        # Leave runtime ownership to the packages selected from the signed repositories.
        for path in "${elf_runtime_paths[@]}"; do
            if [[ -f runtime/$path ]]; then cmp "runtime/$path" "$cps_fixture/image/$path"; fi
            rm -f -- "runtime/$path"
        done
        for path in "${cps_library_paths[@]}"; do rm -f -- "runtime/$path"; done
        touch runtime/etc/cps-vm-assert-cps-runtime
    else
        copy_runtime_deps runtime proto/usr/bin/cps
    fi
    if [[ $mode == rollback ]]; then
        prepare_fat_modules runtime "$trial_release" "${CPS_VM_TRIAL_MODULES_ROOT:-/}"
        touch runtime/etc/cps-vm-assert-extra-publisher
    fi
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
    cp -a -- "$cps_fixture/runtime-seed" signed-seed
    cp -a -- "$busybox_repo" signed-busybox
    cp -- "$cps_fixture/release-public.der" cps-public.der
    cp -- "$cps_fixture/runtime-seed-public.der" seed-public.der
    cp -- "$busybox_key" busybox-public.der
    openssl genpkey -algorithm ED25519 -outform DER -out profile-private.der
    openssl pkey -inform DER -in profile-private.der -pubout -outform DER -out profile-public.der
    cat > boot-stack.kdl <<'MANIFEST'
schema-version 1
package "test/boot-stack" publisher="vm-profile" version="1.0.0-0" {
    require "test/boot-state"
    require "test/boot-runtime"
    require "system/kernel"
    require "system/cps"
    require "system/busybox"
}
MANIFEST
    "$cps" publish --repo profile-repo --proto proto --signing-key profile-private.der boot-stack.kdl
    base_test_catalog=$(cat repo/current)
    refresh_vm_profile
    base_profile_catalog=$(cat profile-repo/current)
    {
        cat <<'CONFIG'
schema-version 1
release-profile "boot"
publisher "vm-profile" {
    repository "profile-repo"
    trust-key "profile-public.der"
}
CONFIG
        while read -r name repository key; do
            printf 'publisher "%s" {\n    repository "%s"\n    trust-key "%s"\n}\n' "$name" "$repository" "$key"
        done < <(profile_companions)
    } > image.kdl
    packages=(test/boot-stack)
else
    cat > image.kdl <<CONFIG
schema-version 1
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
"$cps" plan --root base --output install.json install "${packages[@]}"
"$cps" apply --root base install.json
"$cps" verify --root base --pristine
if [[ -n $cps_fixture ]]; then
    for path in usr/bin/cps "${cps_library_paths[@]}" "${elf_runtime_paths[@]}"; do
        cmp "base/$path" "$cps_fixture/image/$path"
    done
fi
if $profiled; then
    env -i base/lib64/ld-linux-x86-64.so.2 --inhibit-cache --library-path "$PWD/base/usr/lib" \
        base/usr/bin/cps publisher list --root base >/dev/null
fi

base_id=$("$cps" boot-env activate --known-good --root base --esp esp \
    --root-partuuid "$base_uuid" --options "$options")
base_id=${base_id#Activated }
test -f "esp/loader/entries/$base_id.conf"
if [[ $mode == rollback ]]; then
    # A new upgrade should still be testable from an explicitly selected base.
    "$cps" boot-env rollback --esp esp "$base_id"
    if ! $profiled; then
        publish_extra_marker
        "$cps" publisher add --root base --repo extras --trust-key extras-public.der extras
    fi
fi

publish_state 2.0.0-0 trial
if [[ $mode == kernel-upgrade || $mode == rollback ]]; then
    publish_kernel 2.0.0-0 "$trial_release" "$trial_kernel"
fi
if [[ $mode == rollback ]]; then
    cat > retirement.kdl <<RELEASE
schema-version 1
release "$fixture_publisher" base="$(cat repo/current)" {
    obsolete "cps://$fixture_publisher/test/legacy-marker@2.0.0-0"
}
RELEASE
    "$cps" repo publish-release --repo repo --signing-key private.der retirement.kdl
fi
if $profiled; then
    "$cps" plan --root base --output before-profile.json update > before-profile.log
    grep -Fq "\"$base_test_catalog\"" before-profile.json
    if grep -q 'Update .*system/kernel' before-profile.log; then
        echo 'The companion update escaped its signed profile pin' >&2
        exit 1
    fi
    echo 'Verified the signed profile kept the previous kernel after the companion advanced'
    refresh_vm_profile "$base_profile_catalog" "$base_test_catalog"
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
    cmp "staged/usr/lib/modules/$trial_release/initrd" "${prepared%/*}/initrd"
    printf 'corrupt prepared kernel' > "$prepared"
    if "$cps" boot-env activate --root staged --esp esp --root-partuuid "$trial_uuid" \
        --options "$options" 2> rejected.log; then
        echo "Activation accepted a corrupted prepared kernel" >&2
        exit 1
    fi
    grep -q 'kernel artifact digest mismatch' rejected.log
    cp "$trial_kernel" "$prepared"
    diff -r esp-before esp
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

"$cps" boot-env activate --root staged --esp esp --root-partuuid "$trial_uuid" \
    --options "$options" --tries 2
"$cps" boot-env activate --root staged --esp esp --root-partuuid "$trial_uuid" \
    --options "$options" --tries 9
test -f "esp/loader/entries/$trial_id+2.conf"
test ! -e "esp/loader/entries/$trial_id+9.conf"
cmp "esp-before/loader/entries/$base_id.conf" "esp/loader/entries/$base_id.conf"

if [[ $mode == kernel-upgrade || $mode == rollback ]]; then
    cmp "$kernel" "esp/EFI/cps/$base_id/vmlinuz.efi"
    cmp "$trial_kernel" "esp/EFI/cps/$trial_id/vmlinuz.efi"
    cmp "base/usr/lib/modules/$base_release/initrd" "esp/EFI/cps/$base_id/initramfs.img"
    cmp "staged/usr/lib/modules/$trial_release/initrd" "esp/EFI/cps/$trial_id/initramfs.img"
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
    # The guest still identifies this as the trial, but the manifest hash fails.
    printf 'trial\n\n' > staged/etc/cps-vm-state
fi

# Pack the two separate roots and the EFI directory into a GPT disk.
# All construction uses regular files and works without loop devices.
truncate -s 96M esp.fat
mkfs.fat -F 32 esp.fat >/dev/null
mcopy -s -i esp.fat esp/EFI esp/loader ::/
for tree in base staged; do
    truncate -s "${root_mib}M" "$tree.ext4"
    mkfs.ext4 -q -F -d "$tree" "$tree.ext4"
done
base_mib=97
trial_mib=$((base_mib + root_mib))
truncate -s "$((trial_mib + root_mib + 1))M" disk.raw
sfdisk --quiet disk.raw <<PARTITIONS
label: gpt
unit: sectors

start=2048, size=196608, type=uefi
start=$((base_mib * 2048)), size=$((root_mib * 2048)), type=linux, uuid=$base_uuid
start=$((trial_mib * 2048)), size=$((root_mib * 2048)), type=linux, uuid=$trial_uuid
PARTITIONS
dd if=esp.fat of=disk.raw bs=1M seek=1 conv=notrunc,sparse status=none
dd if=base.ext4 of=disk.raw bs=1M seek="$base_mib" conv=notrunc,sparse status=none
dd if=staged.ext4 of=disk.raw bs=1M seek="$trial_mib" conv=notrunc,sparse status=none
cp "$vars" vars.fd

args=()
if [[ -n $qemu_data ]]; then args+=(-L "$qemu_data"); fi
run_vm() {
    local log=$1
    if ! timeout "${CPS_VM_TIMEOUT:-180}" "$qemu" "${args[@]}" \
        -machine q35,accel=tcg -m 768 -smp 2 -nic none -display none -monitor none \
        -serial "file:$log" \
        -drive "if=pflash,format=raw,unit=0,readonly=on,file=$code" \
        -drive if=pflash,format=raw,unit=1,file=vars.fd \
        -drive if=virtio,format=raw,file=disk.raw; then
        test ! -f "$log" || tail -80 "$log" >&2
        return 1
    fi
}
check_busybox() {
    if [[ -n $busybox_repo ]] && $systemd_guest; then
        local log=$1
        tr -d '\r' < "$log" | awk '
            /^CPS_VM_BOOT: / { boots++ }
            /^CPS_VM_BUSYBOX: / {
                print
                if ($0 != "CPS_VM_BUSYBOX: v1.37.0") invalid = 1
                versions++
            }
            END { exit (invalid || boots == 0 || boots != versions) }
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
        publishers=$(tr -d '\r' < serial.log | grep '^CPS_VM_PUBLISHER: ' || :)
        test "$publishers" = $'CPS_VM_PUBLISHER: trial extras\nCPS_VM_PUBLISHER: trial extras'
        obsoletions=$(tr -d '\r' < serial.log | grep '^CPS_VM_OBSOLETION: ' || :)
        test "$obsoletions" = $'CPS_VM_OBSOLETION: trial removed\nCPS_VM_OBSOLETION: trial removed'
        # Bring the VM's blessed entry back into the offline ESP directory.
        rm "esp/loader/entries/$trial_id+2.conf"
        cp blessed.conf "esp/loader/entries/$trial_id.conf"
        "$cps" boot-env list --esp esp | tee before-rollback.log
        assert_default() {
            awk -F '\t' -v id="$1" -v part="$2" -v release="$3" \
                '$1 == "*" && $2 == "known-good" && $3 == id && $4 == part && index($5, release) {found=1} END {exit !found}' "$4"
        }
        assert_default "$trial_id" "$trial_uuid" "$trial_release" before-rollback.log
        result=$("$cps" boot-env rollback --esp esp)
        test "$result" = "Selected $base_id for the next boot"
        "$cps" boot-env list --esp esp | tee after-rollback.log
        assert_default "$base_id" "$base_uuid" "$base_release" after-rollback.log
        grep -Fq "$trial_id" after-rollback.log
        cmp blessed.conf "esp/loader/entries/$trial_id.conf"
        cmp previous.conf "esp/loader/entries/$base_id.conf"
        mcopy -o -i disk.raw@@1048576 esp/loader/loader.conf ::/loader/loader.conf
        run_vm rollback.log
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
        printf '%s\n' "$obsoletions" "$rollback_boots" "$rollback_kernels" "$rollback_publishers" "$rollback_obsoletions"
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
