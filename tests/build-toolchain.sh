# Helpers shared by the source build fixtures.

append_toolchain_publisher() {
    local pins=$1 config=$2 name=$3 repository=$4 key=$5
    local fingerprint
    fingerprint=$(tail -c 32 -- "$key" | sha256sum | cut -d' ' -f1)
    printf '        publisher "%s" catalog="%s" key-sha256="%s"\n' \
        "$name" "$(cat "$repository/current")" "$fingerprint" >> "$pins"
    printf 'publisher "%s" {\n    repository %s\n    trust-key %s\n}\n' \
        "$name" "$(toolchain_kdl_string "$repository")" "$(toolchain_kdl_string "$key")" >> "$config"
}

toolchain_kdl_string() {
    local value=$1
    value=${value//\\/\\\\}
    value=${value//\"/\\\"}
    printf '"%s"' "$value"
}
