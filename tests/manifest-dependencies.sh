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
arch=$(uname -m)
mkdir -p app/usr/bin private/usr/bin runtime/usr/{bin,lib/demo} tools/usr/local/bin
printf 'int answer(void) { return 42; }\n' > library.c
printf 'extern int answer(void); void _start(void) { answer(); }\n' > app.c
printf 'CPS_DEMO_1.0 { global: answer; local: *; };\n' > library.map
cc -shared -fPIC -nostdlib -Wl,-soname,libcps-demo.so.1 -Wl,--version-script,library.map \
    -o runtime/usr/lib/libcps-demo.so.1.0 library.c
cc -shared -fPIC -nostdlib -Wl,-soname,libcps-private.so.1 \
    -o runtime/usr/lib/demo/libcps-private.so.1 library.c
cc -pie -nostdlib -Wl,-e,_start,--dynamic-linker,/lib/cps-loader.so \
    -o app/usr/bin/demo app.c runtime/usr/lib/libcps-demo.so.1.0
cc -pie -nostdlib -Wl,-e,_start,--dynamic-linker,/lib/cps-loader.so \
    -Wl,-rpath,'$ORIGIN/../lib/demo' -o private/usr/bin/private app.c \
    runtime/usr/lib/demo/libcps-private.so.1
cp runtime/usr/lib/libcps-demo.so.1.0 runtime/usr/lib/cps-loader.so
ln -s libcps-demo.so.1.0 runtime/usr/lib/libcps-demo.so.1
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
    "$cps" publish manifests --repo "$publisher-repo" --proto "$publisher" \
        --key "$publisher.der" "$publisher.kdl" >/dev/null
}
publish_package runtime 1.0.0-0 demo/runtime
runtime_pin=$(cat runtime-repo/current)
# Publication derives the soname capabilities from the library payloads.
"$cps" manifest generated --proto runtime runtime.kdl > runtime-generated.kdl
library="cap/elf/$arch/libcps-demo.so.1"
diff -u - runtime-generated.kdl <<GENERATED
provides "$library"
provides "$library/CPS_DEMO_1.0"
GENERATED
grep -rlF "provides \"$library/CPS_DEMO_1.0\"" runtime-repo/manifests > /dev/null
if sed '/^package .* {$/a\    provides "cap/elf/x86_64/libcps-demo.so.1"' runtime.kdl > authored.kdl &&
    "$cps" publish manifests --repo runtime-repo --proto runtime --key runtime.der authored.kdl 2> authored.log; then
    echo 'publication accepted an authored ELF capability' >&2
    exit 1
fi
grep -F 'reserved for declarations that publication generates' authored.log
publish_package runtime 2.0.0-0 demo/runtime
publish_package tools 1.0.0-0 demo/python
tools_pin=$(cat tools-repo/current)
publish_package tools 1.0.0-0 demo/alternate-python
ambiguous_pin=$(cat tools-repo/current)
"$cps" manifest generate --proto app --publisher runtime --version 1.0.0-0 demo/app > app.kdl
"$cps" manifest generate --proto private --publisher runtime --version 1.0.0-0 demo/private > private.kdl
"$cps" manifest generated --proto app app.kdl > app-generated.kdl
grep -Fx "require-capability \"$library/CPS_DEMO_1.0\" // [\"usr/bin/demo\"]" app-generated.kdl
mv runtime-repo/blobs runtime-blobs
mv tools-repo/blobs tools-blobs

suggest() {
    "$cps" manifest dependencies --proto "${proto:-app}" --config image.kdl \
        --catalog "runtime=$runtime_pin" --catalog "tools=$1" \
        --path-dir /usr/local/bin "${@:2}"
}
suggest "$tools_pin" app.kdl > suggested.kdl
test "$(grep -c '^require ' suggested.kdl)" = 2
grep -Fx 'require "demo/runtime" version="=1.0.0-0"' suggested.kdl
grep -Fx 'require "demo/python" version="=1.0.0-0"' suggested.kdl
grep -F 'ELF interpreter "/lib/cps-loader.so"' suggested.kdl
grep -Fx "// require-capability \"$library/CPS_DEMO_1.0\" from cps://runtime/demo/runtime@1.0.0-0 for [\"usr/bin/demo\"]" suggested.kdl
if grep -F 'ELF library' suggested.kdl; then
    echo 'suggested a path-based require for a generated library capability' >&2
    exit 1
fi
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

# A library outside the library directories is private to its package.
if proto=private suggest "$tools_pin" private.kdl > private.out 2> private.log; then
    echo 'expected a generated requirement without a provider' >&2
    exit 1
fi
grep -F "[\"usr/bin/private\"] need cap/elf/$arch/libcps-private.so.1: no provider in pinned catalogs" private.log
grep -F 'cps://runtime/demo/runtime@1.0.0-0 ships /usr/lib/demo/libcps-private.so.1 outside the library directories' private.log
echo 'Verified generated library capabilities, offline dependency suggestions, and direct requirement checks'
