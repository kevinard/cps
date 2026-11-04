#!/usr/bin/env bash
set -euo pipefail
project=$(cd -- "$(dirname -- "$0")/.." && pwd)
systemd-run --user --scope --quiet --collect -p 'Delegate=cpu memory pids' \
    bash -eu -s -- "$project/tests/build.sh" <<'SCOPE'
IFS= read -r membership < /proc/self/cgroup
case "$membership" in
    0::/*/run-*.scope) ;;
    *) printf 'Expected a temporary delegated systemd scope\n' >&2; exit 1 ;;
esac
root=/sys/fs/cgroup/${membership#0::/}
mkdir "$root/supervisor"
printf '0\n' > "$root/supervisor/cgroup.procs"
printf '+cpu +memory +pids\n' > "$root/cgroup.subtree_control"
export CPS_TEST_CGROUP_ROOT="$root"
exec bash "$1"
SCOPE
