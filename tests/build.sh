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
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <sys/resource.h>
#include <sys/utsname.h>
#include <sys/xattr.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "cgroup-tasks") == 0) {
        pid_t children[32];
        unsigned count = 0;
        for (; count < sizeof(children) / sizeof(children[0]); count++) {
            pid_t child = fork();
            if (child < 0) {
                assert(errno == EAGAIN);
                break;
            }
            if (child == 0)
                for (;;) pause();
            children[count] = child;
        }
        assert(count > 0 && count < sizeof(children) / sizeof(children[0]));
        puts("build task ceiling reached");
        fflush(stdout);
        sleep(3);
        for (unsigned i = 0; i < count; i++) {
            assert(kill(children[i], SIGKILL) == 0);
            assert(waitpid(children[i], NULL, 0) == children[i]);
        }
        return 0;
    }
    if (argc == 2 && strcmp(argv[1], "cgroup-memory") == 0) {
        const size_t size = 128 * 1024 * 1024;
        volatile unsigned char *bytes = malloc(size);
        assert(bytes != NULL);
        for (size_t i = 0; i < size; i += 4096)
            bytes[i] = 1;
        return 0;
    }
    if (argc == 3 && strcmp(argv[1], "runaway") == 0) {
        pid_t child = fork();
        assert(child >= 0);
        if (child == 0) {
            assert(setsid() >= 0);
            assert(signal(SIGTERM, SIG_IGN) != SIG_ERR);
            printf("detached build child started %s\n", argv[2]);
            fflush(stdout);
        }
        for (;;) pause();
    }
    if (argc == 2 && strcmp(argv[1], "limits") == 0) {
        const char *scratch[] = {"/build", "/tmp", "/run"};
        for (unsigned i = 0; i < sizeof(scratch) / sizeof(scratch[0]); i++) {
            struct statvfs stats;
            assert(statvfs(scratch[i], &stats) == 0);
            assert(stats.f_blocks * stats.f_frsize == 1048576);
        }
        struct rlimit limit;
        assert(getrlimit(RLIMIT_FSIZE, &limit) == 0);
        assert(limit.rlim_cur == 4096 && limit.rlim_max == 4096);
        assert(signal(SIGXFSZ, SIG_IGN) != SIG_ERR);
        int fd = open("/out/too-large", O_CREAT | O_WRONLY, 0644);
        assert(fd >= 0);
        assert(ftruncate(fd, 4097) < 0 && errno == EFBIG);
        puts("worker scratch and file limits verified");
        return 0;
    }
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
    assert(getxattr("/usr/bin/probe", "security.capability", NULL, 0) < 0 &&
           (errno == ENODATA || errno == ENOTSUP));
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
    install -D -m755 probe proto/usr/bin/probe
    printf '    file "usr/bin/probe" mode="0755" uid=0 gid=0 capabilities="cap_net_raw=ep"\n'
    printf '    hardlink "usr/bin/probe-alias" target="usr/bin/probe"\n'
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
    hardlink "hello-alias" target="hello"
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

recipe '    test /usr/bin/probe -ef /usr/bin/probe-alias
    probe-alias
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

# Resolve the probe from a separately signed publisher and its shell from the
# primary catalog. Each repository retains a newer, unusable probe as well.
install -m755 probe proto/usr/bin/probe
# Give the companion ELF a distinct payload digest while keeping it executable.
printf 'tools publisher\n' >> proto/usr/bin/probe
sed 's/publisher="canopa"/publisher="tools"/' probe.kdl > tools-probe.kdl
"$cps" publish --repo tools-repo --proto proto --signing-key other.der tools-probe.kdl
tools_catalog=$(cat tools-repo/current)
printf 'wrong companion executable\n' > proto/usr/bin/probe
sed 's/version="1.0.0-0" {/version="2.0.0-0" {/' tools-probe.kdl > tools-newer.kdl
"$cps" publish --repo tools-repo --proto proto --signing-key other.der tools-newer.kdl
"$cps" repo withdraw --repo tools-repo --signing-key other.der cps://tools/build/probe@1.0.0-0
fingerprint() { tail -c 32 -- "$1" | sha256sum | cut -d' ' -f1; }
awk -v primary="$catalog" -v companion="$tools_catalog" \
    -v primary_key="$(fingerprint public.der)" -v companion_key="$(fingerprint other-public.der)" '
    /^    toolchain / {
        print "    toolchain solver=\"sat-v1\" {"
        printf "        publisher \"canopa\" catalog=\"%s\" key-sha256=\"%s\"\n", primary, primary_key
        printf "        publisher \"tools\" catalog=\"%s\" key-sha256=\"%s\"\n", companion, companion_key
        next
    }
    /^        package "build\/probe"/ {
        print "        package \"cps://tools/build/probe@1.0.0-0\""
        next
    }
    {print}
