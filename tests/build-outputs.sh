#!/usr/bin/env bash
# One sandboxed build that publishes several packages: collected file lists,
# the unpackaged-file check, split debug information, metadata, and job counts.
set -euo pipefail
export LC_ALL=C
umask 022

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-$project/target/debug/cps}")
work=$(mktemp -d "${TMPDIR:-/tmp}/cps-build-outputs.XXXXXX")
trap 'rm -rf -- "$work"' EXIT
cd -- "$work"

arch=$(uname -m)
sha() { sha256sum -- "$1" | cut -d' ' -f1; }
fingerprint() { sha256sum -- "$1" | cut -d' ' -f1; }
keypair() {
    openssl genpkey -algorithm ED25519 -outform DER -out "$1.der"
    openssl pkey -inform DER -in "$1.der" -pubout -outform DER -out "$1-public.der"
}
expect_failure() {
    local expected=$1
    shift
    if "$@" > rejected.log 2>&1; then
        echo "unexpected success: $*" >&2
        exit 1
    fi
    grep -F -- "$expected" rejected.log || { cat rejected.log >&2; return 1; }
}
absent() {
    if grep -E -- "$1" "$2"; then
        echo "unexpected match for $1 in $2" >&2
        exit 1
    fi
}
keypair toolchain
keypair builder
keypair release

