#!/bin/sh
set -eu
export PATH=/usr/bin:/bin
bb=/bin/busybox
work=/var/tmp/cps-modes
finish() {
    result=$?
    if [ "$result" != 0 ]; then
        for log in group-denied denied interrupted drift; do
            if [ -f "$work/$log" ]; then "$bb" cat "$work/$log"; fi
        done
        echo "CPS_VM_MODES: failed ($result)"
    fi
    "$bb" sync
    "$bb" poweroff -f
}
trap finish EXIT
mount -t devtmpfs devtmpfs /dev
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t tmpfs tmpfs /tmp
"$bb" mkdir -p "$work"
chmod 0755 "$work"
root=$work/image
probe=/usr/local/libexec/mode-probe
cps=/usr/bin/cps
repo=/repository
"$cps" image-create --root "$work/groups" --repo "$repo" --publisher modetest --trust-root /trust.json
"$cps" plan --root "$work/groups" --output "$work/groups.json" install test/groups
if "$probe" no-fsetid "$cps" apply --root "$work/groups" "$work/groups.json" > "$work/group-denied" 2>&1; then exit 1; fi
rejection=$("$bb" cat "$work/group-denied")
case "$rejection" in *'did not retain the requested ownership and mode'*) ;; *) echo "$rejection"; exit 1 ;; esac
test ! -e "$work/groups/srv/shared"
test "$("$cps" transaction status --root "$work/groups")" = "$(printf 'Image revision 0\nNo pending transaction')"
"$cps" verify --root "$work/groups" --pristine
"$cps" image-create --root "$root" --repo "$repo" --publisher modetest --trust-root /trust.json
"$cps" plan --root "$root" --output "$work/install.json" install test/modes@1.0.0-0
if "$probe" no-fsetid "$cps" apply --root "$root" "$work/install.json" > "$work/denied" 2>&1; then exit 1; fi
rejection=$("$bb" cat "$work/denied")
case "$rejection" in *'did not retain the requested ownership and mode'*) ;; *) echo "$rejection"; exit 1 ;; esac
test ! -e "$root/bin/helper"
test "$("$cps" transaction status --root "$root")" = "$(printf 'Image revision 0\nNo pending transaction')"
"$cps" verify --root "$root" --pristine
if "$probe" deny-link "$cps" apply --root "$root" "$work/install.json" > "$work/interrupted" 2>&1; then exit 1; fi
test -f "$root/bin/helper"
test ! -e "$root/bin/helper-link"
test "$("$bb" stat -c '%a' "$root/bin/helper")" = 6755
"$cps" transaction status --root "$root"
"$bb" mv "$repo" /repository-offline
"$cps" transaction resume --root "$root"
"$cps" verify --root "$root" --pristine
test "$root/bin/helper" -ef "$root/bin/helper-link"
"$probe" launch "$root/bin/helper-link" 0 0
"$probe" sticky "$root/tmp/root-owned"
"$probe" inherit "$root/srv/shared/from-user"
"$bb" rm "$root/srv/shared/from-user"
generated=$("$cps" manifest generate --proto "$root/bin" --publisher modetest --version 1.0.0-0 test/generated)
case "$generated" in *'mode="6755"'*hardlink*) ;; *) echo "$generated"; exit 1 ;; esac
chmod 0755 "$root/bin/helper"
chmod 0777 "$root/tmp" "$root/srv/shared"
if "$cps" verify --root "$root" > "$work/drift" 2>&1; then exit 1; fi
"$bb" mv /repository-offline "$repo"
"$cps" plan --root "$root" --output "$work/repair.json" repair
"$cps" apply --root "$root" "$work/repair.json"
"$cps" verify --root "$root" --pristine
test "$root/bin/helper" -ef "$root/bin/helper-link"
test "$("$bb" stat -c '%a' "$root/tmp")" = 1777
test "$("$bb" stat -c '%a' "$root/srv/shared")" = 2777
"$probe" launch "$root/bin/helper-link" 0 0
"$cps" plan --root "$root" --output "$work/clone.json" install test/modes@1.0.0-0
"$cps" boot-env stage --source "$root" --root "$work/clone" "$work/clone.json"
"$cps" verify --root "$work/clone" --pristine
"$probe" launch "$work/clone/bin/helper-link" 0 0
for version in 2 3 4; do
    "$cps" plan --root "$root" --output "$work/update.json" install "test/modes@$version.0.0-0"
    "$cps" apply --root "$root" "$work/update.json"
    "$cps" verify --root "$root" --pristine
    test "$root/bin/helper" -ef "$root/bin/helper-link"
    case "$version" in
        2) "$probe" launch "$root/bin/helper-link" 0 65534 ;;
        3) "$probe" launch "$root/bin/helper-link" 65534 0 ;;
        4) "$probe" launch "$root/bin/helper-link" 65534 65534 ;;
    esac
done
"$probe" launch "$work/clone/bin/helper-link" 0 0
echo 'CPS_VM_MODES: verified privileged execution, sticky deletion, group inheritance, offline recovery, repair, clone isolation, and privilege removal'