' build.kdl > multi-build.kdl
cat > toolchain.kdl <<'CONFIG'
schema-version 1
publisher "tools" {
    repository "tools-repo"
    trust-key "other-public.der"
}
publisher "canopa" {
    repository "repo"
    trust-key "public.der"
}
CONFIG
"$cps" build --toolchain-config toolchain.kdl --source-cache cache \
    --builder-signing-key builder.der --check-reproducible --output multi multi-build.kdl
cmp first/proto/hello multi/proto/hello
"$cps" publish-bundle --repo multi-output --signing-key signing.der --builder-key builder-public.der \
    --recipe-sha256 "$(sha multi-build.kdl)" multi
"$cps" repo attestation --repo multi-output --publisher canopa --trust-key public.der \
    cps://canopa/demo/hello@1.0.0-0 > multi-evidence.json
python3 - "$catalog" "$tools_catalog" <<'PYTHON'
import base64
import json
import sys

with open("multi-evidence.json") as f:
    evidence = json.loads(base64.b64decode(json.load(f)["payload"]))
assert evidence["predicateType"] == "urn:canopa:cps:build-provenance:v3"
toolchain = evidence["predicate"]["toolchain"]
assert [(p["name"], p["catalog_sha256"]) for p in toolchain["publishers"]] == [
    ("canopa", sys.argv[1]), ("tools", sys.argv[2])
]
assert toolchain["package_manifests"]["build/shell"]["publisher"] == "canopa"
assert toolchain["package_manifests"]["build/probe"]["publisher"] == "tools"
assert toolchain["package_manifests"]["build/probe"]["version"] == "1.0.0-0"
PYTHON
sed 's/other-public.der/public.der/' toolchain.kdl > wrong-key.kdl
if "$cps" build --toolchain-config wrong-key.kdl --source-cache cache \
    --output rejected-multi multi-build.kdl > rejected-multi.log 2>&1; then
    echo 'Accepted a toolchain key outside the recipe pins' >&2
    exit 1
fi
grep -F 'toolchain key digest mismatch for tools' rejected-multi.log
test ! -e rejected-multi

cat > limits.kdl <<'KDL'
schema-version 1
worker timeout-seconds=2 scratch-bytes=1048576 file-bytes=4096 output-bytes=4 log-bytes=128
KDL
recipe '    probe limits
    printf "%2048s\n" "log output"
    printf "hello" > "$DESTDIR/hello"' > limits.build.kdl
if build_package --sources sources --worker-config limits.kdl --output limited limits.build.kdl 2> limits.log; then
    echo 'oversized bundle unexpectedly succeeded' >&2
    exit 1
fi
grep -F 'worker scratch and file limits verified' limits.log
grep -F 'build log limit reached' limits.log
grep -F 'declared outputs exceed 4 bytes at hello' limits.log
test "$(wc -c < limits.log)" -lt 512
test ! -e limited

token="cps-runaway-$$"
recipe "    exec probe runaway $token" > runaway.kdl
if timeout 15 "$cps" build --repo repo --publisher canopa --trust-key public.der --sources sources \
    --worker-config limits.kdl --output runaway runaway.kdl 2> runaway.log; then
    echo 'runaway build unexpectedly succeeded' >&2
    exit 1
fi
grep -F "detached build child started $token" runaway.log
grep -F 'isolated build exceeded its 2-second deadline' runaway.log
test ! -e runaway
if pgrep -f -- "^probe runaway $token$"; then
    echo 'sandbox child survived the build deadline' >&2
    exit 1
fi

