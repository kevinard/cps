#!/usr/bin/env bash
set -euo pipefail
umask 077

fail() { printf '%s\n' "$*" >&2; exit 1; }

# The deployment adapter for ci/release.sh. REPO PUBLISHER CATALOG runs
# `cps repo deploy`, which needs the targets, snapshot, and timestamp keys.
# Adding "refresh" runs `cps repo refresh`, which re-signs the timestamp, and
# the snapshot when it would expire first, for the catalog already deployed.
mode=deploy
if [[ $# == 4 && $4 == refresh ]]; then
    mode=refresh
    set -- "$1" "$2" "$3"
fi
[[ $# == 3 && $3 =~ ^[0-9a-f]{64}$ ]] || fail 'Expected REPO PUBLISHER CATALOG_SHA256 [refresh]'
cps=${CPS_BIN:?Set CPS_BIN to the built CPS executable}

# CPS_MIRROR_TUF_KEYS holds ROLE.pem, ROLE.der, ROLE.signer.kdl, or their
# ROLE-NAME forms for each TUF role. List several keys to meet a threshold.
args=(
    --repo "$1"
    --output "${CPS_MIRROR_DIR:?Set CPS_MIRROR_DIR to the HTTPS document directory}"
    --tuf-root "${CPS_MIRROR_TUF_ROOT:?Set CPS_MIRROR_TUF_ROOT to the original signed TUF root JSON}"
    --trust-root "${CPS_MIRROR_TRUST_ROOT:?Set CPS_MIRROR_TRUST_ROOT to the publisher trust root that clients pin}"
    --key-dir "${CPS_MIRROR_TUF_KEYS:?Set CPS_MIRROR_TUF_KEYS to the TUF release keys directory}"
)
# cps verifies the trust root and takes the publisher from it. Refuse a
# configuration whose trust root belongs to another publisher.
trusted=$(jq -er '.signed | @base64d | fromjson | .publisher | strings' -- "$CPS_MIRROR_TRUST_ROOT") ||
    fail "Cannot read the publisher of $CPS_MIRROR_TRUST_ROOT"
[[ $trusted == "$2" ]] || fail "The trust root is for publisher $trusted, not $2"
if [[ -n ${CPS_MIRROR_TUF_ACTIVE_ROOT:-} ]]; then
    args+=(--active-tuf-root "$CPS_MIRROR_TUF_ACTIVE_ROOT")
fi
if [[ -n ${CPS_MIRROR_TIMESTAMP_LIFETIME:-} ]]; then
    args+=(--timestamp-lifetime "$CPS_MIRROR_TIMESTAMP_LIFETIME")
fi
if [[ -n ${CPS_MIRROR_SNAPSHOT_LIFETIME:-} ]]; then
    args+=(--snapshot-lifetime "$CPS_MIRROR_SNAPSHOT_LIFETIME")
fi

if [[ $mode == refresh ]]; then
    exec "$cps" repo refresh "${args[@]}" --catalog "$3"
fi
args+=(--catalog "$3" --objects-threshold "${CPS_MIRROR_OBJECTS_THRESHOLD:-1}")
if [[ -n ${CPS_MIRROR_TARGETS_LIFETIME:-} ]]; then
    args+=(--targets-lifetime "$CPS_MIRROR_TARGETS_LIFETIME")
fi
if [[ -n ${CPS_MIRROR_INITIAL_VERSION:-} ]]; then
    args+=(--initial-version "$CPS_MIRROR_INITIAL_VERSION")
fi
if [[ -n ${CPS_MIRROR_LIST:-} ]]; then
    args+=(--mirror-list "$CPS_MIRROR_LIST")
fi
exec "$cps" repo deploy "${args[@]}"
