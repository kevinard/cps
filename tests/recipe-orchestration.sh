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
zero=$(printf '0%.0s' {1..64})
one=$(printf '1%.0s' {1..64})
sha() { sha256sum -- "$1" | cut -d' ' -f1; }

# Recipes for a library, a tool built with the library's toolchain package,
# and an application that requires both.
recipe() {
    local dir=$1 name=$2 toolchain=$3 body=$4
    mkdir -p "recipes/$dir"
    cat > "recipes/$dir/$dir.kdl" <<MANIFEST
schema-version 1 kind="package-manifest"
package "$name" publisher="canopa" version="1.0.0-0" {
$body}
MANIFEST
    cat > "recipes/$dir/$dir.build.kdl" <<RECIPE
schema-version 1 kind="build-recipe"
// Reviewed recipe for $name
build architecture="$arch" epoch=1700000000 {
    toolchain solver="sat-v2" {
        publisher "seed"   catalog="$zero" key-sha256="$zero" // seed toolchain
        package "$toolchain"
    }
    manifest "$dir.kdl" sha256="$(sha "recipes/$dir/$dir.kdl")"
    script "true"
}
RECIPE
}
recipe libfoo lib/foo build/cc '    provides "cap/foo-headers"
'
recipe tool devel/tool lib/foo ''
recipe app apps/app build/cc '    require "devel/tool"
    require-capability "cap/foo-headers"
'
recipe other apps/other build/cc '    require "lib/foo"
'
"$cps" build-order recipes > order.txt
diff -u - order.txt <<ORDER
recipes/libfoo/libfoo.build.kdl
recipes/other/other.build.kdl
recipes/tool/tool.build.kdl
recipes/app/app.build.kdl
ORDER
"$cps" build-order --explain recipes > explained.txt
grep -Fx '    after recipes/tool/tool.build.kdl: apps/app requires devel/tool' explained.txt
grep -Fx '    after recipes/libfoo/libfoo.build.kdl: apps/app requires cap/foo-headers from lib/foo' explained.txt
grep -Fx '    after recipes/libfoo/libfoo.build.kdl: toolchain needs lib/foo' explained.txt

cp -r recipes cyclic
sed -i 's/^package "lib\/foo" \(.*\) {$/package "lib\/foo" \1 {\n    require "apps\/app"/' cyclic/libfoo/libfoo.kdl
if "$cps" build-order cyclic > /dev/null 2> cycle.log; then
    echo 'build-order accepted a dependency cycle' >&2
    exit 1
fi
grep -F 'build dependency cycle' cycle.log
grep -F 'lib/foo requires apps/app' cycle.log
cp recipes/app/app.build.kdl recipes/app/duplicate.build.kdl
if "$cps" build-order recipes 2> duplicate.log; then
    echo 'build-order accepted two producers of one package' >&2
    exit 1
fi
grep -F 'both produce apps/app' duplicate.log
rm recipes/app/duplicate.build.kdl

# Repinning rewrites only the catalog digest and keeps comments and spacing.
before=$(cat recipes/tool/tool.build.kdl)
"$cps" recipe repin --dry-run --toolchain-catalog "seed=$one" recipes > dry.txt
test "$(grep -c '^would repin ' dry.txt)" = 4
test "$(cat recipes/tool/tool.build.kdl)" = "$before"
if "$cps" recipe repin --check --toolchain-catalog "seed=$one" recipes > stale.txt 2> stale.log; then
    echo 'repin --check accepted stale toolchain pins' >&2
    exit 1
fi
test "$(grep -c '^stale ' stale.txt)" = 4
grep -F '4 pin(s) in 4 recipe(s) are stale' stale.log
test "$(cat recipes/tool/tool.build.kdl)" = "$before"
"$cps" recipe repin --toolchain-catalog "seed=$one" recipes > repinned.txt
test "$(cat recipes/tool/tool.build.kdl)" = "${before//$zero\" key/$one\" key}"
grep -Fx "repinned recipes/tool/tool.build.kdl toolchain seed catalog $zero -> $one" repinned.txt
grep -Fx "recipe recipes/tool/tool.build.kdl sha256 $(sha recipes/tool/tool.build.kdl)" repinned.txt
"$cps" recipe repin --toolchain-catalog "seed=$one" recipes > again.txt
test ! -s again.txt
"$cps" recipe repin --check --toolchain-catalog "seed=$one" recipes

