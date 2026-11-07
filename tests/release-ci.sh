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
schema-version 1
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
    printf 'schema-version 1\nrelease "canopa" base="%s" refresh-profiles=#true\n' "$(cat repo/current)" > baseline.kdl
    "$CPS_BIN" repo publish-release --repo repo --signing-key key.der baseline.kdl
    if [[ $version == 1 ]]; then
        cat repo/current > requests/next/cases/desktop/baselines
    fi
done
base=$(cat repo/current)
policy 3 0
printf 'schema-version 1\nrelease "canopa" base="%s" refresh-profiles=#true\n' "$base" > requests/next/release.kdl
cat > requests/next/cases/desktop/image.kdl <<CONFIG
schema-version 1
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
printf 'deploy %s\n' "$3" >> "$CPS_TEST_LOG"
DEPLOY
chmod +x "$CPS_RELEASE_VM_GATE" "$CPS_RELEASE_ROLLOUT_GATE" "$CPS_RELEASE_DEPLOY"
release() { bash "$project/ci/release.sh" "$@"; }
if CPS_TEST_FAIL_GATE=upgrade release promote next; then
    echo 'An upgrade check failed but promotion succeeded' >&2
    exit 1
fi
[[ $(cat repo/current) == "$base" ]]
! rg -q '^deploy ' "$CPS_TEST_LOG"
: > "$CPS_TEST_LOG"
release promote next
candidate=$(cat repo/current)
[[ $candidate != "$base" ]]
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
jq -e '.catalog.release_profiles.desktop.rollout_percentage == 50' "repo/catalogs/$widened"
diff -u <(jq '.catalog | del(.sequence, .release_profiles.desktop.rollout_percentage)' "repo/catalogs/$candidate") \
    <(jq '.catalog | del(.sequence, .release_profiles.desktop.rollout_percentage)' "repo/catalogs/$widened")
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
jq -e '.catalog.release_profiles.desktop.rollout_percentage == 100' "repo/catalogs/$complete"
[[ $(cat repo/.ci-deployed-catalog) == "$widened" ]]
CPS_TEST_FAIL_GATE=rollout release advance
[[ $(cat repo/.ci-deployed-catalog) == "$complete" ]]
release advance
test "$(grep -Fc "deploy $complete" "$CPS_TEST_LOG")" = 1
printf '{"catalog":"%s","profile":"desktop","percentages":[0,50,100]}\n' "$base" > "$CPS_RELEASE_ROLLOUT_PLAN"
! release advance
echo 'Verified digest-pinned promotion, deployment retry, and health-gated rollout'
