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

# Build a read-only rescue system for maintenance steps on an offline disk.
# It holds CPS and the tools staging needs. The image database records
# absolute repository locations, so the repositories keep their host paths.
prepare_rescue() {
    local tree=rescue-tree program binary path
    mkdir -p "$tree"/{bin,etc,usr/bin,dev,proc,sys,run,volume,efi}
    printf 'root:x:0:0:root:/root:/bin/sh\n' > "$tree/etc/passwd"
    printf 'root:x:0:\n' > "$tree/etc/group"
    cp -L -- "$busybox" "$tree/bin/busybox"
    copy_runtime_deps "$tree" "$busybox"
    ln -s busybox "$tree/bin/sh"
    strip --strip-debug -o "$tree/usr/bin/cps" "$cps"
    copy_runtime_deps "$tree" "$cps"
    # kernel-install and its BLS plugin prepare manifest-managed kernels.
    for program in mount umount insmod sfdisk kernel-install install chown mkdir grep tr stat rm cp \
        sqlite3 bwrap; do
        binary=$(command -v "$program") || continue
        cp -L -- "$binary" "$tree/usr/bin/$program"
        copy_runtime_deps "$tree" "$binary"
    done
    if [[ -f /usr/lib/kernel/install.d/90-loaderentry.install ]]; then
        mkdir -p "$tree/usr/lib/kernel/install.d"
        cp /usr/lib/kernel/install.d/90-loaderentry.install "$tree/usr/lib/kernel/install.d/"
    fi
    prepare_fat_modules "$tree" "${CPS_VM_KERNEL_RELEASE:-$(uname -r)}" "${CPS_VM_KERNEL_MODULES_ROOT:-/}"
    mkdir -p "$tree$PWD"
    for path in "$@"; do
        if [[ -e $path ]]; then cp -al -- "$path" "$tree$PWD/"; fi
    done
    cat > "$tree/init" <<'GUEST'
#!/bin/sh
bb=/bin/busybox
export PATH=/usr/bin:/bin TMPDIR=/run
mount -t devtmpfs devtmpfs /dev
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t tmpfs tmpfs /run
while read -r module name; do insmod "$module"; done < "/vm-modules/$($bb uname -r)/load"
mount -t btrfs -o subvolid=5 /dev/vda2 /volume
mount -t vfat /dev/vda1 /efi
tr -d '\000' < /dev/vdc > /run/step
$bb sh -eu /run/step
umount /efi
umount /volume
$bb sync
$bb poweroff -f
GUEST
    chmod 0755 "$tree/init"
    truncate -s 4G rescue.btrfs
    "${CPS_VM_MKFS_BTRFS:-mkfs.btrfs}" -q -f --rootdir "$tree" --shrink rescue.btrfs
}

# Run a shell step from standard input in the rescue system, against the
# Btrfs volume at /volume and the ESP at /efi of the disk.
run_rescue() {
    local name=$1 disk=$2 data_args=()
    {
        printf 'bb=/bin/busybox\n'
        cat
        printf "echo 'CPS_VM_RESCUE: step completed'\n"
    } > "rescue-$name.sh"
    cp -- "rescue-$name.sh" "rescue-$name.step"
    truncate -s 1M "rescue-$name.step"
    if [[ -n $qemu_data ]]; then data_args=(-L "$qemu_data"); fi
    timeout "${CPS_VM_TIMEOUT:-180}" "$qemu" "${data_args[@]}" \
        -machine q35,accel=tcg -m 768 -smp 2 -nic none -display none -monitor none \
        -serial "file:rescue-$name.serial" -kernel "$rescue_kernel" \
        -append 'root=/dev/vdb rootfstype=btrfs rootwait ro console=ttyS0 loglevel=3 init=/init' \
        -drive "if=virtio,format=raw,file=$disk" \
        -drive if=virtio,format=raw,readonly=on,file=rescue.btrfs \
        -drive "if=virtio,format=raw,readonly=on,file=rescue-$name.step" || {
        tail -80 "rescue-$name.serial" >&2
        return 1
    }
    tr -d '\r' < "rescue-$name.serial" > "rescue-$name.log"
    if ! grep -Fxq 'CPS_VM_RESCUE: step completed' "rescue-$name.log"; then
        tail -80 "rescue-$name.log" >&2
        return 1
    fi
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
    "$cps" repo trust init --repo "$data/repo" --publisher captest --key cap-private.der >/dev/null
    cp "$data/repo/trust/1.json" "$data/trust.json"
    for version in 1 2; do
        flags=
        if [[ $version == 1 ]]; then flags='capabilities="cap_net_raw=ep"'; fi
        cat > cap.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
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
        # systemd's private libraries load each other through the executable's
        # RUNPATH, which the manifest generator does not follow. Expose them by soname.
        for binary in "$root"/usr/lib/systemd/lib*.so; do
            [[ -e $binary ]] && ln -sfn "systemd/${binary##*/}" "$root/usr/lib/${binary##*/}"
        done
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
# Every environment shares the root partition. Its subvolume names the entry.
flags=
for arg in $("$bb" cat /proc/cmdline); do
    case "$arg" in rootflags=subvolid=*) flags=$arg ;; esac
