# Helpers for the QEMU fixture. This is sourced by systemd-boot-vm.sh.

copy_runtime_deps() {
    local root=$1 binary=$2 merged=${3:-} links library directory
    links=$(ldd "$binary" 2>&1 || :)
    if [[ $links == *'not found'* ]]; then
        printf '%s\n' "$links" >&2
        return 1
    fi
    while read -r library; do
        if [[ -n $merged ]]; then
            # Keep the SONAME while resolving the trusted host directory aliases.
            directory=$(realpath -e -- "${library%/*}")
            mkdir -p -- "$root$directory"
            cp -L -- "$library" "$root$directory/${library##*/}"
        else
            cp -L --parents "$library" "$root/"
        fi
    done < <(awk '/=> \// { print $3 } /^[[:space:]]*\// { print $1 }' <<< "$links")
}

prepare_fat_modules() {
    local root=$1 release=$2 source=$3
    local dir="$root/vm-modules/$release"
    mkdir -p "$dir"
    : > "$dir/load"
    if [[ ${CPS_VM_FAT_BUILTIN:-0} == 1 ]]; then return; fi
    local dependencies command module name kernel_name
    dependencies=$(
        modprobe --config /dev/null --ignore-install --show-depends --dirname "$source" --set-version "$release" vfat &&
        modprobe --config /dev/null --ignore-install --show-depends --dirname "$source" --set-version "$release" nls_iso8859-1
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
        kernel_name=${name%.ko}
        printf '/vm-modules/%s/%s %s\n' "$release" "$name" "${kernel_name//-/_}" >> "$dir/load"
    done <<< "$dependencies"
}

setup_capability_guest() {
    local root=$1 cps=$2 script=$3
    local data="$root/usr/lib/cps-cap-test"
    local command binary version flags
    mkdir -p "$data" cap-proto/bin
    for command in cp getcap setcap setpriv; do
        binary=$(command -v "$command")
        cp -L "$binary" "$root/usr/bin/$command"
        copy_runtime_deps "$root" "$binary"
    done
    cp "$script" "$root/usr/local/libexec/cps-vm-capabilities"
    env -u LD_LIBRARY_PATH -u LD_PRELOAD cc -O2 -Wall -Wextra -Werror -static -x c \
        -o cap-proto/bin/raw-probe - <<'C'
#include <errno.h>
#include <netinet/in.h>
#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>

int main(void)
{
    if (getuid() != 65534 || geteuid() != 65534)
        return 1;
    int fd = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    if (fd >= 0)
        return close(fd) != 0;
    if (errno == EPERM)
        return 77;
    perror("raw socket");
    return 1;
}
C
    openssl genpkey -algorithm ED25519 -outform DER -out cap-private.der
    openssl pkey -inform DER -in cap-private.der -pubout -outform DER -out "$data/public.der"
    for version in 1 2; do
        flags=
        if [[ $version == 1 ]]; then flags='capabilities="cap_net_raw=ep"'; fi
        cat > cap.kdl <<MANIFEST
schema-version 1
package "test/raw-probe" publisher="captest" version="$version.0.0-0" {
    file "bin/raw-probe" mode="0755" uid=0 gid=0 $flags
    hardlink "bin/raw-probe-alias" target="bin/raw-probe"
}
MANIFEST
        "$cps" publish --repo "$data/repo" --proto cap-proto --signing-key cap-private.der cap.kdl
    done
}

setup_systemd_guest() {
    local root=$1 signed=${2:-} merged=${3:-} util_linux=${4:-}
    local signed_insmod=${5:-false}
    local systemd=${CPS_VM_SYSTEMD_DIR:-/usr/lib/systemd}
    local binary unit command
    mkdir -p "$root"/{etc/systemd/system,usr/bin,usr/lib/systemd/{system,system-generators},usr/local/libexec,vm-modules}
    local commands=()
    if $signed_insmod; then
        touch "$root/etc/cps-vm-assert-built-insmod"
    else
        commands+=(insmod)
    fi
    if [[ -n $util_linux ]]; then
        touch "$root/etc/cps-vm-assert-built-util-linux"
    else
        commands+=(mount)
    fi
    if [[ -z $signed ]]; then
        local programs=(systemd systemd-bless-boot system-generators/systemd-bless-boot-generator)
        if [[ -x "$systemd/systemd-executor" ]]; then programs+=(systemd-executor); fi
        for binary in "${programs[@]}"; do
            cp -L "$systemd/$binary" "$root/usr/lib/systemd/$binary"
            copy_runtime_deps "$root" "$systemd/$binary"
        done
        commands+=(systemctl)
        for unit in basic.target boot-complete.target local-fs.target multi-user.target \
            paths.target shutdown.target slices.target sockets.target swap.target \
            sysinit.target systemd-bless-boot.service timers.target; do
            cp -L "$systemd/system/$unit" "$root/usr/lib/systemd/system/"
        done
        copy_runtime_deps "$root" "$(command -v blkid)" # systemd's ESP discovery loads libblkid
    else
        touch "$root/etc/cps-vm-assert-built-systemd"
    fi
    for command in "${commands[@]}"; do
        binary=$(command -v "$command")
        cp -L "$binary" "$root/usr/bin/$command"
        copy_runtime_deps "$root" "$binary" "$merged"
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
insmod=/usr/bin/insmod
if [ -f /etc/cps-vm-assert-built-insmod ]; then
    [ /bin/insmod -ef "$bb" ]
    insmod=/bin/insmod
fi
: > /run/cps-vm-loaded-modules
while read -r module name; do
    "$insmod" "$module"
    [ -d "/sys/module/$name" ]
    printf '%s\n' "$name" >> /run/cps-vm-loaded-modules
done < "/vm-modules/$release/load"
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
ExecStart=/usr/local/libexec/cps-vm-observe-linked
StandardOutput=tty
StandardError=inherit
UNIT
    cat > "$root/usr/local/libexec/cps-vm-observe" <<'GUEST'
#!/bin/busybox sh
set -e
bb=/bin/busybox
[ /etc/cps-vm-state -ef /etc/cps-vm-boot-state ]
state=$("$bb" cat /etc/cps-vm-boot-state)
if [ -f /etc/cps-vm-assert-merged-usr ]; then
    for path in /bin /sbin /lib /lib64 /usr/sbin /usr/lib64; do [ -L "$path" ]; done
    [ /bin/busybox -ef /usr/bin/busybox ]
    [ /lib64/ld-linux-x86-64.so.2 -ef /usr/lib/ld-linux-x86-64.so.2 ]
    /bin/sh -ec 'test "$1" = merged' sh merged
    printf 'CPS_VM_USR: %s merged\n' "$state"
fi
if [ -f /etc/cps-vm-assert-built-systemd ]; then
    [ /proc/1/exe -ef /usr/lib/systemd/systemd ]
    version=$(/usr/lib/systemd/systemd --version)
    case "$version" in 'systemd 262 (v262.1)'*) ;; *) exit 1 ;; esac
    printf 'CPS_VM_SYSTEMD: %s v262.1\n' "$state"
