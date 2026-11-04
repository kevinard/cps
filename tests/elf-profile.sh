#!/usr/bin/env bash
set -euo pipefail

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-"$project/target/debug/cps"}")
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd -- "$work"

arch=$(uname -m)
uid=$(id -u)
gid=$(id -g)
for publisher in canopa runtime; do
    openssl genpkey -algorithm ED25519 -outform DER -out "$publisher.der"
    openssl pkey -inform DER -in "$publisher.der" -pubout -outform DER -out "$publisher-public.der"
done
mkdir -p app/usr/bin runtime/usr/lib/demo
printf 'int answer(void) { return 42; }\n' > library.c
printf 'extern int answer(void); void _start(void) { answer(); }\n' > app.c
cc -shared -fPIC -nostdlib -Wl,-soname,libcps-demo.so.1 \
    -o runtime/usr/lib/demo/libcps-demo.so.1.0 library.c
cc -pie -nostdlib -Wl,-e,_start,--dynamic-linker,/lib64/cps-loader.so \
    -Wl,-rpath,'$ORIGIN/../lib/demo' -o app/usr/bin/demo app.c \
    runtime/usr/lib/demo/libcps-demo.so.1.0
cat > app.kdl <<MANIFEST
schema-version 1
package "demo/app" publisher="canopa" version="1.0.0-0" {
    require "demo/runtime"
    file "usr/bin/demo" mode="0755" uid=$uid gid=$gid
}
MANIFEST
"$cps" publish --repo repo --proto app --signing-key canopa.der app.kdl
base=$(cat repo/current)

publish_runtime() {
    "$cps" manifest generate --proto runtime --publisher runtime --version "$1" \
        --uid "$uid" --gid "$gid" demo/runtime > runtime.kdl
    "$cps" publish --repo companion --proto runtime --signing-key runtime.der runtime.kdl
}
write_policy() {
    cat > repo/publish-policy.kdl <<POLICY
schema-version 1
publisher "canopa" {
    builder-key "../runtime-public.der"
    profile "desktop" architecture="$arch" {
        package "demo/app"
        elf-check {
            library-dir "/usr/lib"
        }
        publisher "runtime" catalog="$(cat companion/current)" {
            repository "../companion"
            trust-key "../runtime-public.der"
        }
    }
}
POLICY
}
refresh() {
    cat > refresh.kdl <<RELEASE
schema-version 1
release "canopa" base="$base" refresh-profiles=#true
RELEASE
    "$cps" repo publish-release --repo repo --signing-key canopa.der refresh.kdl
}
reject_refresh() {
    if refresh > rejected.log 2>&1; then
        echo 'ELF release check accepted an invalid composition' >&2
        exit 1
    fi
    grep -F "$1" rejected.log || { cat rejected.log >&2; exit 1; }
    test "$(cat repo/current)" = "$base"
}

publish_runtime 1.0.0-0
write_policy
reject_refresh 'missing ELF interpreter /lib64/cps-loader.so'

# A native ET_DYN fixture supplies the interpreter path for static inspection.
cp runtime/usr/lib/demo/libcps-demo.so.1.0 runtime/usr/lib/cps-loader.so
ln -s /usr/lib runtime/lib64
publish_runtime 2.0.0-0
write_policy
reject_refresh 'missing ELF dependency libcps-demo.so.1'

ln -s libcps-demo.so.1.0 runtime/usr/lib/demo/libcps-demo.so.1
publish_runtime 3.0.0-0
write_policy
library_hash=$(sha256sum runtime/usr/lib/demo/libcps-demo.so.1.0 | cut -d' ' -f1)
library_blob="companion/blobs/$library_hash"
cp -- "$library_blob" saved-library
printf x | dd of="$library_blob" bs=1 seek=0 conv=notrunc status=none
reject_refresh 'payload digest differs from manifest'
mv -- saved-library "$library_blob"
refresh
base=$(cat repo/current)

mv runtime/usr/lib/demo/libcps-demo.so.1 runtime/usr/lib/demo/old-link
printf 'not an ELF library\n' > runtime/usr/lib/demo/libcps-demo.so.1
publish_runtime 4.0.0-0
write_policy
reject_refresh 'incompatible ELF dependency libcps-demo.so.1'
rm -- runtime/usr/lib/demo/libcps-demo.so.1
ln -s libcps-demo.so.1.0/ runtime/usr/lib/demo/libcps-demo.so.1
publish_runtime 5.0.0-0
write_policy
reject_refresh 'missing ELF dependency libcps-demo.so.1'
sed -i '/elf-check {/,/        }/d' repo/publish-policy.kdl
reject_refresh 'release drops ELF checking for signed profile desktop'

cat > image.kdl <<CONFIG
schema-version 1
architecture "$arch"
release-profile "desktop"
publisher "canopa" {
    repository "repo"
    trust-key "canopa-public.der"
}
publisher "runtime" {
    repository "companion"
    trust-key "runtime-public.der"
}
CONFIG
"$cps" image-create --root image --config image.kdl
"$cps" plan --root image --output install.json install demo/app
"$cps" apply --root image install.json
"$cps" verify --root image --pristine
test -L image/usr/lib/demo/libcps-demo.so.1
echo 'Verified the signed ELF profile with a merged /usr and a companion publisher'
