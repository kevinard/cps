#!/usr/bin/env bash
set -euo pipefail

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=${CPS_TEST_BINARY:-"$project/target/debug/cps"}
work=$(mktemp -d "${TMPDIR:-/tmp}/cps-build-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
cd "$work"

uid=$(id -u)
gid=$(id -g)
arch=$(uname -m)
mkdir -p proto sources
openssl genpkey -algorithm ED25519 -outform DER -out signing.der
openssl pkey -inform DER -in signing.der -pubout -outform DER -out public.der
openssl genpkey -algorithm ED25519 -outform DER -out builder.der
openssl pkey -inform DER -in builder.der -pubout -outform DER -out builder-public.der
openssl genpkey -algorithm ED25519 -outform DER -out other.der
openssl pkey -inform DER -in other.der -pubout -outform DER -out other-public.der

cat > probe.c <<'C'
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <time.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "unstable") == 0) {
        struct timespec now;
        assert(clock_gettime(CLOCK_MONOTONIC, &now) == 0);
        printf("%lld.%09ld\n", (long long)now.tv_sec, now.tv_nsec);
        return 0;
    }
    if (argc == 2 && strcmp(argv[1], "symlink") == 0) {
        assert(symlink("/sources/input", "/out/hello") == 0);
        return 0;
    }

    struct utsname names;
    assert(uname(&names) == 0 && strcmp(names.nodename, "cps-build") == 0);
    assert(getuid() == 1000 && getgid() == 1000);
    assert(socket(AF_INET, SOCK_STREAM, 0) < 0 && errno == EPERM);
    assert(socket(AF_VSOCK, SOCK_STREAM, 0) < 0 && errno == EPERM);
    assert(syscall(SYS_io_uring_setup, 0, NULL) < 0 && errno == ENOSYS);
    assert(unshare(CLONE_NEWUSER) < 0);
    int sockets[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) == 0);
    assert(open("/sources/input", O_WRONLY) < 0);
    assert(chmod("/sources/input", 0644) < 0 && errno == EROFS);
    assert(chmod("/bin/sh", 0755) < 0 && errno == EROFS);
    puts("sandbox isolation verified");
}
C
cc -O2 -static -o probe probe.c

add_file() {
    local input=$1 path=$2
    if [[ ! -e proto/$path ]]; then
        install -D -m755 "$input" "proto/$path"
        printf '    file "%s" mode="0755" uid=0 gid=0\n' "$path"
    fi
}

shell=$(command -v bash)
{
    printf 'schema-version 1\npackage "build/shell" publisher="canopa" version="1.0.0-0" {\n'
    add_file "$shell" bin/sh
    interpreter=$(readelf -l "$shell" | sed -n 's/^.*Requesting program interpreter: \([^]]*\)].*$/\1/p')
    test -n "$interpreter"
    add_file "$interpreter" "${interpreter#/}"
    while IFS= read -r lib; do
        add_file "$lib" "${lib#/}"
    done < <(ldd "$shell" | awk '$2 == "=>" && $3 ~ /^\// { print $3 }')
    printf '}\n'
} > shell.kdl
"$cps" publish --repo repo --proto proto --signing-key signing.der shell.kdl

{
    printf 'schema-version 1\npackage "build/probe" publisher="canopa" version="1.0.0-0" {\n'
    printf '    require "build/shell" version="=1.0.0-0"\n'
    add_file probe usr/bin/probe
    printf '}\n'
} > probe.kdl
"$cps" publish --repo repo --proto proto --signing-key signing.der probe.kdl
catalog=$(cat repo/current)

# The next version cannot run. The build must use the pinned catalog.
printf 'broken executable\n' > proto/usr/bin/probe
sed 's/version="1.0.0-0" {/version="2.0.0-0" {/' probe.kdl > newer.kdl
"$cps" publish --repo repo --proto proto --signing-key signing.der newer.kdl

printf 'hello from pinned input\n' > sources/input
printf 'outside\n' > host-only
cat > manifest.kdl <<MANIFEST
schema-version 1
package "demo/hello" publisher="canopa" version="1.0.0-0" {
    file "hello" mode="0644" uid=$uid gid=$gid
}
MANIFEST
sha() { sha256sum "$1" | cut -d' ' -f1; }
recipe() {
    cat <<RECIPE
schema-version 1
build architecture="$arch" epoch=1700000000 {
    toolchain "$catalog" solver="sat-v1" {
        package "build/probe"
    }
    manifest "manifest.kdl" sha256="$(sha manifest.kdl)"
    source "input" sha256="$(sha sources/input)" size=$(wc -c < sources/input)
    script """
$1
    """
}
RECIPE
}

