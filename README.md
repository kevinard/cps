# Canopa Package System

CPS is an image-oriented Linux package manager written in Rust and inspired by
IPS. It installs signed packages into images, applies every change as a
recoverable transaction, stages changes that need a reboot into trial boot
environments, and builds packages reproducibly from pinned inputs.

- **Signed publishing.** Each publisher signs sharded catalogs under a
  threshold-signed trust root that supports key rotation, revocation, and
  recovery. Signing keys can stay in an HSM or KMS behind an external signer.
  Signed TUF mirrors and index-only caches distribute the catalogs.
- **Image composition.** Signed release profiles, image locks, and offline
  image bundles reproduce an exact composition. Facets select optional content
  such as documentation or debug symbols, and variants such as `arch` select
  between alternatives.
- **Recoverable transactions.** Plans are saved, fetched, and validated before
  apply. A journaled transaction can be resumed or aborted after an
  interruption. Writes follow the dependency order, directories can turn into
  files or symlinks atomically, and sandboxed triggers regenerate shared caches
  and the initramfs. `cps verify` checks every managed file offline.
- **Boot environments.** Changes that need a reboot stage into a Btrfs
  snapshot or an ext4 root slot and boot as a trial with systemd-boot. A
  trial becomes known good after its health check passes, and otherwise the
  next boot falls back to the previous root. On Btrfs, CPS can also sign a
  unified kernel image (UKI) for each environment, have UEFI firmware boot it
  directly, and prune old environments automatically.
- **Reproducible builds.** Offline sandboxed builds from pinned sources and
  toolchains can produce several packages from one recipe and record
  worker-signed attestations that anyone can check with an independent
  rebuild. Publication derives shared-library dependencies from the ELF
  objects, and CPS plans build order and rebuilds across recipes. Projects can
  also be installed directly from Git.

## Documentation

- [Quickstart](docs/quickstart.md) — publish and install a small package in a disposable image.
- [Concepts and vocabulary](docs/concepts.md) — packages, catalogs and trust roots, transactions, facets and variants, and boot environments.
- [Full documentation](docs/README.md) — guides for clients, package maintainers, publishers, and developers.
- [Setup guides](docs/setups/README.md) — local images, mirror-backed profiles, Btrfs systems, and ext4 deployments.
- [CLI reference and man pages](docs/reference/README.md) — command syntax and local manual installation.
- [Data format reference](docs/reference/README.md#configuration-and-data-formats) — manifests, signed catalogs, image state and locks, saved plans, build provenance, payload deltas, and boot metadata.
- [HTML documentation](docs/developer/README.md#build-the-html-manual) — build a searchable site from the same guides.

Build from source with `cargo build --locked --bin cps`. See the
[developer guide](docs/developer/README.md#build-cps) for prerequisites.

[MIT license](LICENSE)
