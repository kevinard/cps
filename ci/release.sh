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
    local marker
    [[ $(current) == "$1" ]] || fail 'The repository advanced before deployment'
    env -u CPS_RELEASE_SIGNING_KEY "$deploy" "$repo" "$publisher" "$1" || return
    [[ $(current) == "$1" ]] || fail 'The repository advanced during deployment'
    marker=$(mktemp "$repo/.ci-deployed.XXXXXX")
    if ! { printf '%s\n' "$1" > "$marker" && mv -T -- "$marker" "$repo/.ci-deployed-catalog"; }; then
        rm -f -- "$marker"
        fail 'Cannot record the deployed catalog'
    fi
    printf 'Deployed catalog %s\n' "$1"
}

widen_catalog() {
    local base=$1 profile=$2 percentage=$3 key gate message hash
    key=${CPS_RELEASE_SIGNING_KEY:?Set CPS_RELEASE_SIGNING_KEY}
    gate=${CPS_RELEASE_ROLLOUT_GATE:?Set CPS_RELEASE_ROLLOUT_GATE to the cohort health adapter}
    [[ $gate = /* && -x $gate ]] || fail 'Use an absolute executable path for the rollout gate'
    [[ $(current) == "$base" ]] || fail 'The requested rollout catalog is stale'
    if env -u CPS_RELEASE_SIGNING_KEY "$gate" "$repo" "$publisher" "$base" "$profile" "$percentage"; then
        :
    elif [[ $? == 75 && ${4:-} == defer ]]; then
        printf 'Waiting for rollout health at catalog %s\n' "$base"
        return 0
    else
        fail 'Rollout health gate failed'
    fi
    message=$("$cps" repo widen-rollout --repo "$repo" --publisher "$publisher" \
        --signing-key "$key" --base "$base" --profile "$profile" --percentage "$percentage") || return
    hash=$(catalog_from_output "$message" Published) || return
    deploy_catalog "$hash"
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
        widen_catalog "$2" "$3" "$4"
        ;;
    advance)
        [[ $# == 1 ]] || fail 'Expected advance'
        policy=${CPS_RELEASE_ROLLOUT_PLAN:?Set CPS_RELEASE_ROLLOUT_PLAN to the approved JSON policy}
        [[ $policy = /* && -f $policy ]] || fail 'Use an absolute path to an existing rollout policy'
        rules=$(jq -ce 'select(
            (.catalog | test("^[0-9a-f]{64}$")) and (.profile | type == "string" and length > 0)
            and (.percentages | type == "array" and length >= 2
                and all(.[]; type == "number" and floor == . and . >= 0 and . <= 100)
                and . == unique)
        )' "$policy")
        initial=$(jq -r '.catalog' <<< "$rules")
        profile=$(jq -r '.profile' <<< "$rules")
        base=$(current)
        digest "$base" || fail 'Invalid current catalog'
        for hash in "$initial" "$base"; do
            printf '%s  %s\n' "$hash" "$repo/catalogs/$hash" | sha256sum --check --status ||
                fail "Cannot verify catalog $hash"
        done
        jq -es --arg profile "$profile" '
            def stable: .catalog | del(.sequence, .release_profiles[$profile].rollout_percentage);
            (.[0] | stable) == (.[1] | stable)
        ' "$repo/catalogs/$initial" "$repo/catalogs/$base" >/dev/null ||
            fail 'Current catalog is outside the approved rollout'
        previous=$(jq -er --arg profile "$profile" '.catalog.release_profiles[$profile].rollout_percentage' \
            "$repo/catalogs/$base")
        first=$(jq -er --arg profile "$profile" '.catalog.release_profiles[$profile].rollout_percentage' \
            "$repo/catalogs/$initial")
        next=$(jq -er --argjson first "$first" --argjson previous "$previous" '
            .percentages | select(.[0] == $first) | index($previous) as $step
            | select($step != null) | .[$step + 1] // "complete"
        ' <<< "$rules") || fail 'Catalog rollout percentage is outside the approved steps'
        if [[ ! -f $repo/.ci-deployed-catalog || $(cat "$repo/.ci-deployed-catalog") != "$base" ]]; then
            deploy_catalog "$base"
        elif [[ $next == complete ]]; then
            printf 'Rollout policy complete at catalog %s\n' "$base"
        else
            widen_catalog "$base" "$profile" "$next" defer
        fi
        ;;
    deploy-current)
        [[ $# == 2 ]] && digest "$2" || fail 'Expected deploy-current CURRENT_SHA256'
        deploy_catalog "$2"
        ;;
    *) fail 'Expected promote, widen, advance, or deploy-current' ;;
esac
