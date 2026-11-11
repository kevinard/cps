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
    repo=repo
    [[ $publisher == canopa ]] || repo=companion
    "$cps" repo trust init --repo "$repo" --publisher "$publisher" --key "$publisher.der" >/dev/null
    cp "$repo/trust/1.json" "$publisher.trust.json"
done
mkdir -p app/usr/bin runtime/usr/lib64
printf 'int answer(void) { return 42; }\n' > library.c
printf 'extern int answer(void); void _start(void) { answer(); }\n' > app.c
cat > current.map <<'MAP'
CPS_DEMO_1.0 {};
CPS_DEMO_2.0 { global: answer; local: *; } CPS_DEMO_1.0;
MAP
printf 'CPS_DEMO_1.0 { global: answer; local: *; };\n' > old.map
cc -shared -fPIC -nostdlib -Wl,-soname,libcps-demo.so.1 \
    -Wl,--version-script,current.map \
    -o runtime/usr/lib64/libcps-demo.so.1.0 library.c
cc -pie -nostdlib -Wl,-e,_start,--dynamic-linker,/lib64/cps-loader.so \
    -o app/usr/bin/demo app.c runtime/usr/lib64/libcps-demo.so.1.0
objcopy --strip-section-headers app/usr/bin/demo
objcopy --strip-section-headers runtime/usr/lib64/libcps-demo.so.1.0
ln -s libcps-demo.so.1.0 runtime/usr/lib64/libcps-demo.so.1
cat > app.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "demo/app" publisher="canopa" version="1.0.0-0" {
    require "demo/runtime"
    file "usr/bin/demo" mode="0755" uid=$uid gid=$gid
}
MANIFEST
"$cps" publish manifests --repo repo --proto app --key canopa.der app.kdl
sed 's/version="1.0.0-0"/version="2.0.0-0"/; s/require "demo\/runtime"/require-any { package "demo\/runtime"; package "demo\/unused"; }/' \
    app.kdl > indirect.kdl
"$cps" publish manifests --repo repo --proto app --key canopa.der indirect.kdl
base=$(cat repo/current)

publish_runtime() {
    "$cps" manifest generate --proto runtime --publisher runtime --version "$1" \
        --uid "$uid" --gid "$gid" demo/runtime > runtime.kdl
    "$cps" publish manifests --repo companion --proto runtime --key runtime.der runtime.kdl
}
write_policy() {
    cat > repo/publish-policy.kdl <<POLICY
schema-version 1 kind="publish-policy"
publisher "canopa" {
    builder-key "../runtime-public.der"
    profile "desktop" architecture="$arch" {
        package "demo/app@1.0.0-0"
        elf-check require-declarations=#true {
            library-dir "/usr/lib"
        }
        publisher "runtime" catalog="$(cat companion/current)" {
            repository "../companion"
            trust-root "../runtime.trust.json"
        }
    }
}
POLICY
}
refresh() {
    cat > refresh.kdl <<RELEASE
schema-version 1 kind="publish-release"
release "canopa" base="$base" refresh-profiles=#true
RELEASE
    "$cps" publish release --repo repo --key canopa.der refresh.kdl
}
reject_refresh() {
    if refresh > rejected.log 2>&1; then
        echo 'ELF release check accepted an invalid composition' >&2
        exit 1
    fi
    grep -F "$1" rejected.log || { cat rejected.log >&2; exit 1; }
    test "$(cat repo/current)" = "$base"
}
library_capability="cap/elf/$arch/libcps-demo.so.1/CPS_DEMO_2.0"

# The runtime provides the generated library capability from /usr/lib64,
# so the solver accepts the composition and the profile gate inspects it.
publish_runtime 1.0.0-0
write_policy
reject_refresh 'missing ELF interpreter /lib64/cps-loader.so'

# A native ET_DYN fixture supplies the interpreter path for static inspection.
mkdir -p runtime/usr/lib
cp runtime/usr/lib64/libcps-demo.so.1.0 runtime/usr/lib/cps-loader.so
ln -s /usr/lib runtime/lib64
publish_runtime 2.0.0-0
write_policy
# The profile searches only /usr/lib, which the gate enforces.
reject_refresh 'missing ELF dependency libcps-demo.so.1'

mv runtime/usr/lib64/libcps-demo.so.1* runtime/usr/lib/
rmdir runtime/usr/lib64
publish_runtime 3.0.0-0
write_policy
library_hash=$(sha256sum runtime/usr/lib/libcps-demo.so.1.0 | cut -d' ' -f1)
library_blob="companion/blobs/$library_hash"
cp -- "$library_blob" saved-library
printf x | dd of="$library_blob" bs=1 seek=0 conv=notrunc status=none
reject_refresh 'payload digest differs from manifest'
mv -- saved-library "$library_blob"
# The generated capability requirement declares the library, but the
# path-based interpreter still needs a direct require.
sed -i 's/demo\/app@1.0.0-0/demo\/app@2.0.0-0/' repo/publish-policy.kdl
reject_refresh 'needs a direct require for cps://runtime/demo/runtime@3.0.0-0 via usr/lib/cps-loader.so'
write_policy
refresh
base=$(cat repo/current)
sed -i 's/ require-declarations=#true//' repo/publish-policy.kdl
reject_refresh 'release drops ELF dependency declaration checking'
write_policy
sed -i '/elf-check .*{/,/        }/d' repo/publish-policy.kdl
reject_refresh 'release drops ELF checking for signed profile desktop'

# Runtimes without a compatible library provide no capability, so the
# profile solver keeps the newest runtime that still provides it.
mv runtime/usr/lib/libcps-demo.so.1 runtime/usr/lib/old-link
printf 'not an ELF library\n' > runtime/usr/lib/libcps-demo.so.1
publish_runtime 4.0.0-0
rm -- runtime/usr/lib/libcps-demo.so.1
ln -s libcps-demo.so.1.0/ runtime/usr/lib/libcps-demo.so.1
publish_runtime 5.0.0-0
rm -- runtime/usr/lib/libcps-demo.so.1
ln -s libcps-demo.so.1.0 runtime/usr/lib/libcps-demo.so.1
cc -shared -fPIC -nostdlib -Wl,-soname,libcps-demo.so.1 \
    -Wl,--version-script,old.map \
    -o runtime/usr/lib/libcps-demo.so.1.0 library.c
objcopy --strip-section-headers runtime/usr/lib/libcps-demo.so.1.0
publish_runtime 6.0.0-0
"$cps" manifest generated --proto runtime runtime.kdl > generated.kdl
if grep -F "$library_capability" generated.kdl; then
    echo 'a library without CPS_DEMO_2.0 provided its capability' >&2
    exit 1
fi
grep -Fx "provides \"cap/elf/$arch/libcps-demo.so.1/CPS_DEMO_1.0\"" generated.kdl
write_policy
refresh

cat > image.kdl <<CONFIG
schema-version 1 kind="image-config"
architecture "$arch"
release-profile "desktop"
publisher "canopa" {
    repository "repo"
    trust-root "canopa.trust.json"
}
publisher "runtime" {
    repository "companion"
    trust-root "runtime.trust.json"
}
CONFIG
"$cps" image create --root image --config image.kdl
"$cps" install --profile --root image -n --save-plan install.json
"$cps" apply --root image install.json
"$cps" verify --root image --pristine --runtime
test -L image/usr/lib/libcps-demo.so.1
"$cps" info --manifest --root image demo/runtime | grep -F 'version="3.0.0-0"'
echo 'Verified generated library capabilities and the signed ELF profile with a merged /usr and a companion publisher'
