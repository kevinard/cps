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
mkdir -p runtime/usr/lib app/usr/bin shadow/usr/override
openssl genpkey -algorithm ED25519 -outform DER -out private.der
openssl pkey -inform DER -in private.der -pubout -outform DER -out public.der

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
    "$cps" image-create --root "$1" --repo repo --publisher canopa --trust-key public.der \
        --arch "$arch"
    "$cps" plan --root "$1" --output base.json install demo/app demo/runtime@1.0.0-0
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
cc -pie -nostdlib -Wl,-e,_start -Wl,-rpath,'$ORIGIN/../override' \
    -o app/usr/bin/demo app.c runtime/usr/lib/libcps-abi.so.1.0
publish app 1.0.0-0 demo/app
publish runtime 1.0.0-0 demo/runtime
install_base image
install_base drifted

library libcps-abi.so.1 bad.map shadow/usr/override/libcps-abi.so.1
publish shadow 1.0.0-0 demo/shadow
"$cps" plan --root image --output shadow.json install demo/shadow
reject shadow.json 'ELF dependency libcps-abi.so.1 at usr/override/libcps-abi.so.1 does not provide required version CPS_ABI_1'
test ! -e image/usr/override/libcps-abi.so.1

rm -- runtime/usr/lib/libcps-abi.so.1{,.0}
library libcps-abi.so.2 good.map runtime/usr/lib/libcps-abi.so.2
publish runtime 2.0.0-0 demo/runtime
"$cps" plan --root image --output upgrade.json update
reject upgrade.json 'missing ELF dependency libcps-abi.so.1 previously provided at usr/lib/libcps-abi.so.1.0'
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

rm -- runtime/usr/lib/libcps-abi.so.2
printf 'int answer(void) { return 43; }\n' > library.c
library libcps-abi.so.1 good.map runtime/usr/lib/libcps-abi.so.1.1
ln -s libcps-abi.so.1.1 runtime/usr/lib/libcps-abi.so.1
publish runtime 3.0.0-0 demo/runtime
"$cps" plan --root image --output compatible.json update
"$cps" apply --root image compatible.json
test "$(readlink image/usr/lib/libcps-abi.so.1)" = libcps-abi.so.1.1
"$cps" verify --root image --pristine
printf '%s\n' 'Verified transaction ELF regressions, pre-existing drift, and compatible upgrades'
