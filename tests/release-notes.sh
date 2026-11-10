#!/usr/bin/env bash
set -euo pipefail
umask 077

project=$(cd -- "$(dirname -- "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd "$work"

mkdir -p mirror bin github/assets
export CPS_MIRROR_DIR=$work/mirror CPS_MIRROR_URL=https://packages.example.test/canopa
export CPS_MIRROR_TUF_ROOT=$work/root.json CPS_MIRROR_TRUST_ROOT=$work/trust-root.json
export CPS_RELEASE_NOTES_SIGNING_KEY=$work/release-signing-key
export GITHUB_SHA=0123456789abcdef0123456789abcdef01234567
catalog=$(printf 'a%.0s' {1..64})
export CPS_TEST_TAG=cps-mirror-$catalog CPS_TEST_GITHUB=$work/github

printf '{"signed":{"_type":"root"}}\n' > root.json
openssl genpkey -algorithm ED25519 -outform DER -out publisher-private.der
"${CPS_BIN:-$project/target/debug/cps}" repo trust init --repo repo --publisher canopa \
    --key publisher-private.der >/dev/null
cp repo/trust/1.json trust-root.json
ssh-keygen -q -t ed25519 -N '' -f "$CPS_RELEASE_NOTES_SIGNING_KEY"
jq -n --rawfile root root.json --arg trust "$(base64 -w0 trust-root.json)" \
    '{schema_version: 1, kind: "publisher-bootstrap", publisher: "canopa", tuf_root: ($root | @base64), trust_root: $trust}' > publisher.json
cp publisher.json mirror/publisher.json

cat > bin/gh <<'GITHUB'
#!/usr/bin/env bash
set -euo pipefail
[[ $1 == release && $3 == "$CPS_TEST_TAG" ]]
cd "$CPS_TEST_GITHUB"
operation=$2
shift 3
case $operation in
    view)
        [[ -f draft ]]
        find assets -type f -printf '%f\n' |
            jq -Rn --argjson draft "$(cat draft)" '{isDraft: $draft, assets: [inputs | {name: .}]}'
        ;;
    create)
        [[ ! -e draft && $1 == --draft && $2 == --latest=false && $3 == --target && $4 == "$GITHUB_SHA" ]]
        printf 'true\n' > draft
        ;;
    upload)
        [[ $(cat draft) == true && ! -e assets/${1##*/} ]]
        [[ ${CPS_TEST_FAIL_SIGNATURE_UPLOAD:-0} != 1 || $1 != *.sig ]]
        cp -- "$1" assets/
        ;;
    download)
        [[ $1 == --pattern && $3 == --dir ]]
        cp -- "assets/$2" "$4"
        ;;
    edit)
        [[ $(cat draft) == true && $1 == --draft=false && $2 == --latest=false && $3 == --notes-file ]]
        [[ -f assets/release-notes.md.sig ]]
        cmp -- "$4" assets/release-notes.md
        printf 'false\n' > draft
        ;;
    *) exit 2 ;;
esac
GITHUB
chmod +x bin/gh
export PATH="$work/bin:$PATH"
publish_notes() { bash "$project/ci/publish-release-notes.sh" canopa "$catalog"; }

jq 'del(.kind)' publisher.json > mirror/publisher.json
if publish_notes; then
    echo 'Signed notes for a publisher descriptor without its kind' >&2
    exit 1
fi
[[ ! -e github/draft ]]

for field in tuf_root trust_root; do
    jq --arg field "$field" '.[$field] = "WA=="' publisher.json > mirror/publisher.json
    if publish_notes; then
        echo "Signed a substituted $field" >&2
        exit 1
    fi
    [[ ! -e github/draft ]]
done
cp publisher.json mirror/publisher.json

if CPS_TEST_FAIL_SIGNATURE_UPLOAD=1 publish_notes; then
    echo 'Published notes after the signature upload failed' >&2
    exit 1
fi
[[ $(cat github/draft) == true && -f github/assets/release-notes.md ]]
[[ ! -e github/assets/release-notes.md.sig ]]
publish_notes
[[ $(cat github/draft) == false ]]

notes=github/assets/release-notes.md
printf 'cps-release namespaces="cps-release-notes" %s\n' "$(cat release-signing-key.pub)" > allowed-signers
ssh-keygen -Y verify -f allowed-signers -I cps-release -n cps-release-notes -s "$notes.sig" < "$notes"
root_hash=$(sha256sum < root.json)
trust_hash=$(sha256sum < trust-root.json)
root_key=$(jq -er '.signed | @base64d | fromjson | .root_keys[0]' trust-root.json)
root_key_hash=$(printf '%s' "$root_key" | python3 -I -c 'import sys; sys.stdout.buffer.write(bytes.fromhex(sys.stdin.read()))' | sha256sum)
rg -Fx "Catalog SHA-256: $catalog" "$notes"
rg -Fx "Mirror: $CPS_MIRROR_URL/" "$notes"
rg -Fx "TUF root SHA-256: ${root_hash%% *}" "$notes"
rg -Fx "Trust root SHA-256: ${trust_hash%% *}" "$notes"
rg -Fx "Root key SHA-256: ${root_key_hash%% *}" "$notes"
# The fingerprint is the one `cps repo trust show` prints for the root key.
"${CPS_BIN:-$project/target/debug/cps}" repo trust show trust-root.json | rg -F "  ${root_key_hash%% *}"
publish_notes

printf '%s\n' 'tampered release notes' >> "$notes"
if publish_notes; then
    echo 'Accepted conflicting published release notes' >&2
    exit 1
fi
if ssh-keygen -Y verify -f allowed-signers -I cps-release -n cps-release-notes -s "$notes.sig" < "$notes"; then
    echo 'Verified a signature over tampered release notes' >&2
    exit 1
fi
printf '%s\n' 'Verified both fingerprints, signed release notes, and publication retries'
