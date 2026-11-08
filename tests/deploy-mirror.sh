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
export CPS_MIRROR_TUF_KEYS=$work/keys CPS_MIRROR_PUBLISHER_KEY=$work/publisher.der
unset CPS_MIRROR_TUF_ACTIVE_ROOT
openssl genpkey -algorithm ED25519 -outform DER -out publisher-private.der
openssl pkey -inform DER -in publisher-private.der -pubout -outform DER -out publisher.der
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
    "$CPS_BIN" publish --repo repo --proto proto --signing-key publisher-private.der manifest.kdl
}

deploy() { "$project/ci/deploy-mirror.sh" repo canopa "$1"; }
exec {mirror_fd}< public
mirror_url=file:///proc/self/fd/$mirror_fd
sync_mirror() {
    "$CPS_BIN" repo sync --repo cache --root root.json --publisher canopa --trust-key publisher.der \
        --metadata "$mirror_url/metadata/" --targets "$mirror_url/targets/" --require-catalog "$1"
}

publish 1
first=$(cat repo/current)
deploy "$first"
sync_mirror "$first"
for target in publisher publisher.der; do
    hash=$(jq -er --arg target "$target" '.signed.targets[$target].hashes.sha256' public/metadata/1.targets.json)
    if [[ $target == publisher ]]; then
        cmp <(printf canopa) "public/targets/$hash.$target"
    else
        cmp publisher.der "public/targets/$hash.$target"
    fi
done
cmp root.json <(jq -er '.tuf_root' public/publisher.json | base64 -d)
cmp publisher.der <(jq -er '.trust_key' public/publisher.json | base64 -d)
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
"$CPS_BIN" repo sync --repo new-cache --root root.json --publisher canopa --trust-key publisher.der \
    --metadata "$mirror_url/metadata/" --targets "$mirror_url/targets/" --require-catalog "$second"
jq -e '.signed.version == 4' new-cache/.tuf/trusted/root.json
printf '%s\n' 'Verified mirror publication, timestamp refresh, root rotation, and 2-of-3 key revocation'
