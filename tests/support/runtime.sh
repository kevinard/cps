# Copy host executables and their shared library closure into a prototype
# tree, and print the matching file actions, so trigger handlers can run
# inside a test image. Source this file, then call:
#   ship_runtime PROTO EXECUTABLE...
ship_runtime() {
    local proto=$1
    shift
    local -A seen=()
    local binary path
    for binary in "$@"; do
        while IFS= read -r path; do
            [[ -n $path && -z ${seen[$path]:-} ]] || continue
            seen[$path]=1
            mkdir -p -- "$proto/$(dirname -- "${path#/}")"
            cp -L -- "$path" "$proto/${path#/}"
            printf '    file "%s" mode="0755" uid=%s gid=%s\n' "${path#/}" "$(id -u)" "$(id -g)"
        done < <(
            printf '%s\n' "$binary"
            ldd "$binary" | awk '$1 ~ /^\// { print $1 } $3 ~ /^\// { print $3 }'
        )
    done
}