fi
if [ -f /etc/cps-vm-assert-built-util-linux ]; then
    [ "$(/usr/bin/blkid -p -s TYPE -o value /dev/vda1)" = vfat ]
    case "$(/usr/bin/mount --version)" in 'mount from util-linux 2.42.4'*) ;; *) exit 1 ;; esac
    links=$(/lib64/ld-linux-x86-64.so.2 --inhibit-cache --list /usr/bin/mount)
    for library in libmount.so.1 libblkid.so.1; do
        case "$links" in *"$library => /usr/lib/$library ("*) ;; *) exit 1 ;; esac
    done
    printf 'CPS_VM_UTIL_LINUX: %s 2.42.4\n' "$state"
fi
if [ -f /etc/cps-vm-assert-built-busybox ]; then
    version=$("$bb" --help 2>&1 | {
        IFS=' ' read -r name release rest
        [ "$name" = BusyBox ]
        printf '%s' "$release"
    })
    printf 'CPS_VM_BUSYBOX: %s\n' "$version"
fi
if [ -f /etc/cps-vm-assert-built-insmod ]; then
    printf 'CPS_VM_INSMOD: %s' "$state"
    while read -r name; do printf ' %s' "$name"; done < /run/cps-vm-loaded-modules
    printf '\n'
fi
if [ -f /etc/cps-vm-assert-kernel ]; then
    printf 'CPS_VM_KERNEL: %s %s\n' "$state" "$("$bb" uname -r)"
fi
if [ -f /etc/cps-vm-assert-cps-runtime ]; then
    links=$(/lib64/ld-linux-x86-64.so.2 --inhibit-cache --list /usr/bin/cps)
    for library in libsqlite3.so.0 libseccomp.so.2 libc.so.6 libm.so.6 libgcc_s.so.1; do
        case "$links" in
            *"$library => /usr/lib/$library ("*) ;;
            *) printf 'Unexpected CPS runtime link for %s\n%s\n' "$library" "$links" >&2; exit 1 ;;
        esac
    done
    printf 'CPS_VM_CPS_RUNTIME: %s verified\n' "$state"
fi
if [ -f /etc/cps-vm-assert-extra-publisher ]; then
    publisher=absent
    if [ -f /etc/cps-vm-extra-marker ]; then
        publisher=$("$bb" cat /etc/cps-vm-extra-marker)
    fi
    printf 'CPS_VM_PUBLISHER: %s %s\n' "$state" "$publisher"
    if [ "$state" = trial ]; then
        [ ! -e /etc/cps-vm-legacy-marker ]
        printf 'CPS_VM_OBSOLETION: trial removed\n'
    else
        [ "$("$bb" cat /etc/cps-vm-legacy-marker)" = legacy ]
        printf 'CPS_VM_OBSOLETION: previous retained\n'
    fi
fi
if [ "$state" = trial ] && [ -f /run/cps-vm-counted ] &&
    [ -f /usr/local/libexec/cps-vm-capabilities ]; then
    "$bb" sh /usr/local/libexec/cps-vm-capabilities
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
    ln "$root/usr/local/libexec/cps-vm-observe" "$root/usr/local/libexec/cps-vm-observe-linked"
}
