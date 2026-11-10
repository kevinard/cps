#!/usr/bin/env bash
set -euo pipefail
project=$(cd -- "$(dirname -- "$0")/.." && pwd)
export CPS_BIN=${CPS_BIN:-$project/target/debug/cps}
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd "$work"
export CPS_RELEASE_REPO=$work/repo CPS_RELEASE_PUBLISHER=canopa
export CPS_RELEASE_INPUTS=$work/requests CPS_RELEASE_SIGNING_KEY=$work/key.der
export CPS_RELEASE_VM_GATE=$work/vm-gate CPS_RELEASE_ROLLOUT_GATE=$work/rollout-gate
export CPS_RELEASE_DEPLOY=$work/deploy CPS_TEST_LOG=$work/checks
export CPS_RELEASE_ROLLOUT_PLAN=$work/rollout.json
openssl genpkey -algorithm ED25519 -outform DER -out key.der
openssl pkey -inform DER -in key.der -pubout -outform DER -out public.der
openssl genpkey -algorithm ED25519 -outform DER -out extra.der
openssl pkey -inform DER -in extra.der -pubout -outform DER -out extra-public.der
mkdir -p proto/usr/share requests/next/cases/desktop
echo 'local addition' > proto/usr/share/extra
"$CPS_BIN" manifest generate --proto proto --publisher extra --version 1.0.0-0 \
    --uid "$(id -u)" --gid "$(id -g)" desktop/extra > extra.kdl
"$CPS_BIN" publish --repo extra --proto proto --signing-key extra.der extra.kdl
extra_pin=$(cat extra/current)
rm proto/usr/share/extra
for version in 1 2 3; do
    echo "$version" > proto/usr/share/cps-test
    "$CPS_BIN" manifest generate --proto proto --publisher canopa --version "$version.0.0-0" \
        --uid "$(id -u)" --gid "$(id -g)" desktop/base > manifest.kdl
    "$CPS_BIN" publish --repo repo --proto proto --signing-key key.der manifest.kdl
done
policy() {
    cat > repo/publish-policy.kdl <<POLICY
schema-version 1 kind="publish-policy"
publisher "canopa" {
    builder-key "unused.der"
    profile "desktop" architecture="$(uname -m)" rollout-percentage=$2 {
        package "desktop/base@$1.0.0-0"
        publisher "extra" catalog="$extra_pin" {
            repository "$work/extra"
            trust-key "$work/extra-public.der"
        }
    }
}
POLICY
}
for version in 1 2; do
    policy "$version" 100
    printf 'schema-version 1 kind="publish-release"\nrelease "canopa" base="%s" refresh-profiles=#true\n' "$(cat repo/current)" > baseline.kdl
    "$CPS_BIN" repo publish-release --repo repo --signing-key key.der baseline.kdl
    if [[ $version == 1 ]]; then
        cat repo/current > requests/next/cases/desktop/baselines
    fi
done
base=$(cat repo/current)
policy 3 0
printf 'schema-version 1 kind="publish-release"\nrelease "canopa" base="%s" refresh-profiles=#true\n' "$base" > requests/next/release.kdl
cat > requests/next/cases/desktop/image.kdl <<CONFIG
schema-version 1 kind="image-config"
architecture "$(uname -m)"
release-profile "desktop"
publisher "canopa" { repository "$work/repo"; trust-key "$work/public.der"; }
publisher "extra" { repository "$work/extra"; trust-key "$work/extra-public.der"; }
CONFIG
cat > requests/next/cases/desktop/before-upgrade <<'HOOK'
#!/usr/bin/env bash
set -euo pipefail
[[ -z ${CPS_RELEASE_SIGNING_KEY:-} ]]
plan=$(mktemp)
trap 'rm -f -- "$plan"' EXIT
"$1" plan --root "$2" --output "$plan" install cps://extra/desktop/extra
"$1" apply --root "$2" "$plan"
HOOK
chmod +x requests/next/cases/desktop/before-upgrade
cat > "$CPS_RELEASE_VM_GATE" <<'GATE'
#!/usr/bin/env bash
set -euo pipefail
[[ -z ${CPS_RELEASE_SIGNING_KEY:-} && $2 == desktop && $(cat "$5/usr/share/cps-test") == 3 ]]
if [[ $1 == upgrade ]]; then
    baseline=$(cat "$6/usr/share/cps-test")
    [[ $baseline == 1 || $baseline == 2 ]]
    [[ $(cat "$5/usr/share/extra") == 'local addition' ]]
