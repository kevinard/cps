#!/bin/sh
# Installed as part of the signed Btrfs profile VM harness.
set -eu
export PATH=/usr/bin:/bin
bb=/usr/bin/busybox

fail_closed() {
    result=$?
    if [ "$result" != 0 ]; then printf 'CPS_VM_SIGNED_BTRFS: failed (%s)\n' "$result"; fi
    "$bb" sync
    "$bb" poweroff -f
}

if [ "${1:-}" != observe ]; then
    trap fail_closed EXIT
    "$bb" mkdir -p /dev /proc /sys /run /efi /volume /environments /repositories
    "$bb" mkdir -p -m 1777 /tmp /var/tmp
    mount -t devtmpfs devtmpfs /dev
    exec </dev/console >/dev/console 2>&1
    mount -t proc proc /proc
    mount -t sysfs sysfs /sys
    mount -t tmpfs tmpfs /run
    mount -t tmpfs tmpfs /tmp
    mount -t vfat /dev/vda1 /efi
    mount -t btrfs -o subvolid=5 /dev/vda2 /volume
    mount -t btrfs -o subvol=@environments /dev/vda2 /environments
    mount -t ext4 -o ro /dev/vdb /repositories
    stage=false
    flags=
    for argument in $("$bb" cat /proc/cmdline); do
        case "$argument" in
            cps.test.stage=1) stage=true ;;
            rootflags=*) flags=$argument ;;
        esac
    done
    if $stage; then
        set --
        while read -r publisher; do
            set -- "$@" --repo "$publisher=/repositories/$publisher"
        done < /etc/cps-btrfs-publishers
        cps publisher set-repository --root / "$@"
        cps verify --root /
        before=$(cps transaction history --root /)
        if cps plan --root / --output /run/blocked.json update > /run/blocked.log 2>&1; then exit 1; fi
        rejection=$("$bb" cat /run/blocked.log)
        case "$rejection" in *'frozen'*) ;; *) printf '%s\n' "$rejection"; exit 1 ;; esac
        cps boot-env capture --source / --output /environments/base
        cps plan --root /environments/base/root --output /run/unfreeze.json unfreeze test/desktop
        cps boot-env stage-btrfs --source /environments/base/root --output /environments/trial /run/unfreeze.json
        trial=/environments/trial/root
        base_release=$("$bb" cat /etc/cps-btrfs-release)
        base_kernel=$(sha256sum "/usr/lib/modules/$base_release/vmlinuz"); base_kernel=${base_kernel%% *}
        cps plan --root "$trial" --output /run/update.json update
        cps apply --root "$trial" /run/update.json
        next_release=$("$bb" cat "$trial/etc/cps-btrfs-release")
        test "$base_release" != "$next_release"
        test ! -e "$trial/usr/lib/modules/$base_release/vmlinuz"
        next_kernel=$(sha256sum "$trial/usr/lib/modules/$next_release/vmlinuz"); next_kernel=${next_kernel%% *}
        test "$base_kernel" != "$next_kernel"
        prepared=false
        for artifact in "$trial"/.cps/kernel-*/linux; do
            digest=$(sha256sum "$artifact"); digest=${digest%% *}
            if [ "$digest" = "$next_kernel" ]; then prepared=true; fi
        done
        test "$prepared" = true
        test "$(sha256sum "/usr/lib/modules/$base_release/vmlinuz")" = "$base_kernel  /usr/lib/modules/$base_release/vmlinuz"
        test "$("$bb" cat "$trial/etc/cps-btrfs-state")" = trial
        test "$("$bb" cat /etc/cps-btrfs-state)" = previous
        test "$(cps transaction history --root /)" = "$before"
        cps verify --root /
        cps verify --root "$trial"
        options='rw console=ttyS0 loglevel=3 init=/usr/local/libexec/cps-profile-btrfs systemd.log_target=console'
        base=$(cps boot-env activate-btrfs --known-good --root /volume/@ --btrfs-volume /volume \
            --esp /efi --root-partuuid 7adff456-af50-498f-b39a-dcf87cb4a7c1 --options "$options")
        printf '%s\n' "${base#Activated }" > /environments/base-id
        cps boot-env activate-btrfs --root /volume/@environments/trial/root --btrfs-volume /volume \
            --esp /efi --root-partuuid 7adff456-af50-498f-b39a-dcf87cb4a7c1 --options "$options"
        cps boot-env list --esp /efi
        printf 'CPS_VM_SIGNED_BTRFS: staged unfreeze and profile upgrade\n'
        exit 0
    fi
    test -n "$flags"
    for entry in /efi/loader/entries/cps-*.conf; do
        test -f "$entry" || continue
        while IFS= read -r line; do
            case "$line" in
                "options "*" $flags "*)
                    name=${entry##*/}
                    case "$name" in *+*) printf 'counted\n' > /run/cps-counted ;; esac
                    ;;
            esac
        done < "$entry"
    done
    mount -t cgroup2 cgroup2 /sys/fs/cgroup
    mount -t efivarfs efivarfs /sys/firmware/efi/efivars
    trap - EXIT
    exec /usr/lib/systemd/systemd
