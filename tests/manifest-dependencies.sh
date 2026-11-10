#!/usr/bin/env bash
set -euo pipefail

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-"$project/target/debug/cps"}")
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd -- "$work"

for publisher in runtime tools; do
    openssl genpkey -algorithm ED25519 -outform DER -out "$publisher.der"
    "$cps" repo trust init --repo "$publisher-repo" --publisher "$publisher" \
        --key "$publisher.der" >/dev/null
    cp "$publisher-repo/trust/1.json" "$publisher.trust.json"
done
cat > image.kdl <<CONFIG
schema-version 1 kind="image-config"
architecture "$(uname -m)"
publisher "runtime" {
    repository "runtime-repo"
    trust-root "runtime.trust.json"
}
publisher "tools" {
    repository "tools-repo"
    trust-root "tools.trust.json"
}
CONFIG
mkdir -p app/usr/bin runtime/usr/{bin,lib/demo} tools/usr/local/bin
printf 'int answer(void) { return 42; }\n' > library.c
printf 'extern int answer(void); void _start(void) { answer(); }\n' > app.c
printf 'CPS_DEMO_1.0 { global: answer; local: *; };\n' > library.map
cc -shared -fPIC -nostdlib -Wl,-soname,libcps-demo.so.1 -Wl,--version-script,library.map \
    -o runtime/usr/lib/demo/libcps-demo.so.1.0 library.c
cc -pie -nostdlib -Wl,-e,_start,--dynamic-linker,/lib/cps-loader.so \
    -Wl,-rpath,'$ORIGIN/../lib/demo' -o app/usr/bin/demo app.c \
    runtime/usr/lib/demo/libcps-demo.so.1.0
cp runtime/usr/lib/demo/libcps-demo.so.1.0 runtime/usr/lib/cps-loader.so
ln -s libcps-demo.so.1.0 runtime/usr/lib/demo/libcps-demo.so.1
ln -s usr/lib runtime/lib
ln -s usr/bin runtime/bin
touch runtime/usr/bin/{env,sh} tools/usr/local/bin/python3 app/usr/bin/local-interpreter
printf '#!/bin/sh\n' > app/usr/bin/shell-script
printf '#!/usr/bin/env python3\n' > app/usr/bin/env-script
printf '#!/usr/bin/env -S python3 -O\n' > app/usr/bin/python-script
printf '#!/usr/bin/local-interpreter\n' > app/usr/bin/local-script
chmod 0755 runtime/usr/bin/{env,sh} tools/usr/local/bin/python3 app/usr/bin/*

publish_package() {
    local publisher=$1 version=$2 name=$3
    "$cps" manifest generate --proto "$publisher" --publisher "$publisher" \
        --version "$version" "$name" > "$publisher.kdl"
    "$cps" publish --repo "$publisher-repo" --proto "$publisher" \
        --signing-key "$publisher.der" "$publisher.kdl" >/dev/null
}
publish_package runtime 1.0.0-0 demo/runtime
runtime_pin=$(cat runtime-repo/current)
publish_package runtime 2.0.0-0 demo/runtime
publish_package tools 1.0.0-0 demo/python
tools_pin=$(cat tools-repo/current)
publish_package tools 1.0.0-0 demo/alternate-python
ambiguous_pin=$(cat tools-repo/current)
"$cps" manifest generate --proto app --publisher runtime --version 1.0.0-0 demo/app > app.kdl
mv runtime-repo/blobs runtime-blobs
mv tools-repo/blobs tools-blobs

suggest() {
    "$cps" manifest dependencies --proto app --config image.kdl \
        --catalog "runtime=$runtime_pin" --catalog "tools=$1" \
        --library-dir /usr/lib --path-dir /usr/local/bin "${@:2}"
}
suggest "$tools_pin" app.kdl > suggested.kdl
test "$(grep -c '^require ' suggested.kdl)" = 2
grep -Fx 'require "demo/runtime" version="=1.0.0-0"' suggested.kdl
grep -Fx 'require "demo/python" version="=1.0.0-0"' suggested.kdl
grep -F 'ELF interpreter "/lib/cps-loader.so"' suggested.kdl
grep -F 'ELF library "libcps-demo.so.1" (GNU versions ["CPS_DEMO_1.0"])' suggested.kdl
grep -F '"usr/bin/shell-script" needs shebang interpreter "/bin/sh"' suggested.kdl
grep -F '"usr/bin/env-script" needs env command "python3"' suggested.kdl
grep -F '"usr/bin/python-script" needs env command "python3"' suggested.kdl

if suggest "$tools_pin" --check app.kdl > missing-requires.kdl 2> requires.log; then
    echo 'expected missing runtime requirements to fail the check' >&2
    exit 1
fi
grep -F 'missing matching direct require for cps://runtime/demo/runtime@1.0.0-0' requires.log
grep -F 'missing matching direct require for cps://tools/demo/python@1.0.0-0' requires.log
sed '/^package .* {$/a\    require "demo/runtime" version=">=1.0.0-0, <2.0.0-0"\n    require "demo/python" version="=1.0.0-0"' app.kdl > declared.kdl
suggest "$tools_pin" --check declared.kdl > checked.kdl
test ! -s checked.kdl
sed 's/=1.0.0-0"$/=2.0.0-0"/' declared.kdl > wrong-version.kdl
if suggest "$tools_pin" --check wrong-version.kdl > missing-requires.kdl 2> requires.log; then
    echo 'expected a mismatched runtime requirement to fail the check' >&2
    exit 1
fi
grep -F 'missing matching direct require for cps://tools/demo/python@1.0.0-0' requires.log

if suggest "$ambiguous_pin" app.kdl > partial.kdl 2> ambiguous.log; then
    echo 'expected an ambiguous provider' >&2
    exit 1
fi
grep -F 'ambiguous providers for /usr/local/bin/python3' ambiguous.log
test "$(grep -c '^require ' partial.kdl)" = 1

printf '#!/missing/interpreter\n' > app/usr/bin/shell-script
if suggest "$tools_pin" app.kdl > partial.kdl 2> missing.log; then
    echo 'expected a missing provider' >&2
    exit 1
fi
grep -F '"/missing/interpreter": no provider in pinned catalogs' missing.log
echo 'Verified offline dependency suggestions and direct requirement checks from ELF and shebangs'
