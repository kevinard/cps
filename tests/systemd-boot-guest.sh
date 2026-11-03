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

setup_systemd_guest() {
    local root=$1
    local systemd=${CPS_VM_SYSTEMD_DIR:-/usr/lib/systemd}
    local binary unit module command
    mkdir -p "$root"/{proc,sys,run,tmp,var/tmp,efi,etc/systemd/system,usr/lib/systemd/system-generators,usr/local/libexec,vm-modules}
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
    copy_runtime_deps "$root" "$root/usr/bin/cps"
    for unit in basic.target boot-complete.target local-fs.target multi-user.target \
        paths.target shutdown.target slices.target sockets.target swap.target \
        sysinit.target systemd-bless-boot.service timers.target; do
        cp -L "$systemd/system/$unit" "$root/usr/lib/systemd/system/"
    done
    printf 'root:x:0:0:root:/root:/bin/sh\n' > "$root/etc/passwd"
    printf 'root:x:0:\n' > "$root/etc/group"
    printf 'ID=canopa\nPRETTY_NAME="Canopa CPS VM"\n' > "$root/etc/os-release"
    openssl rand -hex 16 > "$root/etc/machine-id"
    mkdir -p "$root/etc/systemd/system.conf.d"
    printf '[Manager]\nDefaultStandardOutput=tty\nDefaultStandardError=inherit\n' > \
        "$root/etc/systemd/system.conf.d/vm-console.conf"

    : > "$root/vm-modules/load"
    if [[ ${CPS_VM_FAT_BUILTIN:-0} != 1 ]]; then
        local release=${CPS_VM_KERNEL_RELEASE:-$(uname -r)}
        local dependencies
        dependencies=$(modprobe --show-depends --set-version "$release" vfat nls_iso8859-1)
        while read -r command module _; do
            [[ $command == insmod ]] || continue
            local name=${module##*/}
            name=${name%%.ko*}.ko
            case $module in
                *.zst) zstdcat "$module" > "$root/vm-modules/$name" ;;
                *.xz) xzcat "$module" > "$root/vm-modules/$name" ;;
                *.gz) zcat "$module" > "$root/vm-modules/$name" ;;
                *) cp "$module" "$root/vm-modules/$name" ;;
            esac
            printf '/vm-modules/%s\n' "$name" >> "$root/vm-modules/load"
        done <<< "$dependencies"
    fi

    cat > "$root/init" <<'GUEST'
#!/bin/busybox sh
set -e
mount=/usr/bin/mount
[ -b /dev/vda1 ] || "$mount" -t devtmpfs devtmpfs /dev
"$mount" -t proc proc /proc
"$mount" -t sysfs sysfs /sys
"$mount" -t cgroup2 cgroup2 /sys/fs/cgroup
"$mount" -t efivarfs efivarfs /sys/firmware/efi/efivars
"$mount" -t tmpfs tmpfs /run
while read -r module; do /usr/bin/insmod "$module"; done < /vm-modules/load
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
