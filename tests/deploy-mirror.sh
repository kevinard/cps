#!/usr/bin/env bash
set -euo pipefail
umask 077

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
export CPS_BIN=${CPS_BIN:-$project/target/debug/cps}
work=$(mktemp -d "${TMPDIR:-/tmp}/cps mirror.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
cd "$work"

mkdir -p keys public proto/usr/share
export CPS_MIRROR_DIR=$work/public CPS_MIRROR_TUF_ROOT=$work/root.json
export CPS_MIRROR_TUF_KEYS=$work/keys CPS_MIRROR_TRUST_ROOT=$work/trust-root.json
unset CPS_MIRROR_TUF_ACTIVE_ROOT
openssl genpkey -algorithm ED25519 -outform DER -out publisher-private.der
"$CPS_BIN" repo trust init --repo repo --publisher canopa --key publisher-private.der >/dev/null
cp repo/trust/1.json trust-root.json
signing_key=publisher-private.der
tuftool root init root.json
tuftool root expire root.json 'in 52 weeks'
for role in root targets snapshot timestamp; do
    tuftool root set-threshold root.json "$role" 1
    tuftool root gen-rsa-key root.json "$work/keys/$role.pem" --role "$role"
done
tuftool root sign root.json --key "$work/keys/root.pem"
mv -- keys/root.pem offline-root.pem

publish() {
    printf '%s\n' "$1" > proto/usr/share/example
    "$CPS_BIN" manifest generate --proto proto --publisher canopa --version "$1.0.0-0" \
        --uid "$(id -u)" --gid "$(id -g)" demo/example > manifest.kdl
    "$CPS_BIN" publish --repo repo --proto proto --signing-key "$signing_key" manifest.kdl
}

deploy() { "$project/ci/deploy-mirror.sh" repo canopa "$1"; }
exec {mirror_fd}< public
mirror_url=file:///proc/self/fd/$mirror_fd
sync_mirror() {
    "$CPS_BIN" repo sync --repo cache --root root.json --publisher canopa --trust-root trust-root.json \
        --metadata "$mirror_url/metadata/" --targets "$mirror_url/targets/" --require-catalog "$1"
}

publish 1
first=$(cat repo/current)
deploy "$first"
sync_mirror "$first"
# The trust-root.json target is the newest trust root of the deployed catalog.
trust_target() {
    local hash
    hash=$(jq -er '.signed.targets["trust-root.json"].hashes.sha256' "public/metadata/$1.targets.json")
    cmp "$2" "public/targets/$hash.trust-root.json"
}
hash=$(jq -er '.signed.targets.publisher.hashes.sha256' public/metadata/1.targets.json)
cmp <(printf canopa) "public/targets/$hash.publisher"
trust_target 1 repo/trust/1.json
cmp root.json <(jq -er '.tuf_root' public/publisher.json | base64 -d)
cmp trust-root.json <(jq -er '.trust_root' public/publisher.json | base64 -d)
jq -e '.schema_version == 1 and .kind == "publisher-bootstrap" and .publisher == "canopa"' public/publisher.json
cp public/publisher.json original-descriptor.json
cp public/metadata/timestamp.json original-timestamp.json

publish 2
second=$(cat repo/current)
if deploy "$first"; then
    echo 'A stale catalog was deployed' >&2
    exit 1
fi
mv -- keys/timestamp.pem timestamp.pem
cp -- keys/targets.pem keys/timestamp.pem
if deploy "$second"; then
    echo 'A release was deployed without the timestamp key' >&2
    exit 1
fi
cmp original-timestamp.json public/metadata/timestamp.json
mv -- timestamp.pem keys/timestamp.pem

# Simulate an interrupted publication, then forge the untrusted origin's version.
state=repo/.ci-tuf-state.json
jq '.version = 2 | .published = null' "$state" > state.json
install -m 0600 state.json "$state"
cp public/metadata/{1,2}.targets.json
cp public/metadata/{1,999999999999999}.targets.json
jq '.signed.version = 999999999999999' original-timestamp.json > public/metadata/timestamp.json
deploy "$second"
jq -e '.signed.version == 3' public/metadata/timestamp.json
sync_mirror "$second"
cmp public/metadata/{1,2}.targets.json
cmp original-descriptor.json public/publisher.json

CPS_RELEASE_DEPLOY="$project/ci/deploy-mirror.sh" CPS_RELEASE_REPO="$work/repo" \
    CPS_RELEASE_PUBLISHER=canopa bash "$project/ci/release.sh" deploy-current "$second"
[[ $(cat repo/.ci-deployed-catalog) == "$second" ]]
jq -e '.signed.version == 4' public/metadata/timestamp.json
sync_mirror "$second"

# An existing origin needs a trusted migration floor when signer state is missing.
mv "$state" saved-state.json
if deploy "$second"; then
    echo 'An existing origin was deployed without a trusted version floor' >&2
    exit 1
fi
CPS_MIRROR_INITIAL_VERSION=4 deploy "$second"
jq -e '.signed.version == 5' public/metadata/timestamp.json
sync_mirror "$second"
cp public/metadata/timestamp.json before-rotation.json

mkdir next-keys
cp root.json root-v2.json
tuftool root bump-version root-v2.json
for role in root targets snapshot timestamp; do
    old_key=$(jq -er --arg role "$role" '.signed.roles[$role].keyids[0]' root.json)
    tuftool root remove-key root-v2.json "$old_key"
    key=$work/next-keys/$role.pem
    if [[ $role == root ]]; then key=$work/next-offline-root.pem; fi
    tuftool root gen-rsa-key root-v2.json "$key" --role "$role"
done
tuftool root sign root-v2.json --key "$work/next-offline-root.pem"
export CPS_MIRROR_TUF_ACTIVE_ROOT=$work/root-v2.json CPS_MIRROR_TUF_KEYS=$work/next-keys
if deploy "$second"; then
    echo 'A root without the previous root signature was published' >&2
    exit 1
fi
[[ ! -e public/metadata/2.root.json ]]
cmp before-rotation.json public/metadata/timestamp.json

tuftool root sign root-v2.json --cross-sign root.json \
    --key "$work/offline-root.pem"
cp root-v2.json root-v3.json
tuftool root bump-version root-v3.json
tuftool root set-threshold root-v3.json root 2
mkdir quorum-keys
for member in b c; do
    tuftool root gen-rsa-key root-v3.json "$work/quorum-keys/$member.pem" --role root
done
tuftool root sign root-v3.json --key "$work/next-offline-root.pem" --key "$work/quorum-keys/b.pem"
if CPS_MIRROR_TUF_ACTIVE_ROOT=$work/root-v3.json deploy "$second"; then
    echo 'A root rotation skipped a version' >&2
    exit 1
fi

deploy "$second"
cmp root.json public/metadata/1.root.json
cmp root-v2.json public/metadata/2.root.json
sync_mirror "$second"
jq -e '.signed.version == 2' cache/.tuf/trusted/root.json
if CPS_MIRROR_TUF_ACTIVE_ROOT= CPS_MIRROR_TUF_KEYS=$work/keys deploy "$second"; then
    echo 'A previous root was accepted as active' >&2
    exit 1
fi

CPS_MIRROR_TUF_ACTIVE_ROOT=$work/root-v3.json deploy "$second"
cmp root.json public/metadata/1.root.json
cmp root-v2.json public/metadata/2.root.json
cmp root-v3.json public/metadata/3.root.json
cmp original-descriptor.json public/publisher.json
sync_mirror "$second"
jq -e '.signed.version == 3 and .signed.roles.root.threshold == 2 and
    (.signed.roles.root.keyids | length) == 3' cache/.tuf/trusted/root.json

# Revoke A. B and D meet the new quorum, but the old root also needs C.
cp public/metadata/timestamp.json before-revocation.json
revoked=$(jq -er '.signed.roles.root.keyids[0]' root-v2.json)
cp root-v3.json root-v4.json
tuftool root bump-version root-v4.json
tuftool root remove-key root-v4.json "$revoked"
replacement=$(tuftool root gen-rsa-key root-v4.json "$work/quorum-keys/d.pem" --role root)
tuftool root sign root-v4.json --key "$work/quorum-keys/b.pem" --key "$work/quorum-keys/d.pem"
if CPS_MIRROR_TUF_ACTIVE_ROOT=$work/root-v4.json deploy "$second" > revocation.log 2>&1; then
    echo 'A revocation with only one signature from the old root quorum was published' >&2
    exit 1
fi
rg -F 'Signature threshold of 2 not met for role root (1 valid signatures)' revocation.log
[[ ! -e public/metadata/4.root.json ]]
cmp before-revocation.json public/metadata/timestamp.json

tuftool root sign root-v4.json --cross-sign root-v3.json --key "$work/quorum-keys/c.pem"
CPS_MIRROR_TUF_ACTIVE_ROOT=$work/root-v4.json deploy "$second"
cmp root.json public/metadata/1.root.json
for root_version in 2 3 4; do
    cmp "root-v$root_version.json" "public/metadata/$root_version.root.json"
done
sync_mirror "$second"
jq -e --arg revoked "$revoked" --arg replacement "$replacement" '
    .signed.version == 4 and .signed.roles.root.threshold == 2 and
    (.signed.roles.root.keyids | length) == 3 and
    (.signed.roles.root.keyids | index($revoked)) == null and
    (.signed.roles.root.keyids | index($replacement)) != null
' cache/.tuf/trusted/root.json
"$CPS_BIN" repo sync --repo new-cache --root root.json --publisher canopa --trust-root trust-root.json \
    --metadata "$mirror_url/metadata/" --targets "$mirror_url/targets/" --require-catalog "$second"
jq -e '.signed.version == 4' new-cache/.tuf/trusted/root.json

# Routine refreshes need only the timestamp key. Take the other release keys offline.
export CPS_MIRROR_TUF_ACTIVE_ROOT=$work/root-v4.json
mkdir offline-keys
mv -- next-keys/targets.pem next-keys/snapshot.pem offline-keys/
refresh() { "$project/ci/deploy-mirror.sh" repo canopa "$1" refresh; }
before=$(jq -er '.signed.version' public/metadata/timestamp.json)
snapshot=$(jq -er '.signed.meta["snapshot.json"].version' public/metadata/timestamp.json)
if deploy "$second"; then
    echo 'A release was deployed without the targets key' >&2
    exit 1
fi
refresh "$second"
jq -e --argjson version "$((before + 1))" --argjson snapshot "$snapshot" \
    '.signed.version == $version and .signed.meta["snapshot.json"].version == $snapshot' \
    public/metadata/timestamp.json
sync_mirror "$second"
jq -e --argjson version "$((before + 1))" '.signed.version == $version' cache/.tuf/trusted/timestamp.json

# An external signer can hold the timestamp key.
mv -- next-keys/timestamp.pem timestamp-private.pem
openssl pkey -in timestamp-private.pem -pubout -out timestamp.pub
cat > next-keys/timestamp.signer.kdl <<SIGNER
schema-version 1 kind="signer"
command "$project/tests/support/openssl-signer.sh" "$work/timestamp-private.pem"
public-key "$work/timestamp.pub"
timeout-seconds 10
SIGNER
refresh "$second"
jq -e --argjson version "$((before + 2))" '.signed.version == $version' public/metadata/timestamp.json
sync_mirror "$second"

# A refresh never signs over a snapshot the origin altered.
snapshot_file=public/metadata/$snapshot.snapshot.json
cp -- "$snapshot_file" saved-snapshot.json
cp public/metadata/timestamp.json before-forgery.json
jq '.signed.version = 999' saved-snapshot.json > forged-snapshot.json
cat forged-snapshot.json > "$snapshot_file"
if refresh "$second" > forgery.log 2>&1; then
    echo 'A refresh signed over an altered snapshot' >&2
    exit 1
fi
rg -F 'differs from the signer record' forgery.log
cat saved-snapshot.json > "$snapshot_file"
cmp before-forgery.json public/metadata/timestamp.json

# A snapshot that would expire before the next timestamp needs the snapshot key.
if CPS_MIRROR_TIMESTAMP_LIFETIME='40 days' refresh "$second" > snapshot.log 2>&1; then
    echo 'A refresh outlived its snapshot without the snapshot key' >&2
    exit 1
fi
rg -F 'Supply a snapshot key source' snapshot.log
cp -- offline-keys/snapshot.pem next-keys/
CPS_MIRROR_TIMESTAMP_LIFETIME='40 days' refresh "$second"
jq -e --argjson version "$((before + 3))" \
    '.signed.version == $version and .signed.meta["snapshot.json"].version == $version' \
    public/metadata/timestamp.json
jq -e --argjson version "$((before + 3))" '.signed.version == $version' \
    "public/metadata/$((before + 3)).snapshot.json"
sync_mirror "$second"
"$CPS_BIN" repo sync --repo refreshed-cache --root root.json --publisher canopa --trust-root trust-root.json \
    --metadata "$mirror_url/metadata/" --targets "$mirror_url/targets/" --require-catalog "$second"

# The next content change uses the targets key again and supersedes the refreshes.
rm -- next-keys/timestamp.signer.kdl
mv -- timestamp-private.pem next-keys/timestamp.pem
mv -- offline-keys/targets.pem next-keys/
# Replace the catalog key. The descriptor keeps the pinned trust root, and the
# mirror serves the new version as its trust root target.
openssl genpkey -algorithm ED25519 -outform DER -out catalog-private.der
openssl pkey -inform DER -in catalog-private.der -pubout -outform DER -out catalog.der
"$CPS_BIN" repo trust rotate --repo repo --key publisher-private.der --catalog-key catalog.der >/dev/null
if "$CPS_BIN" publish --repo repo --proto proto --signing-key publisher-private.der manifest.kdl 2>/dev/null; then
    echo 'A revoked catalog key published a catalog' >&2
    exit 1
fi
signing_key=catalog-private.der
publish 3
third=$(cat repo/current)
deploy "$third"
jq -e --argjson version "$((before + 4))" '.signed.version == $version' public/metadata/timestamp.json
trust_target "$((before + 4))" repo/trust/2.json
cmp original-descriptor.json public/publisher.json
sync_mirror "$third"
printf '%s\n' 'Verified mirror publication, timestamp refresh, root rotation, 2-of-3 key revocation, and catalog key rotation'
