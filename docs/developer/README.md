# Develop CPS

[Documentation](../README.md) · [Concepts](../concepts.md) ·
[CLI reference](../reference/README.md)

## On this page

- [Build CPS](#build-cps)
- [Build the HTML manual](#build-the-html-manual)
- [Code layout](#code-layout)
- [Internal records](#internal-records)
- [Run the unit tests](#run-the-unit-tests)
- [Run the shell scenarios](#run-the-shell-scenarios)
- [Measure performance](#measure-performance)

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
```

Use `cargo build --locked --release --bin cps` for an optimized binary in
`target/release/`. Several tests also use the disposable TUF signer
example, built with `cargo build --locked --example tuf-fixture` into
`target/debug/examples/tuf-fixture`. [Build the CPS package fixture](build-fixtures.md#building-cps-itself)
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
write to a fresh destination for deployment. When you change CLI arguments
or help, [regenerate the man pages](../reference/README.md#regenerate-the-man-pages)
and update the affected guide examples.

## Code layout

[`src/main.rs`](../../src/main.rs) parses arguments and dispatches to the
library in [`src/lib.rs`](../../src/lib.rs). Each area below has its own
module.

| Area | Modules |
| --- | --- |
| Package model: manifests, actions, versions, facet and variant selection, licence and homepage metadata | [`src/model.rs`](../../src/model.rs), [`src/model/version.rs`](../../src/model/version.rs), [`src/model/selection.rs`](../../src/model/selection.rs), [`src/model/metadata.rs`](../../src/model/metadata.rs) |
| KDL documents: manifests, recipes, policies, release files, signer descriptors, worker limits, boot receipts | [`src/kdl_format.rs`](../../src/kdl_format.rs) and [`src/kdl_format/`](../../src/kdl_format) |
| Manifest generation, dependency review, and generated `cap/elf/...` capabilities | [`src/generate.rs`](../../src/generate.rs), [`src/generate/dependencies.rs`](../../src/generate/dependencies.rs), [`src/generate/elf.rs`](../../src/generate/elf.rs) |
| Saved plans, dependency resolution, freezes, renames, repair, and update policy | [`src/plan/mod.rs`](../../src/plan/mod.rs), [`src/plan/resolve.rs`](../../src/plan/resolve.rs) with the SAT solver in [`src/plan/resolve/sat.rs`](../../src/plan/resolve/sat.rs), and the other [`src/plan/`](../../src/plan) modules |
| Dependency order of transaction writes and removals | [`src/plan/order.rs`](../../src/plan/order.rs) |
| Applying, resuming, and aborting transactions, and the ELF regression check | [`src/apply.rs`](../../src/apply.rs), [`src/apply/elf.rs`](../../src/apply/elf.rs) |
| Trigger planning, Bubblewrap handlers, overlays, and generated paths | [`src/triggers.rs`](../../src/triggers.rs) |
| Installed-state verification, including `verify --quick` | [`src/verify.rs`](../../src/verify.rs) |
| Image state, path transitions, publishers, locks, and transaction history | [`src/image/`](../../src/image), with path exchanges in [`src/image/root.rs`](../../src/image/root.rs) |
| Running-system commands and live services | [`src/system.rs`](../../src/system.rs), [`src/systemd.rs`](../../src/systemd.rs) |
| Read-only queries | [`src/query.rs`](../../src/query.rs) |
| Publishing, withdrawals, renames, and catalog validation | [`src/repo.rs`](../../src/repo.rs) |
| Catalog roots and content-addressed shards | [`src/repo/shards.rs`](../../src/repo/shards.rs) |
| Signing keys, envelopes, publisher trust roots, and external signers | [`src/repo/keys.rs`](../../src/repo/keys.rs), [`src/repo/keys/trust.rs`](../../src/repo/keys/trust.rs), [`src/repo/keys/signer.rs`](../../src/repo/keys/signer.rs) |
| Mirror export, TUF signing and refresh, sync, deltas, publisher discovery, and cache pruning | [`src/repo/release.rs`](../../src/repo/release.rs) and [`src/repo/release/`](../../src/repo/release), with [`sign.rs`](../../src/repo/release/sign.rs) for `sign-export` and `refresh-metadata` |
| Release profiles and the ELF profile gate | [`src/repo/profiles.rs`](../../src/repo/profiles.rs) and [`src/repo/profiles/`](../../src/repo/profiles) |
| Rebuild inputs, source archives, and Git snapshots | [`src/repo/rebuild.rs`](../../src/repo/rebuild.rs), [`src/repo/source_archive.rs`](../../src/repo/source_archive.rs), [`src/repo/git_snapshots.rs`](../../src/repo/git_snapshots.rs) |
| Build attestations and builder trust | [`src/attestation.rs`](../../src/attestation.rs) |
| Pinned builds, sandboxing, collected file lists, stripping, and Git installs | [`src/build/`](../../src/build) and [`src/git_install.rs`](../../src/git_install.rs) |
| Build order, rebuild plans, and recipe pin refreshes across recipes | [`src/orchestrate.rs`](../../src/orchestrate.rs) |
| Boot environments: ext4 export and deployment, Btrfs staging, ESP edits, kernels, UKIs | [`src/boot/`](../../src/boot) |
| systemd-boot entries, `loader.conf` ownership, direct UEFI boot, destruction, and retention | [`src/boot/systemd_boot.rs`](../../src/boot/systemd_boot.rs) with [`policy.rs`](../../src/boot/systemd_boot/policy.rs), [`uefi.rs`](../../src/boot/systemd_boot/uefi.rs), [`destroy.rs`](../../src/boot/systemd_boot/destroy.rs), and [`prune.rs`](../../src/boot/systemd_boot/prune.rs) |
| Bounded HTTPS transfers and file capabilities | [`src/transport.rs`](../../src/transport.rs), [`src/file_caps.rs`](../../src/file_caps.rs) |
| Cross-module tests | [`src/tests.rs`](../../src/tests.rs) and [`src/tests/`](../../src/tests) |

Most modules also hold their own unit tests.

## Internal records

Pending transaction journals in the image database carry `"format": 1`.
Completed transaction details use the checksum prefix
`cps transaction details v1` followed by a newline. A synchronized cache
stores its trust binding in `.tuf/origin` with the first line
`cps mirror cache v1`. Profile retention records, catalog-pinned file
indexes, Git snapshot reservations, and mirror signer state use JSON
`"format": 1`. A self-contained catalog archive, which packs a catalog
root and its shards into image state, is a Zstandard stream that begins
with `cps catalog archive v1`. An interrupted partition replacement is recorded
in an ESP marker beginning with `cps slot retirement v1`.

## Run the unit tests

```sh
TMPDIR=$PWD/target/tmp cargo test --locked
```

Create `target/tmp` first. CI runs `cargo test --frozen --all-targets`.
Pass a name filter to run part of the suite, for example
`cargo test --locked tests::transitions` or `cargo test --locked shards`.
The unit tests need no privileges. Some use host tools:

| Tool | Tests |
| --- | --- |
| `/usr/bin/bwrap` with unprivileged user namespaces and overlay support | Trigger handlers in `src/tests/triggers.rs`. They print `Skipping: trigger handlers need a working /usr/bin/bwrap` and pass when the sandbox does not start. |
| `openssl` | External signer tests. They skip when OpenSSL is missing. |
| `ssh-keygen` | Git signature tests. They skip when it is missing. |
| `sfdisk` | GPT reader tests with 512-byte and 4096-byte sectors |
| `git`, `ldd` | Git install and trigger fixtures |

Hosts that restrict unprivileged user namespaces, such as Ubuntu with
AppArmor, need `sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`
or an AppArmor profile for `bwrap`, as in the CI workflow. Bubblewrap needs
overlay support (`--overlay-src`) for trigger handlers.

### Crash interruption hooks

The transaction tests simulate a crash by re-running the test binary as a
worker that exits at a chosen point, then resuming or aborting the
interrupted transaction. These environment variables are compiled only into
the test binary and have no effect on `cps` itself. The tests set them; you
do not set them by hand.

| Variable | Effect in the worker |
| --- | --- |
| `CPS_TEST_IMAGE`, `CPS_TEST_PLAN` | Run the worker against this image and saved plan |
| `CPS_TEST_AFTER_PATH` | Exit after writing this path |
| `CPS_TEST_INTERRUPT_STEP` | Exit at a named step: `staged`, `exchanged`, `displaced`, or `published` in a path transition, `triggers` after the trigger runs, or `abort-retired`, `abort-trigger-N`, and `abort-pruned` during an abort |
| `CPS_TEST_NO_EXCHANGE` | Use the fallback without `RENAME_EXCHANGE`, which moves the old object aside first |
| `CPS_TEST_ABORT` | Run `cps transaction abort` instead of apply |

## Run the shell scenarios

The scripts in [`tests/`](../../tests) exercise the compiled CLI against
disposable repositories, image roots, mirrors, and disks. Run them from
the checkout root with `bash tests/NAME.sh` after building CPS. Each script
works in a new directory from `mktemp`, so set `TMPDIR` to a local
filesystem with space for repositories and images, such as
`TMPDIR=$PWD/target/tmp`. Scripts that apply triggers create their overlays
inside the image, so that filesystem must also work as an overlay upper
directory.

The scripts find CPS through one of two variables, defaulting to
`target/debug/cps`:

| Variable | Read by |
| --- | --- |
| `CPS_BIN` | `deploy-mirror.sh`, `hwdb-trigger.sh`, `mediators.sh`, `release-ci.sh`, `release-notes.sh`, `sharded-mirror.sh`, `systemd-actions.sh`, `triggers.sh`, and the VM tests |
| `CPS_TEST_BINARY` | The other scenarios and the fixture builders |
| `CPS_TUF_FIXTURE` | `sharded-mirror.sh`: the disposable TUF signer, defaulting to `target/debug/examples/tuf-fixture` |

CI sets both CPS variables to the same binary and runs this set:

```sh
export CPS_BIN="$PWD/target/debug/cps" CPS_TEST_BINARY="$PWD/target/debug/cps"
export CPS_TUF_FIXTURE="$PWD/target/debug/examples/tuf-fixture"
for test in package-identity elf-transactions systemd-actions hwdb-trigger triggers mediators \
    package-freezes git-snapshots release-ci release-notes sharded-mirror deploy-mirror boot-export \
    recipe-orchestration; do
  bash "tests/$test.sh"
done
bash tests/boot-deploy.sh --esp
bash tests/transaction-progress.sh
```

Every scenario uses OpenSSL to create disposable keys.

| Scenario | Covers | Also needs |
| --- | --- | --- |
| [`package-identity.sh`](../../tests/package-identity.sh) | Upstream version spelling, package IDs, and architectures | |
| [`manifest-dependencies.sh`](../../tests/manifest-dependencies.sh) | Generated library capabilities, offline dependency suggestions, and direct requirement checks | A C compiler, Python 3 |
| [`package-freezes.sh`](../../tests/package-freezes.sh) | Offline freezes, version constraints, staged unfreezing | |
| [`mediators.sh`](../../tests/mediators.sh) | Mediated defaults, preferences, boot staging, and reprovisioning | `jq` |
| [`transaction-progress.sh`](../../tests/transaction-progress.sh) | Plan validation, progress, read-only roots, and aborting a journaled apply | `jq` |
| [`elf-transactions.sh`](../../tests/elf-transactions.sh) | Generated requirements, transaction ELF regressions, drift, and compatible upgrades | A C compiler |
| [`elf-profile.sh`](../../tests/elf-profile.sh) | The signed ELF profile gate with a merged `/usr` and a companion publisher | A C compiler, GNU `objcopy` with `--strip-section-headers` |
| [`systemd-actions.sh`](../../tests/systemd-actions.sh) | Offline sysusers, tmpfiles, and presets on a cloned root | `systemd-sysusers`, `systemd-tmpfiles`, `systemctl`. It works under `/tmp`. |
| [`triggers.sh`](../../tests/triggers.sh) | Ordered, confined, journaled, and removable trigger outputs, and a generated initramfs | `bwrap` as for the unit tests, `ldd`, `rg`. With `kernel-install` and its `90-loaderentry.install` plugin it also checks kernel preparation. |
| [`hwdb-trigger.sh`](../../tests/hwdb-trigger.sh) | The systemd hardware database as an ordinary trigger, retry, and abort | `bwrap`, `systemd-hwdb`, `rg` |
| [`git-snapshots.sh`](../../tests/git-snapshots.sh) | Git selectors, tag versions, snapshot reservations, and concurrent preparation | `git`, `jq` |
| [`sources.sh`](../../tests/sources.sh) | HTTPS source fetching, cache races, and offline import | Python 3 |
| [`build.sh`](../../tests/build.sh) | Isolated builds, reproducibility, signed provenance, and installation | A C compiler, `make`, Python 3 |
| [`build-outputs.sh`](../../tests/build-outputs.sh) | One build with several packages, collected file lists, the unpackaged-file check, split debug information, and job counts | A C compiler, binutils, Python 3 |
| [`systemd-build-worker.sh`](../../tests/systemd-build-worker.sh) | `build.sh` inside a delegated systemd user scope with cgroup limits | A systemd user session |
| [`recipe-orchestration.sh`](../../tests/recipe-orchestration.sh) | Build order, cycle reports, pin refreshes and checks, and soname rebuild plans | A C compiler |
| [`release-ci.sh`](../../tests/release-ci.sh) | Prepare, promote, widen, deploy, and refresh with a 2-of-2 catalog threshold and test adapters | `jq`, `rg` |
| [`release-notes.sh`](../../tests/release-notes.sh) | Signed mirror release notes and publication retries with a local GitHub CLI fixture | `jq`, `rg`, `ssh-keygen`, Python 3 |
| [`sharded-mirror.sh`](../../tests/sharded-mirror.sh) | The deployment adapter with sharded targets, signer state checks, and sync | `jq`, `flock`, the TUF fixture |
| [`deploy-mirror.sh`](../../tests/deploy-mirror.sh) | Mirror publication, refresh with only the timestamp key, an external timestamp signer, TUF root rotation and 2-of-3 revocation, publisher catalog key rotation, and descriptor trust root updates | `tuftool`, `jq`, `flock`, `rg` |
| [`boot-export.sh`](../../tests/boot-export.sh) | Atomic ext4 export, late failure cleanup, and concurrent preparation | e2fsprogs with `debugfs`, `setfattr` |
| [`boot-deploy.sh`](../../tests/boot-deploy.sh) | GPT deployment, partition isolation, and damaged layouts. With `--esp`, raw-disk activation and rollback. | e2fsprogs, `sfdisk`, `blkid`, `setfattr`, Python 3. With `--esp`, dosfstools (`mkfs.fat`, `fsck.fat`) and mtools. |

An unprivileged caller can set `CPS_ESP_TOOLS_DIR` to the absolute path of a
directory with trusted `fsck.fat` and mtools binaries. CPS then runs those
instead of the ones on `PATH` when it edits an ESP inside a disk image,
which also applies to `boot-deploy.sh --esp`.

The VM and disk tests and the signed fixture builders are described in
[Test boot and rollback in a VM](boot-tests.md) and
[Build signed packages for tests](build-fixtures.md). The other scripts in
`tests/`, such as `seed-native-c.sh`, `seed-git-build.sh`,
`build-toolchain.sh`, and the `*-guest.sh` files, are helpers that those
scenarios call.

## Measure performance

Two benchmarks print JSON wall-clock timings. Use release binaries for
production measurements. They run on the local filesystem under `TMPDIR`
and do not flush the operating system's file cache.

[`tests/benchmark-catalog.py`](../../tests/benchmark-catalog.py) builds a
reproducible distribution with dependency fan-out, `require-any`,
capabilities, and recommendations, and publishes its version history with
batched `cps publish` calls. It times publishing, installing and upgrading
a dependency closure, publishing one more package into the large catalog,
offline verification, file search before and after a signed file index,
and TUF export and sync into empty and existing caches. It also reports
each command's peak resident set size.

```sh
cargo build --locked --release --bin cps --example tuf-fixture
python3 tests/benchmark-catalog.py --release --packages 30000 --versions 3 --files 40
```

Options set the package count (`--packages`), versions (`--versions`),
file actions per version (`--files`), manifests per signed catalog update
(`--batch`), closure size (`--closure`), and random seed (`--seed`).
`--cps` and `--tuf-fixture` select other binaries, `--work DIR` keeps the
generated repository, and `--skip-mirror` skips the TUF measurements.

[`tests/benchmark-apply.py`](../../tests/benchmark-apply.py) provisions an
image with 50,000 managed files by default, then times installing,
removing, and reinstalling a 10-file package, a 1000-file package upgrade,
and full and `--quick` verification. Repeat `--cps` to compare builds on
copies of the same provisioned image.

```sh
cargo build --locked --release --bin cps
python3 tests/benchmark-apply.py --cps old/cps --cps target/release/cps
```

`--packages`, `--files`, and `--size` shape the image, `--repeat` sets the
verification runs, of which the minimum is reported, and `--work DIR` keeps
the published repository and provisioned image for reuse.
