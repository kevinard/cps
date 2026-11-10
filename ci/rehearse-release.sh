#!/usr/bin/env bash
set -euo pipefail
umask 077

fail() { printf '%s\n' "$*" >&2; exit 1; }
digest() { [[ $1 =~ ^[0-9a-f]{64}$ ]]; }

[[ $# == 4 ]] || fail 'Expected CASES CANDIDATE_SHA256 BASE_SHA256 VM_GATE'
cases=$1
candidate=$2
base=$3
gate=$4
cps=${CPS_BIN:?}
repo=${CPS_REHEARSAL_REPO:?Set CPS_REHEARSAL_REPO to the prepared catalog snapshot}
publisher=${CPS_RELEASE_PUBLISHER:?}
digest "$candidate" && { [[ -z $base ]] || digest "$base"; } && [[ $candidate != "$base" ]] ||
    fail 'Invalid rehearsal candidate or base catalog digest'
[[ $cps = /* && -x $cps && $repo = /* && -d $repo && $cases = /* && -d $cases && $gate = /* && -x $gate ]] ||
    fail 'Use absolute paths for CPS, the snapshot, the rehearsal cases, and the VM gate'
[[ $(cat -- "$repo/candidate") == "$candidate" && $(cat -- "$repo/base") == "$base" ]] ||
    fail 'The rehearsal snapshot does not match the requested candidate and base'
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
private=$work/repo
mkdir "$private"

# Copy only repository objects. The test repository has its own current pointer.
for dir in catalogs catalog-shards manifests blobs attestations file-indexes; do
    if [[ -d $repo/$dir ]]; then
        cp -a --reflink=auto -- "$repo/$dir" "$private/"
    fi
done
printf '%s\n' "$candidate" > "$private/current"
catalog="$private/catalogs/$candidate"
printf '%s  %s\n' "$candidate" "$catalog" | sha256sum --check --status
jq -e --arg publisher "$publisher" \
    '(.signed | @base64d | fromjson) | .publisher == $publisher and (.release_profiles | length > 0)' "$catalog" >/dev/null
jq -r '(.signed | @base64d | fromjson).release_profiles | keys[]' "$catalog" > "$work/profiles"
touch "$work/tested"

install_profile() {
    local config=$1 hash=$2 root=$3
    "$cps" image-create --config "$config" --bootstrap-catalog "$hash" --primary-repo "$private" --root "$root"
    "$cps" plan --root "$root" --output "$work/transaction.json" install-profile
    "$cps" validate --root "$root" "$work/transaction.json"
    "$cps" apply --root "$root" "$work/transaction.json"
    "$cps" verify --root "$root"
}

check_pin() {
    "$cps" image-lock --root "$1" |
        jq -er --arg publisher "$publisher" --arg digest "$2" \
            'select(.publishers[0] | .name == $publisher and .catalog == $digest) | .profile'
}

shopt -s nullglob
for config in "$cases"/*/image.kdl; do
    case_dir=${config%/*}
    case_work=$(mktemp -d "$work/case.XXXXXX")
    fresh=$case_work/fresh
    install_profile "$config" "$candidate" "$fresh"
    profile=$(check_pin "$fresh" "$candidate")
    "$gate" fresh "$profile" "$candidate" "" "$fresh" ""
    printf '%s\n' "$profile" >> "$work/tested"

    baselines=()
    if [[ -n $base ]]; then
        recorded=$(jq -r --arg profile "$profile" \
            '((.signed | @base64d | fromjson).release_profiles[$profile].composition | length) > 0' \
            "$private/catalogs/$base")
        if [[ $recorded == true ]]; then
            baselines+=("$base")
        fi
    fi
    if [[ -f $case_dir/baselines ]]; then
        while IFS= read -r hash || [[ -n $hash ]]; do
            [[ $hash =~ ^[0-9a-f]{64}$ && $hash != "$candidate" ]] ||
                { echo "Invalid baseline in $case_dir/baselines" >&2; exit 1; }
            baselines+=("$hash")
        done < "$case_dir/baselines"
    fi
    for hash in "${baselines[@]}"; do
        before="$case_work/base-$hash"
        after="$case_work/upgrade-$hash"
        [[ ! -e $before ]] || continue
        install_profile "$config" "$hash" "$before"
        [[ $(check_pin "$before" "$hash") == "$profile" ]]
        if [[ -x $case_dir/before-upgrade ]]; then
            "$case_dir/before-upgrade" "$cps" "$before" "$hash" "$candidate"
        fi
        "$cps" plan --root "$before" --output "$work/transaction.json" update --force-phased-update
        "$cps" validate --root "$before" "$work/transaction.json"
        "$cps" boot-env stage --source "$before" --root "$after" "$work/transaction.json"
        [[ $(check_pin "$after" "$candidate") == "$profile" ]]
        "$gate" upgrade "$profile" "$candidate" "$hash" "$after" "$before"
    done
done

LC_ALL=C sort -u "$work/tested" -o "$work/tested"
diff -u "$work/profiles" "$work/tested"
printf 'Rehearsed catalog %s\nBase catalog %s\n' "$candidate" "${base:-(empty)}"
