#!/usr/bin/env bash
set -euo pipefail

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=$(realpath -- "${CPS_TEST_BINARY:-"$project/target/debug/cps"}")
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd "$work"
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null

git init --quiet --initial-branch=main upstream
git -C upstream config user.name "CPS test"
git -C upstream config user.email "cps@example.invalid"
printf 'first\n' > upstream/hello
git -C upstream add hello
git -C upstream commit --quiet -m first
first=$(git -C upstream rev-parse HEAD)
git -C upstream branch topic/snapshot
git -C upstream tag tag-only
git -C upstream tag -a release -m "Annotated release"
printf 'uncommitted\n' > upstream/hello

openssl genpkey -algorithm ED25519 -outform DER -out private.der
openssl pkey -inform DER -in private.der -pubout -outform DER -out public.der
mkdir proto
cat > template.kdl <<'KDL'
schema-version 1 kind="package-manifest"
package "demo/snapshot" publisher="demo" version="0.0.0-42"
KDL
"$cps" publish --repo repo --proto proto --signing-key private.der template.kdl
manifest_sha256=$(sha256sum template.kdl | cut -d' ' -f1)
cat > template.build.kdl <<KDL
schema-version 1 kind="build-recipe"
build architecture="x86_64" epoch=1700000000 {
    toolchain solver="sat-v2" {
        publisher "seed" catalog="$(printf '%064d' 0)" key-sha256="$(printf '%064d' 0)"
        package "devel/base"
    }
    manifest "template.kdl" sha256="$manifest_sha256"
    source "git.tar" size=0 sha256="$(printf '%064d' 0)"
    option "MODE" "test"
    script "tar -xf \\"\$CPS_SOURCES/git.tar\\""
}
KDL

snapshot_from() {
    local output=$1
    shift
    "$cps" source git-snapshot --git-repo "${git_repo:-$work/upstream}" --repo repo \
        --trust-key public.der --output "$output" "$@" template.build.kdl
}

snapshot() {
    snapshot_from "$2" --branch "$1"
}

snapshot main first | grep -Fx "Prepared cps://demo/demo/snapshot@0.0.0-43 from main@$first"
snapshot main repeat | grep -F 'snapshot@0.0.0-43'
cmp first/build.kdl repeat/build.kdl
cmp first/sources/git.tar repeat/sources/git.tar
grep -E "upstream-version=\"?main@$first\"?( |$)" first/manifest.kdl
test "$(tar -xOf first/sources/git.tar hello)" = first
test "$(git get-tar-commit-id < first/sources/git.tar)" = "$first"
manifest_sha256=$(sha256sum first/manifest.kdl | cut -d' ' -f1)
grep -E "sha256=\"?$manifest_sha256\"?( |$)" first/build.kdl
grep -F 'option "MODE" "test"' first/build.kdl
"$cps" source import --from first/sources --cache cache first/build.kdl
"$cps" publish --repo repo --proto proto --signing-key private.der first/manifest.kdl
snapshot main published | grep -F 'snapshot@0.0.0-43'
"$cps" repo withdraw --repo repo --signing-key private.der cps://demo/demo/snapshot@0.0.0-43
snapshot main withdrawn | grep -F 'snapshot@0.0.0-44'

printf 'middle\n' > upstream/hello
git -C upstream commit --quiet -am middle
middle=$(git -C upstream rev-parse HEAD)
printf 'second\n' > upstream/hello
git -C upstream commit --quiet -am second
second=$(git -C upstream rev-parse HEAD)
snapshot main later > later.log &
main_job=$!
snapshot topic/snapshot topic > topic.log &
topic_job=$!
wait "$main_job"
wait "$topic_job"
test "$(awk -F '[@ ]' '{print $3}' later.log topic.log | sort)" = $'0.0.0-45\n0.0.0-46'
grep -F "from main@$second" later.log
grep -F "from topic/snapshot@$first" topic.log
test "$(tar -xOf later/sources/git.tar hello)" = second
test "$(tar -xOf topic/sources/git.tar hello)" = first

git -C upstream branch release
snapshot_from tagged --tag release | grep -Fx "Prepared cps://demo/demo/snapshot@0.0.0-47 from tag:release@$first"
test "$(git get-tar-commit-id < tagged/sources/git.tar)" = "$first"
grep -E "upstream-version=\"?tag:release@$first\"?( |$)" tagged/manifest.kdl
snapshot_from lightweight --tag tag-only | grep -F "from tag:tag-only@$first"
test "$(tar -xOf lightweight/sources/git.tar hello)" = first

