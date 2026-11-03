# Canopa Package System

This Rust prototype manages disposable Linux directory images from a signed local repository. It stores immutable package manifests and SHA-256 file payloads, plans changes across the image, and verifies installed files offline.

## Build and try it

Build with `cargo build`. The prototype needs Linux with `openat2`, `/proc`, and the SQLite development library. The example uses OpenSSL to generate a local Ed25519 signing key. Run it as your regular user, with package UID and GID matching that user.

```sh
cps="$PWD/target/debug/cps"
work=$(mktemp -d)
cd "$work"
mkdir -p proto/usr/share/cps-demo
printf 'Hello from Canopa\n' > proto/usr/share/cps-demo/hello

openssl genpkey -algorithm ED25519 -outform DER -out private.der
openssl pkey -inform DER -in private.der -pubout -outform DER -out public.der

cat > hello.toml <<MANIFEST
format = 1
publisher = "canopa"
name = "demo/hello"
version = "1.0.0-0"

[[action]]
kind = "file"
path = "usr/share/cps-demo/hello"
mode = "0644"
uid = $(id -u)
gid = $(id -g)
MANIFEST

"$cps" publish --repo repo --proto proto --signing-key private.der hello.toml
"$cps" image-create --root image --repo repo --publisher canopa --trust-key public.der
"$cps" plan --root image --output install.json install demo/hello
"$cps" apply --root image install.json
"$cps" verify --root image
```

To upgrade, publish the revised recipe under a new version and run `cps plan --root image --output upgrade.json update`, then apply the saved plan. Each plan pins a signed catalog and an image revision. Keep the saved plan until apply finishes. If interrupted, rerun `apply` with the same plan. Once writes begin, recovery uses the payloads staged inside the image and can finish offline.

## Manifest and image behavior

- Versions have the numeric form `major.minor.patch-revision`. A `[[dependency]]` has a `kind` of `require` or `incorporate`, a `package` name, and a version constraint such as `">=1.0.0-0, <2.0.0-0"`. Only `require` pulls in an absent package.
- Actions support `file`, `dir`, and `symlink`. A symlink specifies `target` in place of `mode`. The optional `arch` and `facet` selectors match image policy. Facets default to enabled and can be set with `image-create --facet doc.man=false`.
- Files marked `preserve = true` keep local edits through an upgrade, including a package ownership transfer at the same path. When the vendor file changes, find its new copy in `image/.cps/config-updates/<revision>/`.
- The image must be inactive and reside on one filesystem. The prototype rejects directory metadata changes and transitions between a directory and a file or symlink. Directories left behind when their ownership ends are retained.

Run `cargo test` for the desktop upgrade and interruption test.
