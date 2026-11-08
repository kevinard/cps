# Canopa Package System

CPS is an image-oriented Linux package manager written in Rust and inspired by
IPS. It manages signed repositories and release profiles, applies recoverable
package transactions, stages upgrades into trial boot environments, and verifies
installed files offline. Its build tools use pinned inputs and can produce
reproducible, independently signed build evidence.

- [Quickstart](docs/quickstart.md) — publish and install a small package in a disposable image.
- [Full documentation](docs/README.md) — guides for clients, package maintainers, publishers, and developers.
- [HTML documentation](docs/developer/README.md#build-the-html-manual) — build a searchable site from the same guides.
- [Concepts and vocabulary](docs/concepts.md) — package identity, catalog pins, trust, and boot environments.
- [Setup guides](docs/setups/README.md) — local images, mirror-backed profiles, Btrfs systems, and ext4 deployments.
- [CLI reference and man pages](docs/reference/README.md) — command syntax and local manual installation.

Build from source with `cargo build --locked --bin cps`. See the
[developer guide](docs/developer/README.md#build-cps) for prerequisites.

See the [data format reference](docs/reference/README.md#configuration-and-data-formats)
for package manifests, signed catalogs, image state and locks, saved plans,
build provenance, payload deltas, and boot metadata.

[MIT license](LICENSE)
