# Develop CPS

[Documentation](../README.md) · [Concepts](../concepts.md) ·
[CLI reference](../reference/README.md)

## Build CPS

Build on Linux with `/proc` and kernel support for `openat2`. Install a Rust
toolchain that supports the crate's 2024 edition and locked dependencies, a
C linker, pkg-config, and the SQLite and libseccomp development libraries.
The [CI Dockerfile](../../ci/Dockerfile) and its [Rust archive hashes](../../ci/rust-dist.sha256)
describe the pinned build environment used by the project.

Run these commands from the checkout root.

```sh
cargo build --locked --bin cps
target/debug/cps --version
cargo test --locked
```

Use `cargo build --locked --release --bin cps` for an optimized binary in
`target/release/`. [Build the CPS package fixture](build-fixtures.md#building-cps-itself)
to exercise the separately pinned packaging and attestation workflow.

## Build the HTML manual

Use Python 3.11 or newer. From the checkout root, install the Markdown
renderer and build the site.

```sh
python3 -m venv target/docs-venv
target/docs-venv/bin/python -m pip install -r docs/website/requirements.txt
target/docs-venv/bin/python docs/website/build.py
```

Open `target/docs/index.html` directly in a browser. The site also works on
any static web host. To serve a local preview, run
`python3 -m http.server --bind 127.0.0.1 --directory target/docs 8000`
and open [the local preview](http://127.0.0.1:8000/).

Edit the Markdown chapters to update both the repository docs and the
website. The build checks local links and anchors. Use `--output DIR` to
write to a fresh destination for deployment.

## Follow a command through the code

| Area | Entry points |
| --- | --- |
| Argument parsing and command dispatch | [`src/main.rs`](../../src/main.rs) and library exports in [`src/lib.rs`](../../src/lib.rs) |
| Running-system orchestration | [`src/system.rs`](../../src/system.rs) and live services in [`src/systemd.rs`](../../src/systemd.rs) |
| Image state, publisher trust, and transaction history | [`src/image/`](../../src/image) |
| Saved plans, dependency selection, and update policy | [`src/plan/`](../../src/plan) |
| Staging, apply, and installed-state verification | [`src/apply.rs`](../../src/apply.rs) and [`src/verify.rs`](../../src/verify.rs) |
| Signed catalogs, build evidence, and mirror sync | [`src/repo.rs`](../../src/repo.rs), [`src/attestation.rs`](../../src/attestation.rs), and [`src/repo/release/`](../../src/repo/release) |
| Pinned builds, sandboxing, and Git installs | [`src/build/`](../../src/build) and [`src/git_install.rs`](../../src/git_install.rs) |
| KDL input and boot layouts | [`src/kdl_format.rs`](../../src/kdl_format.rs) and [`src/boot/`](../../src/boot) |

## Internal records

Pending transaction journals in the image database carry `"format": 1`.
Completed transaction details use the checksum prefix
`cps transaction details v1` followed by a newline. A synchronized cache
stores its trust binding in `.tuf/origin` with the first line
`cps mirror cache v1`. Profile retention records, catalog-pinned file
indexes, Git snapshot reservations, and signer state use JSON
`"format": 1`. An interrupted partition replacement is recorded in an
ESP marker beginning with `cps slot retirement v1`.

## Exercise the relevant workflow

Run the Rust tests for changes to the solver, catalog validation, and
transaction state. The shell tests exercise the compiled CLI against
disposable repositories, image roots, mirrors, and disks. Their
prerequisites depend on the workflow.

| Change | Test entry points |
| --- | --- |
| Package identity, dependency inference, or freezes | [`package-identity.sh`](../../tests/package-identity.sh), [`manifest-dependencies.sh`](../../tests/manifest-dependencies.sh), [`package-freezes.sh`](../../tests/package-freezes.sh) |
| Apply preflight, progress, or offline systemd | [`elf-transactions.sh`](../../tests/elf-transactions.sh), [`transaction-progress.sh`](../../tests/transaction-progress.sh), [`systemd-actions.sh`](../../tests/systemd-actions.sh) |
| Isolated builds or source pinning | [`build.sh`](../../tests/build.sh), [`build-outputs.sh`](../../tests/build-outputs.sh), [`sources.sh`](../../tests/sources.sh), [`systemd-build-worker.sh`](../../tests/systemd-build-worker.sh) |
| Release gates or mirror sync | [`release-ci.sh`](../../tests/release-ci.sh), [`sharded-mirror.sh`](../../tests/sharded-mirror.sh), [`deploy-mirror.sh`](../../tests/deploy-mirror.sh) |
| Boot deployment, fallback, or blessing | [VM and disk tests](boot-tests.md) |

See the [CI workflow](../../.github/workflows/ci.yml) for the commands,
fixtures, and environment used in CI. When you change CLI arguments or
help, [regenerate the man pages](../reference/README.md#regenerate-the-man-pages)
and update the affected guide examples.

## Measure catalog operations

Run the benchmark on a disposable signed repository.

```sh
cargo build --locked --bin cps --example tuf-fixture
python3 tests/benchmark-catalog.py --packages 1000 --versions 2 --closure 128
```

The benchmark emits JSON wall-clock timings. It publishes version history,
installs and upgrades a dependency closure with conflicting alternatives,
verifies the installed root, and compares file lookup before and after
publishing a signed index. It also measures TUF export and metadata refresh
into empty and existing caches using a local mirror. The package count,
version history, and closure size are configurable. Use release binaries
via `--cps` and `--tuf-fixture` for production measurements. The runner uses
the local filesystem and does not flush the operating system's file cache.
