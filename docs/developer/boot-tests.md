# Test boot and rollback in a VM

[Documentation](../README.md) · [Developers](README.md)

The VM tests boot disposable Btrfs disks under QEMU with OVMF and check
staging, activation, blessing, fallback, and rollback end to end. They use
regular files and software emulation, need no root on the host, and run from
the checkout root after `cargo build`. The CI `boot` job runs
`systemd-boot-vm.sh health-fail` and two `boot-capture-vm.sh`
configurations. See the [CI workflow](../../.github/workflows/ci.yml).

## On this page

- [Common settings](#common-settings)
- [Offline disk images](#offline-disk-images)
- [systemd-boot scenarios](#systemd-boot-scenarios)
  - [Signed fixtures and release profiles](#signed-fixtures-and-release-profiles)
- [Btrfs snapshot and boot tests](#btrfs-snapshot-and-boot-tests)
- [File modes and capabilities in a guest](#file-modes-and-capabilities-in-a-guest)
- [Rebuild the VM fixtures after packaging changes](#rebuild-the-vm-fixtures-after-packaging-changes)
- [Unit tests for retention, loader policy, and direct UEFI boot](#unit-tests-for-retention-loader-policy-and-direct-uefi-boot)

## Common settings

The VM scripts read CPS from `CPS_BIN`, defaulting to `target/debug/cps`,
and share these overrides:

| Variable | Value |
| --- | --- |
| `CPS_VM_QEMU` | QEMU executable, default `qemu-system-x86_64` |
| `CPS_VM_MKFS_BTRFS` | Trusted `mkfs.btrfs` with `--rootdir`, `--subvol`, and `--shrink`, default from the path |
| `CPS_VM_QEMU_DATA` | Directory passed to QEMU with `-L` for its firmware data |
| `CPS_VM_OVMF_CODE`, `CPS_VM_OVMF_VARS` | Matching non-Secure-Boot x86-64 OVMF code and variables files |
| `CPS_VM_SYSTEMD_BOOT` | systemd-boot EFI binary, default `/usr/lib/systemd/boot/efi/systemd-bootx64.efi` |
| `CPS_VM_KERNEL`, `CPS_VM_KERNEL_RELEASE` | EFI-stub Linux kernel, default `/boot/vmlinuz-linux`, and its release, default `uname -r` |
| `CPS_VM_BUSYBOX` | BusyBox with `sh`, `cat`, `reboot`, and `poweroff`, default `/usr/lib/initcpio/busybox` |
| `CPS_VM_FAT_BUILTIN` | `1` if the guest kernel has built-in VFAT and its character sets |
| `CPS_VM_TIMEOUT` | Seconds each QEMU run may take |

The defaults match Arch Linux paths. The kernel needs built-in virtio
block, GPT, Btrfs, and serial console support. `CPS_DISK_TOOLS_DIR` names
the trusted btrfs-progs, dosfstools, and mtools that `cps boot-env image`
runs, as described in [offline disk images](../setups/disk-image.md#checks-and-failure-cleanup).
Fixture roots that install a
package with a [trigger](../maintainer/triggers.md), such as the signed
systemd package and its hardware database trigger, run the handler on the
host, which needs a working `/usr/bin/bwrap`.

## Offline disk images

[`tests/boot-image.sh`](../../tests/boot-image.sh) runs without a VM. It
writes a root with `cps boot-env image` and checks the Btrfs layout, the
registration that exists only in the image, owners, modes, links, and
extended attributes, and the known-good entry on the ESP. It repeats an image
under `SOURCE_DATE_EPOCH`, and checks the rejections, the systemd-boot
version check, failure cleanup, and concurrent writes. It needs btrfs-progs,
dosfstools, mtools, `sfdisk`, OpenSSL, and `setfattr`.

## systemd-boot scenarios

[`tests/systemd-boot-vm.sh SCENARIO`](../../tests/systemd-boot-vm.sh)
assembles a base root from signed packages, runs `verify --pristine`, and
writes it to a Btrfs disk with `cps boot-env image`. A rescue system then
boots with the host kernel from a read-only disk holding CPS and the test
repositories, captures the base, stages the saved upgrade with
`boot-env stage-btrfs`, and activates it with `boot-env activate-btrfs`,
as a maintenance boot of an installed system would. The test then boots the
disk through OVMF and systemd-boot. Its runtime package imports prebuilt host
binaries, libraries, and VFAT and ISO-8859-1 modules into a prototype, then
generates the manifest and publishes it through CPS. A plain copy of the base
with the same plan applied lets the host check the staged files.

| Scenario | Checks |
| --- | --- |
| `fallback` (default) | Activation rejects a missing fallback kernel and repeats idempotently, the previous root boots after two failed attempts, and activation cannot replenish the exhausted counter |
| `bless` | systemd runs as PID 1, installs the health gate through the CPS package, removes the counter, and boots the blessed root again |
| `health-fail` | A managed file changed after activation makes the health gate block blessing through both attempts |
| `catalog-health-fail` | A corrupted archived catalog in the trial's state fails catalog verification, and systemd-boot falls back after both attempts |
| `kernel-upgrade` | Activation rejects a corrupted prepared kernel, then the kernel and initrd on the ESP and `uname -r` from both roots |
| `facet-change` | Staging documentation from the installed manifest and preserving edited configuration while disabling its facet, then the blessed trial and a rollback to the original facet selection |
| `capabilities` | A signed capability-bearing package in the guest: failed preflight without `CAP_SETFCAP`, raw socket access as an unprivileged user, repair, cloning, and capability removal during an upgrade |
| `rollback` | A second signed publisher, an upgrade that adds a dependency from it and removes an obsolete one, replay rejection, a trial selected with `preferred` after the base was pinned, blessing, `boot-env list` and `boot-env rollback` in the rescue system, a rejected rollback to a corrupt fallback kernel, and a boot of the previous kernel and dependencies |

```sh
CPS_VM_OVMF_CODE=/path/to/OVMF_CODE.fd \
CPS_VM_OVMF_VARS=/path/to/OVMF_VARS.fd \
CPS_VM_KERNEL=/path/to/vmlinuz \
CPS_VM_BUSYBOX=/path/to/busybox \
CPS_VM_MKFS_BTRFS=/path/to/mkfs.btrfs \
tests/systemd-boot-vm.sh bless
```

The script needs `qemu-system-x86_64`, btrfs-progs, dosfstools (`mkfs.fat`,
`fsck.fat`), mtools, `sfdisk`, OpenSSL, binutils `strip`, and `timeout`. The
rescue system copies the host's util-linux `mount` and `sfdisk`, kmod
`insmod`, and the GNU tools that `kernel-install` uses, so BusyBox needs only
`sh` and basic file applets. `catalog-health-fail` uses `sqlite3`. The
blessing fixture needs the local systemd userspace, util-linux, binutils
`readelf`, kmod, and kernel support for EFI variables and FAT.

For `kernel-upgrade`, provide the original kernel as `CPS_VM_KERNEL` with
`CPS_VM_KERNEL_RELEASE`, and the upgrade as `CPS_VM_TRIAL_KERNEL` with
`CPS_VM_TRIAL_KERNEL_RELEASE`. Both releases must be distinct, and both
kernels need the built-in storage and console support listed above. This
scenario also uses `cpio` and the host's `kernel-install` and
`90-loaderentry.install` plugin, which the rescue system runs.

The `capabilities` scenario also requires a C compiler with static libc,
`setpriv`, `getcap`, and `setcap`.

In the `rollback` scenario both kernels need VFAT support. To load modules
from extracted kernel packages, set `CPS_VM_KERNEL_MODULES_ROOT` and
`CPS_VM_TRIAL_MODULES_ROOT` to the roots containing their indexed
`lib/modules` trees. Set `CPS_VM_FAT_BUILTIN=1` when both kernels build VFAT
in, such as the [kernel fixtures](build-fixtures.md).

### Signed fixtures and release profiles

These variants of the `rollback` scenario use the
[signed package fixtures](build-fixtures.md). Use a CPS build from this
checkout on the host.

`CPS_VM_RELEASE_PROFILE=1` assembles the roots from a signed release
profile with the signed CPS and BusyBox fixtures. The test installs the
signed base composition, publishes the companion update, and checks that
the profile still selects the old kernel. It advances the profile through
the automatic upgrade check before staging and booting the trial. In the
local repository scenario, the test also moves the primary and companion
repositories together and checks that the previous saved plan is stale. Add
`CPS_VM_STAGED_TRANSACTIONS=1` to apply a second reviewed transaction, a
freeze, to the private staged root in the rescue system before activation.
The test checks that the trial keeps its original boot parent and that the
guest sees the freeze only in the trial.

`CPS_VM_TUF_MIRRORS=1`, with the same signed profile fixtures, signs
separate local TUF mirrors with throwaway keys using the disposable signer
from `cargo build --locked --example tuf-fixture`. Set
`CPS_VM_TUF_FIXTURE` if it is not at `target/debug/examples/tuf-fixture`.
The test syncs the base profile index-only and fetches its planned
payloads. It advances a companion while the primary still pins the previous
catalog, and checks that pruning keeps the previous kernel after its mirror
drops the pin. It then repairs a copy of the base root from a signed
archive after deleting that cached kernel. It also restores a missing
catalog from the archive and replays the saved profile installation against
the original cache paths. The test rejects a replayed TUF timestamp and
verifies that missing or corrupt payloads block promotion with
`--profile-payloads`. After a successful retry, it takes the mirrors
offline, so the rescue system stages the upgrade from the prefetched cache,
and boots through blessing and rollback.

`CPS_VM_REPROVISION=1`, with `CPS_VM_RELEASE_PROFILE=1` and
`SOURCE_DATE_EPOCH` set, installs the signed profile in two fresh roots and
compares their saved plans and disk images before booting the upgrade
scenario. The images must match apart from the subvolume identities that
btrfs-progs still randomizes, as described in
[reproducible images](../setups/disk-image.md#reproducible-images). It also
needs `btrfs`.

The fixture variables select the signed packages: `CPS_VM_CPS_FIXTURE`,
`CPS_VM_KERNEL_FIXTURE`, `CPS_VM_SYSTEMD_FIXTURE`, `CPS_VM_USR_BASE_FIXTURE`,
`CPS_VM_UTIL_LINUX_FIXTURE`, and `CPS_VM_BUSYBOX_REPO` with
`CPS_VM_BUSYBOX_TRUST_ROOT`. Each is described with its fixture in
[Build signed packages for tests](build-fixtures.md). `CPS_VM_CPS_VERSION`
and `CPS_VM_BUSYBOX_VERSION` override the expected package versions when a
fixture was built with another revision.

## Btrfs snapshot and boot tests

Run [`tests/boot-capture-vm.sh`](../../tests/boot-capture-vm.sh) with
`CPS_VM_BTRFS` and `CPS_VM_MKFS_BTRFS` set to trusted Btrfs utilities,
QEMU, and a kernel with built-in Btrfs support. The test captures
its running root, rejects a nested subvolume outside the exclusions, and
checks that `var/lib/machines` and an `--exclude-subvolume` path become
empty directories with the source metadata. It verifies that later source
writes leave the read-only capture intact, and checks failed and concurrent
staging. It boots the staged Btrfs subvolume and the previous root. For
native boot with a custom kernel, set
`CPS_VM_KERNEL_RELEASE` to the guest kernel's release.

These settings extend it:

| Setting | Adds |
| --- | --- |
| `CPS_VM_BTRFS_BOOT=1` | Native boot through systemd-boot with the package's health service and blessing. Needs `CPS_VM_OVMF_CODE`, `CPS_VM_OVMF_VARS`, a trusted `CPS_VM_SYSTEMD_BOOT`, and the installed systemd runtime. Tests activation, a transaction waiting while registration completes, blessing, the trial's default on reboot, rejection of altered subvolume metadata, and rollback. It also fills the EFI partition, checks that activation fails its free-space check with a pointer to `boot-env prune` and keeps the known-good boot selection, and retries after freeing space. |
| `CPS_VM_BTRFS_HEALTH_FAIL=1` | Changes a managed file's metadata at trial boot and checks that two unblessed attempts fall back to the original subvolume. `readiness` instead fails a readiness check after the image verifies. |
| `CPS_VM_BTRFS_DISCARD_FAILED=1` | With a failed trial, checks from the fallback that `boot-env prune --dry-run` lists the exhausted trial without changing the ESP, that `boot-env discard-trial` removes its root and boot files, and that nothing is then left to prune or discard |
| `CPS_VM_BTRFS_DESTROY_FAILED=1` | With a failed trial, removes the exhausted trial with `boot-env destroy-btrfs` |
| `CPS_VM_BTRFS_DESTROY=1` | With a successful trial, stages another trial from the blessed root, rejects destroying protected and mounted roots, and retires the old root after an interrupted destruction |
| `CPS_VM_STAGED_TRANSACTIONS=1` | With a successful trial, freezes, unfreezes, and upgrades the private staged root, and rejects a stale plan |
| `CPS_VM_SYSTEM_COMMANDS=1` | With a successful trial, the everyday commands: a live install, a live upgrade of a running service, a staged upgrade, rollback with the live package still installed, a live removal, and a staged removal of its unused dependency with the repository unavailable |
| `CPS_VM_GIT_INSTALLS=1` | With `CPS_VM_SYSTEM_COMMANDS=1`, a tracked Git install that advances, a pinned one that stays at its commit, and rollback of the tracked commit |
| `CPS_VM_KERNEL_FIXTURE` | With native boot, stages the signed Btrfs-capable kernel from `tests/build-kernel.sh` as part of the upgrade, requires its builder signature, audits the installed evidence after boot, and checks the running kernel in the trial and the previous root |

Run [`tests/boot-profile-btrfs-vm.sh`](../../tests/boot-profile-btrfs-vm.sh)
to test a native Btrfs root assembled entirely through a signed release
profile. Set `CPS_VM_CPS_FIXTURE`, `CPS_VM_KERNEL_FIXTURE`,
`CPS_VM_SYSTEMD_FIXTURE`, `CPS_VM_USR_BASE_FIXTURE`,
`CPS_VM_UTIL_LINUX_FIXTURE`, and `CPS_VM_BUSYBOX_FIXTURE` to the
corresponding build outputs. Also set `CPS_VM_NEXT_KERNEL_FIXTURE` to a
distinct signed kernel build and `CPS_VM_UTILITIES_FIXTURE` to the
[GNU tools fixture](build-fixtures.md#building-gnu-tools-for-boot-preparation),
and provide the QEMU, OVMF, and trusted `CPS_VM_MKFS_BTRFS` overrides.

The test installs the source-built CPS, systemd, util-linux, BusyBox, and
Btrfs-capable kernel with builder trust enforced by the signed profile. The
ELF runtime comes from the signed merged `/usr` seed. It keeps signing keys
outside the guest and mounts its repositories read-only. It captures the
running root, stages an unfreeze, applies the profile and kernel upgrade to
that private root, boots and blesses it, checks the default on reboot, and
explicitly rolls back to the previous root with its original freeze and
kernel. It imports both verified kernel bundles under a temporary release
key and pins their builder keys in the signed profile. The staged guest
runs its packaged kernel-install and GNU utilities to prepare fresh
artifacts. The test checks distinct kernel bytes, prepared artifacts, and
the running release on both trial boots and after rollback.

## File modes and capabilities in a guest

[`tests/file-modes-vm.sh`](../../tests/file-modes-vm.sh) boots a small Btrfs
root under QEMU and checks privileged execution, sticky deletion, group
inheritance, offline recovery, repair, clone isolation, and privilege
removal as root in the guest. It needs a C compiler with static libc,
`strip`, `mkfs.btrfs`, and the common QEMU, kernel, and BusyBox settings.

## Rebuild the VM fixtures after packaging changes

The signed fixtures pin their manifests by digest, so a change under
[`packaging/`](../../packaging) reaches the VM only after you rebuild the
fixture into a new output directory and point the VM test at it.

[`packaging/systemd/vm.kdl`](../../packaging/systemd/vm.kdl) is the manifest
of the VM's `system/systemd-vm` package, including its `hwdb` trigger, and
[`vm-build.kdl.in`](../../packaging/systemd/vm-build.kdl.in) is its recipe
template. After changing either, rebuild the systemd fixture into a new
directory, which the script requires.

```sh
CPS_SYSTEMD_SOURCE_DIR=/path/to/verified-sources \
CPS_SYSTEMD_CPS_FIXTURE=target/cps-fixture \
CPS_SYSTEMD_UTIL_LINUX_FIXTURE=target/util-linux-fixture \
tests/build-systemd.sh target/systemd-fixture-2
```

Then set `CPS_VM_SYSTEMD_FIXTURE=target/systemd-fixture-2` with the other
signed fixture settings for the next VM run. `build-systemd.sh` substitutes the worker's user and group IDs and the
package revision into a copy of `vm.kdl`, pins its digest in the recipe,
builds twice, and publishes the result with signed build evidence. The
revision follows the libseccomp and util-linux inputs, not the manifest, so
a rebuilt fixture reuses the package ID with new content. Keep each build
in its own output directory, because a repository rejects a package ID
republished with different content. `CPS_VM_SYSTEMD_VERSION` overrides the
revision that `systemd-boot-vm.sh` expects. The health and bless units in [`packaging/systemd/`](../../packaging/systemd)
and [`packaging/cps.kdl`](../../packaging/cps.kdl) are read directly by the
VM tests, so changes to them apply on the next run.

## Unit tests for retention, loader policy, and direct UEFI boot

`cargo test` covers the pure parts of these features without privileges.
The retention tests in `src/boot/systemd_boot/prune.rs` check which
environments prune protects and removes. The loader policy tests check that
administrator lines are preserved and that conflicts name the key. The
direct UEFI tests run activation, blessing, rollback, and option removal
against a temporary directory that stands in for efivarfs. The UKI tests
check a hand-built PE image. Other tests cover the GPT reader with 512-byte
and 4096-byte sectors, using `sfdisk`, and the automount mount-table check.

No VM scenario covers direct UEFI boot. It would need `ukify`,
systemd-stub, and an `sbctl` key in the guest, plus OVMF variables that
keep `BootNext` across resets.
