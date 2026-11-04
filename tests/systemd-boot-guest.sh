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

# Boot a rescue root with the inputs on a read-only disk.
# The writable disk is the one later booted through systemd-boot.
run_device_guest() {
    local mode=$1 source=$2 output=$3 stage=${4:-}
    local inputs="device-$mode-inputs"
    local program binary name repository key data_args=() scratch_args=()
    local programs=(mount sfdisk blkid e2fsck)
    mkdir -p "$inputs"/{bin,usr/bin,dev,proc,sys,tmp,esp,fallback}
    cp -L -- "$busybox" "$inputs/bin/busybox"
    copy_runtime_deps "$inputs" "$busybox"
    ln -s busybox "$inputs/bin/sh"
    strip --strip-debug -o "$inputs/usr/bin/cps" "$cps"
    copy_runtime_deps "$inputs" "$cps"
    if [[ $mode == installed ]]; then programs+=(cp mke2fs); fi
    for program in "${programs[@]}"; do
        binary=$(command -v "$program")
        cp -L -- "$binary" "$inputs/usr/bin/$program"
        copy_runtime_deps "$inputs" "$binary"
    done
    if [[ $mode == installed ]]; then
        # The image database records absolute repository locations.
        mkdir -p "$inputs$PWD" "$inputs/scratch" "$inputs/installed-copy"
        cp -a -- profile-repo profile-public.der "$inputs$PWD/"
        while read -r name repository key; do
            cp -a -- "$repository" "$key" "$inputs$PWD/"
        done < <(profile_companions)
        truncate -s "$((root_mib * 2 + 128))M" scratch.raw
        mke2fs -q -F -t ext4 scratch.raw
        dd if="$source" of=installed-copy.raw bs=1M skip="$trial_mib" count="$root_mib" \
            conv=sparse status=none
        scratch_args=(-drive if=virtio,format=raw,file=scratch.raw
                      -drive if=virtio,format=raw,file=installed-copy.raw)
    elif [[ $mode != rollback ]]; then
        cp -a -- "$stage" "$inputs/staged"
        cp --sparse=always -- "$stage.ext4" "$inputs/staged.ext4"
        cp -- "$stage.kdl" "$inputs/staged.kdl"
        if [[ $mode != bootstrap ]]; then cp -- base.kdl "$inputs/"; fi
    fi
    {
        printf '#!/bin/sh\nset -eu\nbase_uuid=%q\ntrial_uuid=%q\noptions=%q\n' \
            "$base_uuid" "$trial_uuid" "$options"
        cat <<'GUEST'
trap '/bin/busybox poweroff -f' EXIT
export PATH=/usr/bin:/bin
mount -t devtmpfs devtmpfs /dev
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t tmpfs tmpfs /tmp
mount -t vfat /dev/vda1 /esp
activate() {
    cps boot-env activate-device --root /staged --options "$options" "$@"
}
reject() {
    expected=$1
    shift
    if activate "$@" > /tmp/rejected 2>&1; then
        echo "CPS_VM_DEVICE: unexpected activation"
        exit 1
    fi
    /bin/busybox cat /tmp/rejected
    /bin/busybox grep -Fq "$expected" /tmp/rejected
}
GUEST
        if [[ $mode == bootstrap ]]; then
            cat <<'GUEST'
if cps boot-env activate --known-good --root /staged --esp /esp \
    --root-partuuid "$base_uuid" --options "$options" > /tmp/rejected 2>&1; then
    echo 'CPS_VM_DEVICE: unchecked activation wrote the mounted ESP'
    exit 1
fi
/bin/busybox grep -F 'use boot-env activate-device for a mounted VFAT ESP' /tmp/rejected
set -- --esp /esp --device /dev/vda2 --root-partuuid "$base_uuid" --receipt /staged.kdl
reject 'use --known-good to bootstrap' "$@" --root-image /staged.ext4
reject 'root payload size must match' "$@" --known-good --root-image /staged.kdl
test ! -e /esp/loader
result=$(activate "$@" --known-good --root-image /staged.ext4)
reject 'environment is already registered' "$@" --known-good --root-image /staged.ext4
test "$(activate "$@" --known-good)" = "$result"
printf 'CPS_VM_BOOTSTRAP: %s\n' "${result#Activated }"
GUEST
        elif [[ $mode == initial ]]; then
            cat <<'GUEST'
mount -t ext4 -o ro,noload /dev/vda2 /fallback
reject 'claiming the target root partition exclusively' --esp /esp --device /dev/vda2 \
    --root-partuuid "$base_uuid" --receipt /staged.kdl --root-image /staged.ext4
/bin/busybox umount /fallback
reject 'distinct root PARTUUID' --esp /esp --device /dev/vda2 \
    --root-partuuid "$base_uuid" --receipt /staged.kdl --root-image /staged.ext4
reject 'does not match GPT root PARTUUID' --esp /esp --device /dev/vda3 \
    --root-partuuid "$base_uuid" --receipt /staged.kdl --root-image /staged.ext4
reject 'must name the root of a mounted EFI partition' --esp /esp/EFI --device /dev/vda3 \
    --root-partuuid "$trial_uuid" --receipt /staged.kdl --root-image /staged.ext4
reject 'receipt is for a different image or revision' --esp /esp --device /dev/vda3 \
    --root-partuuid "$trial_uuid" --receipt /base.kdl --root-image /staged.ext4
mount -t ext4 -o ro,noload /dev/vda2 /fallback
activate --esp /esp --device /dev/vda3 \
    --root-partuuid "$trial_uuid" --receipt /staged.kdl --root-image /staged.ext4
reject 'environment is already registered' --esp /esp --device /dev/vda3 \
    --root-partuuid "$trial_uuid" --receipt /staged.kdl --root-image /staged.ext4
activate --esp /esp --device /dev/vda3 \
    --root-partuuid "$trial_uuid" --receipt /staged.kdl
GUEST
        elif [[ $mode == rollback ]]; then
            printf 'base_id=%q\n' "$base_id"
            cat <<'GUEST'
bb=/bin/busybox
mount -t ext4 -o ro,noload /dev/vda2 /fallback
entry="/esp/loader/entries/$base_id.conf"
before=$("$bb" sha256sum /esp/loader/loader.conf)
reject_rollback() {
    expected=$1
    shift
    if cps boot-env rollback "$@" > /tmp/rejected 2>&1; then
        echo 'CPS_VM_DEVICE: unexpected rollback'
        exit 1
    fi
    "$bb" grep -F "$expected" /tmp/rejected
    test "$("$bb" sha256sum /esp/loader/loader.conf)" = "$before"
}
reject_rollback 'must name the root of a mounted EFI partition' --esp /esp/EFI "$base_id"
"$bb" cp "$entry" /tmp/entry.conf
"$bb" sed "s/root=PARTUUID=$base_uuid/root=PARTUUID=$trial_uuid/" /tmp/entry.conf > "$entry"
reject_rollback 'does not contain the expected ext4 filesystem' --esp /esp "$base_id"
"$bb" cat /tmp/entry.conf > "$entry"
result=$(cps boot-env rollback --esp /esp)
test "$result" = "Selected $base_id for the next boot"
printf '%s\n' "$result"
GUEST
        elif [[ $mode == installed ]]; then
            printf 'base_id=%q\nroot_mib=%q\n' "$base_id" "$root_mib"
            cat <<'GUEST'
bb=/bin/busybox
mount -t ext4 -o ro,noload /dev/vda3 /fallback
mount -t ext4 /dev/vdc /scratch
test "$("$bb" cat /fallback/etc/cps-vm-booted-source)" = booted
cps verify --root /fallback
cps plan --root /fallback --output /scratch/upgrade.json update > /tmp/plan
"$bb" grep -F 'test/boot-state 2.0.0-0 -> 3.0.0-0' /tmp/plan
if cps apply --root / /scratch/upgrade.json > /tmp/rejected 2>&1; then
    echo 'CPS_VM_DEVICE: applied a plan to the running root'
    exit 1
fi
"$bb" grep -F 'operation requires an inactive image root' /tmp/rejected
mount -t ext4 /dev/vdd /installed-copy
: > /installed-copy/.write-probe
before=$("$bb" sha256sum /installed-copy/.cps/state.sqlite)
if cps apply --root /installed-copy /scratch/upgrade.json > /tmp/rejected 2>&1; then
    echo 'CPS_VM_DEVICE: applied a plan to a deployed root'
    exit 1
fi
"$bb" grep -F 'use boot-env stage to prepare package changes from a deployed ext4 root' /tmp/rejected
test "$("$bb" sha256sum /installed-copy/.cps/state.sqlite)" = "$before"
test "$("$bb" cat /installed-copy/etc/cps-vm-state)" = trial
"$bb" umount /installed-copy
mount --bind /fallback/etc /fallback/run
if cps boot-env stage --source /fallback --root /scratch/recycled /scratch/upgrade.json > /tmp/rejected 2>&1; then
    echo 'CPS_VM_DEVICE: staged a root with a nested bind mount'
    exit 1
fi
"$bb" grep -F 'opening tree entry "run"' /tmp/rejected
test ! -e /scratch/recycled
"$bb" umount /fallback/run
result=$(cps boot-env stage --source /fallback --root /scratch/recycled /scratch/upgrade.json)
id=${result#Staged }
id=${id%% at *}
test "$("$bb" cat /scratch/recycled/etc/cps-vm-booted-source)" = booted
test "$("$bb" cat /scratch/recycled/etc/cps-vm.conf)" = administrator-setting
test "$("$bb" cat /scratch/recycled/etc/cps-vm-policy.conf)" = administrator-policy
test "$("$bb" stat -c %a /fallback/etc/cps-vm-policy.conf)" = 644
test "$("$bb" stat -c %a /scratch/recycled/etc/cps-vm-policy.conf)" = 600
cps plan --root /scratch/recycled --output /scratch/config.json resolve-config etc/cps-vm.conf --take vendor
cps apply --root /scratch/recycled /scratch/config.json
cps plan --root /scratch/recycled --output /scratch/policy.json resolve-config etc/cps-vm-policy.conf --take local
cps apply --root /scratch/recycled /scratch/policy.json
test "$(cps config list --root /scratch/recycled)" = 'No pending configuration updates'
cps boot-env export --receipt --root /scratch/recycled --output /scratch/recycled.ext4 \
    --size-mib "$root_mib" > /scratch/recycled.kdl
cps boot-env activate-device --root /scratch/recycled --esp /esp --device /dev/vda2 \
    --root-partuuid "$base_uuid" --receipt /scratch/recycled.kdl --root-image /scratch/recycled.ext4 \
    --replace "$base_id" --options "$options"
"$bb" umount /scratch
echo "CPS_VM_INSTALLED_STAGE: $id"
GUEST
        else
            printf 'mode=%q\nbase_id=%q\ntrial_id=%q\n' "$mode" "$base_id" "$trial_id"
            cat <<'GUEST'
mount -t ext4 -o ro,noload /dev/vda3 /fallback
old="/esp/loader/entries/$base_id.conf"
marker="/esp/loader/entries/$base_uuid.cps-retired"
set -- --esp /esp --device /dev/vda2 --root-partuuid "$base_uuid" --receipt /staged.kdl
reject 'cannot replace the known-good base' "$@" --replace "$trial_id" --root-image /staged.ext4
reject 'distinct root PARTUUID' "$@" --root-image /staged.ext4
reject 'root payload size must match' "$@" --replace "$base_id" --root-image /base.kdl
test -f "$old"
test ! -e "$marker"
if [ "$mode" = resume ]; then
    # Recreate the durable state after retirement and an interrupted root write.
    printf '%s\n' "$base_id" > "$marker"
    /bin/busybox rm "$old"
    /bin/busybox sync
    /bin/busybox dd if=/dev/zero of=/dev/vda2 bs=1M count=1 conv=fsync
    cps boot-env list --esp /esp > /tmp/environments
    /bin/busybox grep -F "$trial_id" /tmp/environments | /bin/busybox grep '^\*'
    if /bin/busybox grep -F "$base_id" /tmp/environments; then
        exit 1
    fi
    reject 'an interrupted slot replacement' "$@" --root-image /staged.ext4
    echo 'CPS_VM_DEVICE: resuming after a partial root write'
fi
activate "$@" --replace "$base_id" --root-image /staged.ext4
test ! -e "$old"
test ! -e "$marker"
test ! -e "/esp/EFI/cps/$base_id"
GUEST
        fi
        if [[ $mode != bootstrap ]]; then printf '/bin/busybox umount /fallback\n'; fi
        cat <<'GUEST'
/bin/busybox umount /esp
echo 'CPS_VM_DEVICE: operation completed'
GUEST
    } > "$inputs/init"
    chmod 0755 "$inputs/init"
    truncate -s "$((root_mib + 256))M" "$inputs.raw"
    mke2fs -q -F -t ext4 -d "$inputs" "$inputs.raw"
    cp --reflink=auto --sparse=always "$source" "$output"
    if [[ -n $qemu_data ]]; then data_args=(-L "$qemu_data"); fi
    timeout "${CPS_VM_TIMEOUT:-180}" "$qemu" "${data_args[@]}" \
        -machine q35,accel=tcg -m 768 -smp 2 -nic none -display none -monitor none \
        -serial "file:device-$mode.log" -kernel "$trial_kernel" \
        -append 'root=/dev/vdb rootfstype=ext4 rootwait ro console=ttyS0 loglevel=3 init=/init' \
        -drive "if=virtio,format=raw,file=$output" \
        -drive "if=virtio,format=raw,readonly=on,file=$inputs.raw" "${scratch_args[@]}" || {
        tail -80 "device-$mode.log" >&2
        return 1
    }
    tr -d '\r' < "device-$mode.log" > "device-$mode-activation.log"
    cat "device-$mode-activation.log"
    grep -Fxq 'CPS_VM_DEVICE: operation completed' "device-$mode-activation.log"
    if [[ $mode == resume ]]; then
        grep -Fxq 'CPS_VM_DEVICE: resuming after a partial root write' "device-$mode-activation.log"
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
root_arg=
for arg in $("$bb" cat /proc/cmdline); do
    case "$arg" in root=PARTUUID=*) root_arg=$arg ;; esac
done
[ -n "$root_arg" ]
for entry in /efi/loader/entries/cps-*+*.conf; do
    [ -f "$entry" ] || continue
    while IFS= read -r line; do
        case "$line" in "options $root_arg "*) : > /run/cps-vm-counted; break ;; esac
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
        [ "$facets" = "$(printf 'FACET\tVALUE\ndevel\tfalse\noptional\tfalse')" ]
        printf 'CPS_VM_FACET: %s docs present\n' "$state"
    else
        [ "$state" = previous ]
        [ "$facets" = "$(printf 'FACET\tVALUE\ndevel\tfalse\ndoc.man\tfalse')" ]
        printf 'CPS_VM_FACET: previous docs absent\n'
    fi
fi
if [ -f /etc/cps-vm-assert-installed-stage ] && [ "$state" = trial ]; then
    printf 'booted\n' > /etc/cps-vm-booted-source
fi
if [ -f /etc/cps-vm.conf ]; then
    printf 'CPS_VM_CONFIG: %s %s\n' "$state" "$("$bb" cat /etc/cps-vm.conf)"
fi
if [ -f /etc/cps-vm-policy.conf ]; then
    printf 'CPS_VM_POLICY: %s %s\n' "$state" "$("$bb" cat /etc/cps-vm-policy.conf)"
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
