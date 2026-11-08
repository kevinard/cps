#!/usr/bin/env bash
set -euo pipefail
umask 077
shopt -s nullglob

fail() { printf '%s\n' "$*" >&2; exit 1; }

[[ $# == 3 && $3 =~ ^[0-9a-f]{64}$ ]] || fail 'Expected REPO PUBLISHER CATALOG_SHA256'
repo=$(realpath -e -- "$1")
publisher=$2
catalog=$3
cps=${CPS_BIN:?Set CPS_BIN to the built CPS executable}
mirror=$(realpath -e -- "${CPS_MIRROR_DIR:?Set CPS_MIRROR_DIR to the HTTPS document directory}")
bootstrap_root=$(realpath -e -- "${CPS_MIRROR_TUF_ROOT:?Set CPS_MIRROR_TUF_ROOT to the original signed TUF root JSON}")
active_root=$(realpath -e -- "${CPS_MIRROR_TUF_ACTIVE_ROOT:-$bootstrap_root}")
publisher_key=$(realpath -e -- "${CPS_MIRROR_PUBLISHER_KEY:?Set CPS_MIRROR_PUBLISHER_KEY to the publisher public DER key}")
keys=$(realpath -e -- "${CPS_MIRROR_TUF_KEYS:?Set CPS_MIRROR_TUF_KEYS to the TUF release keys directory}")
[[ -d $mirror && $mirror != / && $repo != "$mirror" && $repo != "$mirror/"* && $mirror != "$repo/"* ]] ||
    fail 'Use a dedicated mirror directory separate from the publishing repository'
[[ $keys != "$mirror" && $keys != "$mirror/"* ]] ||
    fail 'Keep signing keys outside the mirror directory'
signing=()
for role in targets snapshot timestamp; do
    key=$(realpath -e -- "$keys/$role.pem")
    [[ -f $key && $key != "$mirror/"* ]] || fail "Invalid private key for $role"
    signing+=("--$role-key" "$key")
done

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
if [[ -e $state || -L $state ]]; then
    [[ -f $state && ! -L $state && -O $state && $(stat -c %a -- "$state") == 600 ]] ||
        fail 'TUF publication state must be a private file owned by the signer'
    previous=$(jq -er --arg mirror "$mirror" 'select(.mirror == $mirror) | .version' "$state") ||
        fail 'Invalid TUF publication state or a different mirror is configured'
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
        --argjson published "$1" '{mirror: $mirror, version: $version, published: $published}' > "$state_tmp"
    sync -- "$state_tmp"
    mv -T -- "$state_tmp" "$state"
    sync -- "$repo"
}

stage=$(mktemp -d -- "$(dirname -- "$mirror")/.cps-mirror.XXXXXXXX")
state_tmp=
trap 'rm -rf -- "$stage"; if [[ -n $state_tmp ]]; then rm -f -- "$state_tmp"; fi' EXIT
cp -- "$bootstrap_root" "$stage/bootstrap-root.json"
cp -- "$active_root" "$stage/root.json"
cp -- "$publisher_key" "$stage/publisher.der"
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

"$cps" repo export --repo "$repo" --publisher "$publisher" --trust-key "$stage/publisher.der" \
    --output "$stage/export"
[[ $(cat -- "$stage/export/current") == "$catalog" ]] || fail 'Exported a different catalog'

jq -n --arg publisher "$publisher" --rawfile tuf_root "$stage/bootstrap-root.json" \
    --arg trust_key "$(base64 -w0 -- "$stage/publisher.der")" \
    '{schema_version: 1, publisher: $publisher, tuf_root: ($tuf_root | @base64), trust_key: $trust_key}' \
    > "$stage/publisher.json"
if [[ -e $mirror/publisher.json || -L $mirror/publisher.json ]]; then
    [[ -f $mirror/publisher.json && ! -L $mirror/publisher.json ]] &&
        cmp -s -- "$stage/publisher.json" "$mirror/publisher.json" ||
        fail 'The mirror already advertises different publisher bootstrap material'
fi

release_expiry=$(date -u -d '+30 days' '+%Y-%m-%dT%H:%M:%SZ')
timestamp_expiry=$(date -u -d '+7 days' '+%Y-%m-%dT%H:%M:%SZ')
"$cps" repo sign-export --root "$stage/root.json" "${signing[@]}" \
    --version "$version" --release-expires "$release_expiry" --timestamp-expires "$timestamp_expiry" \
    --output "$stage/signed" "$stage/export"
cp -- "$stage/roots/"*.root.json "$stage/signed/metadata/"

# Use an open directory so local verification works with spaces in the staging path.
exec {stage_fd}< "$stage/signed"
stage_url=file:///proc/self/fd/$stage_fd
"$cps" repo sync --repo "$stage/verified" --root "$stage/bootstrap-root.json" --publisher "$publisher" \
    --trust-key "$stage/publisher.der" --metadata "$stage_url/metadata/" --targets "$stage_url/targets/" \
    --index-only --require-catalog "$catalog"
exec {stage_fd}<&-
[[ $(read_root_version "$stage/verified/.tuf/trusted/root.json") == "$root_version" ]] ||
    fail 'TUF verification did not reach the active root through the published root history'

timestamp_hash=$(sha256sum < "$stage/signed/metadata/timestamp.json")
publication=$(jq -n --arg catalog "$catalog" --arg expires "$timestamp_expiry" \
    --arg sha256 "${timestamp_hash%% *}" '{catalog: $catalog, expires: $expires, sha256: $sha256}')
state_tmp=$(mktemp "$repo/.ci-tuf-state.XXXXXXXX")
record_state null
state_tmp=

mkdir -p -m 0755 -- "$metadata" "$targets"
device=$(stat -c %d -- "$stage")
for directory in "$mirror" "$metadata" "$targets"; do
    [[ $(stat -c %d -- "$directory") == "$device" ]] ||
        fail 'The mirror and its parent must be on the same filesystem'
done

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
publish_immutable "$stage/publisher.json" "$mirror/publisher.json"
install -m 0644 -- "$stage/signed/metadata/timestamp.json" "$stage/timestamp.next"
sync -f -- "$stage"
# The new root may revoke the old timestamp key. Publish it just before the timestamp.
publish_immutable "$stage/roots/$active_filename" "$metadata/$active_filename"
sync -- "$metadata/$active_filename" "$metadata"
mv -T -- "$stage/timestamp.next" "$timestamp"
sync -f -- "$mirror"
state_tmp=$(mktemp "$repo/.ci-tuf-state.XXXXXXXX")
record_state "$publication"
state_tmp=
printf 'Published TUF version %s for catalog %s at %s\n' "$version" "$catalog" "$mirror"
