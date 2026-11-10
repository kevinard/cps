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
- [Nested subvolumes](#nested-subvolumes)
- [Capturing a running Btrfs root](#capturing-a-running-btrfs-root)
- [Reclaiming old Btrfs environments](#reclaiming-old-btrfs-environments)
  - [Retention and pruning](#retention-and-pruning)

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

The node also sets the [retention policy](#retention-and-pruning) and the
[nested subvolumes](#nested-subvolumes) that new roots leave out. The
following example shows the defaults plus one exclusion.

```kdl
boot-btrfs volume="/cps" environments="/cps/@environments" esp="/efi" keep-environments=3 auto-prune=#true {
    exclude-subvolume "var/lib/docker"
}
```

`keep-environments` (1 to 99, default 3) is the number of recent known-good
environments to keep in addition to the protected ones. `auto-prune=#false`
turns off pruning of known-good environments before staging and after a
direct UEFI promotion. Failed and abandoned trials are still removed before
staging. Each `exclude-subvolume` child adds a path, relative to the root, to
the default exclusions.

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
  A Btrfs snapshot omits nested subvolume contents. See
  [Nested subvolumes](#nested-subvolumes).
- Mount the dedicated, writable VFAT systemd-boot ESP at `esp`. CPS reads
  the GPT with the disk's logical block size, so disks with 512-byte and
  4096-byte (4Kn) sectors both work. The ESP can be an automount point, as
  created by `systemd-gpt-auto-generator` or an `x-systemd.automount` fstab
  entry. CPS opens the directory to trigger the mount, then checks that one
  writable VFAT mount sits on the trigger. It keeps the ESP open for the
  operation, so the automount cannot expire while CPS is using it. When
  booting by Btrfs filesystem UUID, the ESP can be on a different disk.
- Supply a kernel and initramfs that can discover the Btrfs filesystem by
  its unique UUID, including any device assembly and unlock needed by your
  layout. CPS publishes Type #1 systemd-boot entries and selects the root with
  `root=UUID=… rootfstype=btrfs rootflags=subvolid=…`. Keep any `/` line in
  `/etc/fstab` usable across snapshots by omitting `subvol=` and `subvolid=`
  from that line. Keep the selectors for shared mounts.

For example, on a LUKS2 installation using mkinitcpio's systemd and
`sd-encrypt` hooks, mount the top level from the open mapping and pass the
LUKS container UUID in `rd.luks.name=`. CPS reads the distinct filesystem
UUID from the mounted Btrfs volume. Prefer a manifest-managed kernel that
declares its `kernel-install` artifacts, as described in
[systemd integration](../maintainer/systemd.md). CPS prepares the kernel and
initramfs inside each staged root, so omit `--kernel` and `--initrd`.

```sh
mount -t btrfs -o subvolid=5 /dev/mapper/root /cps
cps boot-env activate-btrfs --known-good --root /cps/@ \
    --btrfs-volume /cps --esp /efi \
    --options "rd.luks.name=$luks_uuid=root rw"
```

Here `$luks_uuid` is the UUID of the encrypted container. Set `options` to
the same value in the `boot-btrfs` node for system package commands, and omit
`root-partuuid` in that node.

An image that supplies boot files manually must keep them as regular files at
version-independent paths that its own kernel update replaces, for example
`boot/vmlinuz` and `boot/cps-initramfs.img`. Pass those paths with `--kernel`
and `--initrd`, and set the same `kernel` and `initrd` values in
`boot-btrfs`. A path that contains a kernel release, such as
`usr/lib/modules/$(uname -r)/vmlinuz`, is fixed at bootstrap. It stops
matching the root after the first kernel upgrade, and activation of the
upgraded environment fails or starts the old kernel. The firmware-launched UKIs and their embedded command lines are
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
machine, `boot-env list`, Btrfs rollback, `destroy-btrfs` and `prune` use
the firmware entries. After a failed trial boots the fallback, the next
staged change removes the failed trial, or run `cps boot-env discard-trial`.
After a successful promotion, the bless service applies the
[retention policy](#retention-and-pruning) unless `auto-prune=#false`. CPS
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

## Nested subvolumes

A Btrfs snapshot does not descend into nested subvolumes. In the snapshot,
each one becomes an empty placeholder directory that cannot hold files.
systemd creates `/var/lib/machines` and `/var/lib/portables` as subvolumes
on Btrfs, and container storage drivers create others.

CPS excludes `var/lib/machines` and `var/lib/portables` by default. Add more
paths with `exclude-subvolume` in `boot-btrfs`, or with `--exclude-subvolume`
on `boot-env capture` and `stage-btrfs`. In a new root, CPS replaces the
placeholder at an excluded path with an ordinary empty directory. That
directory gets the mode, owner, and access and modification times of the
source subvolume's top directory. Its extended attributes, including any
security label, are not copied. The new root starts with that directory
empty. The data stays in the source root's subvolume, and rollback does not
change it. To share the data across environments, make it a sibling
subvolume and mount it at that path from each root's `/etc/fstab`.

CPS rejects a nested subvolume at any other path and names the path in the
error. It also rejects an excluded subvolume that contains CPS-managed
files, or a path that contains the CPS image state, because the new root
would lose them. `destroy-btrfs` and `prune` refuse to delete a root that
still contains nested subvolumes. Delete or move them first.

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
CPS handles [nested subvolumes](#nested-subvolumes) as described above. It
then verifies the package database, installed files, and catalog archives,
and makes the capture read-only.
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

The command verifies the baseline and its plan, checks free space, applies the transaction in
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

### Retention and pruning

Each environment keeps its own Btrfs root and its own kernel and initramfs,
or UKI, on the ESP. `cps boot-env prune` applies the retention policy from
`boot-btrfs` by running `destroy-btrfs` for each environment the policy no
longer needs. Use `--dry-run` to run every check and list the removals
without changing anything, and `--keep` to override `keep-environments`.

```sh
cps boot-env prune --dry-run
cps boot-env prune
```

Prune never removes the running environment, the environment selected for
the next boot, the loader's configured default, the recorded fallback of
the running or selected environment, or a trial that is pending or running.
From the rest, it removes the following, in order:

1. Environments with a `.cps-destroy` retirement record, finishing their
   cleanup.
2. Failed trials, whose boot attempts are exhausted or whose firmware boot
   was cancelled.
3. Abandoned trials. These still have attempts left but are no longer
   selected, for example after a rollback. Prune first renames the entry to
   zero attempts, which systemd-boot never selects, and then destroys it.
4. Known-good environments older than the `keep-environments` most recent
   ones.

Each removal repeats all the `destroy-btrfs` checks, including the
mount, nested subvolume, and fallback checks. If one environment cannot be
removed, prune reports it and continues with the others. The command exits
with a nonzero status after a failure. Prune skips a root that is still the Btrfs
filesystem default subvolume, such as an installation's original `@`, and
reports it until you choose another default, as described above. Prune refuses to run while a
trial is pending, because the destroy checks require a known-good selection.

With `auto-prune` enabled, which is the default, `cps install`, `upgrade`,
and the other system commands prune before they stage a change. The
`cps-uefi-bless.service` unit also prunes after it promotes a trial.
Automatic pruning reports problems as warnings and never blocks the
transaction. When `auto-prune=#false`, staging still removes failed and
abandoned trials, but keeps all known-good environments. Empty `upgrade-*`
directories left in `environments` are removed as well.

To give up on a trial, run `cps boot-env discard-trial`. If a trial is
selected for the next boot, the command first selects its known-good
fallback, as `rollback` does. This includes a direct UEFI trial scheduled
with `BootNext`. It then removes every failed or abandoned trial that is not
running. If the system is running the trial, the command selects the
fallback and keeps the running root. Reboot, and the next prune or staged
change removes it. While a trial is pending, system package commands refuse
to change the running root and point to this command.

Before staging, CPS checks the Btrfs filesystem's free space with
`statvfs(3)`. Btrfs reports the space that new data can use under the data
profile, including unallocated device space. CPS requires the sizes of the
plan's new payloads plus a 32 MiB reserve for metadata and kernel
preparation. The check is an early estimate. Btrfs can still run out of
metadata space, which `btrfs filesystem usage` shows. Before activation
copies boot files, CPS checks the ESP's free space against the exact sizes
of the kernel and initramfs or UKI, the entry and a `loader.conf` copy,
rounded up to whole clusters, plus a 1 MiB margin. Both checks fail before
any change and suggest `cps boot-env prune`.

Set `CPS_VM_BTRFS_DESTROY=1` for the native VM test to stage and bless another
root, test the destruction guards, recover from the kernel's default-subvolume
refusal and an injected artifact-cleanup failure, and boot the retained fallback
after cleanup. This fixture uses a native C compiler for its failure-injection
helper. Combine
`CPS_VM_BTRFS_HEALTH_FAIL=1` and `CPS_VM_BTRFS_DESTROY_FAILED=1` to verify
that an exhausted trial with damaged managed files can be discarded.
