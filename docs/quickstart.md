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
openssl pkey -inform DER -in private.der -pubout -outform DER -out public.der

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
    trust-key "public.der"
}
CONFIG

"$cps" publish --repo repo --proto proto --signing-key private.der hello.kdl
"$cps" image-create --root image --config image.kdl
"$cps" plan --root image --output install.json install demo/hello
"$cps" fetch --root image --plan install.json
"$cps" validate --root image install.json
"$cps" apply --root image install.json
"$cps" verify --root image --pristine
cat image/usr/share/cps-demo/hello
```

You should see `Hello from Canopa`. The temporary directory contains the
prototype, throwaway signing keys, signed repository, saved plan, and
installed image. The package manifest uses your UID and GID so CPS can
install the file without elevated privileges.

- [Inspect the package and its ownership](client/queries.md)
- [Update, remove, or repair a package in an image](client/images.md)
- [Choose a setup for your environment](setups/README.md)
