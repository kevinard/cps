#!/usr/bin/env bash
# Exercise file-pattern triggers on offline images: coalesced and ordered
# runs, confinement to declared outputs, journaled recovery, facet
# recomposition, removal of generated paths, and a generated initramfs.
set -euo pipefail
project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=${CPS_BIN:-$project/target/debug/cps}
# shellcheck source=tests/support/runtime.sh
source "$project/tests/support/runtime.sh"
for command in bwrap ldd openssl rg; do
    command -v "$command" >/dev/null || { echo "Missing command: $command" >&2; exit 1; }
done
work=$(mktemp -d)
trap '[[ -n ${KEEP:-} ]] || rm -rf -- "$work"' EXIT
cd "$work"
uid=$(id -u)
gid=$(id -g)
owner="uid=$uid gid=$gid"

openssl genpkey -algorithm ED25519 -outform DER -out private.der
"$cps" repo trust init --repo repo --publisher canopa --key private.der >/dev/null

publish() {
    local name=$1 body=$2
    printf 'schema-version 1 kind="package-manifest"\npackage "%s" publisher="canopa" version="%s" {\n%s}\n' \
        "$name" "${3:-1.0.0-0}" "$body" > manifest.kdl
    "$cps" publish --repo repo --proto proto --signing-key private.der manifest.kdl
}
script() {
    mkdir -p "proto/$(dirname -- "$1")"
    cat > "proto/$1"
    chmod 0755 "proto/$1"
    printf '    file "%s" mode="0755" %s\n' "$1" "$owner"
}
item() {
    mkdir -p proto/usr/share/test-items
    printf '%s\n' "$2" > "proto/usr/share/test-items/$1.item"
    printf '    file "usr/share/test-items/%s.item" mode="0644" %s %s\n' "$1" "$owner" "${3:-}"
}
plan_apply() {
    local root=$1
    shift
    "$cps" plan --root "$root" --output plan.json "$@" > plan.log
    "$cps" apply --root "$root" plan.json
}

mkdir -p proto
publish test/runtime "$(ship_runtime proto /usr/bin/bash /usr/bin/cat /usr/bin/sort /usr/bin/ls /usr/bin/sha256sum /usr/bin/mkdir /usr/bin/rm /usr/bin/wc)
    symlink \"bin\" target=\"usr/bin\" $owner
"

# The index trigger joins every item. It counts its runs in a second output
# and fails while the image contains a marker, to interrupt a transaction.
publish test/index "    require \"test/runtime\"
    dir \"usr/share/test-items\" mode=\"0755\" $owner
