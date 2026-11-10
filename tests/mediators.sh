#!/usr/bin/env bash
set -euo pipefail
project=$(cd -- "$(dirname -- "$0")/.." && pwd)
cps=${CPS_BIN:-$project/target/debug/cps}
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd "$work"
openssl genpkey -algorithm ED25519 -outform DER -out private.der
"$cps" repo trust init --repo repo --publisher canopa --key private.der >/dev/null

publish() {
    local name=$1 priority=$2
    mkdir -p "$name/usr/bin"
    printf '%s\n' "$name" > "$name/usr/bin/editor-$name"
    ln -s "editor-$name" "$name/usr/bin/editor"
    ln -s "editor-$name" "$name/usr/bin/edit"
    cat > package.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "editors/$name" publisher="canopa" version="1.0.0-0" {
    file "usr/bin/editor-$name" mode="0755" uid=$(id -u) gid=$(id -g)
    symlink "usr/bin/editor" target="editor-$name" uid=$(id -u) gid=$(id -g)
    symlink "usr/bin/edit" target="editor-$name" uid=$(id -u) gid=$(id -g)
    mediate "usr/bin/editor" group="editor" priority=$priority
    mediate "usr/bin/edit" group="editor" priority=$priority
}
MANIFEST
    chmod 0755 "$name/usr/bin/editor-$name"
    "$cps" publish --repo repo --proto "$name" --signing-key private.der package.kdl
}
assert_links() {
    test "$(readlink "$1/usr/bin/editor")" = "$2"
    test "$(readlink "$1/usr/bin/edit")" = "$2"
    "$cps" verify --root "$1" --pristine
}
publish basic 10
publish advanced 20
cat > image.kdl <<CONFIG
schema-version 1 kind="image-config"
publisher "canopa" { repository "repo"; trust-root "repo/trust/1.json"; }
CONFIG
"$cps" image-create --root image --config image.kdl
"$cps" plan --root image --output install.json install editors/basic editors/advanced
"$cps" apply --root image install.json
assert_links image editor-advanced
test -f image/usr/bin/editor-basic
test -f image/usr/bin/editor-advanced

"$cps" plan --root image --output prefer.json set-mediator editor --provider editors/basic
"$cps" boot-env stage --source image --root staged prefer.json
assert_links image editor-advanced
assert_links staged editor-basic
"$cps" mediator --root staged --json | jq -e '.[0] | .preferred == "editors/basic" and .selected == "editors/basic"'
"$cps" image-lock --root staged --export bundle
"$cps" image-create --root replica --config image.kdl --bundle bundle
"$cps" plan --root replica --output install-lock.json install-lock bundle/image.lock.json
"$cps" apply --root replica install-lock.json
assert_links replica editor-basic

"$cps" plan --root staged --output remove.json remove editors/basic
"$cps" apply --root staged remove.json
assert_links staged editor-advanced
"$cps" mediator --root staged --json | jq -e '.[0] | .preferred == "editors/basic" and .selected == "editors/advanced"'
"$cps" plan --root staged --output reinstall.json install editors/basic
"$cps" apply --root staged reinstall.json
assert_links staged editor-basic
"$cps" plan --root staged --output reset.json set-mediator editor
"$cps" apply --root staged reset.json
assert_links staged editor-advanced
echo 'Verified mediated defaults, persistent preferences, boot staging, and offline reprovisioning'
