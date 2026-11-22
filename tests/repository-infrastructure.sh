#!/usr/bin/env bash
set -euo pipefail

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
export CPS_BIN=${CPS_BIN:-$project/target/debug/cps}
work=$(mktemp -d "${TMPDIR:-/tmp}/cps-infrastructure.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
workspace=$work/publisher
origin=$work/origin
replica=$work/replica
cps() { "$CPS_BIN" "$@"; }

# A new publisher: separate CPS and TUF keys, offline keys apart.
cps repo init --publisher canopa --output "$workspace"
[[ $(stat -c %a -- "$workspace") == 700 ]]
for key in "$workspace"/{offline/*.der,keys/catalog.der,keys/tuf/*.der}; do
    [[ $(stat -c %a -- "$key") == 600 ]]
done
[[ $(sha256sum "$workspace"/{offline/*.der,keys/catalog.der,keys/tuf/*.der} | cut -d ' ' -f 1 | sort -u | wc -l) == 7 ]]
if cps repo init --publisher canopa --output "$workspace"; then exit 1; fi

mkdir -p "$work/proto/usr/share"
publish() {
    printf '%s\n' "$1" > "$work/proto/usr/share/example"
    cps manifest generate --proto "$work/proto" --publisher canopa --version "$1.0.0-0" \
        demo/example > "$work/manifest.kdl"
    cps publish manifests --repo "$workspace/repo" --proto "$work/proto" \
        --key "$workspace/keys/catalog.der" "$work/manifest.kdl"
}

# The origin web root, with the publisher's signed list of public mirrors.
cat > "$workspace/mirrors.kdl" <<'EOF'
schema-version 1 kind="mirror-list"
mirror "https://mirror.invalid/canopa/" region="eu"
EOF
cps repo nginx-config --host origin.example --tls-cert /etc/tls/cert.pem \
    --tls-key /etc/tls/key.pem --web-root "$origin" > "$work/origin.conf"
grep -F 'location = /metadata/timestamp.json' "$work/origin.conf"
grep -F "alias $origin/metadata/;" "$work/origin.conf"
# Several publishers share one server, each under its own prefix.
cps repo nginx-config --host origin.example --tls-cert /etc/tls/cert.pem \
    --tls-key /etc/tls/key.pem --web-root "/canopa=$origin" --web-root "/runtime=$work/runtime" > "$work/shared.conf"
[[ $(grep -c '^server {' "$work/shared.conf") == 1 ]]
grep -F 'location = /canopa/metadata/timestamp.json' "$work/shared.conf"
grep -F "alias $work/runtime/targets/;" "$work/shared.conf"
mkdir -m 0755 "$origin"
if cps repo deploy --workspace "$workspace" -o "$origin"; then exit 1; fi
publish 1
mv -- "$workspace/offline" "$work/offline"
(umask 077 && cps repo deploy --workspace "$workspace" -o "$origin")
[[ -f $workspace/repo/.tuf-publication.json && $(stat -c %a -- "$workspace/repo/.tuf-publication.json") == 600 ]]
hash=$(jq -er '.signed.targets.mirrors.hashes.sha256' "$origin/metadata/1.targets.json")
cmp "$workspace/mirrors.kdl" "$origin/targets/$hash.mirrors"

# A full replica, verified before it is served, readable by the web server
# whatever the umask of the service.
(umask 077 && cps repo replicate --tuf-root "$workspace/tuf-root.json" -o "$replica" "$origin")
diff -r -- "$origin" "$replica"
[[ -z $(find "$origin" "$replica" \( -type f ! -perm -0444 \) -o \( -type d ! -perm -0555 \)) ]]
[[ -f $work/.replica.replica-verified.json ]]
cp -- "$replica/metadata/timestamp.json" "$work/first-timestamp.json"
target=$(find "$replica/targets" -type f -name '*.trust-root.json' -print -quit)
inode=$(stat -c %i -- "$target")
publish 2
cps repo deploy --workspace "$workspace" -o "$origin" --timestamp-lifetime 1h
cps repo replicate --tuf-root "$workspace/tuf-root.json" -o "$replica" "$origin"
cmp "$origin/metadata/timestamp.json" "$replica/metadata/timestamp.json"
# Unchanged files are reused, and the previous release stays for clients mid-download.
[[ $(stat -c %i -- "$target") == "$inode" ]]
[[ -f $replica/metadata/1.snapshot.json ]]
# Nothing new: the replica is left alone, previous release included.
cps repo replicate --tuf-root "$workspace/tuf-root.json" -o "$replica" "$origin" | grep -F 'already serves'
[[ -f $replica/metadata/1.snapshot.json ]]
# A refresh re-signs the deployed catalog even after an undeployed publish.
deployed=$(cat "$workspace/repo/current")
publish 3
if cps repo refresh --workspace "$workspace" -o "$origin" --catalog "$(cat "$workspace/repo/current")"; then exit 1; fi
cps repo refresh --workspace "$workspace" -o "$origin" --catalog "$deployed" | grep -F "for catalog $deployed"
cps repo replicate --tuf-root "$workspace/tuf-root.json" -o "$replica" "$origin"
cmp "$origin/metadata/timestamp.json" "$replica/metadata/timestamp.json"
jq -e '.signed.version == 3' "$replica/metadata/timestamp.json"
# A re-signed timestamp of the same release keeps the release before it too.
[[ -f $replica/metadata/1.snapshot.json ]]

# The replica refuses a rolled-back source and keeps serving its release.
cp -- "$origin/metadata/timestamp.json" "$work/current-timestamp.json"
cp -- "$work/first-timestamp.json" "$origin/metadata/timestamp.json"
if cps repo replicate --tuf-root "$workspace/tuf-root.json" -o "$replica" "$origin" 2> "$work/rollback.log"; then exit 1; fi
grep -F 'older than the replica' "$work/rollback.log"
jq -e '.signed.version == 3' "$replica/metadata/timestamp.json"
cp -- "$work/current-timestamp.json" "$origin/metadata/timestamp.json"

# A damaged source never becomes a new replica.
damaged=$work/damaged-origin
cp -a -- "$origin" "$damaged"
for pointer in "$damaged"/targets/*.current; do
    printf 'damaged\n' > "$pointer"
done
if cps repo replicate --tuf-root "$workspace/tuf-root.json" -o "$work/bad-replica" "$damaged" 2> "$work/damaged.log"; then exit 1; fi
grep -F 'does not match its signed length and digest' "$work/damaged.log"
[[ ! -e $work/bad-replica ]]

# A damaged replica file of the same length is fetched again, not reused.
printf 'X' | dd of="$target" bs=1 seek=0 count=1 conv=notrunc status=none
cps repo deploy --workspace "$workspace" -o "$origin"
cps repo replicate --tuf-root "$workspace/tuf-root.json" -o "$replica" "$origin"
cmp -- "$origin/targets/${target##*/}" "$target"

# A client fails over from an unreachable mirror and keeps the signed mirror list.
current=$(cat "$workspace/repo/current")
cps repo sync --repo "$work/cache" --tuf-root "$workspace/tuf-root.json" \
    --trust-root "$workspace/repo/trust/1.json" --url "file://$work/no-origin/" \
    --fallback-url "file://$replica/" --require-catalog "$current" 2> "$work/failover.log"
grep -F 'Trying the next mirror' "$work/failover.log"
cmp "$workspace/mirrors.kdl" "$work/cache/.tuf/signed-mirrors"

# Mirror lists and ranking for a publisher configured in an image.
cps image create --root "$work/image" --repo "$work/cache" --trust-root "$workspace/repo/trust/1.json"
mkdir -p "$work/image/etc/cps/mirrors"
cat > "$work/image/etc/cps/mirrors/canopa.kdl" <<EOF
schema-version 1 kind="mirror-list"
mirror "file://$replica/"
EOF
cps publisher mirror list --root "$work/image" --json > "$work/mirrors.json"
# The local list's mirror comes first; a duplicate of it in the configured source is dropped.
jq -e --arg replica "file://$replica/" --arg missing "file://$work/no-origin/" '
    .[0].mirrors | map([.url, .source]) == [
        [$replica, "local"],
        [$missing, "configured"],
        ["https://mirror.invalid/canopa/", "signed"]
    ]
' "$work/mirrors.json"
cps publisher mirror rank --root "$work/image" --json > "$work/ranked.json"
jq -e --arg replica "file://$replica/" '
    .[0].mirrors[0].url == $replica and .[0].mirrors[0].probe.status == "current" and
    ([.[0].mirrors[] | select(.url == "https://mirror.invalid/canopa/") | .probe.status] == ["unreachable"])
' "$work/ranked.json"
cps publisher mirror list --root "$work/image" | grep -F 'current, timestamp 4'
# A publisher that cannot be ranked is reported, and the command fails.
mv -- "$work/cache/.tuf/trusted/root.json" "$work/trusted-root.json"
if cps publisher mirror rank --root "$work/image" > "$work/rank-failed.log" 2>&1; then exit 1; fi
grep -F 'Ranking failed: ' "$work/rank-failed.log"
grep -F 'could not rank the mirrors of canopa' "$work/rank-failed.log"
mv -- "$work/trusted-root.json" "$work/cache/.tuf/trusted/root.json"

# A LAN read-through cache needs no CPS on its host.
cps repo nginx-config --host cache.example --tls-cert /etc/tls/cert.pem --tls-key /etc/tls/key.pem \
    --cache-dir /var/cache/nginx/canopa --upstream https://origin.example/canopa --max-size 5g > "$work/cache.conf"
grep -F 'max_size=5g' "$work/cache.conf"
grep -F 'proxy_cache_key $uri;' "$work/cache.conf"
grep -F 'proxy_ssl_verify on;' "$work/cache.conf"
grep -F 'proxy_pass https://origin.example/canopa/metadata/timestamp.json;' "$work/cache.conf"
cps repo nginx-config --host cache.example --tls-cert /etc/tls/cert.pem --tls-key /etc/tls/key.pem \
    --cache-dir /var/cache/nginx/cps --upstream /canopa=https://origin.example/canopa \
    --upstream /runtime=https://origin.example/runtime > "$work/caches.conf"
grep -F 'location /runtime/targets/ {' "$work/caches.conf"
[[ $(grep -c '^proxy_cache_path' "$work/caches.conf") == 1 ]]
printf '%s\n' 'Verified publisher setup, deployment, replicas, client failover, and mirror ranking'
