#!/usr/bin/env bash
set -euo pipefail
umask 077

fail() { printf '%s\n' "$*" >&2; exit 1; }
digest() { [[ $1 =~ ^[0-9a-f]{64}$ ]]; }

cps=${CPS_BIN:?Set CPS_BIN to the built CPS executable}
repo=${CPS_RELEASE_REPO:?Set CPS_RELEASE_REPO to the publishing repository}
publisher=${CPS_RELEASE_PUBLISHER:?Set CPS_RELEASE_PUBLISHER}
deploy=${CPS_RELEASE_DEPLOY:?Set CPS_RELEASE_DEPLOY to the mirror deployment adapter}
[[ $cps = /* && -x $cps && $repo = /* && -d $repo && $deploy = /* && -x $deploy ]] ||
    fail 'CPS, the repository, and the deployment adapter need absolute paths'

current() {
    if [[ -e $repo/current ]]; then
        cat "$repo/current"
    fi
}

catalog_from_output() {
    local message=$1 prefix=$2 hash
    hash=${message#"$prefix catalog "}
    digest "$hash" || fail "Unexpected CPS publication result: $message"
    printf '%s\n' "$hash"
}

deploy_catalog() {
    [[ $(current) == "$1" ]] || fail 'The repository advanced before deployment'
    env -u CPS_RELEASE_SIGNING_KEY "$deploy" "$repo" "$publisher" "$1"
    printf 'Deployed catalog %s\n' "$1"
}

case ${1:-} in
    promote)
        [[ $# == 2 && $2 =~ ^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$ ]] || fail 'Expected promote REQUEST'
        inputs=${CPS_RELEASE_INPUTS:?Set CPS_RELEASE_INPUTS to the release requests directory}
        key=${CPS_RELEASE_SIGNING_KEY:?Set CPS_RELEASE_SIGNING_KEY}
        gate=${CPS_RELEASE_VM_GATE:?Set CPS_RELEASE_VM_GATE to the boot and service test adapter}
        [[ $inputs = /* && $gate = /* && -x $gate ]] || fail 'Use absolute paths for inputs and the VM gate'
        request="$inputs/$2"
        [[ -f $request/release.kdl && -d $request/cases ]] || fail 'Release request needs release.kdl and cases/'
        base=$(current)
        [[ -z $base ]] || digest "$base" || fail 'Invalid current catalog'
        publish=("$cps" repo publish-release --repo "$repo" --signing-key "$key")
        if [[ -n ${CPS_RELEASE_SOURCE_CACHE:-} ]]; then
            publish+=(--source-cache "$CPS_RELEASE_SOURCE_CACHE")
        fi
        message=$("${publish[@]}" --prepare "$request/release.kdl")
        candidate=$(catalog_from_output "$message" Prepared)
        [[ $(current) == "$base" ]] || fail 'The repository advanced during preparation'
        [[ $candidate != "$base" ]] || fail 'The request produced no new catalog'
        env -u CPS_RELEASE_SIGNING_KEY bash "$(dirname -- "$0")/rehearse-release.sh" \
            "$request/cases" "$candidate" "$base" "$gate"
        message=$("${publish[@]}" --expect-catalog "$candidate" "$request/release.kdl")
        promoted=$(catalog_from_output "$message" Published)
        [[ $promoted == "$candidate" ]] || fail 'Promotion returned a different catalog'
        deploy_catalog "$promoted"
        ;;
    widen)
        [[ $# == 4 ]] && digest "$2" && [[ $4 =~ ^(100|[1-9][0-9]?)$ ]] ||
            fail 'Expected widen CURRENT_SHA256 PROFILE PERCENTAGE'
        key=${CPS_RELEASE_SIGNING_KEY:?Set CPS_RELEASE_SIGNING_KEY}
        gate=${CPS_RELEASE_ROLLOUT_GATE:?Set CPS_RELEASE_ROLLOUT_GATE to the cohort health adapter}
        [[ $gate = /* && -x $gate ]] || fail 'Use an absolute executable path for the rollout gate'
        [[ $(current) == "$2" ]] || fail 'The requested rollout catalog is stale'
        env -u CPS_RELEASE_SIGNING_KEY "$gate" "$repo" "$publisher" "$2" "$3" "$4"
        message=$("$cps" repo widen-rollout --repo "$repo" --publisher "$publisher" \
            --signing-key "$key" --base "$2" --profile "$3" --percentage "$4")
        deploy_catalog "$(catalog_from_output "$message" Published)"
        ;;
    deploy-current)
        [[ $# == 2 ]] && digest "$2" || fail 'Expected deploy-current CURRENT_SHA256'
        deploy_catalog "$2"
        ;;
    *) fail 'Expected promote, widen, or deploy-current' ;;
esac