$(script usr/libexec/test-index <<'HANDLER'
#!/usr/bin/bash
set -eu
cd /usr/share/test-items
shopt -s nullglob
items=(*.item)
if (( ${#items[@]} )); then cat "${items[@]}" | sort > index; else : > index; fi
echo run >> runs
HANDLER
)
    trigger \"index\" {
        run \"usr/libexec/test-index\"
        watch \"usr/share/test-items/*.item\"
        output \"usr/share/test-items/index\"
        output \"usr/share/test-items/runs\"
    }
"
# The summary trigger watches the index trigger's output, so it runs after
# it. It fails while the image contains a marker, to interrupt a transaction.
publish test/summary "    require \"test/index\"
    dir \"usr/share/test-summary\" mode=\"0755\" $owner
$(script usr/libexec/test-summary <<'HANDLER'
#!/usr/bin/bash
set -eu
[[ ! -e /etc/test-summary-fail ]] || exit 42
wc -l < /usr/share/test-items/index > /usr/share/test-summary/count
echo run >> /usr/share/test-summary/runs
HANDLER
)
    trigger \"summary\" timeout=30 {
        run \"usr/libexec/test-summary\"
        watch \"usr/share/test-items/index\"
        output \"usr/share/test-summary/count\"
        output \"usr/share/test-summary/runs\"
    }
"
publish test/a "$(item a alpha)"
publish test/b "$(item b bravo)"
publish test/c "$(item c charlie 'facet="doc.extra"')"

"$cps" image-create --root image --repo repo --publisher canopa --trust-root repo/trust/1.json
plan_apply image change-facet doc.extra=false
plan_apply image install test/summary test/a test/b test/c
rg -Fx 'Run trigger test/index:index' plan.log
rg -Fx 'Run trigger test/summary:summary' plan.log
test "$(rg -n 'Run trigger' plan.log | cut -d: -f1 | sort -n | head -1)" -lt \
    "$(rg -n 'Run trigger test/summary' plan.log | cut -d: -f1)"
printf 'alpha\nbravo\n' | cmp - image/usr/share/test-items/index
test "$(wc -l < image/usr/share/test-items/runs)" -eq 1
test "$(cat image/usr/share/test-summary/count)" -eq 2
test "$(wc -l < image/usr/share/test-summary/runs)" -eq 1
"$cps" verify --root image
"$cps" verify --root image --quick
"$cps" verify --root image --pristine

# Facet recomposition changes a watched path, and both triggers run once.
plan_apply image change-facet doc.extra=default
printf 'alpha\nbravo\ncharlie\n' | cmp - image/usr/share/test-items/index
test "$(wc -l < image/usr/share/test-items/runs)" -eq 2
test "$(cat image/usr/share/test-summary/count)" -eq 3

# An unrelated change runs nothing.
publish test/other "    dir \"usr/share/other\" mode=\"0755\" $owner
"
plan_apply image install test/other
if rg -F 'Run trigger' plan.log; then
    echo 'an unrelated change ran triggers' >&2
    exit 1
fi
test "$(wc -l < image/usr/share/test-items/runs)" -eq 2

# Packages cannot ship generated paths, at publication or in a plan.
mkdir -p proto/usr/share/test-items proto/usr/share/test-summary
echo stale > proto/usr/share/test-items/index
if publish test/self-clash "    dir \"usr/share/test-items\" mode=\"0755\" $owner
    file \"usr/share/test-items/index\" mode=\"0644\" $owner
$(script usr/libexec/test-clash <<< '#!/usr/bin/bash')
    trigger \"index\" {
        run \"usr/libexec/test-clash\"
        watch \"usr/share/test-items/*.item\"
        output \"usr/share/test-items/index\"
    }
" 2> clash.log; then
    echo 'published a trigger output that covers its own package path' >&2
    exit 1
fi
rg -F 'covers the packaged path usr/share/test-items/index' clash.log
echo stale > proto/usr/share/test-summary/count
publish test/clash "    file \"usr/share/test-summary/count\" mode=\"0644\" $owner
"
if "$cps" plan --root image --output clash.json install test/clash 2> clash.log; then
    echo 'planned a package path inside a trigger output' >&2
    exit 1
fi
rg -F 'path conflict for usr/share/test-summary/count' clash.log

# A failed trigger keeps the transaction pending. Resuming runs only the
# triggers that had not finished.
publish test/d "$(item d delta)"
mkdir image/etc
touch image/etc/test-summary-fail
"$cps" plan --root image --output fail.json install test/d > plan.log
if "$cps" apply --root image fail.json 2> fail.log; then
    echo 'expected the trigger handler to fail' >&2
    exit 1
fi
rg -F 'trigger test/summary:summary failed' fail.log
"$cps" transaction status --root image | rg -Fx 'Offline actions pending'
test "$(wc -l < image/usr/share/test-items/runs)" -eq 3
test "$(wc -l < image/usr/share/test-summary/runs)" -eq 2
rm image/etc/test-summary-fail
rmdir image/etc
mv repo repo.offline
"$cps" transaction resume --root image
mv repo.offline repo
test "$(wc -l < image/usr/share/test-items/runs)" -eq 3
test "$(wc -l < image/usr/share/test-summary/runs)" -eq 3
test "$(cat image/usr/share/test-summary/count)" -eq 4
"$cps" verify --root image

# Aborting reruns the triggers over the restored files. A handler that fails
# during the abort keeps it pending, and finishing it runs only the
# triggers that had not finished.
publish test/e "$(item e echo)"
mkdir image/etc
touch image/etc/test-summary-fail
"$cps" plan --root image --output fail.json install test/e > plan.log
if "$cps" apply --root image fail.json 2> fail.log; then
    echo 'expected the trigger handler to fail' >&2
    exit 1
fi
rg -Fx echo image/usr/share/test-items/index
test "$(wc -l < image/usr/share/test-items/runs)" -eq 4
if "$cps" transaction abort --root image 2> fail.log; then
    echo 'expected the trigger handler to fail during the abort' >&2
    exit 1
