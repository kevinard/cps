#!/usr/bin/env bash
# Run with a writable temporary directory and local, non-Secure-Boot OVMF firmware.
set -euo pipefail

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=${CPS_BIN:-$project/target/debug/cps}
qemu=${CPS_VM_QEMU:-qemu-system-x86_64}
efi=${CPS_VM_SYSTEMD_BOOT:-/usr/lib/systemd/boot/efi/systemd-bootx64.efi}
kernel=${CPS_VM_KERNEL:-/boot/vmlinuz-linux}
busybox=${CPS_VM_BUSYBOX:-/usr/lib/initcpio/busybox}
code=${CPS_VM_OVMF_CODE:?Set CPS_VM_OVMF_CODE to x86-64 OVMF_CODE}
vars=${CPS_VM_OVMF_VARS:?Set CPS_VM_OVMF_VARS to the matching OVMF_VARS template}
qemu_data=${CPS_VM_QEMU_DATA:-}

for command in "$qemu" mkfs.fat mcopy mdir mkfs.ext4 sfdisk openssl timeout; do
    command -v "$command" >/dev/null || { echo "Missing command: $command" >&2; exit 1; }
done
for file in "$cps" "$efi" "$kernel" "$busybox" "$code" "$vars"; do
    test -r "$file" || { echo "Missing input: $file" >&2; exit 1; }
done

work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd -- "$work"

base_uuid=7adff456-af50-498f-b39a-dcf87cb4a7c1
trial_uuid=b52060cf-81ab-4b8a-ad84-3aa69000ed49
options='ro console=ttyS0 loglevel=3 init=/init'
mkdir -p esp/EFI/BOOT proto/etc
cp "$efi" esp/EFI/BOOT/BOOTX64.EFI
openssl genpkey -algorithm ED25519 -outform DER -out private.der
openssl pkey -inform DER -in private.der -pubout -outform DER -out public.der

publish_state() {
    local version=$1 state=$2
    printf '%s\n' "$state" > proto/etc/cps-vm-state
    cat > manifest.toml <<MANIFEST
format = 1
publisher = "canopa"
name = "test/boot-state"
version = "$version"

[[action]]
kind = "file"
path = "etc/cps-vm-state"
mode = "0644"
uid = $(id -u)
gid = $(id -g)
MANIFEST
    "$cps" publish --repo repo --proto proto --signing-key private.der manifest.toml
}

publish_state 1.0.0-0 previous
"$cps" image-create --root base --repo repo --publisher canopa --trust-key public.der
"$cps" plan --root base --output install.json install test/boot-state
"$cps" apply --root base install.json

mkdir -p base/{boot,bin,dev}
cp "$kernel" base/boot/vmlinuz
cp -L "$busybox" base/bin/busybox
# Support either static BusyBox or a dynamically linked host build.
links=$(ldd "$busybox" 2>&1 || :)
if [[ $links == *'not found'* ]]; then
    printf '%s\n' "$links" >&2
    exit 1
fi
while read -r library; do
    cp -L --parents "$library" base/
done < <(awk '/=> \// { print $3 } /^[[:space:]]*\// { print $1 }' <<< "$links")
cat > base/init <<'GUEST'
#!/bin/busybox sh
set -e
state=$(/bin/busybox cat /etc/cps-vm-state)
printf 'CPS_VM_BOOT: %s\n' "$state"
if [ "$state" = trial ]; then
    /bin/busybox reboot -f
elif [ "$state" = previous ]; then
    /bin/busybox poweroff -f
fi
GUEST
chmod 0755 base/init

base_id=$("$cps" boot-env activate --known-good --root base --esp esp \
    --root-partuuid "$base_uuid" --options "$options")
base_id=${base_id#Activated }
test -f "esp/loader/entries/$base_id.conf"

publish_state 2.0.0-0 trial
"$cps" plan --root base --output update.json update
cp -a esp esp-before
trial=$("$cps" boot-env stage --source base --root staged update.json)
trial_id=${trial#Staged }
trial_id=${trial_id%% at *}
diff -r esp-before esp
test "$(cat base/etc/cps-vm-state)" = previous
test "$(cat staged/etc/cps-vm-state)" = trial
"$cps" verify --root base
"$cps" verify --root staged

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

# Pack the two separate roots and the EFI directory into a GPT disk.
# All construction uses regular files and works without loop devices.
truncate -s 96M esp.fat
mkfs.fat -F 32 esp.fat >/dev/null
mcopy -s -i esp.fat esp/EFI esp/loader ::/
for tree in base staged; do
    truncate -s 64M "$tree.ext4"
    mkfs.ext4 -q -F -d "$tree" "$tree.ext4"
done
truncate -s 226M disk.raw
sfdisk --quiet disk.raw <<PARTITIONS
label: gpt
unit: sectors

start=2048, size=196608, type=uefi
start=198656, size=131072, type=linux, uuid=$base_uuid
start=329728, size=131072, type=linux, uuid=$trial_uuid
PARTITIONS
dd if=esp.fat of=disk.raw bs=1M seek=1 conv=notrunc,sparse status=none
dd if=base.ext4 of=disk.raw bs=1M seek=97 conv=notrunc,sparse status=none
dd if=staged.ext4 of=disk.raw bs=1M seek=161 conv=notrunc,sparse status=none
cp "$vars" vars.fd

args=()
if [[ -n $qemu_data ]]; then args+=(-L "$qemu_data"); fi
if ! timeout "${CPS_VM_TIMEOUT:-180}" "$qemu" "${args[@]}" \
    -machine q35,accel=tcg -m 768 -smp 2 -nic none -display none -monitor none \
    -serial file:serial.log \
    -drive "if=pflash,format=raw,unit=0,readonly=on,file=$code" \
    -drive if=pflash,format=raw,unit=1,file=vars.fd \
    -drive if=virtio,format=raw,file=disk.raw; then
    test ! -f serial.log || tail -80 serial.log >&2
    exit 1
fi

expected=$'CPS_VM_BOOT: trial\nCPS_VM_BOOT: trial\nCPS_VM_BOOT: previous'
boots=$(tr -d '\r' < serial.log | grep '^CPS_VM_BOOT: ' || :)
if [[ $boots != "$expected" ]]; then
    printf 'Unexpected boot order:\n%s\n' "$boots" >&2
    tail -80 serial.log >&2
    exit 1
fi
mcopy -i disk.raw@@1048576 "::/loader/entries/$trial_id+0-2.conf" exhausted.conf
mcopy -i disk.raw@@1048576 "::/loader/entries/$base_id.conf" previous.conf
cmp "esp/loader/entries/$base_id.conf" previous.conf
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
