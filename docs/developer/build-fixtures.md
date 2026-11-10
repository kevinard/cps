# Build signed packages for tests

[Documentation](../README.md) · [Developers](README.md)

## On this page

- [Building CPS itself](#building-cps-itself)
- [Testing a source-built Linux kernel](#testing-a-source-built-linux-kernel)
- [Testing source-built systemd and systemd-boot](#testing-source-built-systemd-and-systemd-boot)
- [Testing a merged `/usr` image](#testing-a-merged-usr-image)
- [Building util-linux for the boot image](#building-util-linux-for-the-boot-image)
- [Building GNU tools for boot preparation](#building-gnu-tools-for-boot-preparation)
- [Testing a source-built BusyBox](#testing-a-source-built-busybox)

Each builder in `tests/build-*.sh` takes a new output directory and runs
CPS from `CPS_TEST_BINARY` (default `target/debug/cps`). The source builds
run twice in fresh offline sandboxes and publish the matching outputs with
signed build evidence. The merged `/usr` base publishes signed prebuilt
files instead. The outputs feed the
[VM tests](boot-tests.md#signed-fixtures-and-release-profiles). Package
manifests and recipe templates live under [`packaging/`](../../packaging),
and recipes pin each manifest by digest, so rebuild a fixture after
changing its packaging. See
[Rebuild the VM fixtures after packaging changes](boot-tests.md#rebuild-the-vm-fixtures-after-packaging-changes).

The fixture recipes take their parallelism from the worker: plain `make` reads
`MAKEFLAGS`, and Ninja and Cargo receive `CPS_BUILD_JOBS`. CPS strips the ELF
outputs with the seeded toolchain's `objcopy` and ships their debug
information under the `debug.symbols` facet. See
[parallel jobs](../maintainer/builds.md#parallel-jobs) and
[stripping](../maintainer/builds.md#stripping-and-debug-information).

## Building CPS itself

On an x86-64 Arch Linux worker with Rust, the native GCC toolchain, jq, zstd, and
Python 3.11 or newer, build the fixture in a new output directory. The worker needs a
sparse crates.io cache populated for the selected revision's `Cargo.lock` and a
directory with the SQLite 3.54.1 autoconf archive and the libseccomp 2.6.1 release
archive. The recipes verify the archive lengths and digests on import.
[SQLite downloads](https://www.sqlite.org/download.html) · [libseccomp release](https://github.com/seccomp/libseccomp/releases/tag/v2.6.1)

```sh
cargo fetch --locked
CPS_MIRROR_TUF_ROOT=/path/to/reviewed-public-root.json \
  CPS_RUNTIME_SOURCE_DIR=/path/to/runtime-sources tests/build-cps.sh target/cps-fixture HEAD
```

The fixture takes the package manifests, build recipes, and worker seed scripts
from the selected Git commit. It checks each cached crate
archive against `Cargo.lock` before vendoring from a fresh offline Cargo home.
It builds SQLite and libseccomp twice offline and publishes both to an initial
protected catalog. Their packages carry the headers, pkg-config files, and
linker symlinks under the `devel` facet. The `cps-build-libs` profile
validates both packages with that facet enabled.

The x86-64 CPS package declares its target in the signed manifest. Its
recipe pins that library catalog, the Rust and C toolchain catalog,
and the `runtime-seed` catalog with the worker's prebuilt ELF loader and C
and GCC runtime libraries. The seed provides both loader paths used by CPS
and glibc as hard links to one inode. After two matching builds, the release
adds CPS, `system/canopa-trust`, and `system/git-build` to the catalog and validates the signed
`cps-runtime` profile with `devel` disabled. The trust package pins the
reviewed public root by size and SHA-256 and installs its exact bytes at
`/etc/cps/tuf-root.json` with mode `0644`. Its package version is `1.0.0-N`,
where `N` is the TUF root version. Provide the public bootstrap root from the
offline signing ceremony through `CPS_MIRROR_TUF_ROOT`. The Git build package
ships the default configuration, public key, and a signed local catalog with
BusyBox and its runtime files. The worker takes BusyBox from
`/usr/lib/initcpio/busybox` or `CPS_GIT_BUILD_BUSYBOX`. The fixture installs
the profile, checks the installed root and Git toolchain, and uses the image's loader and libraries
to run the installed CPS binary and verify the image.

The output includes `source-revision`, the recipes, source cache, toolchain
repository, protected package repository, and runtime seed repository. Set
`CPS_VM_CPS_FIXTURE` to that output directory for a systemd VM boot test. The
VM fixture installs the exact protected CPS, trust, and Git build packages and their dependencies from
the signed repositories. Set `CPS_BIN` to `image/usr/bin/cps` inside the output
directory to also use that executable to assemble the guest.

## Testing a source-built Linux kernel

On the x86-64 Arch Linux build worker, provide GCC, binutils, libelf headers,
`bc`, Bison, Flex, M4, XZ, and the usual native build tools. Verify the Linux 6.12.115
release signature and place `linux-6.12.115.tar.xz` in a local source directory.
[Linux release signatures](https://www.kernel.org/signature.html)

```sh
CPS_KERNEL_SOURCE_DIR=/path/to/verified-sources tests/build-kernel.sh target/kernel-fixture
```

The recipe pins the archive, VM configuration, and signed worker toolchain.
CPS builds the EFI kernel twice in fresh offline sandboxes, verifies the matching
outputs, and publishes `system/kernel-qemu` with signed build evidence. Set
`CPS_KERNEL_BC` to an extracted trusted `bc` binary if the worker does not have
it installed. The worker toolchain still comes from prebuilt Arch packages.

Set `CPS_KERNEL_VARIANT=next` with a new output directory to build package
revision `6.12.115-2`. That variant uses release `6.12.115-cps-vm-next` and
`CONFIG_HZ_1000=y`. The base variant uses `6.12.115-cps-vm`. Both use the same
verified upstream archive, and each has its own pinned manifest and configuration.
Use the base and next outputs as `CPS_VM_KERNEL_FIXTURE` and
`CPS_VM_NEXT_KERNEL_FIXTURE` for the signed native Btrfs test.

For the signed-profile rollback test, also set `CPS_VM_KERNEL_FIXTURE` to the
build output directory. The signed trial installs the kernel and its build
configuration directly from the fixture repository. This kernel has built-in
virtio, ext4, Btrfs, and VFAT support and boots the QEMU test root without an initrd.
The previous root keeps its original kernel and initrd.

## Testing source-built systemd and systemd-boot

On the x86-64 Arch worker, provide Meson, Ninja, Python, Jinja2, pyelftools,
gperf, and patchelf. The standalone fixture also uses the worker's libmount
and libblkid headers. Place the
`v262.1` source archive in the source directory as `systemd-262.1.tar.gz`.
[systemd v262.1 release](https://github.com/systemd/systemd/releases/tag/v262.1)

```sh
CPS_SYSTEMD_SOURCE_DIR=/path/to/verified-sources \
CPS_SYSTEMD_CPS_FIXTURE=target/cps-fixture \
tests/build-systemd.sh target/systemd-fixture
```

The fixture pins the archive and the signed worker toolchain. It builds the
VM's systemd components twice offline and publishes the matching outputs with
signed build evidence. The standalone build publishes systemd revision
`262.1.0-3` and a separate seed for the worker's prebuilt libmount and
libblkid. With `CPS_SYSTEMD_UTIL_LINUX_FIXTURE`, the revision is
`262.1.0-4`, or `262.1.0-5` with util-linux `2.42.4-1`.
`CPS_SYSTEMD_LIBSECCOMP_VERSION` selects the libseccomp package from the
CPS fixture, `2.6.1-1` by default. Set `CPS_SYSTEMD_TOOLS_ROOT` to a trusted
extracted Arch package tree if gperf, patchelf, Jinja2, and pyelftools are
available there instead of on the worker.

Set `CPS_VM_SYSTEMD_FIXTURE` to the build output for signed-profile rollback.
The test installs systemd from the signed repository, copies the installed EFI
loader to the ESP, and checks the packaged PID 1 on each boot. Combine this
with `CPS_VM_KERNEL_FIXTURE` to test the source-built trial kernel. The signed
util-linux and BusyBox fixtures provide the guest's mount and module-loading
commands. Offline staging uses the host's systemd helpers.

## Testing a merged `/usr` image

Build the merged base from the CPS fixture's signed runtime catalog and build
BusyBox for canonical installation under `/usr/bin`.

```sh
tests/build-usr-base.sh target/usr-base-fixture target/cps-fixture
CPS_BUSYBOX_MERGED_USR=1 tests/build-busybox.sh target/busybox-usr-fixture
```

The signed `system/filesystem` package owns the compatibility links. The ELF
runtime package places the x86-64 interpreter under `/usr/lib`. Applications
can still start through `/lib64`, and scripts can use `/bin/sh`. Package file
actions use canonical paths. CPS rejects file actions beneath compatibility
links and can verify their exact targets with `verify --pristine`.
[systemd filesystem hierarchy requirements](https://systemd.io/SYSTEMD_FILE_HIERARCHY_REQUIREMENTS/)

Set `CPS_VM_USR_BASE_FIXTURE` to the merged base output alongside the signed
systemd fixture, and point the BusyBox repository and key at the merged
BusyBox output. Both roots are provisioned with the layout before booting.
The test checks the aliases, shell, and interpreter on the trial and rollback
boots and fails if systemd reports an unmerged `/usr`.

## Building util-linux for the boot image

On the x86-64 Arch worker, verify the util-linux 2.42.4 release signature and
place `util-linux-2.42.4.tar.xz` in a local source directory.
[util-linux release archive and signature](https://www.kernel.org/pub/linux/utils/util-linux/v2.42/)

```sh
CPS_UTIL_LINUX_SOURCE_DIR=/path/to/verified-sources \
tests/build-util-linux.sh target/util-linux-fixture target/usr-base-fixture
```

The fixture configures `mount`, `umount`, `blkid`, `fdisk`, `sfdisk`, and their
libraries, runs `make install`, and builds twice offline. Its template collects
the installed tree instead of naming each file, and the recipe discards the
libtool archives. It publishes the matching outputs with signed build evidence,
then installs the package into the signed merged base and runs the tools with
its loader. The package also delivers the headers and linker inputs under the
`devel` facet, the manual pages under `doc.man`, and split debug information
under `debug.symbols`.

Set `CPS_SYSTEMD_UTIL_LINUX_FIXTURE=target/util-linux-fixture` when running
`tests/build-systemd.sh`. The build uses util-linux, libseccomp, and ELF
runtime packages from their pinned publisher catalogs. The util-linux
package supplies the GPT tools for native boot activation. The libseccomp
headers, pkg-config metadata, and library come from the same signed package.

For signed-profile rollback, set `CPS_VM_UTIL_LINUX_FIXTURE` to the util-linux
output, `CPS_VM_SYSTEMD_FIXTURE` to the matching systemd build, and
`CPS_VM_USR_BASE_FIXTURE` to the merged base. The release profile pins the
util-linux catalog with the `devel` facet disabled. The guest mounts the ESP
with the packaged `mount` and probes it with `blkid` on the trial, default
reboot, and rollback.

## Building GNU tools for boot preparation

Place the verified `coreutils-9.12.tar.xz` and `grep-3.12.tar.xz` releases in
a local source directory. Verify their detached signatures using the GNU
keyring and the expected release keys: coreutils uses
`6C37DC12121A5006BC1DB804DF6FD971306037D9`, and grep uses
`155D3FC500C834486D1EEA677FD9FCCB000BEEEE`.
[Coreutils sources and signature](https://ftp.gnu.org/gnu/coreutils/)
· [Grep release announcement](https://lists.gnu.org/archive/html/info-gnu/2025-04/msg00008.html)

```sh
CPS_GNU_SOURCE_DIR=/path/to/verified-sources \
tests/build-gnu-tools.sh target/gnu-tools-fixture target/usr-base-fixture
```

The KDL recipes pin each archive's size and SHA-256 digest. The fixture
builds the GNU coreutils commands as one binary with command symlinks and
builds GNU grep separately. The coreutils template collects the installed
binary, command symlinks, and `libstdbuf.so` with three patterns. Both builds run twice in the isolated CPS
environment before publication with manifest-bound builder evidence.
The installation check resolves libraries from the signed merged base and
exercises `install`, file modes, and the GNU grep `-G` behavior used by
systemd's kernel-install plugin. Package dependencies require the ELF
runtime. The boot profile explicitly selects these utility packages.

## Testing a source-built BusyBox

On an x86-64 Arch Linux worker with a native GCC toolchain, run
`tests/build-busybox.sh target/busybox-fixture` with a new output directory.
It signs a seed catalog containing the worker's compiler and utilities, then
pins that catalog, the upstream BusyBox 1.38.0 archive, and the two ash fixes
for CVE-2026-38754 and CVE-2026-38755 in the recipe. CPS compiles
the static binary twice in fresh sandboxes and publishes it to an
attestation-required test repository. Set `CPS_BUSYBOX_SOURCE_DIR` to a
directory containing the pinned archive to run without network access.
The seed catalog identifies the prebuilt compiler used by this test.
[BusyBox release archives](https://busybox.net/downloads/) ·
[IFS cleanup patch](https://lists.busybox.net/pipermail/busybox/2026-June/092360.html) ·
[Function recursion patch](https://lists.busybox.net/pipermail/busybox/2026-June/092354.html)

To use that package in the VM, set `CPS_VM_BUSYBOX_REPO` to
`target/busybox-fixture/repo` and `CPS_VM_BUSYBOX_TRUST_ROOT` to
`target/busybox-fixture/release.trust.json`. The fixture installs the exact
package from that publisher. The systemd guest loads its prepared modules
with the packaged `insmod` applet, checks the loaded modules in sysfs, and
reports the version and loaded module names on each boot. The host still prepares
the module files for kernels with modular FAT support.
