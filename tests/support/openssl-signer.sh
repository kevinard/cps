#!/usr/bin/env bash
# Example CPS external signer (protocol 1) for tests and as a template.
#
# A signer descriptor runs this script with the private key path as its only
# argument. CPS writes the message to stdin and reads the raw signature from
# stdout. A production helper would forward the message to a PKCS#11 token,
# an HSM, or a cloud KMS instead of reading a local key file.
set -euo pipefail
umask 077
[[ $# == 1 && ${CPS_SIGNER_PROTOCOL:-} == 1 ]] ||
    { printf 'usage: CPS_SIGNER_PROTOCOL=1 %s PRIVATE_KEY\n' "$0" >&2; exit 2; }
message=$(mktemp)
trap 'rm -f -- "$message"' EXIT
cat > "$message"
case ${CPS_SIGNER_ALGORITHM:-} in
    ed25519) openssl pkeyutl -sign -rawin -inkey "$1" -in "$message" ;;
    rsassa-pss-sha256)
        openssl dgst -sha256 -sign "$1" -sigopt rsa_padding_mode:pss -sigopt rsa_pss_saltlen:digest \
            -binary "$message" ;;
    ecdsa-sha2-nistp256) openssl dgst -sha256 -sign "$1" -binary "$message" ;;
    *) printf 'unsupported signing algorithm %s\n' "${CPS_SIGNER_ALGORITHM:-}" >&2; exit 2 ;;
esac
