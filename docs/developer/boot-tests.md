# Test boot and rollback in a VM

[Documentation](../README.md) · [Developers](README.md)

## ext4 disk and systemd-boot tests

The [VM test](../../tests/systemd-boot-vm.sh) deploys two CPS exports to a GPT disk
and activates them in its FAT ESP, then boots through OVMF and systemd-boot
under QEMU.
The `fallback` scenario checks that staging leaves the ESP alone and that the
previous root boots after two failed attempts. The `bless` scenario runs
systemd as PID 1, installs the health gate through the CPS package, and checks
that systemd removes the counter and boots the blessed root again. The
`health-fail` scenario corrupts a managed file in the exported partition and
checks that the health gate blocks blessing through both attempts.
`catalog-health-fail` uses `sqlite3` and `debugfs` to corrupt the archived
catalog in the trial partition after activation. It checks for the catalog
digest error and verifies that systemd-boot falls back after both attempts. The
`kernel-upgrade` scenario checks the kernel and initrd on the ESP and reports
`uname -r` from both roots.
The `facet-change` scenario stages documentation from the installed manifest
and preserves edited configuration while disabling its facet. It checks the
blessed trial after reboot and rolls back to the original facet selection.

Build with `cargo build`. Run [the export test](../../tests/boot-export.sh) with
e2fsprogs, OpenSSL, and `setfattr` to check the payload and failure cleanup.
[The deployment test](../../tests/boot-deploy.sh) also uses `sfdisk` and `blkid`
to check partition isolation and reject damaged or ambiguous layouts,
stale receipts, modified payloads, and missing fallback roots.
Run it with `--esp` and mtools and dosfstools installed to exercise raw-disk
activation and rollback, including preservation of unrelated EFI files.
The VM test additionally needs `qemu-system-x86_64`, `mkfs.fat`, mtools,
systemd-boot, and matching non-Secure-Boot x86-64 OVMF code and
variables files. Supply an EFI-stub Linux kernel with built-in virtio block,
GPT, ext4, and serial console support, plus a compatible BusyBox with `sh`,
`cat`, `reboot`, and `poweroff`.

```sh
CPS_VM_OVMF_CODE=/path/to/OVMF_CODE.fd \
CPS_VM_OVMF_VARS=/path/to/OVMF_VARS.fd \
CPS_VM_KERNEL=/path/to/vmlinuz \
CPS_VM_BUSYBOX=/path/to/busybox \
tests/systemd-boot-vm.sh bless
```

The script defaults to Arch Linux paths for the kernel, BusyBox, and systemd-boot. Set `CPS_VM_SYSTEMD_BOOT` if the EFI binary is elsewhere. It uses regular files and software emulation.

The fixture assembles each root from signed packages and runs `verify --pristine`
before packing it into the VM disk. Its runtime package imports prebuilt host
binaries, libraries, and VFAT and ISO-8859-1 modules into a prototype, then
generates the manifest and publishes it through CPS. The blessing fixture needs
the local systemd userspace, util-linux, binutils `readelf` and `strip`, kmod, and kernel
support for EFI variables and FAT. Set `CPS_VM_KERNEL_RELEASE` for another
installed kernel, or `CPS_VM_FAT_BUILTIN=1` if VFAT and its character sets are
built in.

For `kernel-upgrade`, provide the original kernel as `CPS_VM_KERNEL` with `CPS_VM_KERNEL_RELEASE`, and the upgrade as `CPS_VM_TRIAL_KERNEL` with `CPS_VM_TRIAL_KERNEL_RELEASE`. Both releases must be distinct, and both kernels need the built-in storage and console support listed above. This scenario also uses `cpio` and the host's `kernel-install` and `90-loaderentry.install` plugin.

The `capabilities` scenario also requires a C compiler with static libc,
`setpriv`, `getcap`, and `setcap`. It tests a signed capability-bearing
package inside the disposable guest, including failed preflight without
`CAP_SETFCAP`, raw socket access as an unprivileged user, repair, cloning,
and capability removal during an upgrade.

