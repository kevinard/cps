#!/usr/bin/env bash
set -euo pipefail

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=${CPS_TEST_BINARY:-"$project/target/debug/cps"}
work=$(mktemp -d "${TMPDIR:-/tmp}/cps-source-test.XXXXXX")
server=
cleanup() {
    if [[ -n $server ]]; then
        kill "$server" 2>/dev/null || :
        wait "$server" 2>/dev/null || :
    fi
    rm -rf -- "$work"
}
trap cleanup EXIT
cd "$work"

openssl req -x509 -newkey rsa:2048 -noenc -days 1 -subj '/CN=CPS source test CA' \
    -addext 'basicConstraints=critical,CA:TRUE' -keyout ca.key -out ca.pem 2> openssl.log
openssl req -newkey rsa:2048 -noenc -subj '/CN=localhost' \
    -keyout server.key -out server.csr 2>> openssl.log
printf 'subjectAltName=IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' > extensions
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 1 \
    -extfile extensions -out server.pem 2>> openssl.log

python3 - <<'PYTHON' > server.log 2>&1 &
import http.server
import ssl
from pathlib import Path


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def do_GET(self):
        if self.path == '/redirect':
            self.send_response(302)
            self.send_header('Location', '/payload')
            self.send_header('Content-Length', '0')
            self.end_headers()
            return
        body = {'/payload': b'abc', '/short': b'ab', '/long': b'abcd', '/bad': b'xyz'}[self.path]
        self.send_response(200)
        chunked = self.path in ('/short', '/long')
        if chunked:
            self.send_header('Transfer-Encoding', 'chunked')
        else:
            self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(b'%x\r\n%s\r\n0\r\n\r\n' % (len(body), body) if chunked else body)


with http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler) as server:
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain('server.pem', 'server.key')
    server.socket = context.wrap_socket(server.socket, server_side=True)
    Path('port').write_text(str(server.server_port))
    server.serve_forever()
PYTHON
server=$!
for _ in {1..100}; do
    [[ -s port ]] && break
    kill -0 "$server" 2>/dev/null || { cat server.log >&2; exit 1; }
    sleep 0.05
done
test -s port
export SSL_CERT_FILE="$work/ca.pem" NO_PROXY=127.0.0.1
base="https://127.0.0.1:$(cat port)"
printf abc > expected
digest=$(sha256sum expected | cut -d' ' -f1)
recipe() {
    cat <<RECIPE
schema-version 1
build architecture="x86_64" epoch=1700000000 {
    toolchain "$digest" {
        package "build/base"
    }
    manifest "manifest.kdl" sha256="$digest"
    ${2:-}
    source "nested/input" sha256="$digest" size=3$1
    script "true"
}
RECIPE
}
fetch() {
    "$cps" source fetch --cache "$1" "$2"
}
reject() {
    local path=$1 error=$2
    recipe " url=\"$base/$path\"" > "$path.kdl"
    if fetch "cache-$path" "$path.kdl" > reject.log 2>&1; then
        echo "unexpected success for $path" >&2
        exit 1
    fi
    grep -F "$error" reject.log
    test -z "$(ls -A "cache-$path/sha256")"
}

alias_source="source \"alias\" sha256=\"$digest\" size=3"
recipe " url=\"$base/payload\"" "$alias_source" > good.kdl
fetch cache good.kdl &
first=$!
fetch cache good.kdl &
second=$!
wait "$first"
wait "$second"
cmp expected "cache/sha256/$digest"
test "$(stat -c %a "cache/sha256/$digest")" = 444
test "$(ls "cache/sha256")" = "$digest"

reject bad 'source digest differs from its pin'
reject short 'source is shorter than its pinned size'
reject long 'source exceeded its pinned size'
reject redirect 'expected HTTP 200'

mkdir -p mixed/nested
printf patch > mixed/fix.patch
patch_digest=$(sha256sum mixed/fix.patch | cut -d' ' -f1)
recipe " url=\"$base/payload\"" "source \"fix.patch\" sha256=\"$patch_digest\" size=5" > mixed.kdl
"$cps" source fetch --cache mixed-cache --from mixed mixed.kdl
cmp mixed/fix.patch "mixed-cache/sha256/$patch_digest"
cmp expected "mixed-cache/sha256/$digest"
printf xyz > mixed/nested/input
if "$cps" source fetch --cache mixed-rejected --from mixed mixed.kdl > reject.log 2>&1; then
    echo 'a corrupt local input fell back to the upstream URL' >&2
    exit 1
fi
grep -F 'source digest or length mismatch for nested/input' reject.log
test ! -e "mixed-rejected/sha256/$digest"

kill "$server"
wait "$server" 2>/dev/null || :
server=
fetch cache good.kdl
mkdir -p local/nested
cp expected local/nested/input
recipe '' "$alias_source" > local.kdl
"$cps" source import --cache imported --from local local.kdl
cmp expected "imported/sha256/$digest"
chmod u+w "imported/sha256/$digest"
printf xyz > "imported/sha256/$digest"
if fetch imported local.kdl > reject.log 2>&1; then
    echo 'corrupt cache object accepted' >&2
    exit 1
fi
grep -F 'differs from its pin' reject.log

echo 'Verified HTTPS source fetching, cache races, and offline import'
