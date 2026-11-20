#!/usr/bin/env bash
# Run a command in the pinned CI image as the runner's UID.
#
# Usage: ci/run.sh [docker run options...] -- COMMAND [ARG...]
#
# The image is CPS_CI_IMAGE (default cps-ci). The workspace and RUNNER_TEMP are
# mounted at their own paths, Cargo keeps its home under RUNNER_TEMP, and tests
# keep their images on the runner's disk: overlayfs, the container's own /tmp,
# cannot hold a trigger handler's overlay.
set -euo pipefail

image=${CPS_CI_IMAGE:-cps-ci}
temp=${RUNNER_TEMP:?Set RUNNER_TEMP}
mkdir -p "$temp/cps-cargo" "$temp/cps-tmp"

# The runner's UID needs an account in the container: ssh-keygen refuses to run
# without one.
if [[ ! -e $temp/cps-passwd || ! -e $temp/cps-group ]]; then
    docker run --rm "$image" cat /etc/passwd > "$temp/cps-passwd"
    docker run --rm "$image" cat /etc/group > "$temp/cps-group"
    printf 'runner:x:%s:%s::/tmp:/usr/bin/bash\n' "$(id -u)" "$(id -g)" >> "$temp/cps-passwd"
    printf 'runner:x:%s:\n' "$(id -g)" >> "$temp/cps-group"
fi

options=()
while (($#)) && [[ $1 != -- ]]; do
    options+=("$1")
    shift
done
if (($# < 2)); then
    echo "usage: ci/run.sh [docker run options...] -- COMMAND [ARG...]" >&2
    exit 2
fi
shift

exec docker run --rm --user "$(id -u):$(id -g)" --env HOME=/tmp --env RUNNER_TEMP \
    --env TMPDIR="$temp/cps-tmp" \
    --volume "$temp/cps-passwd:/etc/passwd:ro" \
    --volume "$temp/cps-group:/etc/group:ro" \
    --env CARGO_HOME="$temp/cps-cargo" --volume "$PWD:$PWD" \
    --volume "$temp:$temp" --workdir "$PWD" "${options[@]}" "$image" "$@"
