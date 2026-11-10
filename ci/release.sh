#!/usr/bin/env bash
set -euo pipefail
umask 077

fail() { printf '%s\n' "$*" >&2; exit 1; }
digest() { [[ $1 =~ ^[0-9a-f]{64}$ ]]; }

cps=${CPS_BIN:?Set CPS_BIN to the built CPS executable}
repo=${CPS_RELEASE_REPO:?Set CPS_RELEASE_REPO to the publishing repository}
publisher=${CPS_RELEASE_PUBLISHER:?Set CPS_RELEASE_PUBLISHER}
[[ $cps = /* && -x $cps && $repo = /* && -d $repo ]] ||
    fail 'CPS and the repository need absolute paths'
if [[ ${1:-} != prepare ]]; then
    deploy=${CPS_RELEASE_DEPLOY:?Set CPS_RELEASE_DEPLOY to the mirror deployment adapter}
    [[ $deploy = /* && -x $deploy ]] || fail 'Use an absolute executable path for deployment'
fi

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
    if [[ -n ${CPS_RELEASE_DEPLOYED_OUTPUT:-} ]]; then
        printf 'catalog=%s\n' "$1" >> "$CPS_RELEASE_DEPLOYED_OUTPUT"
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
    prepare|promote)
        operation=$1
        [[ $# == 3 && $2 =~ ^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$ ]] ||
            fail 'Expected prepare REQUEST EXPORT_DIRECTORY or promote REQUEST REHEARSED_SHA256'
        inputs=${CPS_RELEASE_INPUTS:?Set CPS_RELEASE_INPUTS to the release requests directory}
        key=${CPS_RELEASE_SIGNING_KEY:?Set CPS_RELEASE_SIGNING_KEY}
        [[ $inputs = /* ]] || fail 'Use an absolute release requests directory'
        # A command: source names an external signer descriptor instead of a key file.
        source=${key#command:}
        source=$(realpath -e -- "${source#file:}")
        [[ -f $source && $source != "$(realpath -e -- "$repo")/"* ]] ||
            fail 'Keep the publisher signing key outside the publishing repository'
        if [[ $key == command:* ]]; then key=command:$source; else key=$source; fi
        request="$inputs/$2/release.kdl"
        [[ -f $request ]] || fail 'Release request needs release.kdl'
        publish=("$cps" repo publish-release --repo "$repo" --signing-key "$key")
        if [[ -n ${CPS_RELEASE_SOURCE_CACHE:-} ]]; then
            publish+=(--source-cache "$CPS_RELEASE_SOURCE_CACHE")
        fi
        if [[ $operation == prepare ]]; then
            output=$3
            [[ $output = /* && ! -e $output && ! -L $output ]] ||
                fail 'Use a new absolute export directory'
            base=$(current)
            [[ -z $base ]] || digest "$base" || fail 'Invalid current catalog'
            message=$("${publish[@]}" --prepare "$request")
            candidate=$(catalog_from_output "$message" Prepared)
            exec {repo_lock}< "$repo/publish.lock"
            flock "$repo_lock"
            [[ $(current) == "$base" ]] || fail 'The repository advanced during preparation'
            [[ $candidate != "$base" ]] || fail 'The request produced no new catalog'
            stage=$(mktemp -d -- "$(dirname -- "$output")/.cps-rehearsal.XXXXXXXX")
            trap 'rm -rf -- "$stage"' EXIT
            for dir in catalogs catalog-shards manifests blobs attestations file-indexes; do
                if [[ ! -e $repo/$dir && ! -L $repo/$dir ]]; then
                    continue
                fi
                [[ -d $repo/$dir && ! -L $repo/$dir ]] ||
                    fail "Invalid publishing object directory $dir"
                unexpected=$(find "$repo/$dir" -regextype posix-extended -mindepth 1 -maxdepth 1 \
                    \( ! -type f -o ! -regex '.*/[0-9a-f]{64}' \) -print -quit)
                [[ -z $unexpected ]] || fail "Unexpected file in publishing object directory $dir"
                cp -a --reflink=auto -- "$repo/$dir" "$stage/"
            done
            printf '%s\n' "$candidate" > "$stage/candidate"
            printf '%s\n' "$base" > "$stage/base"
            mv -T --no-clobber -- "$stage" "$output"
            [[ ! -e $stage ]] || fail 'The rehearsal export destination already exists'
            printf 'Prepared catalog %s\nBase catalog %s\n' "$candidate" "${base:-(empty)}"
        else
            candidate=$3
            digest "$candidate" || fail 'Expected the rehearsed catalog SHA-256'
            printf '%s  %s\n' "$candidate" "$repo/catalogs/$candidate" | sha256sum --check --status ||
                fail 'The rehearsed catalog is missing from the publishing repository or has changed'
            message=$("${publish[@]}" --expect-catalog "$candidate" "$request")
            promoted=$(catalog_from_output "$message" Published)
            [[ $promoted == "$candidate" ]] || fail 'Promotion returned a different catalog'
            deploy_catalog "$promoted"
        fi
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
            def stable: (.signed | @base64d | fromjson) | del(.sequence, .release_profiles[$profile].rollout_percentage);
            (.[0] | stable) == (.[1] | stable)
        ' "$repo/catalogs/$initial" "$repo/catalogs/$base" >/dev/null ||
            fail 'Current catalog is outside the approved rollout'
        previous=$(jq -er --arg profile "$profile" '(.signed | @base64d | fromjson).release_profiles[$profile].rollout_percentage' \
            "$repo/catalogs/$base")
        first=$(jq -er --arg profile "$profile" '(.signed | @base64d | fromjson).release_profiles[$profile].rollout_percentage' \
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
    refresh-mirror)
        [[ $# == 1 ]] || fail 'Expected refresh-mirror'
        mirror=$(realpath -e -- "${CPS_MIRROR_DIR:?Set CPS_MIRROR_DIR to the HTTPS document directory}")
        timestamp=$mirror/metadata/timestamp.json
        deployed=$repo/.ci-deployed-catalog
        state=$repo/.ci-tuf-state.json
        if [[ ! -e $deployed && ! -L $deployed ]]; then
            [[ ! -e $state && ! -L $state && ! -e $timestamp && ! -L $timestamp ]] ||
                fail 'Run deploy-current to record the deployed catalog before refreshing.'
            printf '%s\n' 'The first mirror deployment is pending.'
            exit 0
        fi
        base=$(current)
        digest "$base" && [[ -f $deployed && ! -L $deployed && $(cat "$deployed") == "$base" ]] ||
            fail 'Refresh requires the current catalog to be recorded as deployed. Complete the deployment with deploy-current first.'
        if [[ -e $state || -L $state ]]; then
            [[ -f $state && ! -L $state && -O $state && $(stat -c %a -- "$state") == 600 ]] ||
                fail 'TUF publication state must be a private file owned by the signer'
            version=$(jq -ers --arg mirror "$mirror" '
                select(length == 1) | .[0]
                | select(keys == ["format", "mirror", "published", "version"] and .format == 1 and .mirror == $mirror)
                | .version | numbers
            ' "$state") &&
                [[ $version =~ ^(0|[1-9][0-9]{0,14})$ ]] ||
                fail 'Invalid TUF publication state or a different mirror is configured'
        fi
        if [[ -f $state && -f $timestamp && ! -L $timestamp ]] &&
            record=$(jq -er --arg mirror "$mirror" --arg base "$base" '
                select(.format == 1 and .mirror == $mirror and .published.catalog == $base) | .published |
                select(.expires | strings | test("^[0-9]{4}(-[0-9]{2}){2}T[0-9]{2}(:[0-9]{2}){2}Z$")) |
                select(.sha256 | strings | test("^[0-9a-f]{64}$")) | [.expires, .sha256] | @tsv
            ' "$state") &&
            IFS=$'\t' read -r expires expected <<< "$record" &&
            expiry=$(date -u --date="$expires" +%s) &&
            (( expiry > $(date -u --date='+48 hours' +%s) )) &&
            checksum=$(timeout 5 head -c 1048577 -- "$timestamp" | sha256sum) &&
            [[ ${checksum%% *} == "$expected" ]]; then
            printf 'Mirror timestamp is valid until %s.\n' "$expires"
            exit 0
        fi
        # Routine refreshes re-sign metadata for the deployed catalog. The
        # adapter needs no targets key or publisher key for this.
        env -u CPS_RELEASE_SIGNING_KEY "$deploy" "$repo" "$publisher" "$base" refresh
        [[ $(current) == "$base" ]] || fail 'The repository advanced during the refresh'
        printf 'Refreshed mirror metadata for catalog %s\n' "$base"
        ;;
    *) fail 'Expected prepare, promote, widen, advance, deploy-current, or refresh-mirror' ;;
esac