fi
rg -F 'trigger test/summary:summary failed' fail.log
"$cps" transaction status --root image | rg -Fx 'Abort in progress'
test ! -e image/usr/share/test-items/e.item
printf 'alpha\nbravo\ncharlie\ndelta\n' | cmp - image/usr/share/test-items/index
test "$(wc -l < image/usr/share/test-items/runs)" -eq 5
rm image/etc/test-summary-fail
rmdir image/etc
"$cps" transaction resume --root image | rg -Fx 'Aborted the pending transaction'
test "$(wc -l < image/usr/share/test-items/runs)" -eq 5
test "$(wc -l < image/usr/share/test-summary/runs)" -eq 4
test "$(cat image/usr/share/test-summary/count)" -eq 4
"$cps" transaction status --root image | rg -Fx 'No pending transaction'
"$cps" verify --root image --pristine

# Removing the declaring packages removes what they generated.
plan_apply image remove test/summary
rg -Fx 'Remove paths generated by trigger test/summary:summary' plan.log
test ! -e image/usr/share/test-summary
plan_apply image remove test/index test/a test/b test/c test/d
test ! -e image/usr/share/test-items
"$cps" verify --root image --pristine

# Handlers cannot write outside their declared outputs. The sandbox has no
# network, a read-only image, and discards changes to undeclared paths.
publish test/escape "    require \"test/runtime\"
    dir \"usr/share/test-escape\" mode=\"0755\" $owner
$(script usr/libexec/test-escape <<'HANDLER'
#!/usr/bin/bash
set -u
cd /usr/share/test-escape
echo declared > declared
! echo x 2>/dev/null > /usr/bin/escape || exit 10
! echo x 2>/dev/null > /etc-escape || exit 11
! echo x 2>/dev/null > /usr/lib/sysimage/cps/escape || exit 12
[[ ! -e /usr/lib/sysimage/cps/state.sqlite ]] || exit 16
# The shared /var is not part of the image: handlers get a private, empty one.
[[ -d /var && -z $(ls -A /var) ]] || exit 17
echo x > /var/scratch || exit 18
! (exec 3<>/dev/tcp/127.0.0.1/22) 2>/dev/null || exit 13
echo x > /tmp/scratch || exit 14
[[ ${HOME-} == /tmp && -z ${CPS_TEST_SECRET-} ]] || exit 15
echo undeclared > undeclared
HANDLER
)
    trigger \"escape\" {
        run \"usr/libexec/test-escape\"
        watch \"usr/share/test-escape\"
        output \"usr/share/test-escape/declared\"
    }
"
"$cps" image-create --root escape --repo repo --publisher canopa --trust-root repo/trust/1.json
"$cps" plan --root escape --output escape.json install test/escape > /dev/null
if CPS_TEST_SECRET=1 "$cps" apply --root escape escape.json 2> escape.log; then
    echo 'a trigger published a path outside its outputs' >&2
    exit 1
fi
rg -F 'wrote usr/share/test-escape/undeclared' escape.log
test ! -e escape/usr/share/test-escape/undeclared
test ! -e escape/usr/share/test-escape/declared
test ! -e escape/usr/bin/escape
test ! -e escape/etc-escape
test ! -e escape/usr/lib/sysimage/cps/escape
test -z "$(ls -A escape/var)"

# A trigger can generate a kernel's initramfs. The boot artifacts follow
# each regeneration, and a prepackaged initrd keeps working without one.
if command -v kernel-install >/dev/null && [[ -x /usr/lib/kernel/install.d/90-loaderentry.install ]]; then
    release=6.0.0-test
    mkdir -p "proto/usr/lib/modules/$release/kernel" proto/usr/lib/firmware
    echo kernel > "proto/usr/lib/modules/$release/vmlinuz"
    echo base > "proto/usr/lib/modules/$release/kernel/base.ko"
    echo extra > "proto/usr/lib/modules/$release/extra.ko"
    publish test/kernel "    file \"usr/lib/modules/$release/vmlinuz\" mode=\"0644\" $owner
    file \"usr/lib/modules/$release/kernel/base.ko\" mode=\"0644\" $owner
    kernel-install \"$release\"