fi

trap fail_closed EXIT
cps verify --root /
state=$("$bb" cat /etc/cps-btrfs-state)
test "$("$bb" uname -r)" = "$("$bb" cat /etc/cps-btrfs-release)"
test /proc/1/exe -ef /usr/lib/systemd/systemd
case "$(/usr/lib/systemd/systemd --version)" in 'systemd 262 (v262.1)'*) ;; *) exit 1 ;; esac
case "$(mount --version)" in 'mount from util-linux 2.42.4'*) ;; *) exit 1 ;; esac
case "$(sfdisk --version)" in 'sfdisk from util-linux 2.42.4'*) ;; *) exit 1 ;; esac
for path in /bin /sbin /lib /lib64; do test -L "$path"; done
links=$(/lib64/ld-linux-x86-64.so.2 --inhibit-cache --list /usr/bin/cps)
for library in libc.so.6 libm.so.6 libgcc_s.so.1 libsqlite3.so.0 libseccomp.so.2; do
    case "$links" in *"$library => /usr/lib/$library ("*) ;; *) printf '%s\n' "$links"; exit 1 ;; esac
done
while read -r publisher; do
    cps publisher audit-builds --root / "$publisher"
done < /etc/cps-btrfs-build-publishers
printf 'CPS_VM_SIGNED_BTRFS: %s signed stack and kernel verified\n' "$state"
printf 'CPS_VM_SIGNED_BTRFS_KERNEL: %s %s\n' "$state" "$("$bb" uname -r)"
base=$("$bb" cat /environments/base-id)
freezes=$(cps freeze --root /)
if [ "$state" = trial ]; then
    case "$freezes" in *test/desktop*) exit 1 ;; esac
    if [ -f /run/cps-counted ]; then
        systemctl is-active --quiet cps-boot-health.service
        systemctl is-active --quiet systemd-bless-boot.service
        printf 'CPS_VM_SIGNED_BTRFS: trial blessed\n'
    else
        listing=$(cps boot-env list --esp /efi)
        printf '%s\n' "$listing"
        # Both roots must now be known-good, with the trial selected by default.
        case "$listing" in *'trial ('*) exit 1 ;; esac
        printf 'CPS_VM_SIGNED_BTRFS: trial default after reboot\n'
        cps boot-env rollback --esp /efi --btrfs-volume /volume "$base"
    fi
    "$bb" sync
    exec "$bb" reboot -f
fi
test "$state" = previous
expected=$(printf 'test/desktop\t1.0.0-0\t"Review before upgrade"')
case "$freezes" in *"$expected"*) ;; *) printf '%s\n' "$freezes"; exit 1 ;; esac
listing=$(cps boot-env list --esp /efi)
case "$listing" in *"$base"*) ;; *) exit 1 ;; esac
printf 'CPS_VM_SIGNED_BTRFS: previous root restored with its freeze and kernel\n'
