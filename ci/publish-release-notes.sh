#!/usr/bin/env bash
set -euo pipefail
umask 077

fail() { printf '%s\n' "$*" >&2; exit 1; }

[[ $# == 2 && $1 =~ ^[a-z0-9][a-z0-9._-]*$ && $2 =~ ^[0-9a-f]{64}$ ]] ||
    fail 'Expected PUBLISHER CATALOG_SHA256'
publisher=$1
catalog=$2
mirror_url=${CPS_MIRROR_URL:?Set CPS_MIRROR_URL to the public HTTPS mirror URL}
invalid_url_chars='[[:space:][:cntrl:]`?#@]'
[[ $mirror_url =~ ^https://[^/]+(/.*)?$ && ! $mirror_url =~ $invalid_url_chars ]] ||
    fail 'Expected a public HTTPS mirror URL'
[[ $mirror_url == */ ]] || mirror_url+=/
mirror=$(realpath -e -- "${CPS_MIRROR_DIR:?Set CPS_MIRROR_DIR to the deployed mirror directory}")
root=$(realpath -e -- "${CPS_MIRROR_TUF_ROOT:?Set CPS_MIRROR_TUF_ROOT to the trusted root JSON}")
key=$(realpath -e -- "${CPS_MIRROR_PUBLISHER_KEY:?Set CPS_MIRROR_PUBLISHER_KEY to the trusted publisher public key}")
signer=$(realpath -e -- "${CPS_RELEASE_NOTES_SIGNING_KEY:?Set CPS_RELEASE_NOTES_SIGNING_KEY to the OpenSSH signing key}")
for file in "$root" "$key" "$signer"; do
    [[ -f $file && $file != "$mirror/"* ]] ||
        fail 'Keep the trusted inputs and release notes signing key outside the public mirror'
done
[[ -f $mirror/publisher.json && ! -L $mirror/publisher.json ]] || fail 'Missing deployed publisher.json'

stage=$(mktemp -d)
trap 'rm -rf -- "$stage"' EXIT
cp -- "$root" "$stage/root.json"
cp -- "$key" "$stage/publisher.der"
cp -- "$mirror/publisher.json" "$stage/publisher.json"
descriptor=$stage/publisher.json
[[ $(stat -c %s -- "$descriptor") -le 2097152 ]] || fail 'Deployed publisher.json exceeds the CPS size limit'
jq -e --arg publisher "$publisher" \
    '.schema_version == 1 and .kind == "publisher-bootstrap" and .publisher == $publisher' "$descriptor" >/dev/null ||
    fail 'The deployed descriptor names a different publisher or schema'
jq -er '.tuf_root' "$descriptor" | base64 -d | cmp -s -- "$stage/root.json" - ||
    fail 'The deployed descriptor has a different TUF root'
jq -er '.trust_key' "$descriptor" | base64 -d | cmp -s -- "$stage/publisher.der" - ||
    fail 'The deployed descriptor has a different publisher key'

root_hash=$(sha256sum < "$stage/root.json")
key_hash=$(sha256sum < "$stage/publisher.der")
notes=$stage/release-notes.md
{
    printf '## CPS mirror release\n\n'
    printf '%s\n' '```text'
    printf 'Publisher: %s\nMirror: %s\nCatalog SHA-256: %s\n' "$publisher" "$mirror_url" "$catalog"
    printf 'TUF root SHA-256: %s\nPublisher key SHA-256: %s\n' "${root_hash%% *}" "${key_hash%% *}"
    printf '%s\n\n' '```'
    printf '%s\n' 'Verify the attached release-notes.md and its signature with your trusted CPS release notes signing key before accepting these fingerprints.'
} > "$notes"

public_key=$(ssh-keygen -y -f "$signer" </dev/null)
[[ $public_key == 'ssh-ed25519 '* ]] || fail 'Expected an Ed25519 release notes signing key'
ssh-keygen -Y sign -f "$signer" -n cps-release-notes "$notes" </dev/null

tag=cps-mirror-$catalog
if ! release=$(gh release view "$tag" --json isDraft,assets 2>/dev/null); then
    gh release create "$tag" --draft --latest=false --target "${GITHUB_SHA:?Set the release commit SHA}" \
        --title "CPS $publisher catalog ${catalog:0:12}" --notes-file "$notes"
    release='{"isDraft":true,"assets":[]}'
fi
draft=$(jq -r '.isDraft' <<< "$release")
mkdir "$stage/previous"
for asset in release-notes.md release-notes.md.sig; do
    if jq -e --arg asset "$asset" 'any(.assets[]; .name == $asset)' <<< "$release" >/dev/null; then
        gh release download "$tag" --pattern "$asset" --dir "$stage/previous"
        cmp -s -- "$stage/$asset" "$stage/previous/$asset" || fail "Conflicting release asset $asset for $tag"
    else
        [[ $draft == true ]] || fail "The published release $tag is missing $asset"
        gh release upload "$tag" "$stage/$asset"
    fi
done
if [[ $draft == true ]]; then
    gh release edit "$tag" --draft=false --latest=false --notes-file "$notes"
fi

printf 'Signed mirror release notes available for %s\n' "$tag"