The `rollback` scenario adds another signed publisher after activating the base
environment. The upgrade installs a dependency from that publisher and removes
an obsolete dependency. The staged root rejects a replay of the base
catalog while the base root still accepts that catalog. After systemd
blesses the trial and reboots it as the default, the test runs
`boot-env list` and `boot-env rollback` against the disk.
It boots the resulting disk and checks that the previous kernel and retired
dependency are present and that the added package is absent. Both kernels need
VFAT support. To load modules from extracted kernel packages, set
`CPS_VM_KERNEL_MODULES_ROOT` and `CPS_VM_TRIAL_MODULES_ROOT` to the roots
containing their indexed `lib/modules` trees.

Set `CPS_VM_RECYCLE_SLOT=1` with `rollback` to stage and bless a second
upgrade on the original root slot, then roll it back to the first upgrade.
The scenario accepts a vendor copy for one setting and keeps another
administrator edit with tighter permissions. It checks both settings in
the guest across rollback.

Set `CPS_VM_RELEASE_PROFILE=1` in the rollback test to assemble the roots from
a signed release profile. Use a CPS build from this checkout on the host and provide the
signed CPS and BusyBox fixtures. The test installs the signed base
composition, publishes the companion update, and checks that the profile
still selects the old kernel. It advances the profile through the automatic
upgrade check before staging and booting the trial. In the local repository
scenario with slot recycling disabled, the test also moves the primary and
companion repositories together and checks that the previous saved plan is
stale.

Build the disposable mirror signer with
`cargo build --locked --example tuf-fixture`. With the same signed profile
fixtures, set `CPS_VM_TUF_MIRRORS=1` for the rollback scenario. The test signs
separate local TUF mirrors with throwaway keys, syncs the base profile
index-only, and fetches its planned payloads. It advances a companion while
the primary still pins the previous catalog, and checks that pruning keeps
the previous kernel after its mirror drops the pin. It then repairs a copy
of the base root from a signed archive after deleting that cached kernel.
It also restores a missing catalog from the archive and replays the saved
profile installation against the original cache paths.
The test rejects a replayed TUF timestamp and verifies that missing or corrupt
payloads block promotion with `--profile-payloads`. After a successful retry,
it takes the mirrors offline and prepares the upgrade in one command. It
deploys that bundle and boots through blessing and rollback.
This scenario runs without slot reuse.

With the signed kernel fixture, set `CPS_VM_DEVICE_ACTIVATION=1` to bootstrap
the base and activate the trial from a rescue VM with a writable virtio disk.
The test checks rejection of unchecked ESP writes, mounted or registered root
targets, and incorrect partition or receipt identities. It then verifies the
written partitions and boots the result through the blessing and rollback
checks. Rollback runs
against the mounted ESP in the rescue VM and rejects a mismatched root
before selecting and booting the original kernel.
Combine it with `CPS_VM_RECYCLE_SLOT=1` to also exercise direct replacement,
recovery from a partial write after retirement, and boot and rollback of
the recovered upgrade.
With those options and `CPS_VM_RELEASE_PROFILE=1`, set
`CPS_VM_INSTALLED_STAGE=1` to plan and stage the second upgrade inside the
rescue VM from the first upgrade's read-only root partition. This variant
checks that state written during the first boot reaches the staged tree and
that the fallback partition stays byte-for-byte intact during deployment. It
also checks that `apply` rejects a writable copy of the deployed root.

With `CPS_VM_RELEASE_PROFILE=1`, offline disk activation, and
`SOURCE_DATE_EPOCH` set, use `CPS_VM_REPROVISION=1` to install the signed
profile in two fresh roots and compare the saved plans, ext4 exports, KDL
receipts, and full GPT disks before booting the upgrade scenario.

## Btrfs snapshot and boot tests

