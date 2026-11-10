#!/usr/bin/env bash
set -euo pipefail

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-$project/target/debug/cps}")
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd -- "$work"

arch=$(uname -m)
uid=$(id -u)
gid=$(id -g)
mkdir -p runtime/usr/lib app/usr/bin shadow/usr/override loader/usr/lib
openssl genpkey -algorithm ED25519 -outform DER -out private.der
"$cps" repo trust init --repo repo --publisher canopa --key private.der >/dev/null

printf 'int answer(void) { return 42; }\n' > library.c
printf 'extern int answer(void); void _start(void) { answer(); }\n' > app.c
printf 'CPS_ABI_1 { global: answer; local: *; };\n' > good.map
printf 'CPS_ABI_0 { global: answer; local: *; };\n' > bad.map
library() {
    cc -shared -fPIC -nostdlib -Wl,-soname,"$1" -Wl,--version-script,"$2" \
        -o "$3" library.c
}
publish() {
    "$cps" manifest generate --proto "$1" --publisher canopa --version "$2" \
        --uid "$uid" --gid "$gid" "$3" > generated.kdl
    "$cps" publish --repo repo --proto "$1" --signing-key private.der generated.kdl
}
install_base() {
    "$cps" image-create --root "$1" --repo repo --publisher canopa --trust-root repo/trust/1.json \
        --arch "$arch"
    "$cps" plan --root "$1" --output base.json install demo/app demo/loader demo/runtime@1.0.0-0
    "$cps" apply --root "$1" base.json
}
reject() {
    if "$cps" apply --root image "$1" > rejected.log 2>&1; then
        echo 'A transaction that breaks the managed library applied successfully' >&2
        exit 1
    fi
    grep -F 'transaction would break managed shared-library dependencies' rejected.log
    grep -F "cps://canopa/demo/app@1.0.0-0: usr/bin/demo: $2" rejected.log
    "$cps" transaction status --root image | grep -Fx 'No pending transaction'
    "$cps" verify --root image --pristine
}
library_capability="cap/elf/$arch/libcps-abi.so.1/CPS_ABI_1"
reject_plan() {
    if "$cps" plan --root image --output rejected.json "$@" > rejected.log 2>&1; then
        echo "The solver accepted a plan without the required library: $*" >&2
        exit 1
    fi
    grep -F "$library_capability" rejected.log
}

library libcps-abi.so.1 good.map runtime/usr/lib/libcps-abi.so.1.0
ln -s libcps-abi.so.1.0 runtime/usr/lib/libcps-abi.so.1
cp runtime/usr/lib/libcps-abi.so.1.0 loader/usr/lib/cps-test-loader
chmod 0755 loader/usr/lib/cps-test-loader
cc -pie -nostdlib -Wl,-e,_start,--dynamic-linker,/usr/lib/cps-test-loader -Wl,-rpath,'$ORIGIN/../override' \
    -o app/usr/bin/demo app.c runtime/usr/lib/libcps-abi.so.1.0
publish app 1.0.0-0 demo/app
publish loader 1.0.0-0 demo/loader
publish runtime 1.0.0-0 demo/runtime
install_base image

# Publication generated the soname and version requirements of demo/app,
# so the solver refuses to drop their provider.
"$cps" info --root image demo/app | grep -Fx "    require-capability \"$library_capability\""
reject_plan remove demo/runtime
"$cps" plan --root image --output mark.json mark automatic demo/runtime
"$cps" apply --root image mark.json
"$cps" plan --root image --output autoremove.json autoremove
"$cps" apply --root image autoremove.json
"$cps" verify --root image --pristine
test -L image/usr/lib/libcps-abi.so.1

# Interpreters stay path-based, so the transaction check guards them.
"$cps" plan --root image --output loader.json remove demo/loader
reject loader.json 'missing ELF interpreter /usr/lib/cps-test-loader previously provided at usr/lib/cps-test-loader'

# Facet content provides no capability, because facets never gate dependencies.
missing='missing ELF dependency libcps-abi.so.1 previously provided at usr/lib/libcps-abi.so.1.0'
"$cps" manifest generate --proto runtime --publisher canopa --version 1.1.0-0 \
    --uid "$uid" --gid "$gid" demo/runtime |
    sed '/^    symlink /s/$/ facet="runtime.library"/' > generated.kdl
"$cps" publish --repo repo --proto runtime --signing-key private.der generated.kdl
reject_plan install demo/runtime@1.1.0-0