"
    publish test/module "    file \"usr/lib/modules/$release/extra.ko\" mode=\"0644\" $owner
"
    publish test/mkinitrd "    require \"test/runtime\"
$(script usr/libexec/test-mkinitrd <<'HANDLER'
#!/usr/bin/bash
set -eu
shopt -s nullglob globstar
for kernel in /usr/lib/modules/*/vmlinuz; do
    directory=${kernel%/vmlinuz}
    {
        echo "initramfs for ${directory##*/}"
        for module in "$directory"/**/*.ko; do echo "${module#"$directory"/}"; done
        [[ ! -e /etc/crypttab ]] || cat /etc/crypttab
    } > "$directory/initrd"
done
HANDLER
)
    trigger \"initramfs\" {
        run \"usr/libexec/test-mkinitrd\"
        watch \"usr/lib/modules/**\"
        watch \"usr/lib/firmware/**\"
        watch \"etc/crypttab\"
        output \"usr/lib/modules/*/initrd\"
    }
"
    "$cps" image-create --root boot --repo repo --publisher canopa --trust-root repo/trust/1.json
    plan_apply boot install test/kernel test/mkinitrd
    rg -Fx "Prepare Linux $release with kernel-install" plan.log
    printf 'initramfs for %s\nkernel/base.ko\n' "$release" | cmp - "boot/usr/lib/modules/$release/initrd"
    prepared() {
        local dir
        dir=$(ls -d boot/usr/lib/sysimage/cps/kernel-*)
        test "$(sha256sum < "$dir/initrd" | cut -d' ' -f1)" = "$(cat "$dir/initrd.sha256")"
        cmp "$dir/initrd" "boot/usr/lib/modules/$release/initrd"
    }
    prepared
    plan_apply boot install test/module
    rg -Fx 'Run trigger test/mkinitrd:initramfs' plan.log
    rg -Fx "Prepare Linux $release with kernel-install" plan.log
    rg -Fx extra.ko "boot/usr/lib/modules/$release/initrd"
    prepared
    "$cps" verify --root boot --pristine
    # Preparing the unchanged kernel for a regenerated initramfs replaces
    # its prepared artifacts. Aborting regenerates the initramfs from the
    # restored modules and prepares the kernel again. A kernel-install shim
    # damages a module, so the transaction fails its final check after it
    # prepared the kernel and stays pending.
    plan_apply boot remove test/module
    if rg -Fx extra.ko "boot/usr/lib/modules/$release/initrd"; then
        echo 'the initramfs kept a removed module' >&2
        exit 1
    fi
    mkdir shim
    cat > shim/kernel-install <<SHIM
#!/usr/bin/bash
$(command -v kernel-install) "\$@" || exit
[[ ! -e $work/damage ]] || echo damaged > $work/boot/usr/lib/modules/$release/kernel/base.ko
SHIM
    chmod 0755 shim/kernel-install
    "$cps" plan --root boot --output module.json install test/module > plan.log
    touch damage
    if PATH=$work/shim:$PATH "$cps" apply --root boot module.json 2> fail.log; then
        echo 'expected the damaged module to fail the transaction' >&2
        exit 1
    fi
    rm damage
    rg -F 'left an inconsistent image' fail.log
    rg -Fx extra.ko "boot/usr/lib/modules/$release/initrd"
    prepared
    echo base > "boot/usr/lib/modules/$release/kernel/base.ko"
    "$cps" transaction abort --root boot
    test "$(find boot/usr/lib/sysimage/cps -maxdepth 1 -name 'kernel-*' | wc -l)" -eq 1
    printf 'initramfs for %s\nkernel/base.ko\n' "$release" | cmp - "boot/usr/lib/modules/$release/initrd"
    prepared
    "$cps" verify --root boot --pristine
    plan_apply boot remove test/mkinitrd
    test ! -e "boot/usr/lib/modules/$release/initrd"
    without=0
    for dir in boot/usr/lib/sysimage/cps/kernel-*; do
        [[ -e $dir/initrd ]] || without=$((without + 1))
    done
    test "$without" -eq 1
    "$cps" verify --root boot --pristine
else
    echo 'Skipping the generated initramfs: kernel-install and its BLS plugin are unavailable'
fi
echo 'Verified ordered, confined, journaled, and removable trigger outputs'