if [[ -n ${CPS_TEST_CGROUP_ROOT:-} ]]; then
    cgroup_config() {
        cat <<CGROUP
schema-version 1
worker timeout-seconds=$1 {
    cgroup "$2" cpu-percent=50 memory-bytes=67108864 tasks=16
}
CGROUP
    }
    cgroup_config 15 "$CPS_TEST_CGROUP_ROOT" > cgroup.kdl
    cgroup_config 2 "$CPS_TEST_CGROUP_ROOT" > cgroup-timeout.kdl
    cgroup_config 2 "$work" > invalid-cgroup.kdl
    recipe '    probe cgroup-tasks
    printf "bounded" > "$DESTDIR/hello"' > cgroup.build.kdl
    build_package --sources sources --worker-config cgroup.kdl --output bounded cgroup.build.kdl > cgroup.log 2>&1 &
    job=$!
    for _ in {1..100}; do
        grep -qF 'build task ceiling reached' cgroup.log && break
        kill -0 "$job" 2>/dev/null || { cat cgroup.log >&2; exit 1; }
        sleep 0.05
    done
    grep -F 'build task ceiling reached' cgroup.log
    job_group=$(find "$CPS_TEST_CGROUP_ROOT" -maxdepth 1 -type d -name '.cps-build-*')
    test -n "$job_group"
    test "$(cat "$job_group/cpu.max")" = '50000 100000'
    test "$(cat "$job_group/memory.max")" = 67108864
    test "$(cat "$job_group/memory.swap.max")" = 0
    test "$(cat "$job_group/pids.current")" = 16
    grep -E '^max [1-9][0-9]*$' "$job_group/pids.events"
    wait "$job"
    test ! -e "$job_group"
    test "$(cat bounded/proto/hello)" = bounded

    recipe '    exec probe cgroup-memory' > oom.kdl
    if build_package --sources sources --worker-config cgroup.kdl --output oom oom.kdl 2> oom.log; then
        echo 'build exceeded its cgroup memory limit' >&2
        exit 1
    fi
    grep -F 'isolated build exceeded its cgroup memory limit' oom.log
    test ! -e oom
    if build_package --sources sources --worker-config cgroup-timeout.kdl --output cgroup-runaway runaway.kdl 2> cgroup-runaway.log; then
        echo 'runaway build unexpectedly succeeded inside a cgroup' >&2
        exit 1
    fi
    grep -F 'isolated build exceeded its 2-second deadline' cgroup-runaway.log
    test ! -e cgroup-runaway
    test -z "$(find "$CPS_TEST_CGROUP_ROOT" -maxdepth 1 -name '.cps-build-*')"
    if build_package --sources sources --worker-config invalid-cgroup.kdl --output invalid-cgroup runaway.kdl 2> invalid-cgroup.log; then
        echo 'build ran with an invalid cgroup' >&2
        exit 1
    fi
    grep -F 'is not a cgroup v2 directory' invalid-cgroup.log
    test ! -e invalid-cgroup
fi

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
import hashlib
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
subjects = {subject['name']: subject for subject in statement['subject']}
assert set(subjects) == {'manifest.kdl', 'published-manifest.kdl', 'proto/hello'}
catalog = json.loads((Path('release/catalogs') / Path('release/current').read_text().strip()).read_bytes())
manifest_hash = catalog['catalog']['entries']['cps://canopa/demo/hello@1.0.0-0']
manifest = (Path('release/manifests') / manifest_hash).read_bytes()
published = subjects['published-manifest.kdl']
assert published['digest']['sha256'] == manifest_hash == hashlib.sha256(manifest).hexdigest()
assert published['annotations']['cps:size'] == len(manifest)
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
"$cps" image-create --root image --repo witnessed --publisher canopa --trust-key public.der
"$cps" plan --root image --output install.json install demo/hello
"$cps" apply --root image install.json
"$cps" verify --root image --pristine
test image/hello -ef image/hello-alias
test "$(cat image/hello-alias)" = 'hello from pinned input 1700000000'

openssl genpkey -algorithm ED25519 -outform DER -out successor.der
"$cps" repo rotate-key --repo witnessed --publisher canopa --base "$witnessed_catalog" \
    --signing-key signing.der --new-signing-key successor.der
rotated_catalog=$(cat witnessed/current)

# Build the two packages in a coordinated upgrade on both workers.
printf 'hello from pinned input\n' > sources/input
cat > manifest.kdl <<MANIFEST
schema-version 1
package "demo/hello" publisher="canopa" version="2.0.0-0" {
    require "library/platform" version="=1.0.0-0"
    file "hello" mode="0644" uid=$uid gid=$gid
    hardlink "hello-alias" target="hello"
}
MANIFEST
recipe '    printf "hello 2\n" > "$DESTDIR/hello"' > hello2.kdl
build_package --sources sources --builder-signing-key builder.der --check-reproducible --output hello2-a hello2.kdl
build_package --sources sources --builder-signing-key other.der --check-reproducible --output hello2-b hello2.kdl
build_package --sources sources --builder-signing-key signing.der --check-reproducible --output hello2-retired hello2.kdl

cat > manifest.kdl <<MANIFEST
schema-version 1
package "library/platform" publisher="canopa" version="1.0.0-0" {
    file "platform" mode="0644" uid=$uid gid=$gid
}
MANIFEST
recipe '    printf "platform\n" > "$DESTDIR/platform"' > platform.kdl
build_package --sources sources --builder-signing-key builder.der --check-reproducible --output platform-a platform.kdl
build_package --sources sources --builder-signing-key other.der --check-reproducible --output platform-b platform.kdl