recipe '    probe
    test "$PWD" = /build
    test "$HOME" = /build/home
    test "$LC_ALL" = C
    test "$TZ" = UTC
    test "$SOURCE_DATE_EPOCH" = 1700000000
    test -z "${CPS_TEST_SECRET:-}"
    test ! -e "'"$work"'/host-only"
    test ! -e /proc/1/root'"$work"'/host-only
    test ! -e /out/hello
    printf "writable\n" > "$HOME/test"
    read -r line < "$CPS_SOURCES/input"
    printf "%s %s\n" "$line" "$SOURCE_DATE_EPOCH" > "$DESTDIR/hello"
    printf "omitted\n" > "$DESTDIR/unlisted"' > build.kdl

export CPS_TEST_SECRET=should-be-cleared
build_package() {
    "$cps" build --repo repo --publisher canopa --trust-key public.der "$@"
}
"$cps" source import --from sources --cache cache build.kdl
"$cps" source fetch --cache cache build.kdl
build_package --source-cache cache --builder-signing-key builder.der --check-reproducible --output first build.kdl
(umask 077; build_package --sources sources --builder-signing-key builder.der --check-reproducible --output second build.kdl)
diff -r first second
build_package --sources sources --builder-signing-key other.der --check-reproducible --output unapproved build.kdl
cp build.kdl other-recipe.kdl
printf '\n// A different reviewed input\n' >> other-recipe.kdl
build_package --sources sources --builder-signing-key other.der --check-reproducible --output mismatch other-recipe.kdl
test ! -e first/proto/unlisted
test "$(stat -c '%Y %a' first/proto/hello)" = "1700000000 644"
test "$(stat -c '%Y %a' second/proto/hello)" = "1700000000 644"

recipe '    probe unstable > "$DESTDIR/hello"' > unstable.kdl
if build_package --sources sources --check-reproducible --output unstable unstable.kdl 2> unstable.log; then
    echo "nondeterministic build unexpectedly succeeded" >&2
    exit 1
fi
grep -F 'build is not reproducible for hello' unstable.log
test ! -e unstable

recipe '    probe symlink' > symlink.kdl
if build_package --sources sources --output symlink symlink.kdl 2> symlink.log; then
    echo "symlink build output unexpectedly succeeded" >&2
    exit 1
fi
grep -F 'opening declared build output hello' symlink.log
test ! -e symlink

# A same-length modification must fail digest verification before the sandbox starts.
printf 'x' | dd of=sources/input bs=1 conv=notrunc status=none
if build_package --sources sources --output bad build.kdl 2> bad.log; then
    echo "corrupt source unexpectedly succeeded" >&2
    exit 1
fi
grep -F 'source digest or length mismatch for input' bad.log
test ! -e bad
object=$(find cache/sha256 -type f)
chmod u+w "$object"
printf 'x' | dd of="$object" bs=1 conv=notrunc status=none
if build_package --source-cache cache --output bad-cache build.kdl 2> bad-cache.log; then
    echo 'corrupt cached source unexpectedly succeeded' >&2
    exit 1
fi
grep -F 'source digest or length mismatch for input' bad-cache.log
test ! -e bad-cache

reviewed=$(sha build.kdl)
mkdir release
cp builder-public.der release/
write_policy() {
    cat > release/publish-policy.kdl <<POLICY
schema-version 1
publisher "canopa" {
    builder-key "builder-public.der"
    approve "demo/hello" version="1.0.0-0" sha256="$1"
}
POLICY
}
publish_bundle() {
    "$cps" publish-bundle --repo release --signing-key signing.der "$@"
}
expect_failure() {
    local expected=$1
    shift
    if "$@" > rejected.log 2>&1; then
        echo "unexpected success: $*" >&2
        exit 1
    fi
    grep -F "$expected" rejected.log
}
write_policy "$reviewed"
expect_failure 'publish policy requires an attested build bundle' "$cps" publish \
    --repo release --proto first/proto --signing-key signing.der first/manifest.kdl
expect_failure 'invalid builder attestation signature' publish_bundle \
    --builder-key other-public.der --recipe-sha256 "$reviewed" first
expect_failure 'build signer is not authorized by publish policy' publish_bundle \
    --builder-key other-public.der --recipe-sha256 "$reviewed" unapproved
expect_failure 'build recipe differs from the reviewed digest' publish_bundle \
    --builder-key builder-public.der --recipe-sha256 "$(sha unstable.kdl)" first
write_policy "$(sha unstable.kdl)"
expect_failure 'build recipe is not approved' publish_bundle \
    --builder-key builder-public.der --recipe-sha256 "$reviewed" first
write_policy "$reviewed"
cp -a first modified
printf 'x' | dd of=modified/proto/hello bs=1 conv=notrunc status=none
expect_failure 'attested payload digest mismatch' publish_bundle \
    --builder-key builder-public.der --recipe-sha256 "$reviewed" modified
test ! -e release/current

