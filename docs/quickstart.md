# Quickstart

[Documentation](README.md)

If you already have a [CPS-managed Btrfs system](client/system.md), run
`cps search htop` and then `sudo cps install apps/htop` if the search
returns that package. Use `sudo cps upgrade --dry-run` to preview updates.

## Try CPS in a disposable directory

This walkthrough publishes a one-file package, creates an empty image,
and installs the package into that image. Run it as your regular user
in a shell, starting at the root of the CPS source checkout. You need
[the build prerequisites](developer/README.md#build-cps) and OpenSSL with
Ed25519 support.

```sh
cargo build --locked --bin cps
cps="$PWD/target/debug/cps"
work=$(mktemp -d)
cd "$work"
mkdir -p proto/usr/share/cps-demo
printf 'Hello from Canopa\n' > proto/usr/share/cps-demo/hello

openssl genpkey -algorithm ED25519 -outform DER -out private.der
"$cps" repo trust init --repo repo --publisher canopa --key private.der

cat > hello.kdl <<MANIFEST
schema-version 1 kind="package-manifest"
package "demo/hello" publisher="canopa" version="1.0.0-0" {
    file "usr/share/cps-demo/hello" mode="0644" uid=$(id -u) gid=$(id -g)
}
MANIFEST

cat > image.kdl <<'CONFIG'
schema-version 1 kind="image-config"
publisher "canopa" {
    repository "repo"
    trust-root "repo/trust/1.json"
}
CONFIG

"$cps" publish manifests --repo repo --proto proto --key private.der hello.kdl
"$cps" image create --root image --config image.kdl
"$cps" install --root image -y demo/hello
"$cps" verify --root image --pristine
cat image/usr/share/cps-demo/hello
```

You should see `Hello from Canopa`. The temporary directory holds the
prototype tree, a throwaway signing key, the signed repository with its trust
root, and the installed image.

- `repo trust init` creates version 1 of the publisher's
  [trust root](concepts.md#publishers-catalogs-and-trust) in
  `repo/trust/1.json`, with `private.der` as both its only root key and its
  only catalog key. The image configuration pins that file. A production
  publisher keeps its root keys offline and separate from its catalog keys, as
  described in [Publisher trust root](publisher/publishing.md#publisher-trust-root).
- `publish manifests` reads the file contents from `proto`, stores the
  payload, and signs a new catalog that lists `demo/hello`.
- `install --root image` runs every step of a
  [transaction](concepts.md#images-and-transactions): it resolves the change,
  downloads and verifies its payloads, writes the files, and commits the new
  image revision. `-y` confirms the displayed change without a prompt. To
  review a change before it is applied, save the plan with
  `-n --save-plan`, as described in
  [Inactive images](client/images.md#save-review-and-apply-a-plan).
- `verify --pristine` checks every installed file against its manifest and
  confirms that the image holds nothing unmanaged.

The package manifest uses your UID and GID so CPS can install the file
without elevated privileges.

- [Inspect the package and its ownership](client/queries.md)
- [Update, remove, or repair a package in an image](client/images.md)
- [Choose a setup for your environment](setups/README.md)