# A toolchain with a shell, a few utilities, and objcopy from the worker.
copy_elf() {
    local source=$1 path=$2
    mkdir -p -- "toolchain-proto/${path%/*}"
    cp -L -- "$source" "toolchain-proto/$path"
    while IFS= read -r library; do
        if [[ ! -e toolchain-proto/${library#/} ]]; then
            mkdir -p -- "toolchain-proto/$(dirname -- "${library#/}")"
            cp -L -- "$library" "toolchain-proto/${library#/}"
        fi
    done < <(ldd "$source" | awk '$2 == "=>" && $3 ~ /^\// { print $3 } $1 ~ /^\// { print $1 }')
}
copy_elf "$(command -v bash)" bin/sh
for tool in cat install ln mkdir objcopy; do
    copy_elf "$(command -v "$tool")" "usr/bin/$tool"
done
"$cps" manifest generate --proto toolchain-proto --publisher seed --version 1.0.0-0 \
    --arch "$arch" build/tools > tools.kdl
"$cps" repo trust init --repo toolchain --publisher seed --key toolchain.der >/dev/null
cp toolchain/trust/1.json seed.trust.json
"$cps" publish manifests --repo toolchain --proto toolchain-proto --key toolchain.der tools.kdl

# The application is prebuilt with symbols and a build ID, then installed by the recipe.
mkdir sources
cat > hello.c <<'C'
#include <stdio.h>
int main(void) { puts("hello"); return 0; }
C
cc -g -O1 -Wl,--build-id=sha1 -o sources/hello hello.c
build_id=$(readelf -n sources/hello | sed -n 's/^ *Build ID: \([0-9a-f]*\)$/\1/p')
test -n "$build_id"
debug_path="usr/lib/debug/.build-id/${build_id:0:2}/${build_id:2}.debug"

# Relocatable objects have no build IDs. Archives may contain repeated member
# names; retain their index and all members so they still link after stripping.
mkdir -p one two
# Incompressible code data makes accidental duplication visible even when the
# sidecar is compressed. Fat LTO also serializes this data in compiler sections.
python3 - <<'PYTHON'
import random
from pathlib import Path
payload = random.Random(42).randbytes(65536)
Path("one/answer.c").write_text(
    "const unsigned char payload[] = {" + ",".join(map(str, payload)) + "};\n"
    "int answer(void) { return 40; }\n"
)
PYTHON
printf '%s\n' 'int extra(void) { return 2; }' > two/answer.c
cc -g -O1 -flto -ffat-lto-objects -c one/answer.c -o one/answer.o
cc -g -O1 -c two/answer.c -o two/answer.o
cp one/answer.o sources/answer.o
cc -O1 -c one/answer.c -o sources/plain.o
cp sources/plain.o long_plain_archive_member_name.o
ar qcD sources/libanswer.a one/answer.o two/answer.o long_plain_archive_member_name.o
ranlib -D sources/libanswer.a
# A module signature covers the whole file and sits after the ELF container, so
# stripping would invalidate it. This trailer has the kernel's layout: struct
# module_signature (here with an empty signature) and the marker.
cp two/answer.o sources/signed.ko
printf '\0\0\0\0\0\0\0\0\0\0\0\0~Module signature appended~\n' >> sources/signed.ko

cat > app.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "demo/app" publisher="canopa" summary="Demo application" license="MIT OR Apache-2.0" homepage="https://example.org/app/" {
    require "demo/app-data" version="={version}"
    collect "usr/bin/*"
    collect "usr/lib/*"
    collect "usr/include/**"
    collect "usr/share/man/**"
    collect "usr/share/locale/*/**"
}
MANIFEST
cat > app-data.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "demo/app-data" publisher="canopa" license="CC0-1.0" {
    collect "usr/share/app/**"
}
MANIFEST
script='    install -Dm755 "$CPS_SOURCES/hello" "$DESTDIR/usr/bin/hello"
    for object in answer.o libanswer.a plain.o signed.ko; do
        install -Dm644 "$CPS_SOURCES/$object" "$DESTDIR/usr/lib/$object"
    done
    ln -s hello "$DESTDIR/usr/bin/hi"
    mkdir -p "$DESTDIR/usr/include" "$DESTDIR/usr/share/man/man1" "$DESTDIR/usr/share/app" \
        "$DESTDIR/usr/share/locale/pt_BR/LC_MESSAGES" "$DESTDIR/usr/share/doc/app"
    printf "int hello(void);\n" > "$DESTDIR/usr/include/hello.h"
    printf ".TH HELLO 1\n" > "$DESTDIR/usr/share/man/man1/hello.1"
    printf "ola\n" > "$DESTDIR/usr/share/locale/pt_BR/LC_MESSAGES/hello.mo"
    printf "data\n" > "$DESTDIR/usr/share/app/data.txt"
    printf "readme\n" > "$DESTDIR/usr/share/doc/app/README"
    test "$MAKEFLAGS" = "-j$CPS_BUILD_JOBS"'
recipe() {
    cat <<RECIPE
schema-version 1 kind="build-recipe"
build architecture="$arch" epoch=1700000000 {
    toolchain solver="sat-v2" {
        publisher "seed" catalog-sha256="$(cat toolchain/current)" key-sha256="$(fingerprint seed.trust.json)"
        package "build/tools"
    }
    version "1.2.0-1" upstream-version="1.2"
    manifest "app.kdl" sha256="$(sha app.kdl)"
    manifest "app-data.kdl" sha256="$(sha app-data.kdl)"
$1
    source "hello" sha256="$(sha sources/hello)" size=$(wc -c < sources/hello)
    source "answer.o" sha256="$(sha sources/answer.o)" size=$(wc -c < sources/answer.o)
    source "libanswer.a" sha256="$(sha sources/libanswer.a)" size=$(wc -c < sources/libanswer.a)
    source "plain.o" sha256="$(sha sources/plain.o)" size=$(wc -c < sources/plain.o)
    source "signed.ko" sha256="$(sha sources/signed.ko)" size=$(wc -c < sources/signed.ko)
    script """
$2
    """
}
RECIPE
}
recipe '    discard "usr/share/doc/**"' "$script" > build.kdl
build() {
    "$cps" build --repo toolchain --trust-root seed.trust.json --sources sources "$@"
}
build --check-reproducible --key builder.der --output first build.kdl > first.log
grep -Fx "Built cps://canopa/demo/app@1.2.0-1 at first" first.log
grep -Fx "Built cps://canopa/demo/app-data@1.2.0-1 at first" first.log
build --check-reproducible --key builder.der --output second build.kdl
diff -r first second

# The expanded manifests are explicit, and debug information is a facet of the same package.
app=first/manifests/demo/app.kdl
cmp app.kdl first/templates/app.kdl
grep -E 'version="?1.2.0-1"? upstream-version="?1.2"?' "$app"
grep -F 'license="MIT OR Apache-2.0"' "$app"
grep -E 'require "?demo/app-data"? version="=1.2.0-1"' "$app"
grep -E 'file "?usr/bin/hello"? mode="0755"' "$app"
grep -E 'symlink "?usr/bin/hi"? target="?hello"?' "$app"
grep -E 'file "?usr/include/hello.h"? mode="0644".* facet="?devel"?' "$app"
grep -E 'file "?usr/share/man/man1/hello.1"? .* facet="?doc.man"?' "$app"
grep -E 'file "?usr/share/locale/pt_BR/LC_MESSAGES/hello.mo"? .* facet="?locale.pt_br"?' "$app"
grep -E "file \"?$debug_path\"? mode=\"0644\".* facet=\"?debug.symbols\"?" "$app"
grep -E 'file "?usr/share/app/data.txt"?' first/manifests/demo/app-data.kdl
absent usr/share/app "$app"
test ! -e first/proto/usr/share/doc
test ! -e first/proto/usr/bin/hi

# The shipped object keeps its build ID and loses its symbols.
test "$(readelf -n first/proto/usr/bin/hello | sed -n 's/^ *Build ID: //p')" = "$build_id"
readelf -S first/proto/usr/bin/hello > sections
absent '\.(symtab|debug_info)' sections
readelf -S "first/proto/$debug_path" | grep -F .debug_info
test "$(readelf -n "first/proto/$debug_path" | sed -n 's/^ *Build ID: //p')" = "$build_id"

# Linkable objects lose only debug sections. Their optional deltas restore
# complete objects and archives when static dependencies need debugging.
printf '%s\n' 'int answer(void); int extra(void);' \
    'int main(void) { return answer() + extra() != 42; }' > link.c
for object in answer.o libanswer.a; do
    delta="usr/lib/debug/usr/lib/$object.cps-debug"
    grep -E "file \"?$delta\"? mode=\"0644\".* facet=\"?debug.symbols\"?" "$app"
    readelf -SW "first/proto/usr/lib/$object" > sections
    absent '\] \.(debug_|zdebug_)' sections
    grep -F .symtab sections
    "$cps" debug restore "first/proto/usr/lib/$object" \
        --symbols "first/proto/$delta" --output "restored-$object"
    # Binutils normalizes archive headers/indexes while making the temporary
    # unstripped copy. Restoration must match that entire normalized file.
    objcopy --enable-deterministic-archives "sources/$object" "expected-$object"
    cmp "expected-$object" "restored-$object"
    test "$(wc -c < "first/proto/$delta")" -lt "$(( $(wc -c < "sources/$object") / 4 ))"
    readelf -SW "restored-$object" | grep -F .debug_info
done
cc link.c first/proto/usr/lib/libanswer.a -o linked
./linked
cc link.c restored-libanswer.a -o linked-debug
./linked-debug
cc -r first/proto/usr/lib/answer.o -o linked.o
cc -r restored-answer.o -o restored-linked.o
expect_failure 'debug delta does not match the stripped file' \
    "$cps" debug restore sources/plain.o \
    --symbols first/proto/usr/lib/debug/usr/lib/answer.o.cps-debug --output wrong.o
test ! -e wrong.o
expect_failure 'output must not exist' \
    "$cps" debug restore first/proto/usr/lib/answer.o \
    --symbols first/proto/usr/lib/debug/usr/lib/answer.o.cps-debug --output restored-answer.o
cmp sources/answer.o restored-answer.o
cmp sources/plain.o first/proto/usr/lib/plain.o
test ! -e first/proto/usr/lib/debug/usr/lib/plain.o.cps-debug
# Signed modules with debug sections ship byte-for-byte unchanged.
readelf -SW sources/signed.ko | grep -F .debug_info
cmp sources/signed.ko first/proto/usr/lib/signed.ko
test ! -e first/proto/usr/lib/debug/usr/lib/signed.ko.cps-debug
ar t sources/libanswer.a > expected-members
ar t first/proto/usr/lib/libanswer.a > stripped-members
cmp expected-members stripped-members
test "$(wc -l < stripped-members)" = 3
# A debugger sees source lines and types after the static objects are restored.
readelf --debug-dump=info linked-debug | grep -F DW_AT_name > debug-names
grep -F one/answer.c debug-names
grep -F two/answer.c debug-names

# Without stripping the installed object ships unchanged.
recipe '    discard "usr/share/doc/**"
    strip #false' "$script" > unstripped.kdl
build --output unstripped unstripped.kdl
cmp sources/hello unstripped/proto/usr/bin/hello
cmp sources/answer.o unstripped/proto/usr/lib/answer.o
cmp sources/libanswer.a unstripped/proto/usr/lib/libanswer.a
absent usr/lib/debug unstripped/manifests/demo/app.kdl

# Installed files that no output claims fail the build.
recipe '' "$script" > unpackaged.kdl
expect_failure 'the build installed 1 file(s) that no output claims' build --output unpackaged unpackaged.kdl
grep -Fx '  usr/share/doc/app/README' rejected.log
test ! -e unpackaged
# Packages never ship /var: tmpfiles.d creates it at boot. Empty directories
# are not package content, and a discard pattern drops what the install writes.
var_script="$script
    mkdir -p \"\$DESTDIR/var/lib/app\" \"\$DESTDIR/var/cache/app\"
    printf \"state\\n\" > \"\$DESTDIR/var/lib/app/state\""
recipe '    discard "usr/share/doc/**"' "$var_script" > var.kdl
expect_failure 'the build installed 1 file(s) under $DESTDIR/var' build --output var var.kdl
grep -F 'tmpfiles.d' rejected.log
grep -Fx '  var/lib/app/state' rejected.log
test ! -e var
recipe '    discard "usr/share/doc/**"
    discard "var/**"' "$var_script" > var-discarded.kdl
build --output var-discarded var-discarded.kdl
absent '(^| |")var/' var-discarded/manifests/demo/app.kdl
test ! -e var-discarded/proto/var
cp app-data.kdl app-data.kdl.saved
printf 'schema-version 1 kind="package-manifest"\npackage "demo/app-data" publisher="canopa" {\n    collect "usr/**"\n}\n' > app-data.kdl
recipe '    discard "usr/share/doc/**"' "$script" > overlapping.kdl
expect_failure 'matches collect rules in both demo/app and demo/app-data' build --output overlapping overlapping.kdl
mv app-data.kdl.saved app-data.kdl

# The job count comes from the worker, and outputs must not depend on it.
printf 'schema-version 1 kind="build-worker"\nworker jobs=3\n' > worker.kdl
recipe '    discard "usr/share/doc/**"' "$script
    test \"\$CPS_BUILD_JOBS\" = 3" > jobs.kdl
build --worker-config worker.kdl --output jobs jobs.kdl
"$cps" develop --repo toolchain --trust-root seed.trust.json --sources sources \
    --worker-config worker.kdl jobs.kdl -- /bin/sh -c 'test "$CPS_BUILD_JOBS" = 3 && test "$MAKEFLAGS" = -j3'
recipe '    discard "usr/share/doc/**"' "$script
    printf \"%s\\n\" \"\$CPS_BUILD_JOBS\" > \"\$DESTDIR/usr/share/app/jobs\"" > parallel.kdl
expect_failure 'build is not reproducible for usr/share/app/jobs' \
    build --check-reproducible --worker-config worker.kdl --output parallel parallel.kdl
expect_failure 'build option JOBS is reserved' build --output reserved <(
    recipe '    discard "usr/share/doc/**"
    option "JOBS" "1"' "$script")

# Both outputs publish in one catalog update under one attestation.
mkdir repo
"$cps" repo trust init --repo repo --publisher canopa --key release.der >/dev/null
cp repo/trust/1.json canopa.trust.json
cp builder-public.der repo/
reviewed=$(sha build.kdl)
cat > repo/publish-policy.kdl <<POLICY
schema-version 1 kind="publish-policy"
publisher "canopa" archive-build-inputs=#true {
    builder-key "builder-public.der"
    approve "demo/app" version="1.2.0-1" sha256="$reviewed"
    approve "demo/app-data" version="1.2.0-1" sha256="$reviewed"
}
POLICY
"$cps" publish bundle --repo repo --key release.der --builder-key builder-public.der \
    --recipe-sha256 "$reviewed" first > published.log
test "$(wc -l < published.log)" = 2
for package in app app-data; do
    "$cps" repo attestation --repo repo --trust-root canopa.trust.json \
        "cps://canopa/demo/$package@1.2.0-1" > "$package-attestation.json"
done
cmp app-attestation.json app-data-attestation.json
python3 - "$debug_path" <<'PYTHON'
import base64
import json
import sys

statement = json.loads(base64.b64decode(json.load(open('app-attestation.json'))['payload']))
assert statement['predicate']['packages'] == [
    'cps://canopa/demo/app@1.2.0-1', 'cps://canopa/demo/app-data@1.2.0-1']
names = {subject['name'] for subject in statement['subject']}
assert {'manifest/demo/app', 'manifest/demo/app-data', 'proto/usr/bin/hello',
        'proto/' + sys.argv[1], 'proto/usr/share/app/data.txt'} <= names, names
PYTHON

# Clients see the metadata, and the debug facet is off by default.
# The application links the C library, so its generated ELF requirements are
# satisfied by the seed toolchain publisher that ships it.
cat > image.kdl <<'CONFIG'
schema-version 1 kind="image-config"
publisher "canopa" {
    repository "repo"
    trust-root "canopa.trust.json"
}
publisher "seed" {
    repository "toolchain"
    trust-root "seed.trust.json"
}
CONFIG
"$cps" image create --root image --config image.kdl
"$cps" info --manifest --root image --available demo/app > info.kdl
grep -F 'license="MIT OR Apache-2.0"' info.kdl
grep -E 'homepage="https://example.org/app/"' info.kdl
"$cps" install --root image -n --save-plan install.json demo/app
"$cps" apply --root image install.json
test -x image/usr/bin/hello
test -e image/usr/share/app/data.txt
test ! -e "image/$debug_path"
test ! -e image/usr/lib/debug/usr/lib/answer.o.cps-debug
test ! -e image/usr/lib/debug/usr/lib/libanswer.a.cps-debug

# Enabling debug symbols installs the matching data, and disabling them drops
# it without changing the executable or the linkable objects.
"$cps" facet set --root image -y demo/app:debug.symbols=true
cmp "first/proto/$debug_path" "image/$debug_path"
for object in answer.o libanswer.a; do
    cmp "first/proto/usr/lib/debug/usr/lib/$object.cps-debug" "image/usr/lib/debug/usr/lib/$object.cps-debug"
done
"$cps" facet set --root image -y demo/app:debug.symbols=false
test ! -e "image/$debug_path"
test ! -e image/usr/lib/debug/usr/lib/answer.o.cps-debug
test ! -e image/usr/lib/debug/usr/lib/libanswer.a.cps-debug
cmp first/proto/usr/bin/hello image/usr/bin/hello
cmp first/proto/usr/lib/libanswer.a image/usr/lib/libanswer.a

# Rebuild verification covers every output of the admitted build.
rebuild_recipe=$("$cps" repo rebuild-inputs --repo repo --trust-root canopa.trust.json \
    --output rebuild-inputs cps://canopa/demo/app-data@1.2.0-1)
cmp app.kdl rebuild-inputs/source/app.kdl
cmp app-data.kdl rebuild-inputs/source/app-data.kdl
"$cps" image create --root build-deps-image --config image.kdl
"$cps" install --root build-deps-image --build-deps -n --save-plan build-deps.json demo/app-data
test ! -e build-deps-image/bin/sh
"$cps" apply --root build-deps-image build-deps.json
test -x build-deps-image/bin/sh
test ! -e build-deps-image/usr/bin/hello
test ! -e build-deps-image/usr/share/app/data.txt
"$cps" install --root build-deps-image --build-deps -y cps://canopa/demo/app@1.2.0-1
"$cps" history --root build-deps-image | grep -F 'install --build-deps'
verify_rebuild() {
    "$cps" repo verify-rebuild --repo repo --trust-root canopa.trust.json "$@"
}
verify_rebuild --bundle second cps://canopa/demo/app-data@1.2.0-1
verify_rebuild --bundle second cps://canopa/demo/app@1.2.0-1
cp -a second tampered
sed -E -i 's/facet="?doc\.man"?/facet="doc"/' tampered/manifests/demo/app.kdl
expect_failure 'differs from collect rule usr/share/man/**' verify_rebuild --bundle tampered cps://canopa/demo/app-data@1.2.0-1
echo 'Verified one build with several outputs, collected file lists, and split debug information'
