# Helpers for the QEMU fixture. This is sourced by systemd-boot-vm.sh.

copy_runtime_deps() {
    local root=$1 binary=$2 links library
    links=$(ldd "$binary" 2>&1 || :)
    if [[ $links == *'not found'* ]]; then
        printf '%s\n' "$links" >&2
        return 1
    fi
    while read -r library; do
        cp -L --parents "$library" "$root/"
    done < <(awk '/=> \// { print $3 } /^[[:space:]]*\// { print $1 }' <<< "$links")
}

prepare_fat_modules() {
    local root=$1 release=$2 source=$3
    local dir="$root/vm-modules/$release"
    mkdir -p "$dir"
    : > "$dir/load"
    if [[ ${CPS_VM_FAT_BUILTIN:-0} == 1 ]]; then return; fi
    local dependencies command module name
    dependencies=$(
        modprobe --show-depends --dirname "$source" --set-version "$release" vfat
        modprobe --show-depends --dirname "$source" --set-version "$release" nls_iso8859-1
    )
    while read -r command module _; do
        [[ $command == insmod ]] || continue
        name=${module##*/}
        name=${name%%.ko*}.ko
        case $module in
            *.zst) zstdcat "$module" > "$dir/$name" ;;
            *.xz) xzcat "$module" > "$dir/$name" ;;
            *.gz) zcat "$module" > "$dir/$name" ;;
            *) cp "$module" "$dir/$name" ;;
        esac
        printf '/vm-modules/%s/%s\n' "$release" "$name" >> "$dir/load"
    done <<< "$dependencies"
}

setup_systemd_guest() {
    local root=$1
    local systemd=${CPS_VM_SYSTEMD_DIR:-/usr/lib/systemd}
    local binary unit command
    mkdir -p "$root"/{etc/systemd/system,usr/bin,usr/lib/systemd/{system,system-generators},usr/local/libexec,vm-modules}
    local programs=(systemd systemd-bless-boot system-generators/systemd-bless-boot-generator)
    if [[ -x "$systemd/systemd-executor" ]]; then programs+=(systemd-executor); fi
    for binary in "${programs[@]}"; do
        cp -L "$systemd/$binary" "$root/usr/lib/systemd/$binary"
        copy_runtime_deps "$root" "$systemd/$binary"
    done
    for command in systemctl mount insmod; do
        binary=$(command -v "$command")
        cp -L "$binary" "$root/usr/bin/$command"
        copy_runtime_deps "$root" "$binary"
    done
    copy_runtime_deps "$root" "$(command -v blkid)" # systemd's ESP discovery loads libblkid
    for unit in basic.target boot-complete.target local-fs.target multi-user.target \
        paths.target shutdown.target slices.target sockets.target swap.target \
        sysinit.target systemd-bless-boot.service timers.target; do
        cp -L "$systemd/system/$unit" "$root/usr/lib/systemd/system/"
    done
    printf 'root:x:0:0:root:/root:/bin/sh\n' > "$root/etc/passwd"
    printf 'root:x:0:\n' > "$root/etc/group"
    printf 'ID=canopa\nPRETTY_NAME="Canopa CPS VM"\n' > "$root/etc/os-release"
    printf '9b6c080d423b4a6090307e0670659a21\n' > "$root/etc/machine-id"
    mkdir -p "$root/etc/systemd/system.conf.d"
    printf '[Manager]\nDefaultStandardOutput=tty\nDefaultStandardError=inherit\n' > \
        "$root/etc/systemd/system.conf.d/vm-console.conf"

    prepare_fat_modules "$root" "${CPS_VM_KERNEL_RELEASE:-$(uname -r)}" "${CPS_VM_KERNEL_MODULES_ROOT:-/}"

    cat > "$root/init" <<'GUEST'
#!/bin/busybox sh
set -e
bb=/bin/busybox
mount=/usr/bin/mount
# The inactive root contains only packaged paths. Set up runtime mountpoints
# before starting systemd.
"$bb" mkdir -p /dev /proc /sys /run /efi
"$bb" mkdir -p -m 1777 /tmp /var/tmp
[ -b /dev/vda1 ] || "$mount" -t devtmpfs devtmpfs /dev
exec </dev/console >/dev/console 2>&1
"$mount" -t proc proc /proc
"$mount" -t sysfs sysfs /sys
"$mount" -t cgroup2 cgroup2 /sys/fs/cgroup
"$mount" -t efivarfs efivarfs /sys/firmware/efi/efivars
"$mount" -t tmpfs tmpfs /run
release=$(/bin/busybox uname -r)
while read -r module; do /usr/bin/insmod "$module"; done < "/vm-modules/$release/load"
"$mount" -t vfat /dev/vda1 /efi
for entry in /efi/loader/entries/cps-*+*.conf; do
    if [ -f "$entry" ]; then : > /run/cps-vm-counted; fi
done
exec /usr/lib/systemd/systemd
GUEST
    cat > "$root/etc/systemd/system/default.target" <<'UNIT'
[Unit]
Description=CPS VM boot
DefaultDependencies=no
Requires=multi-user.target
After=multi-user.target
Wants=cps-vm-observe.service
UNIT
    cat > "$root/etc/systemd/system/cps-vm-observe.service" <<'UNIT'
[Unit]
Description=Observe the result of the boot assessment
After=multi-user.target systemd-bless-boot.service

[Service]
Type=oneshot
ExecStart=/usr/local/libexec/cps-vm-observe
StandardOutput=tty
StandardError=inherit
UNIT
    cat > "$root/usr/local/libexec/cps-vm-observe" <<'GUEST'
#!/bin/busybox sh
set -e
bb=/bin/busybox
state=$("$bb" cat /etc/cps-vm-state)
if [ -f /etc/cps-vm-assert-kernel ]; then
    printf 'CPS_VM_KERNEL: %s %s\n' "$state" "$("$bb" uname -r)"
fi
if [ -f /etc/cps-vm-assert-extra-publisher ]; then
    publisher=absent
    if [ -f /etc/cps-vm-extra-marker ]; then
        publisher=$("$bb" cat /etc/cps-vm-extra-marker)
    fi
    printf 'CPS_VM_PUBLISHER: %s %s\n' "$state" "$publisher"
fi
if [ "$state" = previous ]; then
    printf 'CPS_VM_BOOT: previous\n'
elif [ -f /run/cps-vm-counted ]; then
    if /usr/bin/systemctl is-active --quiet cps-boot-health.service &&
       /usr/bin/systemctl is-active --quiet systemd-bless-boot.service; then
        printf 'CPS_VM_BOOT: trial blessed\n'
    else
        printf 'CPS_VM_BOOT: trial health-failed\n'
    fi
    "$bb" sync
    exec "$bb" reboot -f
else
    printf 'CPS_VM_BOOT: trial default\n'
fi
"$bb" sync
exec "$bb" poweroff -f
GUEST
    chmod 0755 "$root/init" "$root/usr/local/libexec/cps-vm-observe"
}
