# Set up Btrfs boot environments

[Documentation](../README.md) · [Setup guides](README.md)

Start with a CPS-managed Btrfs root.
[Assemble a bootable base root](boot-root.md) if you need to provision one. A
trial uses a snapshot of that root and retains the registered parent as a
fallback. Choose systemd-boot with Type #1 entries, systemd-boot with
signed UKIs, or direct UEFI boot with signed UKIs.

## On this page

- [Configure live system commands](#configure-live-system-commands)
- [Activating Btrfs roots](#activating-btrfs-roots)
  - [Signed UKIs for Btrfs roots](#signed-ukis-for-btrfs-roots)
  - [Direct UEFI boot for Btrfs roots](#direct-uefi-boot-for-btrfs-roots)
- [Health checks and shared state](#health-checks-and-shared-state)
- [Capturing a running Btrfs root](#capturing-a-running-btrfs-root)
- [Reclaiming old Btrfs environments](#reclaiming-old-btrfs-environments)

## Configure live system commands

Add boot settings to the [publisher configuration](../client/publishers.md)
and any [mirror entries](../client/mirrors.md#synchronizing-a-signed-profile)
in `/etc/cps/image.kdl`. For example, a single-device Btrfs root on a GPT disk
can use the following settings.

```kdl
boot-btrfs volume="/cps" environments="/cps/@environments" esp="/efi" root-partuuid="7adff456-af50-498f-b39a-dcf87cb4a7c1" options="rw"
stage-package "system/elf-runtime"
stage-package "system/systemd"
```

Use the real partition UUID and package names. Mount the Btrfs top-level
subvolume read-write at `volume`, create a separate, root-owned subvolume
for `environments` below it, and mount the writable EFI System Partition
at `esp`. Register the initial root with `boot-env activate-btrfs --known-good`
and install the [CPS health service](../../packaging/systemd/cps-boot-health.service)
for trial boot assessment. Keep the
repository caches at stable paths outside the boot roots. Use `options`
for any kernel arguments the installation requires. Use `kernel` and
`initrd` only for images that supply boot files manually.
Omit `root-partuuid` to boot by Btrfs filesystem UUID with an initramfs.
See [Activating Btrfs roots](#activating-btrfs-roots) for the requirements
and a LUKS2 example. Set `uki=#true` for
[signed UKIs](#signed-ukis-for-btrfs-roots) with systemd-boot or
`uefi=#true` for [direct UEFI boot](#direct-uefi-boot-for-btrfs-roots).

## Activating Btrfs roots

CPS snapshots one Btrfs root subvolume per environment. The backing devices
can be assembled and unlocked by the initramfs before it mounts that root.

- Mount the Btrfs top-level subvolume (ID 5) read-write at `volume`.
  Each boot root must be a named subvolume reachable through that mount.
  Keep the paths of registered roots stable.
- Place the root's CPS state and managed files in the root subvolume.
  Store staged roots in a root-owned directory on the same filesystem,
  beneath the top-level mount and outside the boot roots. Shared data such
  as `/home`, `/var/log`, or a swapfile can live on sibling subvolumes
  mounted at ordinary directories in each root.
  A Btrfs snapshot omits nested subvolume contents.
- Mount the dedicated, writable VFAT systemd-boot ESP at `esp`. CPS verifies
  its GPT partition using 512-byte logical sectors. When booting by Btrfs
  filesystem UUID, the ESP can be on a different disk.
- Supply a kernel and initramfs that can discover the Btrfs filesystem by
  its unique UUID, including any device assembly and unlock needed by your
  layout. CPS publishes Type #1 systemd-boot entries and selects the root with
  `root=UUID=… rootfstype=btrfs rootflags=subvolid=…`. Keep any `/` line in
  `/etc/fstab` usable across snapshots by omitting `subvol=` and `subvolid=`
  from that line. Keep the selectors for shared mounts.

For example, on a LUKS2 installation using mkinitcpio's systemd and
`sd-encrypt` hooks, mount the top level from the open mapping and pass the
LUKS container UUID in `rd.luks.name=`. CPS reads the distinct filesystem
UUID from the mounted Btrfs volume. Store the kernel and matching standalone
initramfs in the image root, or use the image's prepared kernel artifacts.

```sh
mount -t btrfs -o subvolid=5 /dev/mapper/root /cps
cps boot-env activate-btrfs --known-good --root /cps/@ \
    --btrfs-volume /cps --esp /efi \
    --kernel "usr/lib/modules/$(uname -r)/vmlinuz" --initrd boot/cps-initramfs.img \
    --options "rd.luks.name=$luks_uuid=root rw"
```

Here `$luks_uuid` is the UUID of the encrypted container and
`/boot/cps-initramfs.img` is the standalone initramfs inside the root. Set
`kernel`, `initrd` and `options` to the corresponding values in the
`boot-btrfs` node for system package commands. Omit `root-partuuid` in that
node. The firmware-launched UKIs and their embedded command lines are
independent of the CPS entries.

For a single-device Btrfs filesystem directly on a GPT Linux filesystem
partition, you can select the partition with `--root-partuuid`.
This mode requires the writable VFAT ESP on the same GPT disk.

```sh
mount -t btrfs -o subvolid=5 "/dev/disk/by-partuuid/$root_partuuid" /mnt/cps-volume
cps boot-env activate-btrfs --known-good --root /mnt/cps-volume/@ \
    --btrfs-volume /mnt/cps-volume --esp /efi --root-partuuid "$root_partuuid"
cps boot-env activate-btrfs --root /mnt/cps-volume/@captures/trial/root \
    --btrfs-volume /mnt/cps-volume --esp /efi --root-partuuid "$root_partuuid"
cps boot-env list --esp /efi
cps boot-env rollback --esp /efi --btrfs-volume /mnt/cps-volume
```

Register the original known-good root before activating its staged upgrade.
Bootstrap can read the running root through its top-level path. Each entry
records the filesystem UUID, subvolume UUID and ID, and root path. CPS checks
these against the mounted Btrfs volume, checks the CPS image identity and
revision, and verifies the fallback root and its boot artifacts. The kernel
receives the recorded subvolume ID through `rootflags`.
[Btrfs ioctl documentation](https://btrfs.readthedocs.io/en/stable/btrfs-ioctl.html)

Native activation marks the root with the `user.cps.boot-environment`
extended attribute before publishing its boot entry. CPS checks that mark
under the image's exclusive lock before applying package transactions.
`stage-btrfs` clears the inherited mark on its private writable snapshot.
Activation retries accept the matching registration. If activation fails
after marking the root, rerun it to finish registration.

### Signed UKIs for Btrfs roots

Use systemd-boot 262 or newer, `systemd-ukify`, and a configured `sbctl`
with its db certificate trusted by the firmware. Sign the systemd-boot
binary you launch with that key as part of your Secure Boot setup.
CPS creates its `loader/loader.conf` when you register the first known-good
environment on an ESP with that path available.
Keep the host's `sbctl` state on a protected persistent mount shared by the
environments, so a root snapshot or rollback does not copy or revert its keys.
[sbctl manual](https://man.archlinux.org/man/sbctl.8.en)

Add `--uki` to the `activate-btrfs --known-good` command above, and add
`uki=#true` to `boot-btrfs` for staged system transactions. For manual trial
activation, pass `--uki` again. Bootstrap a signed known-good environment
before publishing signed trials. The trial and its CPS fallback must use
the same boot artifact type.

CPS builds a UKI from the selected root's kernel and initramfs and embeds
the root selector, the snapshot's subvolume ID, and the configured options.
It uses the host's `sbctl` key to sign and verify the completed image before
publishing it. It stores the UKI under `EFI/cps/<id>/uki.efi` and points a
counted systemd-boot entry at it. Each new environment gets its own signature,
so `sbctl sign --save` is only needed for boot files you manage separately.
The firmware verifies the signed systemd-boot binary and UKI at boot.
[systemd-stub command line](https://www.freedesktop.org/software/systemd/man/262/systemd-stub.html)

### Direct UEFI boot for Btrfs roots

Use the same signed UKI inputs and enrolled `sbctl` db certificate. Boot
the running CPS root in UEFI mode with `efivarfs` mounted, and mount the
writable GPT/VFAT ESP and the Btrfs top level as described above. Use an ESP
without CPS systemd-boot entries. Existing firmware entries for other
systems or recovery UKIs can share the ESP.

Set `uefi=#true` in the `boot-btrfs` node in `/etc/cps/image.kdl`. This
selects signed UKIs and one boot attempt. Enable the packaged promotion
service in the root before staging so each snapshot inherits it.

```sh
systemctl enable cps-uefi-bless.service
cps boot-env activate-btrfs --known-good --uefi --root /cps/@ \
    --btrfs-volume /cps --esp /efi \
    --kernel "usr/lib/modules/$(uname -r)/vmlinuz" --initrd boot/cps-initramfs.img \
    --options "rd.luks.name=$luks_uuid=root rw"
```

Bootstrapping verifies the running root and places its signed UKI first in
`BootOrder`, preserving the other firmware entries. For a staged upgrade,
CPS creates a firmware option for the new UKI and sets only `BootNext`.
The promotion service waits for `boot-complete.target` and
`cps-boot-health.service`. It verifies the running Btrfs snapshot and the
fallback root with `verify --quick` semantics, and checks the firmware's
`BootCurrent`, before making the trial the default. Firmware
consumes `BootNext` before launching the trial, so after a failed trial
the next reset boots the known-good root.
[UEFI Boot Manager specification](https://uefi.org/specs/UEFI/2.11/03_Boot_Manager.html)

Use `--uefi` for manual trial activation. With the ESP mounted on the
machine, `boot-env list`, Btrfs rollback and `destroy-btrfs` use the
firmware entries. After booting the fallback, retire an exhausted trial
with `destroy-btrfs` before staging the same transaction again. CPS
preserves an unrelated `BootNext` and reports it if it conflicts with
activation or rollback.

## Health checks and shared state

Distributions can extend the blessing gate with a systemd drop-in for
`cps-boot-health.service`. For example, install the following at
`/etc/systemd/system/cps-boot-health.service.d/readiness.conf` in the image.

```ini
[Unit]
Wants=canopa-app.service
After=canopa-app.service

[Service]
ExecStartPost=/usr/local/libexec/canopa-check-readiness
TimeoutStartSec=90s
```

The readiness command runs after package verification. Give it a bounded
probe of the service itself, and return nonzero if the service cannot
serve requests. Systemd treats a failing post-start command as a failed
health service, so the trial stays unblessed. The VM fixture's
`CPS_VM_BTRFS_HEALTH_FAIL=readiness` mode exercises a healthy package image
whose readiness probe fails on both trial boots.

Boot rollback selects the previous root and its package state. Data mounted
from a sibling subvolume, another device, or a network service has its own
lifecycle. Keep shared state compatible with the previous system until the
trial is accepted, or coordinate its backup and restore with the application.

## Capturing a running Btrfs root

`boot-env capture` takes a read-only snapshot of a CPS root subvolume and
verifies it before publishing a new directory containing `root/`.
Use a writable sibling subvolume on the same Btrfs filesystem for captures,
mounted separately from the root subvolume.

```sh
cps boot-env capture --source / --output /var/lib/cps/captures/before-upgrade
cps plan --root /var/lib/cps/captures/before-upgrade/root \
    --output /scratch/upgrade.json update
cps fetch --root /var/lib/cps/captures/before-upgrade/root --plan /scratch/upgrade.json
cps boot-env prepare --source /var/lib/cps/captures/before-upgrade/root \
    --output /scratch/upgrade --size-mib 2048 /scratch/upgrade.json
```

The capture retains the image identity and revision, so a saved plan for
that revision also applies to the captured baseline. Snapshot creation
holds the shared CPS image lock. Btrfs supplies a point-in-time view of the
root subvolume while applications continue to write to the running root.
Shared data mounted from other filesystems has its own lifecycle.
CPS rejects nested subvolume stubs in the capture and verifies the package
database, installed files, and catalog archives.
[Btrfs subvolumes](https://btrfs.readthedocs.io/en/latest/Subvolumes.html)

Run with privileges for Btrfs snapshot creation and deletion. A failed
validation removes the private snapshot. An abrupt interruption can leave
a `.cps-capture-*` directory containing `root/` in the capture parent.
To remove a capture, run `btrfs subvolume delete CAPTURE/root`, followed
by `rmdir CAPTURE`.

Use `stage-btrfs` to apply a saved plan in a writable snapshot of the
read-only baseline on the same Btrfs filesystem.

```sh
cps boot-env stage-btrfs --source /var/lib/cps/captures/before-upgrade/root \
    --output /var/lib/cps/captures/trial /scratch/upgrade.json
```

The command verifies the baseline and its plan, applies the transaction in
a private subvolume, prepares the selected kernel, and assigns a fresh CPS
image identity with its recorded parent. It publishes `trial/root/` after
verification. A failed transaction deletes the private subvolume. The new
root is writable for configuration review and boot.

Both the captured baseline and a staged Btrfs root can be
[exported as ext4](ext4.md). See the [Btrfs VM tests](../developer/boot-tests.md#btrfs-snapshot-and-boot-tests)
for capture and activation test scenarios.

## Reclaiming old Btrfs environments

Retire an obsolete registered Btrfs root after its replacement has booted
successfully. Use the host's mount namespace and unmount the old root first.

```sh
cps boot-env list --esp /efi
cps boot-env destroy-btrfs --esp /efi --btrfs-volume /mnt/cps-volume --dry-run "$old_id"
cps boot-env destroy-btrfs --esp /efi --btrfs-volume /mnt/cps-volume "$old_id"
```

The command keeps the selected known-good environment, its recorded fallback,
the running environment's recorded fallback, and the loader's configured default.
It rejects the running root, mounted roots,
roots with nested subvolumes or mounts, and trials with attempts left. It checks
the registered filesystem and subvolume identity, then deletes by subvolume ID.
Only that subvolume and its private CPS boot artifacts are removed. Other
captures and shared subvolumes keep their own contents and lifecycle.

CPS first renames the boot entry to a durable `.cps-destroy` retirement record,
then commits subvolume deletion before removing boot artifacts and the record.
`boot-env list` shows interrupted cleanup as `retiring`. Retry the same command
to finish. Activation of that identity stays blocked until cleanup completes.
Keep the recorded root path stable during this operation.

Btrfs refuses to delete its filesystem default subvolume. If it is the obsolete
root, choose another filesystem default, such as top-level ID 5 with
`btrfs subvolume set-default 5 /mnt/cps-volume`, then retry. CPS boot entries
select their subvolume explicitly. Storage reclamation continues in the
background after the deletion transaction commits.
[Btrfs subvolume deletion](https://btrfs.readthedocs.io/en/latest/btrfs-subvolume.html)

Set `CPS_VM_BTRFS_DESTROY=1` for the native VM test to stage and bless another
root, test the destruction guards, recover from the kernel's default-subvolume
refusal and an injected artifact-cleanup failure, and boot the retained fallback
after cleanup. This fixture uses a native C compiler for its failure-injection
helper. Combine
`CPS_VM_BTRFS_HEALTH_FAIL=1` and `CPS_VM_BTRFS_DESTROY_FAILED=1` to verify
that an exhausted trial with damaged managed files can be discarded.
