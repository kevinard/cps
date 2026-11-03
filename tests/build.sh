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
    toolchain "$catalog" {
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
    "$cps" build --repo repo --publisher canopa --trust-key public.der --sources sources "$@"
}
build_package --check-reproducible --output first build.kdl
(umask 077; build_package --check-reproducible --output second build.kdl)
diff -r first second
test ! -e first/proto/unlisted
test "$(stat -c '%Y %a' first/proto/hello)" = "1700000000 644"
test "$(stat -c '%Y %a' second/proto/hello)" = "1700000000 644"

recipe '    probe unstable > "$DESTDIR/hello"' > unstable.kdl
if build_package --check-reproducible --output unstable unstable.kdl 2> unstable.log; then
    echo "nondeterministic build unexpectedly succeeded" >&2
    exit 1
fi
grep -F 'build is not reproducible for hello' unstable.log
test ! -e unstable

recipe '    probe symlink' > symlink.kdl
if build_package --output symlink symlink.kdl 2> symlink.log; then
    echo "symlink build output unexpectedly succeeded" >&2
    exit 1
fi
grep -F 'opening declared build output hello' symlink.log
test ! -e symlink

# A same-length modification must fail digest verification before the sandbox starts.
printf 'x' | dd of=sources/input bs=1 conv=notrunc status=none
if build_package --output bad build.kdl 2> bad.log; then
    echo "corrupt source unexpectedly succeeded" >&2
    exit 1
fi
grep -F 'source digest or length mismatch for input' bad.log
test ! -e bad

"$cps" publish --repo repo --proto first/proto --signing-key signing.der first/manifest.kdl
first_catalog=$(cat repo/current)
"$cps" publish --repo repo --proto second/proto --signing-key signing.der second/manifest.kdl
test "$(cat repo/current)" = "$first_catalog"
"$cps" image-create --root image --repo repo --publisher canopa --trust-key public.der
"$cps" plan --root image --output install.json install demo/hello
"$cps" apply --root image install.json
"$cps" verify --root image
test "$(cat image/hello)" = 'hello from pinned input 1700000000'
echo "Verified isolated build, reproducibility, and package installation"