publish_bundle --builder-key builder-public.der --recipe-sha256 "$reviewed" first
first_catalog=$(cat release/current)
publish_bundle --builder-key builder-public.der --recipe-sha256 "$reviewed" second
mv release/publish-policy.kdl policy.saved
expect_failure 'protected repository is missing publish-policy.kdl' "$cps" publish \
    --repo release --proto first/proto --signing-key signing.der first/manifest.kdl
expect_failure 'protected repository is missing publish-policy.kdl' publish_bundle \
    --builder-key builder-public.der --recipe-sha256 "$reviewed" first
mv policy.saved release/publish-policy.kdl
test "$(cat release/current)" = "$first_catalog"
"$cps" repo attestation --repo release --publisher canopa --trust-key public.der \
    cps://canopa/demo/hello@1.0.0-0 > envelope.json
python3 - <<'PYTHON'
import base64
import json
from pathlib import Path

envelope = json.loads(Path('envelope.json').read_bytes())
assert envelope == json.loads(Path('first/attestation.json').read_bytes())
kind = envelope['payloadType'].encode('ascii')
payload = base64.b64decode(envelope['payload'], validate=True)
statement = json.loads(payload)
assert statement['_type'] == 'https://in-toto.io/Statement/v1'
assert statement['predicate']['identical_runs'] == 2
assert set(statement['predicate']['toolchain']['package_manifests']) == {'build/shell', 'build/probe'}
assert {subject['name'] for subject in statement['subject']} == {'manifest.kdl', 'proto/hello'}
Path('pae').write_bytes(b'DSSEv1 %d %s %d %s' % (len(kind), kind, len(payload), payload))
Path('signature').write_bytes(base64.b64decode(envelope['signatures'][0]['sig'], validate=True))
PYTHON
openssl pkeyutl -verify -pubin -keyform DER -inkey builder-public.der -rawin -in pae -sigfile signature

mkdir witnessed
cp builder-public.der other-public.der witnessed/
cat > witnessed/publish-policy.kdl <<POLICY
schema-version 1
publisher "canopa" minimum-builders=2 {
    builder-key "builder-public.der"
    builder-key "other-public.der"
    approve "demo/hello" version="1.0.0-0" sha256="$reviewed"
}
POLICY
publish_witnessed() {
    "$cps" publish-bundle --repo witnessed --signing-key signing.der --recipe-sha256 "$reviewed" "$@"
}
expect_failure 'requires 2 distinct build signers' publish_witnessed --builder-key builder-public.der first
expect_failure 'duplicate build signer' publish_witnessed --builder-key builder-public.der \
    --witness builder-public.der=second/attestation.json first
expect_failure 'invalid builder attestation signature' publish_witnessed --builder-key builder-public.der \
    --witness other-public.der=second/attestation.json first
expect_failure 'witness build statement differs' publish_witnessed --builder-key builder-public.der \
    --witness other-public.der=mismatch/attestation.json first
test ! -e witnessed/current
publish_witnessed --builder-key builder-public.der --witness other-public.der=unapproved/attestation.json first
witnessed_catalog=$(cat witnessed/current)
publish_witnessed --builder-key other-public.der --witness builder-public.der=first/attestation.json unapproved
test "$(cat witnessed/current)" = "$witnessed_catalog"
"$cps" repo attestation --repo witnessed --publisher canopa --trust-key public.der \
    cps://canopa/demo/hello@1.0.0-0 > witnessed.json
python3 - <<'PYTHON'
import base64
import hashlib
import json
import subprocess
from pathlib import Path

envelope = json.loads(Path('witnessed.json').read_bytes())
assert envelope['payload'] == json.loads(Path('envelope.json').read_bytes())['payload']
signatures = {entry['keyid']: entry['sig'] for entry in envelope['signatures']}
assert len(envelope['signatures']) == len(signatures) == 2
for path in ('builder-public.der', 'other-public.der'):
    keyid = hashlib.sha256(Path(path).read_bytes()[-32:]).hexdigest()
    Path('signature').write_bytes(base64.b64decode(signatures[keyid], validate=True))
    subprocess.run(['openssl', 'pkeyutl', '-verify', '-pubin', '-keyform', 'DER',
                    '-inkey', path, '-rawin', '-in', 'pae', '-sigfile', 'signature'], check=True)
PYTHON

"$cps" repo export --repo release --publisher canopa --trust-key public.der --output targets
test -f "targets/attestations-$(sha first/attestation.json)"
test ! -e targets/publish-policy.kdl
test ! -e targets/builder-public.der
"$cps" image-create --root image --repo release --publisher canopa --trust-key public.der
"$cps" plan --root image --output install.json install demo/hello
"$cps" apply --root image install.json
"$cps" verify --root image
test "$(cat image/hello)" = 'hello from pinned input 1700000000'
echo "Verified isolated build, reproducibility, signed provenance, and package installation"
