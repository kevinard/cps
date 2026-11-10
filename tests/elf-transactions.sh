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
install_base drifted

missing='missing ELF dependency libcps-abi.so.1 previously provided at usr/lib/libcps-abi.so.1.0'
"$cps" plan --root image --output remove.json remove demo/runtime
reject remove.json "$missing"
"$cps" plan --root image --output loader.json remove demo/loader
reject loader.json 'missing ELF interpreter /usr/lib/cps-test-loader previously provided at usr/lib/cps-test-loader'

"$cps" plan --root image --output mark.json mark automatic demo/runtime
"$cps" apply --root image mark.json
"$cps" plan --root image --output autoremove.json autoremove
reject autoremove.json "$missing"

install_base facet_image
"$cps" manifest generate --proto runtime --publisher canopa --version 1.1.0-0 \
    --uid "$uid" --gid "$gid" demo/runtime |
    sed '/^    symlink /s/$/ facet="runtime.library"/' > generated.kdl
"$cps" publish --repo repo --proto runtime --signing-key private.der generated.kdl
"$cps" plan --root facet_image --output facet-provider.json install demo/runtime@1.1.0-0
"$cps" apply --root facet_image facet-provider.json
"$cps" plan --root facet_image --output facet.json change-facet runtime.library=false
if "$cps" apply --root facet_image facet.json > rejected.log 2>&1; then
    echo 'Disabling a facet removed the working managed library' >&2
    exit 1
fi
grep -F "$missing" rejected.log
"$cps" transaction status --root facet_image | grep -Fx 'No pending transaction'

"$cps" plan --root facet_image --output remove-both.json remove demo/runtime demo/app
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

rm -- runtime/usr/lib/libcps-abi.so.1{,.0}
library libcps-abi.so.2 good.map runtime/usr/lib/libcps-abi.so.2
publish runtime 2.0.0-0 demo/runtime
"$cps" plan --root image --output upgrade.json update
reject upgrade.json "$missing"
test ! -e image/usr/lib/libcps-abi.so.2
if "$cps" boot-env stage --source image --root staged upgrade.json > rejected.log 2>&1; then
    echo 'Staging accepted a transaction that breaks the managed library' >&2
    exit 1
fi
grep -F 'missing ELF dependency libcps-abi.so.1' rejected.log
test ! -e staged

# A symlink already missing on disk was not a working baseline lookup.
rm -- drifted/usr/lib/libcps-abi.so.1
"$cps" plan --root drifted --output drifted.json update
"$cps" apply --root drifted drifted.json
if "$cps" verify --root drifted --runtime > rejected.log 2>&1; then
    echo 'Runtime audit accepted an image without a required library' >&2
    exit 1
fi
grep -F 'missing ELF dependency libcps-abi.so.1' rejected.log

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
printf '%s\n' 'Verified transaction ELF regressions, pre-existing drift, and compatible upgrades'