# A trust root sets key-sha256 for the publisher it names, and a manifest
# edit is repinned from the template next to its recipe.
openssl genpkey -algorithm ED25519 -outform DER -out seed.der
"$cps" repo trust init --repo seed-repo --publisher seed --key seed.der >/dev/null
seed_key=$(sha seed-repo/trust/1.json)
old_manifest=$(sha recipes/tool/tool.kdl)
sed -i 's/^package "devel\/tool" publisher="canopa"/& summary="Tool"/' recipes/tool/tool.kdl
"$cps" recipe repin --trust-root seed-repo/trust/1.json --manifests recipes > keyed.txt
grep -Fx "repinned recipes/tool/tool.build.kdl toolchain seed key-sha256 $zero -> $seed_key" keyed.txt
grep -Fx "repinned recipes/tool/tool.build.kdl manifest tool.kdl sha256 $old_manifest -> $(sha recipes/tool/tool.kdl)" keyed.txt
test "$(grep -c ' key-sha256 ' keyed.txt)" = 4
test "$(grep -c ' manifest ' keyed.txt)" = 1
grep -F "key-sha256=\"$seed_key\" // seed toolchain" recipes/app/app.build.kdl
"$cps" recipe repin --check --trust-root seed-repo/trust/1.json --manifests \
    --toolchain-catalog "seed=$one" recipes

# A soname bump breaks the consumers of the removed soname, and every
# direct dependent of the changed package is listed for a rebuild.
openssl genpkey -algorithm ED25519 -outform DER -out private.der
"$cps" repo trust init --repo repo --publisher canopa --key private.der >/dev/null
cat > image.kdl <<CONFIG
schema-version 1 kind="image-config"
architecture "$arch"
publisher "canopa" {
    repository "repo"
    trust-root "repo/trust/1.json"
}
CONFIG
printf 'int foo(void) { return 1; }\n' > foo.c
printf 'extern int foo(void); void _start(void) { foo(); }\n' > app.c
mkdir -p foo1/usr/lib foo2/usr/lib app/usr/bin empty
cc -shared -fPIC -nostdlib -Wl,-soname,libfoo.so.1 -o foo1/usr/lib/libfoo.so.1 foo.c
cc -shared -fPIC -nostdlib -Wl,-soname,libfoo.so.2 -o foo2/usr/lib/libfoo.so.2 foo.c
cc -pie -nostdlib -Wl,-e,_start -o app/usr/bin/app app.c foo1/usr/lib/libfoo.so.1
publish() {
    "$cps" manifest generate --proto "$1" --publisher canopa --version "$2" \
        --uid "$uid" --gid "$gid" "$3" > "$4.kdl"
    if [[ -n ${5:-} ]]; then sed -i "/^package .* {\$/a\\    require \"$5\"" "$4.kdl"; fi
    "$cps" publish --repo repo --proto "$1" --signing-key private.der "$4.kdl" > /dev/null
}
publish foo1 1.0.0-0 lib/foo foo1
publish app 1.0.0-0 apps/app app
publish empty 1.0.0-0 apps/other other lib/foo
publish empty 1.0.0-0 apps/unrelated unrelated
publish foo2 2.0.0-0 lib/foo foo2
"$cps" rebuild-plan --config image.kdl --catalog "canopa=$(cat repo/current)" \
    --changed lib/foo --recipes recipes > plan.txt 2> plan.log
diff -u - plan.txt <<PLAN
changed cps://canopa/lib/foo@2.0.0-0 from cps://canopa/lib/foo@1.0.0-0
    removes cap/elf/$arch/libfoo.so.1
    adds cap/elf/$arch/libfoo.so.2
breaks cps://canopa/apps/app@1.0.0-0
    requires cap/elf/$arch/libfoo.so.1, which lib/foo no longer provides
    recipe recipes/app/app.build.kdl
rebuild cps://canopa/apps/other@1.0.0-0
    requires lib/foo
    recipe recipes/other/other.build.kdl
recipe recipes/other/other.build.kdl
recipe recipes/app/app.build.kdl
PLAN
test ! -s plan.log
# Plans read the signed summaries, not the manifests.
mv repo/manifests manifests
"$cps" rebuild-plan --config image.kdl --catalog "canopa=$(cat repo/current)" \
    --changed lib/foo@2.0.0-0 > summary-plan.txt
grep -Fx 'breaks cps://canopa/apps/app@1.0.0-0' summary-plan.txt
echo 'Verified recipe build order, cycle reports, pin refreshes and checks, and soname rebuild plans'