snapshot_from historical --commit "${middle^^}" | grep -Fx "Prepared cps://demo/demo/snapshot@0.0.0-49 from $middle"
test "$(git get-tar-commit-id < historical/sources/git.tar)" = "$middle"
grep -E "upstream-version=\"?$middle\"?( |$)" historical/manifest.kdl

reject_snapshot() {
    if snapshot_from rejected "$@" > rejected.log 2>&1; then
        echo "Unexpectedly accepted Git selector: $*" >&2
        exit 1
    fi
    test ! -e rejected
}
reject_snapshot --branch tag-only
reject_snapshot --tag main
reject_snapshot --commit "${first:0:12}"
reject_snapshot --commit "$(git -C upstream rev-parse refs/tags/release)"
reject_snapshot --tag release --branch main
reject_snapshot

reservation=repo/git-snapshots/$(printf '%s' 'cps://demo/demo/snapshot@' | sha256sum | cut -d' ' -f1)
cp "$reservation" reservation.json
jq -e '.format == 1' reservation.json
jq 'del(.format)' reservation.json > "$reservation"
reject_snapshot --branch main
grep -F 'missing field `format`' rejected.log
jq '.format = 2' reservation.json > "$reservation"
reject_snapshot --branch main
grep -F 'unsupported Git snapshot reservation format 2' rejected.log
cp reservation.json "$reservation"

git init --quiet --initial-branch=main --object-format=sha256 upstream-sha256
git -C upstream-sha256 -c user.name='CPS test' -c user.email=cps@example.invalid \
    commit --quiet --allow-empty -m initial
sha256=$(git -C upstream-sha256 rev-parse HEAD)
git -C upstream-sha256 tag release
git_repo="$work/upstream-sha256" snapshot_from sha256-tag --tag release | grep -F "from tag:release@$sha256"
git_repo="$work/upstream-sha256" snapshot_from sha256-commit --commit "$sha256" | grep -F "from $sha256"
test "$(git get-tar-commit-id < sha256-tag/sources/git.tar)" = "$sha256"
cmp sha256-tag/sources/git.tar sha256-commit/sources/git.tar

git -C upstream tag -a v1.2.0 -m "Versioned release" "$second"
snapshot_from numbered --tag v1.2.0 | grep -Fx "Prepared cps://demo/demo/snapshot@1.2.0 from tag:v1.2.0@$second"
grep -F 'version="1.2.0"' numbered/manifest.kdl
test "$(git get-tar-commit-id < numbered/sources/git.tar)" = "$second"
"$cps" publish --repo repo --proto proto --signing-key private.der numbered/manifest.kdl
snapshot_from numbered-repeat --tag v1.2.0 | grep -F 'snapshot@1.2.0 '
cmp numbered/build.kdl numbered-repeat/build.kdl

git -C upstream tag 1.3.0-rc.1 "$second"
snapshot_from prerelease --tag 1.3.0-rc.1 | grep -F 'snapshot@1.3.0-rc.1 '
snapshot main after-release | grep -F 'snapshot@0.0.0-52 '
snapshot_from commit-after-release --commit "$middle" | grep -F 'snapshot@0.0.0-53 '
git -C upstream tag rolling-2026 "$second"
snapshot_from rolling --tag rolling-2026 | grep -F 'snapshot@0.0.0-54 '

git -C upstream tag -f v1.2.0 "$first"
reject_snapshot --tag v1.2.0
grep -F 'already published from a different source' rejected.log
git -C upstream tag v0.0.0-54 "$second"
reject_snapshot --tag v0.0.0-54
grep -F 'overlaps a reserved snapshot version' rejected.log
git -C upstream tag v0.0.0-55 "$second"
snapshot_from numbered-snapshot --tag v0.0.0-55 | grep -F 'snapshot@0.0.0-55 '
snapshot main after-numbered-snapshot | grep -F 'snapshot@0.0.0-56 '
# Leading zeros spell the same release as the reserved 0.0.0-56.
git -C upstream tag v0.0.0-056 "$first"
reject_snapshot --tag v0.0.0-056
grep -F 'overlaps a reserved snapshot version' rejected.log

if snapshot main first > existing.log 2>&1; then
    echo 'An existing snapshot directory was overwritten' >&2
    exit 1
fi
cmp first/build.kdl repeat/build.kdl
echo 'Verified Git selector pinning, tag versions, snapshot reservations, retries, and concurrent preparation'
