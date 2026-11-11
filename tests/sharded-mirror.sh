#!/usr/bin/env bash
set -euo pipefail
umask 077

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
export CPS_BIN=${CPS_BIN:-$project/target/debug/cps}
tuf_fixture=${CPS_TUF_FIXTURE:-$project/target/debug/examples/tuf-fixture}
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd "$work"

mkdir -p keys public proto
export CPS_MIRROR_DIR=$work/public CPS_MIRROR_TUF_ROOT=$work/seed/root.json
export CPS_MIRROR_TUF_KEYS=$work/keys CPS_MIRROR_TRUST_ROOT=$work/trust-root.json
unset CPS_MIRROR_TUF_ACTIVE_ROOT CPS_MIRROR_INITIAL_VERSION
openssl genpkey -algorithm ED25519 -outform DER -out publisher-private.der
"$CPS_BIN" repo trust init --repo repo --publisher canopa --key publisher-private.der >/dev/null
cp repo/trust/1.json trust-root.json

publish() {
    printf '%s\n' "$1" > proto/example
    "$CPS_BIN" manifest generate --proto proto --publisher canopa --version "$1.0.0-0" \
        --uid "$(id -u)" --gid "$(id -g)" example > manifest.kdl
    "$CPS_BIN" publish manifests --repo repo --proto proto --key publisher-private.der manifest.kdl
}

publish 1
"$CPS_BIN" repo export --repo repo --trust-root trust-root.json --output export
"$tuf_fixture" export seed 1
for role in targets snapshot timestamp; do cp seed/fixture-key.der "keys/$role.pem"; done

sync_mirror() {
    "$CPS_BIN" repo sync --repo cache --tuf-root seed/root.json --trust-root trust-root.json \
        --metadata "file://$work/public/metadata/" --targets "file://$work/public/targets/" \
        --require-catalog "$(cat repo/current)"
}
for version in 1 2 3; do
    if [[ $version != 1 ]]; then publish "$version"; fi
    "$project/ci/deploy-mirror.sh" repo canopa "$(cat repo/current)"
    sync_mirror
    jq -e '.signed.targets | keys == ["current", "publisher", "trust-root.json"]' \
        "public/metadata/$version.targets.json"
    jq -e '.signed.meta | length == 17' "public/metadata/$version.snapshot.json"
done
state=repo/.ci-tuf-state.json
jq -e '.format == 1 and .version == 3' "$state"
cp "$state" signer-state.json
cp public/metadata/timestamp.json timestamp.json
for filter in 'del(.format)' '.format = 2'; do
    jq "$filter" signer-state.json > "$state"
    if "$project/ci/deploy-mirror.sh" repo canopa "$(cat repo/current)" > deploy.log 2>&1; then
        echo 'The mirror signer accepted an unsupported state format' >&2
        exit 1
    fi
    grep -F 'Invalid TUF publication state' deploy.log
    cmp public/metadata/timestamp.json timestamp.json
done
cat signer-state.json > "$state"
"$CPS_BIN" image create --root image --repo cache --trust-root trust-root.json
"$CPS_BIN" install --root image -n --save-plan plan.json example
"$CPS_BIN" apply --root image plan.json
grep -Fx 3 image/example
"$CPS_BIN" verify --root image
