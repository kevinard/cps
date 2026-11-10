# Helpers shared by the source build fixtures.

# NAME REPOSITORY TRUST_ROOT pins a toolchain publisher. key-sha256 is the
# SHA-256 of the pinned trust root file.
append_toolchain_publisher() {
    local pins=$1 config=$2 name=$3 repository=$4 trust=$5
    local fingerprint
    fingerprint=$(sha256sum < "$trust" | cut -d' ' -f1)
    printf '        publisher "%s" catalog="%s" key-sha256="%s"\n' \
        "$name" "$(cat "$repository/current")" "$fingerprint" >> "$pins"
    printf 'publisher "%s" {\n    repository %s\n    trust-root %s\n}\n' \
        "$name" "$(toolchain_kdl_string "$repository")" "$(toolchain_kdl_string "$trust")" >> "$config"
}

toolchain_kdl_string() {
    local value=$1
    value=${value//\\/\\\\}
    value=${value//\"/\\\"}
    printf '"%s"' "$value"
}