# A consumer selected by a facet has no generated requirement, so the
# transaction check still protects its working library.
mkdir -p plugin/usr/bin
cp app/usr/bin/demo plugin/usr/bin/demo-plugin
"$cps" manifest generate --proto plugin --publisher canopa --version 1.0.0-0 \
    --uid "$uid" --gid "$gid" demo/plugin |
    sed '/^    file /s/$/ facet="plugin.demo"/' > generated.kdl
"$cps" publish --repo repo --proto plugin --signing-key private.der generated.kdl
"$cps" image-create --root facet_image --repo repo --publisher canopa --trust-root repo/trust/1.json \
    --arch "$arch"
"$cps" plan --root facet_image --output facet-provider.json install demo/loader demo/plugin \
    demo/runtime@1.1.0-0
"$cps" apply --root facet_image facet-provider.json
"$cps" plan --root facet_image --output facet.json change-facet runtime.library=false
if "$cps" apply --root facet_image facet.json > rejected.log 2>&1; then
    echo 'Disabling a facet removed the working managed library' >&2
    exit 1
fi
grep -F "cps://canopa/demo/plugin@1.0.0-0: usr/bin/demo-plugin: $missing" rejected.log
"$cps" transaction status --root facet_image | grep -Fx 'No pending transaction'

# A symlink already missing on disk was not a working baseline lookup.
rm -- facet_image/usr/lib/libcps-abi.so.1
"$cps" plan --root facet_image --output facet.json change-facet runtime.library=false
"$cps" apply --root facet_image facet.json
if "$cps" verify --root facet_image --runtime > rejected.log 2>&1; then
    echo 'Runtime audit accepted an image without a required library' >&2
    exit 1
fi
grep -F 'missing ELF dependency libcps-abi.so.1' rejected.log

"$cps" plan --root facet_image --output remove-both.json remove demo/runtime demo/plugin
"$cps" apply --root facet_image remove-both.json
"$cps" verify --root facet_image --pristine

library libcps-abi.so.1 bad.map shadow/usr/override/libcps-abi.so.1
publish shadow 1.0.0-0 demo/shadow
"$cps" plan --root image --output shadow.json install demo/shadow
if "$cps" validate --root image shadow.json > rejected.log 2>&1; then
    echo 'Validation accepted a transaction that shadows a working library' >&2
    exit 1
fi
grep -F 'does not provide required version CPS_ABI_1' rejected.log
"$cps" transaction status --root image | grep -Fx 'No pending transaction'
reject shadow.json 'ELF dependency libcps-abi.so.1 at usr/override/libcps-abi.so.1 does not provide required version CPS_ABI_1'
test ! -e image/usr/override/libcps-abi.so.1

# A soname bump no longer provides the required capability, so updates keep
# the old provider and an explicit upgrade has no solution.
rm -- runtime/usr/lib/libcps-abi.so.1{,.0}
library libcps-abi.so.2 good.map runtime/usr/lib/libcps-abi.so.2
publish runtime 2.0.0-0 demo/runtime
reject_plan install demo/runtime@2.0.0-0
test ! -e image/usr/lib/libcps-abi.so.2
"$cps" plan --root image --output loader.json remove demo/loader
cp -a --reflink=auto -- image staged
if "$cps" apply --root staged loader.json > rejected.log 2>&1; then
    echo 'A copy accepted a transaction that breaks the managed interpreter' >&2
    exit 1
fi
grep -F 'missing ELF interpreter /usr/lib/cps-test-loader' rejected.log
rm -rf -- staged

rm -- runtime/usr/lib/libcps-abi.so.2
printf 'int answer(void) { return 43; }\n' > library.c
library libcps-abi.so.1 good.map runtime/usr/lib/libcps-abi.so.1.1
ln -s libcps-abi.so.1.1 runtime/usr/lib/libcps-abi.so.1
publish runtime 3.0.0-0 demo/runtime
"$cps" plan --root image --output compatible.json update
"$cps" validate --root image compatible.json
test "$(readlink image/usr/lib/libcps-abi.so.1)" = libcps-abi.so.1.0
"$cps" apply --root image compatible.json
test "$(readlink image/usr/lib/libcps-abi.so.1)" = libcps-abi.so.1.1
"$cps" verify --root image --pristine
printf '%s\n' 'Verified generated library requirements, transaction ELF regressions, pre-existing drift, and compatible upgrades'