done
[ -n "$flags" ]
for entry in /efi/loader/entries/cps-*+*.conf; do
    [ -f "$entry" ] || continue
    while IFS= read -r line; do
        case "$line" in "options "*" $flags "*) : > /run/cps-vm-counted; break ;; esac
    done < "$entry"
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
if [ -f /etc/cps-vm-assert-facet ]; then
    facets=$(/usr/bin/cps facet --root /)
    [ "$("$bb" cat /etc/cps-vm-optional.conf)" = 'local settings' ]
    if [ -f /usr/share/man/man1/cps-vm.1 ]; then
        state=$("$bb" cat /usr/share/man/man1/cps-vm.1)
        [ "$facets" = "$(printf 'RULE\tVALUE\tSOURCE\ndebug.*\tfalse\tdefault\nisa.*\tfalse\tdefault\ndevel\tfalse\tlocal\noptional\tfalse\tlocal')" ]
        printf 'CPS_VM_FACET: %s docs present\n' "$state"
    else
        [ "$state" = previous ]
        [ "$facets" = "$(printf 'RULE\tVALUE\tSOURCE\ndebug.*\tfalse\tdefault\nisa.*\tfalse\tdefault\ndevel\tfalse\tlocal\ndoc.man\tfalse\tlocal')" ]
        printf 'CPS_VM_FACET: previous docs absent\n'
    fi
fi
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
if [ -f /etc/cps-vm-assert-cps-builds ]; then
    audit=$(/usr/bin/cps publisher audit-builds --root / canopa)
    [ "$audit" = 'Verified build evidence for 3 installed package(s) from canopa' ]
    printf 'CPS_VM_CPS_BUILDS: %s verified\n' "$state"
fi
if [ -f /etc/cps-vm-assert-profile-protection ]; then
    if /usr/bin/cps plan --root / --output /run/remove-profile.json remove test/boot-stack \
        > /run/remove-profile.log 2>&1; then
        exit 1
    fi
    rejection=$("$bb" cat /run/remove-profile.log)
    case "$rejection" in
        *'test/boot-stack is protected by the installed signed profile'*) ;;
        *) printf '%s\n' "$rejection" >&2; exit 1 ;;
    esac
    [ ! -e /run/remove-profile.json ]
    printf 'CPS_VM_PROFILE_PROTECTION: %s verified\n' "$state"