Run `tests/boot-capture-vm.sh` with `CPS_VM_BTRFS` and
`CPS_VM_MKFS_BTRFS` set to trusted Btrfs utilities, QEMU, and a kernel
with built-in Btrfs and ext4 support. The test captures its running root,
rejects nested subvolumes, verifies that later source writes leave the
read-only capture intact, and checks failed and concurrent staging. It boots
the staged Btrfs subvolume, the previous root, and the exported ext4 root.
It uses the same `CPS_BIN`, `CPS_VM_QEMU`, `CPS_VM_QEMU_DATA`,
`CPS_VM_KERNEL`, and `CPS_VM_BUSYBOX` overrides as the boot fixture.
For native boot with a custom kernel, set `CPS_VM_KERNEL_RELEASE` to
the guest kernel's release. Set `CPS_VM_FAT_BUILTIN=1` if the guest kernel
has built-in VFAT and NLS support for the EFI partition.

The package's health service and systemd blessing apply to Btrfs trials.
Set `CPS_VM_BTRFS_BOOT=1` on `tests/boot-capture-vm.sh`, provide
`CPS_VM_OVMF_CODE`, `CPS_VM_OVMF_VARS`, and a trusted
`CPS_VM_SYSTEMD_BOOT` EFI binary, and use the installed systemd runtime.
This mode tests activation, a transaction waiting while registration
completes, blessing, the trial's default on reboot, rejection of altered
subvolume metadata, and rollback to the previous root. It also fills the
EFI partition, checks that a failed activation keeps the known-good boot
selection, and retries activation after freeing space.
Add `CPS_VM_BTRFS_HEALTH_FAIL=1` to change a managed file's metadata at
trial boot and verify that two unblessed attempts fall back to the original
subvolume.

Run `tests/boot-profile-btrfs-vm.sh` to test a native Btrfs root assembled
entirely through a signed release profile. Set `CPS_VM_CPS_FIXTURE`,
`CPS_VM_KERNEL_FIXTURE`, `CPS_VM_SYSTEMD_FIXTURE`,
`CPS_VM_USR_BASE_FIXTURE`, `CPS_VM_UTIL_LINUX_FIXTURE`, and
`CPS_VM_BUSYBOX_FIXTURE` to the corresponding build outputs. Also set
`CPS_VM_NEXT_KERNEL_FIXTURE` to a distinct signed kernel build and
`CPS_VM_UTILITIES_FIXTURE` to the [GNU tools fixture](build-fixtures.md#building-gnu-tools-for-boot-preparation), and provide
the QEMU, OVMF, and trusted `CPS_VM_MKFS_BTRFS` overrides. The test installs
the source-built CPS, systemd, util-linux, BusyBox, and Btrfs-capable kernel
with builder trust enforced by the signed profile. The ELF runtime comes
from the signed merged `/usr` seed. It keeps signing keys outside the guest
and mounts its repositories read-only. It captures the running root,
stages an unfreeze, applies the profile and kernel upgrade to that private root, boots
and blesses it, checks the default on reboot, and explicitly rolls back to
the previous root with its original freeze and kernel. It imports both
verified kernel bundles under a temporary release key and pins their builder
keys in the signed profile. The staged guest runs its packaged kernel-install
and GNU utilities to prepare fresh artifacts. The test checks distinct kernel
bytes, prepared artifacts, and the running release on both trial boots and
after rollback.

Set `CPS_VM_KERNEL_FIXTURE` to the output from `tests/build-kernel.sh`
to stage the signed Btrfs-capable kernel as part of the upgrade. The image
requires its builder signature, audits the installed evidence after boot,
and checks the running kernel in both the trial and the previous root.

Set `CPS_VM_SYSTEM_COMMANDS=1` with `CPS_VM_BTRFS_BOOT=1` in
`tests/boot-capture-vm.sh` to exercise the everyday commands. The guest
installs a package live, upgrades its running service, boots a staged
upgrade, and rolls back to the system with the live package still installed.
It then removes the package live and boots a staged removal of its unused
dependency with the repository unavailable.
