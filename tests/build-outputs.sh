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

uid=$(id -u)
gid=$(id -g)
arch=$(uname -m)
sha() { sha256sum -- "$1" | cut -d' ' -f1; }
fingerprint() { tail -c 32 -- "$1" | sha256sum | cut -d' ' -f1; }
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
"$cps" manifest generate --proto toolchain-proto --publisher seed --version 1.0.0-0 --uid "$uid" --gid "$gid" \
    --architecture "$arch" build/tools > tools.kdl
"$cps" publish --repo toolchain --proto toolchain-proto --signing-key toolchain.der tools.kdl

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

cat > app.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "demo/app" publisher="canopa" summary="Demo application" license="MIT OR Apache-2.0" homepage="https://example.org/app/" {
    require "demo/app-data" version="={version}"
    collect "usr/bin/*" uid=$uid gid=$gid
    collect "usr/include/**" uid=$uid gid=$gid
    collect "usr/share/man/**" uid=$uid gid=$gid
    collect "usr/share/locale/*/**" uid=$uid gid=$gid
}
MANIFEST
cat > app-data.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "demo/app-data" publisher="canopa" license="CC0-1.0" {
    collect "usr/share/app/**" uid=$uid gid=$gid
}
MANIFEST
script='    install -Dm755 "$CPS_SOURCES/hello" "$DESTDIR/usr/bin/hello"
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
        publisher "seed" catalog="$(cat toolchain/current)" key-sha256="$(fingerprint toolchain-public.der)"
        package "build/tools"
    }
    version "1.2.0-1" upstream="1.2"
    manifest "app.kdl" sha256="$(sha app.kdl)"
    manifest "app-data.kdl" sha256="$(sha app-data.kdl)"
$1
    source "hello" sha256="$(sha sources/hello)" size=$(wc -c < sources/hello)
    script """
$2
    """
}
RECIPE
}
recipe '    discard "usr/share/doc/**"' "$script" > build.kdl
build() {
    "$cps" build --repo toolchain --publisher seed --trust-key toolchain-public.der --sources sources "$@"
}
build --check-reproducible --builder-signing-key builder.der --output first build.kdl > first.log
grep -Fx "Built cps://canopa/demo/app@1.2.0-1 at first" first.log
grep -Fx "Built cps://canopa/demo/app-data@1.2.0-1 at first" first.log
build --check-reproducible --builder-signing-key builder.der --output second build.kdl
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
grep -E "file \"?$debug_path\"? mode=\"0644\" .* facet=\"?debug.symbols\"?" "$app"
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

# Without stripping the installed object ships unchanged.
recipe '    discard "usr/share/doc/**"
    strip #false' "$script" > unstripped.kdl
build --output unstripped unstripped.kdl
cmp sources/hello unstripped/proto/usr/bin/hello
absent usr/lib/debug unstripped/manifests/demo/app.kdl

# Installed files that no output claims fail the build.
recipe '' "$script" > unpackaged.kdl
expect_failure 'the build installed 1 file(s) that no output claims' build --output unpackaged unpackaged.kdl
grep -Fx '  usr/share/doc/app/README' rejected.log
test ! -e unpackaged
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
"$cps" develop --repo toolchain --publisher seed --trust-key toolchain-public.der --sources sources \
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
"$cps" publish-bundle --repo repo --signing-key release.der --builder-key builder-public.der \
    --recipe-sha256 "$reviewed" first > published.log
test "$(wc -l < published.log)" = 2
for package in app app-data; do
    "$cps" repo attestation --repo repo --publisher canopa --trust-key release-public.der \
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
    trust-key "release-public.der"
}
publisher "seed" {
    repository "toolchain"
    trust-key "toolchain-public.der"
}
CONFIG
"$cps" image-create --root image --config image.kdl
"$cps" info --root image --available demo/app > info.kdl
grep -F 'license="MIT OR Apache-2.0"' info.kdl
grep -E 'homepage="https://example.org/app/"' info.kdl
"$cps" plan --root image --output install.json install demo/app
"$cps" apply --root image install.json
test -x image/usr/bin/hello
test -e image/usr/share/app/data.txt
test ! -e "image/$debug_path"

# Rebuild verification covers every output of the admitted build.
rebuild_recipe=$("$cps" repo rebuild-inputs --repo repo --publisher canopa --trust-key release-public.der \
    --output rebuild-inputs cps://canopa/demo/app-data@1.2.0-1)
cmp app.kdl rebuild-inputs/source/app.kdl
cmp app-data.kdl rebuild-inputs/source/app-data.kdl
verify_rebuild() {
    "$cps" repo verify-rebuild --repo repo --publisher canopa --trust-key release-public.der "$@"
}
verify_rebuild --bundle second cps://canopa/demo/app-data@1.2.0-1
verify_rebuild --bundle second cps://canopa/demo/app@1.2.0-1
cp -a second tampered
sed -E -i 's/facet="?doc\.man"?/facet="doc"/' tampered/manifests/demo/app.kdl
expect_failure 'differs from collect rule usr/share/man/**' verify_rebuild --bundle tampered cps://canopa/demo/app-data@1.2.0-1
echo 'Verified one build with several outputs, collected file lists, and split debug information'