fi
if [ -f /etc/cps-vm-assert-package-intent ]; then
    listing=$(/usr/bin/cps list --root / --intent)
    case "$listing" in
        *"$(printf 'cps://vm-profile/test/boot-stack@1.0.0-0\t-\troot')"*) ;;
        *) printf '%s\n' "$listing" >&2; exit 1 ;;
    esac
    preview=$(/usr/bin/cps plan --root / --output /run/mark-intent.json mark explicit test/boot-stack)
    case "$preview" in
        *'Mark cps://vm-profile/test/boot-stack@1.0.0-0 as locally requested'*) ;;
        *) printf '%s\n' "$preview" >&2; exit 1 ;;
    esac
    if /usr/bin/cps apply --root / /run/mark-intent.json > /run/mark-rejected.log 2>&1; then
        exit 1
    fi
    rejection=$("$bb" cat /run/mark-rejected.log)
    case "$rejection" in
        *'to prepare package changes from an installed root'*) ;;
        *) printf '%s\n' "$rejection" >&2; exit 1 ;;
    esac
    printf 'CPS_VM_PACKAGE_INTENT: %s verified\n' "$state"
fi
if [ -f /etc/cps-vm-frozen-sqlite ]; then
    frozen_version=$("$bb" cat /etc/cps-vm-frozen-sqlite)
    listing=$(/usr/bin/cps freeze --root /)
    expected=$(printf 'system/sqlite\t%s\t"VM runtime compatibility"' "$frozen_version")
    case "$listing" in *"$expected"*) ;; *) printf '%s\n' "$listing" >&2; exit 1 ;; esac
    preview=$(/usr/bin/cps plan --root / --output /run/unfreeze-runtime.json unfreeze system/sqlite)
    case "$preview" in *'Unfreeze system/sqlite'*) ;; *) printf '%s\n' "$preview" >&2; exit 1 ;; esac
    printf 'CPS_VM_FREEZE: %s verified\n' "$state"
fi
if [ -f /etc/cps-vm-staged-transactions ]; then
    listing=$(/usr/bin/cps freeze --root /)
    if [ "$state" = previous ]; then
        case "$listing" in *"$(printf 'test/boot-state\t')"*) exit 1 ;; esac
        printf 'CPS_VM_STAGED_TRANSACTIONS: previous unchanged\n'
    else
        expected=$(printf 'test/boot-state\t2.0.0-0\t"VM staged review"')
        case "$listing" in *"$expected"*) ;; *) printf '%s\n' "$listing" >&2; exit 1 ;; esac
        printf 'CPS_VM_STAGED_TRANSACTIONS: %s reviewed\n' "$state"
    fi
fi
if [ -f /etc/cps-vm-assert-extra-publisher ]; then
    publisher=absent
    if [ -f /etc/cps-vm-extra-marker ]; then
        publisher=$("$bb" cat /etc/cps-vm-extra-marker)
    fi
    printf 'CPS_VM_PUBLISHER: %s %s\n' "$state" "$publisher"
    if [ "$state" != previous ]; then
        [ ! -e /etc/cps-vm-legacy-marker ]
        printf 'CPS_VM_OBSOLETION: %s removed\n' "$state"
    else
        [ "$("$bb" cat /etc/cps-vm-legacy-marker)" = legacy ]
        printf 'CPS_VM_OBSOLETION: previous retained\n'
    fi
fi
if [ -f /etc/cps-vm-assert-profile-reconcile ]; then
    if [ "$state" = previous ]; then
        [ "$("$bb" cat /etc/cps-vm-previous-default)" = 'previous default' ]
        printf 'CPS_VM_PROFILE_RECONCILE: previous retained\n'
    else
        [ ! -e /etc/cps-vm-previous-default ]
        printf 'CPS_VM_PROFILE_RECONCILE: %s removed\n' "$state"
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
        printf 'CPS_VM_BOOT: %s blessed\n' "$state"
    else
        printf 'CPS_VM_BOOT: %s health-failed\n' "$state"
    fi
    "$bb" sync
    exec "$bb" reboot -f
else
    printf 'CPS_VM_BOOT: %s default\n' "$state"
fi
"$bb" sync
exec "$bb" poweroff -f
GUEST
    chmod 0755 "$root/init" "$root/usr/local/libexec/cps-vm-observe"
    ln "$root/usr/local/libexec/cps-vm-observe" "$root/usr/local/libexec/cps-vm-observe-linked"
}