hello2_reviewed=$(sha hello2.kdl)
platform_reviewed=$(sha platform.kdl)
cp public.der witnessed/retired-publisher.der
cat > witnessed/publish-policy.kdl <<POLICY
schema-version 1
publisher "canopa" minimum-builders=2 {
    builder-key "builder-public.der"
    builder-key "other-public.der"
    builder-key "retired-publisher.der"
    approve "demo/hello" version="2.0.0-0" sha256="$hello2_reviewed"
    approve "library/platform" version="1.0.0-0" sha256="$platform_reviewed"
    profile "base" architecture="$arch" upgrade-from="$witnessed_catalog" {
        package "demo/hello"
    }
}
POLICY
expect_failure 'a retired publisher key cannot sign builds' "$cps" publish-bundle \
    --repo witnessed --signing-key successor.der --builder-key public.der \
    --recipe-sha256 "$hello2_reviewed" --witness other-public.der=hello2-b/attestation.json hello2-retired
expect_failure 'checking release profile base' "$cps" publish-bundle \
    --repo witnessed --signing-key successor.der --builder-key builder-public.der \
    --recipe-sha256 "$hello2_reviewed" --witness other-public.der=hello2-b/attestation.json hello2-a
test "$(cat witnessed/current)" = "$rotated_catalog"

cat > wave.kdl <<WAVE
schema-version 1
release "canopa" base="$rotated_catalog" {
    obsolete "cps://canopa/demo/hello@1.1.0-0"
    bundle "cps://canopa/demo/hello@2.0.0-0" path="hello2-a" builder-key="builder-public.der" recipe-sha256="$hello2_reviewed" {
        witness "hello2-b/attestation.json" key="other-public.der"
    }
    bundle "cps://canopa/library/platform@1.0.0-0" path="platform-a" builder-key="builder-public.der" recipe-sha256="$platform_reviewed" {
        witness "platform-b/attestation.json" key="other-public.der"
    }
}
WAVE
publish_wave() {
    "$cps" repo publish-release --repo witnessed --signing-key successor.der "$1"
}
sed '/witness "platform-b\/attestation.json"/d' wave.kdl > incomplete.kdl
expect_failure 'requires 2 distinct build signers' publish_wave incomplete.kdl
test "$(cat witnessed/current)" = "$rotated_catalog"
test -z "$("$cps" repo withdrawals --repo witnessed --publisher canopa --trust-key public.der)"

cp -a witnessed repeat
publish_wave wave.kdl
wave_catalog=$(cat witnessed/current)
test "$wave_catalog" != "$rotated_catalog"
expect_failure 'release base is stale' publish_wave wave.kdl
"$cps" repo publish-release --repo repeat --signing-key successor.der wave.kdl
test "$(cat repeat/current)" = "$wave_catalog"
test "$(find witnessed/catalogs -type f | wc -l)" -eq 3
"$cps" repo withdrawals --repo witnessed --publisher canopa --trust-key public.der \
    | grep -F 'cps://canopa/demo/hello@1.0.0-0'
test "$("$cps" repo obsoletions --repo witnessed --publisher canopa --trust-key public.der)" = \
    'cps://canopa/demo/hello@1.1.0-0'
"$cps" repo attestation --repo witnessed --publisher canopa --trust-key public.der \
    cps://canopa/demo/hello@1.0.0-0 | cmp - witnessed.json
expect_failure 'checking release profile base' "$cps" repo withdraw \
    --repo witnessed --signing-key successor.der cps://canopa/library/platform@1.0.0-0
mv witnessed/publish-policy.kdl gate.saved
expect_failure 'protected repository is missing publish-policy.kdl' "$cps" repo withdraw \
    --repo witnessed --signing-key successor.der cps://canopa/library/platform@1.0.0-0
sed '/profile "base"/,+2d' gate.saved > witnessed/publish-policy.kdl
expect_failure 'requires release composition profiles' "$cps" repo withdraw \
    --repo witnessed --signing-key successor.der cps://canopa/library/platform@1.0.0-0
mv gate.saved witnessed/publish-policy.kdl
test "$(cat witnessed/current)" = "$wave_catalog"
"$cps" plan --root image --output wave.json update
"$cps" apply --root image wave.json
"$cps" verify --root image --pristine
test "$(cat image/hello-alias)" = 'hello 2'
test "$(cat image/platform)" = 'platform'
test image/hello -ef image/hello-alias
echo "Verified isolated build, reproducibility, signed provenance, and package installation"