else
    [[ ! -e $5/usr/share/extra ]]
fi
[[ ${CPS_TEST_FAIL_GATE:-} != "$1" ]]
printf '%s %s\n' "${1}${baseline:+-$baseline}" "$3" >> "$CPS_TEST_LOG"
GATE
cat > "$CPS_RELEASE_ROLLOUT_GATE" <<'GATE'
#!/usr/bin/env bash
set -euo pipefail
[[ -z ${CPS_RELEASE_SIGNING_KEY:-} && $3 == "$(cat "$1/current")" && $4 == desktop ]]
[[ $5 == 50 || $5 == 100 ]]
[[ ${CPS_TEST_FAIL_GATE:-} != rollout ]]
if [[ ${CPS_TEST_FAIL_GATE:-} == waiting ]]; then exit 75; fi
printf 'health %s\n' "$3" >> "$CPS_TEST_LOG"
GATE
cat > "$CPS_RELEASE_DEPLOY" <<'DEPLOY'
#!/usr/bin/env bash
set -euo pipefail
[[ -z ${CPS_RELEASE_SIGNING_KEY:-} && $3 == "$(cat "$1/current")" ]]
[[ ${CPS_TEST_FAIL_GATE:-} != deploy ]]
if [[ $# == 4 ]]; then
    [[ $4 == refresh ]]
    printf 'refresh %s\n' "$3" >> "$CPS_TEST_LOG"
else
    [[ $# == 3 ]]
    printf 'deploy %s\n' "$3" >> "$CPS_TEST_LOG"
fi
DEPLOY
chmod +x "$CPS_RELEASE_VM_GATE" "$CPS_RELEASE_ROLLOUT_GATE" "$CPS_RELEASE_DEPLOY"
release() { bash "$project/ci/release.sh" "$@"; }
reject() {
    if "$@"; then
        printf 'Unexpected success: %s\n' "$*" >&2
        exit 1
    fi
}
reject release promote next
ln -s "$CPS_RELEASE_SIGNING_KEY" "repo/catalogs/$(printf '%064d' 0)"
reject release prepare next "$work/rejected-snapshot"
[[ ! -e $work/rejected-snapshot && $(cat repo/current) == "$base" ]]
rm "repo/catalogs/$(printf '%064d' 0)"
release prepare next "$work/snapshot"
candidate=$(cat snapshot/candidate)
[[ $candidate != "$base" && $(cat snapshot/base) == "$base" ]]
[[ $(cat repo/current) == "$base" && ! -e snapshot/current && ! -e snapshot/publish-policy.kdl ]]
cmp "repo/catalogs/$candidate" "snapshot/catalogs/$candidate"
rehearse() {
    env -u CPS_RELEASE_SIGNING_KEY -u CPS_RELEASE_REPO -u CPS_RELEASE_INPUTS \
        -u CPS_RELEASE_DEPLOY CPS_REHEARSAL_REPO="$work/snapshot" \
        bash "$project/ci/rehearse-release.sh" "$work/requests/next/cases" \
        "$1" "$2" "$CPS_RELEASE_VM_GATE"
}
reject rehearse "$base" "$candidate"
reject rehearse "$candidate" '../not-a-digest'
if CPS_TEST_FAIL_GATE=upgrade rehearse "$candidate" "$base"; then
    echo 'An upgrade check unexpectedly passed' >&2
    exit 1
fi
[[ $(cat repo/current) == "$base" && ! -e $work/outputs ]]
reject rg -q '^deploy ' "$CPS_TEST_LOG"
: > "$CPS_TEST_LOG"
rehearse "$candidate" "$base"
reject release promote next "$(printf '%064d' 0)"
reject release promote next "$base"
[[ $(cat repo/current) == "$base" ]]
CPS_RELEASE_DEPLOYED_OUTPUT=$work/outputs release promote next "$candidate"
[[ $(cat repo/current) == "$candidate" ]]
[[ $(cat "$work/outputs") == "catalog=$candidate" ]]
[[ $(cat "$CPS_TEST_LOG") == "$(printf 'fresh %s\nupgrade-2 %s\nupgrade-1 %s\ndeploy %s' \
    "$candidate" "$candidate" "$candidate" "$candidate")" ]]
if CPS_TEST_FAIL_GATE=rollout release widen "$candidate" desktop 50; then
    echo 'A failed health check widened the rollout' >&2
    exit 1
fi
[[ $(cat repo/current) == "$candidate" ]]
release widen "$candidate" desktop 50
widened=$(cat repo/current)
[[ $candidate != "$widened" ]]
jq -e '(.signed | @base64d | fromjson).release_profiles.desktop.rollout_percentage == 50' "repo/catalogs/$widened"
diff -u <(jq '(.signed | @base64d | fromjson) | del(.sequence, .release_profiles.desktop.rollout_percentage)' "repo/catalogs/$candidate") \
    <(jq '(.signed | @base64d | fromjson) | del(.sequence, .release_profiles.desktop.rollout_percentage)' "repo/catalogs/$widened")
! release widen "$candidate" desktop 50
release deploy-current "$widened"
printf '{"catalog":"%s","profile":"desktop","percentages":[0,50,100]}\n' "$candidate" > "$CPS_RELEASE_ROLLOUT_PLAN"
CPS_TEST_FAIL_GATE=waiting release advance
[[ $(cat repo/current) == "$widened" ]]
if CPS_TEST_FAIL_GATE=deploy release advance; then
    echo 'A failed deployment was reported as successful' >&2
    exit 1
fi
complete=$(cat repo/current)
jq -e '(.signed | @base64d | fromjson).release_profiles.desktop.rollout_percentage == 100' "repo/catalogs/$complete"
[[ $(cat repo/.ci-deployed-catalog) == "$widened" ]]
CPS_TEST_FAIL_GATE=rollout release advance
[[ $(cat repo/.ci-deployed-catalog) == "$complete" ]]
release advance
test "$(grep -Fc "deploy $complete" "$CPS_TEST_LOG")" = 1
printf '{"catalog":"%s","profile":"desktop","percentages":[0,50,100]}\n' "$base" > "$CPS_RELEASE_ROLLOUT_PLAN"
! release advance
echo 'Verified digest-pinned promotion, deployment retry, and health-gated rollout'

export CPS_MIRROR_DIR=$work/mirror
mkdir -p "$CPS_MIRROR_DIR/metadata"
timestamp=$CPS_MIRROR_DIR/metadata/timestamp.json
: > "$CPS_TEST_LOG"
state=$work/repo/.ci-tuf-state.json
timestamp_expires() {
    jq -n --arg expires "$(date -u --date="$1" '+%Y-%m-%dT%H:%M:%SZ')" \
        '{signed: {expires: $expires}}' > "$timestamp"
}
record_timestamp() {
    local checksum
    checksum=$(sha256sum < "$timestamp")
    jq -n --arg mirror "$CPS_MIRROR_DIR" --arg catalog "$complete" \
        --arg expires "$(jq -r .signed.expires "$timestamp")" --arg sha256 "${checksum%% *}" \
        '{format: 1, mirror: $mirror, version: 1, published: {catalog: $catalog, expires: $expires, sha256: $sha256}}' > "$state"
    chmod 600 "$state"
}

timestamp_expires '+3 days'
record_timestamp
release refresh-mirror
[[ ! -s $CPS_TEST_LOG ]]

cp "$state" signer-state.json
for filter in 'del(.format)' '.format = 2'; do
    jq "$filter" signer-state.json > "$state"
    reject release refresh-mirror
    [[ ! -s $CPS_TEST_LOG ]]
done
cat signer-state.json > "$state"

# Future dates in the origin cannot suppress repair of an unknown timestamp.
timestamp_expires '+30 years'
printf '%s\n' "$widened" > repo/.ci-deployed-catalog
reject release refresh-mirror
[[ ! -s $CPS_TEST_LOG ]]
printf '%s\n' "$complete" > repo/.ci-deployed-catalog

if CPS_TEST_FAIL_GATE=deploy release refresh-mirror; then
    echo 'A failed refresh was reported as successful' >&2
    exit 1
fi
[[ ! -s $CPS_TEST_LOG ]]
release refresh-mirror
[[ $(cat "$CPS_TEST_LOG") == "refresh $complete" ]]
: > "$CPS_TEST_LOG"

timestamp_expires '+1 day'
record_timestamp
release refresh-mirror
[[ $(cat "$CPS_TEST_LOG") == "refresh $complete" ]]
: > "$CPS_TEST_LOG"

rm "$timestamp"
release refresh-mirror
[[ $(cat "$CPS_TEST_LOG") == "refresh $complete" ]]
[[ $(cat repo/current) == "$complete" && $(cat repo/.ci-deployed-catalog) == "$complete" ]]
echo 'Verified mirror refresh uses signer state and repairs a forged or missing timestamp'
