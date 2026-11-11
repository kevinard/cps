#!/usr/bin/env bash
set -euo pipefail
umask 077
shopt -s nullglob

fail() { printf '%s\n' "$*" >&2; exit 1; }

# REPO PUBLISHER CATALOG deploys a release and needs the targets, snapshot, and
# timestamp keys. Adding "refresh" re-signs only the timestamp, or the snapshot
# and timestamp, for the catalog that is already published.
mode=deploy
if [[ $# == 4 && $4 == refresh ]]; then
    mode=refresh
    set -- "$1" "$2" "$3"
fi
[[ $# == 3 && $3 =~ ^[0-9a-f]{64}$ ]] || fail 'Expected REPO PUBLISHER CATALOG_SHA256 [refresh]'
repo=$(realpath -e -- "$1")
publisher=$2
catalog=$3
cps=${CPS_BIN:?Set CPS_BIN to the built CPS executable}
mirror=$(realpath -e -- "${CPS_MIRROR_DIR:?Set CPS_MIRROR_DIR to the HTTPS document directory}")
bootstrap_root=$(realpath -e -- "${CPS_MIRROR_TUF_ROOT:?Set CPS_MIRROR_TUF_ROOT to the original signed TUF root JSON}")
active_root=$(realpath -e -- "${CPS_MIRROR_TUF_ACTIVE_ROOT:-$bootstrap_root}")
trust_root=$(realpath -e -- "${CPS_MIRROR_TRUST_ROOT:?Set CPS_MIRROR_TRUST_ROOT to the publisher trust root that clients pin}")
keys=$(realpath -e -- "${CPS_MIRROR_TUF_KEYS:?Set CPS_MIRROR_TUF_KEYS to the TUF release keys directory}")
[[ -d $mirror && $mirror != / && $repo != "$mirror" && $repo != "$mirror/"* && $mirror != "$repo/"* ]] ||
    fail 'Use a dedicated mirror directory separate from the publishing repository'
[[ $keys != "$mirror" && $keys != "$mirror/"* ]] ||
    fail 'Keep signing keys outside the mirror directory'

lifetime() {
    local name=$1 value=${!1:-$2}
    [[ $value =~ ^[1-9][0-9]{0,3}\ (days|hours)$ ]] || fail "$name must be a number of days or hours"
    printf '%s\n' "$value"
}
targets_lifetime=$(lifetime CPS_MIRROR_TARGETS_LIFETIME '30 days')
snapshot_lifetime=$(lifetime CPS_MIRROR_SNAPSHOT_LIFETIME '30 days')
timestamp_lifetime=$(lifetime CPS_MIRROR_TIMESTAMP_LIFETIME '7 days')
objects_threshold=${CPS_MIRROR_OBJECTS_THRESHOLD:-1}
[[ $objects_threshold =~ ^[1-9][0-9]?$ ]] || fail 'CPS_MIRROR_OBJECTS_THRESHOLD must be a small positive integer'

# ROLE.pem or ROLE-NAME.pem is a private key file. ROLE.signer.kdl or
# ROLE-NAME.signer.kdl describes an external signer. List several keys to
# meet a root threshold. Only the roles needed for this operation are read.
role_keys() {
    local role=$1 file
    role_args=()
    for file in "$keys/$role".pem "$keys/$role".signer.kdl "$keys/$role"-*.pem "$keys/$role"-*.signer.kdl; do
        [[ -e $file || -L $file ]] || continue
        file=$(realpath -e -- "$file")
        [[ -f $file && $file != "$mirror/"* ]] || fail "Invalid $role key source $file"
        if [[ $file == *.signer.kdl ]]; then
            role_args+=("--$role-key" "command:$file")
        else
            role_args+=("--$role-key" "$file")
        fi
    done
}

# Serialize updates to this origin and hold the approved catalog stable until publication.
[[ -f $repo/publish.lock && ! -L $repo/publish.lock ]] || fail 'Expected a CPS publishing repository'
exec {repo_lock}< "$repo/publish.lock"
flock "$repo_lock"
[[ $(cat -- "$repo/current") == "$catalog" ]] || fail 'The approved catalog is no longer current'
exec {mirror_lock}< "$mirror"
flock "$mirror_lock"

metadata=$mirror/metadata
targets=$mirror/targets
for directory in "$metadata" "$targets"; do
    [[ ! -L $directory && ( ! -e $directory || -d $directory ) ]] ||
        fail "Invalid mirror directory $directory"
done
timestamp=$metadata/timestamp.json
[[ ! -d $timestamp || -L $timestamp ]] || fail 'Invalid mirror timestamp path'

# Reserve metadata versions on the signing host before exposing them at the origin.
state=$repo/.ci-tuf-state.json
published=null
if [[ -e $state || -L $state ]]; then
    [[ -f $state && ! -L $state && -O $state && $(stat -c %a -- "$state") == 600 ]] ||
        fail 'TUF publication state must be a private file owned by the signer'
    previous=$(jq -ers --arg mirror "$mirror" '
        select(length == 1) | .[0]
        | select(keys == ["format", "mirror", "published", "version"] and .format == 1 and .mirror == $mirror)
        | .version | numbers
    ' "$state") ||
        fail 'Invalid TUF publication state or a different mirror is configured'
    published=$(jq -c '.published' "$state")
elif [[ $mode == refresh ]]; then
    fail 'A metadata refresh needs the signer state of a completed deployment'
elif [[ -n ${CPS_MIRROR_INITIAL_VERSION:-} ]]; then
    previous=$CPS_MIRROR_INITIAL_VERSION
    [[ $previous != 0 ]] || fail 'Use the highest previously reserved metadata version'
else
    existing=("$metadata"/*.{root,snapshot,targets}.json "$metadata"/*.objects-?.json)
    [[ ! -e $repo/.ci-deployed-catalog && ! -e $mirror/publisher.json &&
        ! -L $mirror/publisher.json && ! -e $timestamp && ! -L $timestamp && ${#existing[@]} == 0 ]] ||
        fail 'Missing signer state for an existing mirror. Restore it or set CPS_MIRROR_INITIAL_VERSION from trusted records.'
    previous=0
fi
[[ $previous =~ ^(0|[1-9][0-9]{0,14})$ && $previous != 999999999999999 ]] ||
    fail 'Invalid or exhausted TUF metadata version in signer state'
version=$((previous + 1))

record_state() {
    jq -n --arg mirror "$mirror" --argjson version "$version" \
        --argjson published "$1" '{format: 1, mirror: $mirror, version: $version, published: $published}' > "$state_tmp"
    sync -- "$state_tmp"
    mv -T -- "$state_tmp" "$state"
    sync -- "$repo"
}

stage=$(mktemp -d -- "$(dirname -- "$mirror")/.cps-mirror.XXXXXXXX")
state_tmp=
trap 'rm -rf -- "$stage"; if [[ -n $state_tmp ]]; then rm -f -- "$state_tmp"; fi' EXIT
cp -- "$bootstrap_root" "$stage/bootstrap-root.json"
cp -- "$active_root" "$stage/root.json"
cp -- "$trust_root" "$stage/trust-root.json"
read_root_version() {
    local file=$1 number
    [[ $(stat -c %s -- "$file") -le 1048576 ]] || fail 'TUF root exceeds the CPS size limit'
    number=$(jq -er 'select(.signed.consistent_snapshot == true) | .signed.version' "$file") ||
        fail 'Use a signed TUF root with consistent snapshots enabled'
    [[ $number =~ ^[1-9][0-9]{0,14}$ ]] || fail 'Invalid TUF root version'
    printf '%s\n' "$number"
}
bootstrap_version=$(read_root_version "$stage/bootstrap-root.json")
root_version=$(read_root_version "$stage/root.json")
root_expires=$(jq -er '.signed.expires' "$stage/root.json")
[[ $(date -u -d "$root_expires" +%s) -gt $(date -u +%s) ]] || fail 'The TUF root has expired'

mkdir "$stage/roots"
stage_root() {
    local source=$1 destination=$stage/roots/$2.root.json
    if [[ -f $destination ]]; then
        cmp -s -- "$source" "$destination" || fail "Conflicting TUF root version $2"
    else
        cp -- "$source" "$destination"
    fi
}
stage_root "$stage/bootstrap-root.json" "$bootstrap_version"
latest_root_version=$bootstrap_version
for file in "$metadata"/*.root.json; do
    filename=${file##*/}
    number=${filename%.root.json}
    [[ $number =~ ^[1-9][0-9]{0,14}$ && -f $file && ! -L $file && $(stat -c %s -- "$file") -le 1048576 ]] ||
        fail "Invalid versioned TUF root $file"
    stage_root "$file" "$number"
    if (( number > latest_root_version )); then
        latest_root_version=$number
    fi
done
(( root_version == latest_root_version || root_version == latest_root_version + 1 )) ||
    fail 'Use the latest published TUF root or its immediate successor'
stage_root "$stage/root.json" "$root_version"

sha256() {
    local checksum
    checksum=$(sha256sum < "$1")
    printf '%s\n' "${checksum%% *}"
}

# Verify the staged metadata from the bootstrap root as a new client would.
verify_stage() {
    local metadata_dir=$1 targets_dir=$2 metadata_fd targets_fd
    exec {metadata_fd}< "$metadata_dir" {targets_fd}< "$targets_dir"
    "$cps" repo sync --repo "$stage/verified" --tuf-root "$stage/bootstrap-root.json" \
        --trust-root "$stage/trust-root.json" --metadata "file:///proc/self/fd/$metadata_fd/" \
        --targets "file:///proc/self/fd/$targets_fd/" --index-only --require-catalog "$catalog"
    exec {metadata_fd}<&- {targets_fd}<&-
    [[ $(read_root_version "$stage/verified/.tuf/trusted/root.json") == "$root_version" ]] ||
        fail 'TUF verification did not reach the active root through the published root history'
}

publish_immutable() {
    local source=$1 destination=$2
    if [[ -e $destination || -L $destination ]]; then
        [[ -f $destination && ! -L $destination ]] && cmp -s -- "$source" "$destination" ||
            fail "Conflicting mirror object $destination"
    else
        # Serve immutable copies that outlive the staging directory.
        install -m 0644 -- "$source" "$stage/next"
        ln -T -- "$stage/next" "$destination"
        rm -- "$stage/next"
    fi
}

if [[ $mode == refresh ]]; then
    # The trusted record names the published snapshot. The origin is not trusted.
    record=$(jq -er --arg catalog "$catalog" '
        select(.catalog == $catalog)
        | select(.snapshot.version | numbers | . >= 1 and floor == .)
        | select(.snapshot.sha256 | strings | test("^[0-9a-f]{64}$"))
        | [.snapshot.version, .snapshot.sha256] | @tsv
    ' <<< "$published") ||
        fail 'No completed deployment of this catalog to refresh. Deploy it with deploy-current.'
    IFS=$'\t' read -r snapshot_version snapshot_sha256 <<< "$record"
    (( root_version == latest_root_version )) && cmp -s -- "$stage/root.json" "$metadata/$root_version.root.json" ||
        fail 'Publish the active TUF root with a full deployment before refreshing'
    snapshot=$metadata/$snapshot_version.snapshot.json
    [[ -f $snapshot && ! -L $snapshot && $(stat -c %s -- "$snapshot") -le 16777216 &&
        $(sha256 "$snapshot") == "$snapshot_sha256" ]] ||
        fail "The published snapshot $snapshot_version differs from the signer record"

    mkdir "$stage/metadata"
    cp -- "$stage/roots/"*.root.json "$snapshot" "$stage/metadata/"
    # Copy each role listed by the trusted snapshot and check its digest.
    while IFS=$'\t' read -r role role_version role_sha256; do
        [[ $role =~ ^[A-Za-z0-9_-]+\.json$ && $role_version =~ ^[1-9][0-9]{0,14}$ &&
            $role_sha256 =~ ^[0-9a-f]{64}$ ]] || fail 'Unexpected role in the published snapshot'
        file=$metadata/$role_version.$role
        [[ -f $file && ! -L $file && $(sha256 "$file") == "$role_sha256" ]] ||
            fail "The published $role differs from the snapshot"
        cp -- "$file" "$stage/metadata/"
    done < <(jq -er '.signed.meta | to_entries[] | [.key, .value.version, .value.hashes.sha256] | @tsv' "$snapshot")
    targets_version=$(jq -er '.signed.meta["targets.json"].version' "$snapshot")
    targets_expires=$(jq -er '.signed.expires' "$metadata/$targets_version.targets.json")
    snapshot_expires=$(jq -er '.signed.expires' "$snapshot")
    now=$(date -u +%s)
    (( $(date -u -d "$targets_expires" +%s) > now )) ||
        fail "The targets metadata expired at $targets_expires. Deploy the release with the targets key."
    if (( $(date -u -d "$targets_expires" +%s) < $(date -u -d '+14 days' +%s) )); then
        printf 'Warning: the targets metadata expires at %s. Deploy the release with the targets key before then.\n' \
            "$targets_expires" >&2
    fi

    timestamp_expiry=$(date -u -d "+$timestamp_lifetime" '+%Y-%m-%dT%H:%M:%SZ')
    refresh=("$cps" repo refresh-metadata --tuf-root "$stage/root.json" --snapshot "$snapshot"
        --version "$version" --timestamp-expires "$timestamp_expiry" --output "$stage/refreshed")
    role_keys timestamp
    (( ${#role_args[@]} )) || fail 'Missing timestamp key source'
    refresh+=("${role_args[@]}")
    # Re-sign the snapshot when it would expire within a day of the new timestamp.
    role_keys snapshot
    if (( $(date -u -d "$snapshot_expires" +%s) < $(date -u -d "+$timestamp_lifetime +1 day" +%s) )); then
        (( ${#role_args[@]} )) ||
            fail "The snapshot expires at $snapshot_expires. Supply a snapshot key source to re-sign it."
        snapshot_expiry=$(date -u -d "+$snapshot_lifetime" '+%Y-%m-%dT%H:%M:%SZ')
        refresh+=("${role_args[@]}" --snapshot-expires "$snapshot_expiry")
    fi
    "${refresh[@]}"
    cp -- "$stage/refreshed/"* "$stage/metadata/"
    verify_stage "$stage/metadata" "$targets"
    next_snapshot=$stage/refreshed/$version.snapshot.json
    if [[ -f $next_snapshot ]]; then
        snapshot_version=$version
        snapshot_sha256=$(sha256 "$next_snapshot")
        snapshot_expires=$snapshot_expiry
    fi
    publication=$(jq -c --arg expires "$timestamp_expiry" --arg sha256 "$(sha256 "$stage/refreshed/timestamp.json")" \
        --argjson snapshot_version "$snapshot_version" --arg snapshot_sha256 "$snapshot_sha256" \
        --arg snapshot_expires "$snapshot_expires" '
        .expires = $expires | .sha256 = $sha256
        | .snapshot = {version: $snapshot_version, sha256: $snapshot_sha256, expires: $snapshot_expires}
    ' <<< "$published")
    # Keep the previous record during the reservation so an interrupted refresh can be retried.
    state_tmp=$(mktemp "$repo/.ci-tuf-state.XXXXXXXX")
    record_state "$published"
    state_tmp=
    if [[ -f $next_snapshot ]]; then
        publish_immutable "$next_snapshot" "$metadata/$version.snapshot.json"
    fi
    install -m 0644 -- "$stage/refreshed/timestamp.json" "$stage/timestamp.next"
    sync -f -- "$stage"
    sync -- "$metadata"
    mv -T -- "$stage/timestamp.next" "$timestamp"
    sync -f -- "$mirror"
    state_tmp=$(mktemp "$repo/.ci-tuf-state.XXXXXXXX")
    record_state "$publication"
    state_tmp=
    if [[ -f $next_snapshot ]]; then
        printf 'Refreshed TUF snapshot and timestamp version %s for catalog %s at %s\n' "$version" "$catalog" "$mirror"
    else
        printf 'Refreshed TUF timestamp version %s for catalog %s at %s\n' "$version" "$catalog" "$mirror"
    fi
    exit 0
fi

signing=()
for role in targets snapshot timestamp; do
    role_keys "$role"
    (( ${#role_args[@]} )) || fail "Missing $role key source"
    signing+=("${role_args[@]}")
done
role_keys objects
signing+=("${role_args[@]}" --objects-threshold "$objects_threshold")

"$cps" repo export --repo "$repo" --trust-root "$stage/trust-root.json" \
    --output "$stage/export"
[[ $(cat -- "$stage/export/current") == "$catalog" ]] || fail 'Exported a different catalog'

jq -n --arg publisher "$publisher" --rawfile tuf_root "$stage/bootstrap-root.json" \
    --arg trust_root "$(base64 -w0 -- "$stage/trust-root.json")" \
    '{schema_version: 1, kind: "publisher-bootstrap", publisher: $publisher, tuf_root: ($tuf_root | @base64), trust_root: $trust_root}' \
    > "$stage/publisher.json"
trust_version() {
    local version
    version=$("$cps" repo trust show "$1" | sed -n 's/^Version: \([1-9][0-9]*\)$/\1/p')
    [[ $version =~ ^[1-9][0-9]{0,14}$ ]] || fail "Invalid trust root $1"
    printf '%s\n' "$version"
}
# The descriptor may advance to a later trust root version of the same history.
replace_descriptor=
if [[ -e $mirror/publisher.json || -L $mirror/publisher.json ]]; then
    [[ -f $mirror/publisher.json && ! -L $mirror/publisher.json &&
        $(stat -c %s -- "$mirror/publisher.json") -le 1048576 ]] ||
        fail 'Invalid mirror publisher.json'
    if ! cmp -s -- "$stage/publisher.json" "$mirror/publisher.json"; then
        jq -e --slurpfile next "$stage/publisher.json" \
            'del(.trust_root) == ($next[0] | del(.trust_root))' "$mirror/publisher.json" >/dev/null &&
            jq -er '.trust_root' "$mirror/publisher.json" | base64 -d > "$stage/advertised-trust-root.json" ||
            fail 'The mirror already advertises different publisher bootstrap material'
        # The deployed catalog's history holds both versions unchanged, so they
        # share version 1 and every version between them.
        "$cps" repo withdrawals --repo "$repo" \
            --trust-root "$stage/advertised-trust-root.json" >/dev/null ||
            fail 'The mirror advertises a trust root outside the history of the catalog'
        advertised_version=$(trust_version "$stage/advertised-trust-root.json")
        trust_root_version=$(trust_version "$stage/trust-root.json")
        (( trust_root_version > advertised_version )) ||
            fail "The mirror advertises trust root version $advertised_version. Set CPS_MIRROR_TRUST_ROOT to it or a later version."
        replace_descriptor=1
    fi
fi

release_expiry=$(date -u -d "+$targets_lifetime" '+%Y-%m-%dT%H:%M:%SZ')
timestamp_expiry=$(date -u -d "+$timestamp_lifetime" '+%Y-%m-%dT%H:%M:%SZ')
"$cps" repo sign-export --tuf-root "$stage/root.json" "${signing[@]}" \
    --version "$version" --release-expires "$release_expiry" --timestamp-expires "$timestamp_expiry" \
    --output "$stage/signed" "$stage/export"
cp -- "$stage/roots/"*.root.json "$stage/signed/metadata/"

verify_stage "$stage/signed/metadata" "$stage/signed/targets"

publication=$(jq -n --arg catalog "$catalog" --arg expires "$timestamp_expiry" \
    --arg sha256 "$(sha256 "$stage/signed/metadata/timestamp.json")" --argjson snapshot_version "$version" \
    --arg snapshot_sha256 "$(sha256 "$stage/signed/metadata/$version.snapshot.json")" \
    --arg snapshot_expires "$release_expiry" '{catalog: $catalog, expires: $expires, sha256: $sha256,
        snapshot: {version: $snapshot_version, sha256: $snapshot_sha256, expires: $snapshot_expires}}')
state_tmp=$(mktemp "$repo/.ci-tuf-state.XXXXXXXX")
record_state null
state_tmp=

mkdir -p -m 0755 -- "$metadata" "$targets"
device=$(stat -c %d -- "$stage")
for directory in "$mirror" "$metadata" "$targets"; do
    [[ $(stat -c %d -- "$directory") == "$device" ]] ||
        fail 'The mirror and its parent must be on the same filesystem'
done

for file in "$stage/signed/targets/"*; do
    publish_immutable "$file" "$targets/${file##*/}"
done
for file in "$stage/signed/metadata/$version".objects-?.json; do
    publish_immutable "$file" "$metadata/${file##*/}"
done
for filename in "$version.targets.json" "$version.snapshot.json"; do
    publish_immutable "$stage/signed/metadata/$filename" "$metadata/$filename"
done
active_filename=$root_version.root.json
for file in "$stage/roots/"*.root.json; do
    filename=${file##*/}
    if [[ $filename != "$active_filename" ]]; then
        publish_immutable "$file" "$metadata/$filename"
    fi
done
if [[ -z $replace_descriptor ]]; then
    publish_immutable "$stage/publisher.json" "$mirror/publisher.json"
fi
install -m 0644 -- "$stage/signed/metadata/timestamp.json" "$stage/timestamp.next"
sync -f -- "$stage"
# The new root may revoke the old timestamp key. Publish it just before the timestamp.
publish_immutable "$stage/roots/$active_filename" "$metadata/$active_filename"
sync -- "$metadata/$active_filename" "$metadata"
mv -T -- "$stage/timestamp.next" "$timestamp"
sync -f -- "$mirror"
if [[ -n $replace_descriptor ]]; then
    # Advertise the later trust root once a catalog carrying it is current.
    # An interrupted replacement leaves the earlier version, which still
    # verifies, and the next deployment replaces it.
    install -m 0644 -- "$stage/publisher.json" "$stage/publisher.next"
    sync -- "$stage/publisher.next"
    mv -T -- "$stage/publisher.next" "$mirror/publisher.json"
    sync -f -- "$mirror"
fi
state_tmp=$(mktemp "$repo/.ci-tuf-state.XXXXXXXX")
record_state "$publication"
state_tmp=
printf 'Published TUF version %s for catalog %s at %s\n' "$version" "$catalog" "$mirror"
