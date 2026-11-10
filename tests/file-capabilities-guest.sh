#!/bin/busybox sh
# Run as root in the disposable QEMU guest.
set -eu

cps=/usr/bin/cps
work=/var/tmp/cps-file-capabilities
root=$work/image
probe=$root/bin/raw-probe
alias=$root/bin/raw-probe-alias
repo=/usr/lib/cps-cap-test/repo
trust=/usr/lib/cps-cap-test/trust.json

/bin/busybox mkdir -p "$work"
"$cps" image-create --root "$root" --repo "$repo" --publisher captest --trust-root "$trust"
"$cps" plan --root "$root" --output "$work/install.json" install cps://captest/test/raw-probe@1.0.0-0
if error=$(/usr/bin/setpriv --bounding-set=-setfcap "$cps" apply --root "$root" "$work/install.json" 2>&1); then
    printf '%s\n' 'A capability-bearing install succeeded without CAP_SETFCAP' >&2
    exit 1
fi
printf '%s\n' "$error"
case $error in *'requires CAP_SETFCAP'*) ;; *) exit 1 ;; esac
[ ! -e "$probe" ]
"$cps" verify --root "$root" --pristine

run_probe() {
    /usr/bin/setpriv --reuid=65534 --regid=65534 --clear-groups -- "$1"
}

expect_denied() {
    if run_probe "$1"; then
        printf '%s\n' 'The unprivileged probe unexpectedly opened a raw socket' >&2
        exit 1
    else
        [ "$?" -eq 77 ]
    fi
}

expect_drift() {
    if error=$("$cps" verify --root "$root" 2>&1); then
        printf '%s\n' 'Verification accepted a file capability mismatch' >&2
        exit 1
    fi
    printf '%s\n' "$error"
    case $error in *'file capabilities differ'*) ;; *) exit 1 ;; esac
}

repair() {
    "$cps" plan --root "$root" --output "$work/repair.json" repair test/raw-probe
    "$cps" apply --root "$root" "$work/repair.json"
    "$cps" verify --root "$root" --pristine
    [ "$probe" -ef "$alias" ]
}

"$cps" apply --root "$root" "$work/install.json"
"$cps" verify --root "$root" --pristine
[ "$probe" -ef "$alias" ]
[ "$(/usr/bin/getcap "$probe")" = "$probe cap_net_raw=ep" ]
run_probe "$alias"

generated=$("$cps" manifest generate --proto "$root/bin" --publisher captest --version 1.0.0-0 test/generated)
case $generated in
    *cap_net_raw=ep*hardlink*) ;;
    *) printf '%s\n' 'The prototype manifest lost capability or hard-link metadata' >&2; exit 1 ;;
esac

/usr/bin/setcap -r "$alias"
expect_denied "$probe"
expect_drift
repair
run_probe "$alias"

"$cps" plan --root "$root" --output "$work/clone.json" install cps://captest/test/raw-probe@1.0.0-0
"$cps" boot-env stage --source "$root" --root "$work/clone" "$work/clone.json"
"$cps" verify --root "$work/clone" --pristine
run_probe "$work/clone/bin/raw-probe-alias"

"$cps" plan --root "$root" --output "$work/update.json" update
"$cps" apply --root "$root" "$work/update.json"
"$cps" verify --root "$root" --pristine
[ "$probe" -ef "$alias" ]
[ -z "$(/usr/bin/getcap "$probe")" ]
expect_denied "$alias"
run_probe "$work/clone/bin/raw-probe-alias"

/usr/bin/setcap cap_net_raw=ep "$alias"
expect_drift
repair
expect_denied "$alias"
printf 'CPS_VM_CAPABILITIES: verified\n'
