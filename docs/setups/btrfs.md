# Set up Btrfs boot environments

[Documentation](../README.md) · [Setup guides](README.md)

Start with a CPS-managed Btrfs root.
[Assemble a bootable base root](boot-root.md) if you need to provision one.
Each staged change becomes a new environment: a writable snapshot of the
running root that boots as a trial, with the registered parent as its
fallback. Choose systemd-boot with Type #1 entries, systemd-boot with signed
UKIs, or direct UEFI boot with signed UKIs. Listing, rollback, and the loader
configuration work as described in [boot operations](boot-operations.md).

## On this page

- [Configure live system commands](#configure-live-system-commands)
- [Activating Btrfs roots](#activating-btrfs-roots)
  - [Signed UKIs for Btrfs roots](#signed-ukis-for-btrfs-roots)
  - [Direct UEFI boot for Btrfs roots](#direct-uefi-boot-for-btrfs-roots)
- [Health checks and shared state](#health-checks-and-shared-state)
- [Merging /etc at shutdown](#merging-etc-at-shutdown)
- [Backups before live changes](#backups-before-live-changes)
- [Nested subvolumes](#nested-subvolumes)
- [Capturing a running Btrfs root](#capturing-a-running-btrfs-root)
- [Reclaiming old Btrfs environments](#reclaiming-old-btrfs-environments)
  - [Retention and pruning](#retention-and-pruning)
  - [Discarding a trial](#discarding-a-trial)
  - [Free space checks](#free-space-checks)
- [Testing in a VM](#testing-in-a-vm)

## Configure live system commands

The [system commands](../client/system.md), such as `cps install` and
`cps upgrade`, read `/etc/cps/image.kdl`. Add a `boot-btrfs` node there next
to the [publisher configuration](../client/publishers.md) and any
[mirror entries](../client/mirrors.md#synchronizing-a-signed-profile). For
example, a single-device Btrfs root on a GPT disk can use these settings.

```kdl
boot-btrfs volume="/cps" environments="/cps/@environments" esp="/efi" root-partuuid="7adff456-af50-498f-b39a-dcf87cb4a7c1" options="rw" {
    exclude-subvolume "var/lib/docker"
}
stage-package "system/elf-runtime"
stage-package "system/systemd"
```

| Setting | Default | Meaning |
| --- | --- | --- |
| `volume` | Required | Mount point of the Btrfs top-level subvolume, mounted read-write. |
| `environments` | Required | A separate, root-owned subvolume below `volume` that holds staged roots. |
| `esp` | Required | Mount point of the writable EFI System Partition. |
| `root-partuuid` | None | Boot the root by its GPT partition, as described in [Activating Btrfs roots](#activating-btrfs-roots). Omit it to boot by Btrfs filesystem UUID with an initramfs. |
| `options` | Empty | Kernel arguments that the installation requires. |
| `kernel`, `initrd` | `boot/vmlinuz`, none | Boot files, only for images that supply them manually. |
| `uki` | `#false` | Boot [signed UKIs](#signed-ukis-for-btrfs-roots) with systemd-boot. |
| `uefi` | `#false` | Boot signed UKIs [directly from UEFI](#direct-uefi-boot-for-btrfs-roots). |
| `tries` | 2, or 1 with `uefi` | Boot attempts for a trial, from 1 to 99. Direct UEFI trials allow exactly one. |
| `keep-environments` | 3 | Known-good environments to keep, from 1 to 99. See [Retention and pruning](#retention-and-pruning). |
| `auto-prune` | `#true` | Prune old known-good environments automatically. |
| `live-backup` | `#true` | Save the running root as a [backup environment](#backups-before-live-changes) before each live change. |
| `exclude-subvolume` | | A child node naming a [nested subvolume](#nested-subvolumes), relative to the root, that new roots start empty. Repeat it for more paths. |

Each `stage-package` node names a package whose installation, replacement,
or removal always stages a new environment instead of changing the running
root. CPS also stages changes to boot paths such as the kernel, initramfs,
modules, `/etc/fstab`, and the CPS binary, packages that declare
`requires-reboot`, and triggers whose outputs can cover boot paths. See
[Stage changes for the next boot](../client/system.md#stage-changes-for-the-next-boot).

Use the real partition UUID and package names. Mount the Btrfs top-level
subvolume read-write at `volume`, create the `environments` subvolume below
it, and mount the ESP at `esp`. Keep the repository caches at stable paths
outside the boot roots. Register the initial root with
`boot-env activate-btrfs --known-good` and install the
[CPS health service](../../packaging/systemd/cps-boot-health.service) for
trial boot assessment. The same package enables
`cps-finalize-trial.path`, which [merges /etc](#merging-etc-at-shutdown)
into a staged trial at shutdown.

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
  mounted at ordinary directories in each root. A Btrfs snapshot omits
  nested subvolume contents. See [Nested subvolumes](#nested-subvolumes).
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
[kernel packages](../maintainer/systemd.md#kernel-packages). CPS prepares the
kernel and initramfs inside each staged root, so omit `--kernel` and
`--initrd`.

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
upgraded environment fails or starts the old kernel.

For a single-device Btrfs filesystem directly on a GPT Linux filesystem
partition, you can select the partition with `--root-partuuid`. This mode
requires the writable VFAT ESP on the same GPT disk.

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
Bootstrap can read the running root through its top-level path. A trial gets
two boot attempts unless you pass `--tries`. Each entry records the filesystem
UUID, subvolume UUID and ID, and root path. CPS checks these against the
mounted Btrfs volume, checks the CPS image identity and revision, and verifies
the fallback root and its boot artifacts. The kernel receives the recorded
subvolume ID through `rootflags`.
[Btrfs ioctl documentation](https://btrfs.readthedocs.io/en/stable/btrfs-ioctl.html)

Activation marks the root with the `user.cps.boot-environment` extended
attribute before publishing its boot entry. CPS checks that mark under the
image's exclusive lock and refuses inactive-image transactions such as
`cps apply --root` on a registered root. Change it with the system commands,
or stage a snapshot of a [capture](#capturing-a-running-btrfs-root) with
`stage-btrfs`, which clears the inherited mark on its private writable
snapshot. Activation retries accept the matching
registration. If activation fails after marking the root, rerun it to finish
registration.

### Signed UKIs for Btrfs roots

Use systemd-boot 262 or newer, `systemd-ukify`, and a configured `sbctl`
with its db certificate trusted by the firmware. Sign the systemd-boot
binary you launch with that key as part of your Secure Boot setup. Keep the
host's `sbctl` state on a protected persistent mount shared by the
environments, so a root snapshot or rollback does not copy or revert its keys.
[sbctl manual](https://man.archlinux.org/man/sbctl.8.en)

Add `--uki` to the `activate-btrfs --known-good` command above, and set
`uki=#true` in `boot-btrfs` for staged system transactions. For manual trial
activation, pass `--uki` again. Bootstrap a signed known-good environment
before publishing signed trials. The trial and its fallback must use the same
boot artifact type.

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
CPS creates a firmware boot option for the new UKI and sets only `BootNext`.
Firmware consumes `BootNext` before launching the trial, so after a failed
trial the next reset boots the known-good root.
[UEFI Boot Manager specification](https://uefi.org/specs/UEFI/2.11/03_Boot_Manager.html)

`cps-uefi-bless.service` runs `cps boot-env bless-uefi` once
`boot-complete.target` and `cps-boot-health.service` succeed. The command
verifies the running Btrfs snapshot and the fallback root with
`verify --quick` semantics and checks the firmware's `BootCurrent` before it
makes the trial the default. It then applies the
[retention policy](#retention-and-pruning) unless `auto-prune=#false`.

Use `--uefi` for manual trial activation. With the ESP mounted on the
machine, `boot-env list`, Btrfs rollback, `destroy-btrfs`, and `prune` use
the firmware entries. After a failed trial boots the fallback, the next
staged change removes the failed trial, or run
[`cps boot-env discard-trial`](#discarding-a-trial). CPS preserves an
unrelated `BootNext` and reports it if it conflicts with activation or
rollback.

## Health checks and shared state

`cps-boot-health.service` runs `cps verify --quick --root /` before
`boot-complete.target`, and a trial is only blessed when it succeeds.
Distributions can extend the gate with a systemd drop-in. For example,
install the following at
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
probe of the service itself, and return nonzero if the service cannot serve
requests. Systemd treats a failing post-start command as a failed health
service, so the trial stays unblessed.

Boot rollback selects the previous root and its package state. Data mounted
from a sibling subvolume, another device, or a network service has its own
lifecycle. Keep shared state compatible with the previous system until the
trial is accepted, or coordinate its backup and restore with the application.

## Merging /etc at shutdown

A trial is a snapshot taken when the change is staged. Staging records the
snapshot's `/etc`, before the plan changes it, as the merge base in the
trial's image state. After a system command or `boot-env activate-btrfs`
selects the trial, CPS creates `/run/cps/finalize-trial`, and `cps-finalize-trial.path` starts
`cps-finalize-trial.service`. When the system shuts down, stopping that
service runs `cps boot-env finalize-trial`. The command finds the trial
selected for the next boot whose fallback is the running root, takes the
trial's image lock, and merges the running `/etc` into it as described in
[Local changes before the reboot](../client/system.md#local-changes-before-the-reboot).

- CPS builds the merged tree in the trial's image state, cloning unchanged
  files from the trial's `/etc` and copying local changes from the running
  one with their owner, mode, extended attributes, timestamps, and hard
  links. It then swaps the tree with the trial's `/etc` in one
  `RENAME_EXCHANGE` rename. Pending configuration updates and saved running
  versions are recorded before the swap, because they hold with either
  `/etc`. A crash at any point leaves the trial bootable with the staged or
  the merged `/etc`.
- After the swap, the base becomes the running `/etc` that was merged. A
  repeated run carries only later edits, so `finalize-trial` is safe to run
  by hand before a reboot, and the shutdown run then adds what changed since.
  The trial keeps the report of the last merge as `etc-merge.json` in its
  image state.
- If the merge fails, CPS leaves the trial's `/etc` as it was and cancels
  the trial, as `boot-env rollback` would. Booting a trial without a new
  password or user can lock you out, while the fallback keeps every change
  and the change can be staged again. The service log names the cause.
- Mount points and nested subvolumes inside `/etc` are left alone. `/var`
  is not merged.

The unit stops after the ordinary services, before the local file systems
are unmounted. The ESP and the Btrfs top-level volume must stay mounted until
then, as they do when they belong to `local-fs.target`. Otherwise add a
drop-in, for example
`/etc/systemd/system/cps-finalize-trial.service.d/mounts.conf`.

```ini
[Unit]
RequiresMountsFor=/efi /cps
```

The flag lasts for one boot. If you boot the fallback by hand while a trial
is still selected, run `sudo cps boot-env finalize-trial --arm` to request the
merge at the next shutdown. A trial that is cancelled, rolled back, or
discarded before the shutdown is not merged.

## Backups before live changes

A change that does not need a reboot applies to the running root. Before
it starts, CPS takes a read-only snapshot of the root in
`environments/backup-IMAGE-REVISION/root`, gives it a new image identity, and
registers it as a known-good environment titled `Canopa backup`. The backup
has no fallback and is never selected automatically. If `loader.conf` still
selects the newest entry with `default cps-*.conf`, CPS first pins the running
environment, because the backup would be the newest. With direct UEFI boot,
CPS creates the backup's firmware boot option without changing `BootOrder`.

To return to the system as it was before a live change, select the backup
and reboot. Rollback makes the backup's subvolume writable before it selects
it, because a running root must be writable.

```sh
sudo cps boot-env list --esp /efi
sudo cps boot-env rollback --esp /efi --btrfs-volume /cps "$backup_id"
```

The snapshot shares all data with the running root. CPS verifies it with
`verify --quick` semantics, since it has the same inodes as the root it was
taken from. A retry of the same change, for example after
`cps transaction abort`, reuses the backup of that revision. Interrupted
changes resumed with `cps resume` take no new backup, and neither do
`cps repair` and `cps revert`: they restore package files on a root that
fails verification, so its snapshot could not boot as known-good.

Each backup counts toward `keep-environments` like any other known-good
environment, and [pruning](#retention-and-pruning) removes the oldest first.
With `auto-prune`, CPS prunes before the backup to make room on the ESP and
again after it. Each backup also costs one copy of the kernel and initramfs,
or a signed UKI, on the ESP, checked like any
[activation](#free-space-checks). CPS does not share these files between
entries: a UKI embeds the subvolume it boots, and shared Type #1 files would
need reference counting in every removal path. Keep `keep-environments`
within the ESP's capacity. If a backup cannot be created or registered, the
live change stops before it changes anything. Free space with
`cps boot-env prune`, or set `live-backup=#false` to apply live changes
without a backup.

## Nested subvolumes

A Btrfs snapshot does not descend into nested subvolumes. In the snapshot,
each one becomes an empty placeholder directory that cannot hold files.
systemd creates `/var/lib/machines` and `/var/lib/portables` as subvolumes
on Btrfs, and container storage drivers create others.

CPS always excludes `var/lib/machines` and `var/lib/portables`. Add more
paths with `exclude-subvolume` in `boot-btrfs`, or with `--exclude-subvolume`
on `boot-env capture` and `stage-btrfs`. In a new root, CPS replaces the
placeholder at an excluded path with an ordinary empty directory. That
directory gets the mode, owner, and access and modification times of the
source subvolume's top directory. Its extended attributes, including any
security label, are not copied. The data stays in the source root's
subvolume, and rollback does not change it. To share the data across
environments, make it a sibling subvolume and mount it at that path from each
root's `/etc/fstab`.

CPS rejects a nested subvolume at any other path and names the path in the
error. It also rejects an excluded subvolume that contains CPS-managed
files, or a path that contains the CPS image state, because the new root
would lose them. `destroy-btrfs` and `prune` refuse to delete a root that
still contains nested subvolumes. Delete or move them first.

## Capturing a running Btrfs root

`boot-env capture` takes a read-only snapshot of a CPS root subvolume and
verifies it before publishing a new directory containing `root/`. Use a
writable sibling subvolume on the same Btrfs filesystem for captures, mounted
separately from the root subvolume.

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
Shared data mounted from other filesystems has its own lifecycle. After
handling [nested subvolumes](#nested-subvolumes), CPS verifies the package
database, installed files, and catalog archives, and makes the capture
read-only. [Btrfs subvolumes](https://btrfs.readthedocs.io/en/latest/Subvolumes.html)

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

The command verifies the baseline and its plan,
[checks free space](#free-space-checks), applies the transaction in a private
subvolume, prepares the selected kernel, and assigns a fresh CPS image
identity with its recorded parent. It publishes `trial/root/` after
verification. A failed transaction deletes the private subvolume. The new
root is writable for configuration review and boot.

Both the captured baseline and a staged Btrfs root can be
[exported as ext4](ext4.md).

## Reclaiming old Btrfs environments

Retire an obsolete registered Btrfs root after its replacement has booted
successfully. Use the host's mount namespace and unmount the old root first.
[Retention and pruning](#retention-and-pruning) runs the same command for
every environment the policy no longer needs.

```sh
cps boot-env list --esp /efi
cps boot-env destroy-btrfs --esp /efi --btrfs-volume /mnt/cps-volume --dry-run "$old_id"
cps boot-env destroy-btrfs --esp /efi --btrfs-volume /mnt/cps-volume "$old_id"
```

The command keeps the selected known-good environment, its recorded fallback,
the running environment's recorded fallback, and the loader's configured
default. It rejects the running root, mounted roots, roots with nested
subvolumes or mounts, and trials with attempts left. It checks the registered
filesystem and subvolume identity, then deletes by subvolume ID. Only that
subvolume and its private CPS boot artifacts are removed. Other captures and
shared subvolumes keep their own contents and lifecycle.

CPS first renames the boot entry to a durable `.cps-destroy` retirement
record, then commits subvolume deletion before removing boot artifacts and
the record. `boot-env list` shows interrupted cleanup as `retiring`. Retry the
same command to finish. Activation of that identity stays blocked until
cleanup completes. Keep the recorded root path stable during this operation.

Btrfs refuses to delete its filesystem default subvolume. If it is the
obsolete root, choose another filesystem default, such as top-level ID 5 with
`btrfs subvolume set-default 5 /mnt/cps-volume`, then retry. CPS boot entries
select their subvolume explicitly. Storage reclamation continues in the
background after the deletion transaction commits.
[Btrfs subvolume deletion](https://btrfs.readthedocs.io/en/latest/btrfs-subvolume.html)

### Retention and pruning

Each environment keeps its own Btrfs root and its own kernel and initramfs,
or UKI, on the ESP. `cps boot-env prune` applies the retention policy from
the `boot-btrfs` node in `/etc/cps/image.kdl`, or the file named by
`--config`, by running `destroy-btrfs` for each environment the policy no
longer needs. Use `--dry-run` to run every check and list the removals
without changing anything, and `--keep` to override `keep-environments`.

```sh
cps boot-env prune --dry-run
cps boot-env prune
```

Prune never removes the running environment, the environment selected for
the next boot, the loader's configured default, the recorded fallback of
the running or selected environment, or a trial that is selected or running.
From the rest, it removes:

- Environments with a `.cps-destroy` retirement record, finishing their
  cleanup first.
- Failed trials, whose boot attempts are exhausted or whose firmware boot
  was cancelled.
- Abandoned trials, which still have attempts left but are no longer
  selected, for example after a rollback. Prune first renames the entry to
  zero attempts, which systemd-boot never selects, and then destroys it.
- Known-good environments older than the `keep-environments` newest ones.
  The protected environments above count toward that number when they are
  among the newest, and are kept in any case.

Trials and known-good environments are removed oldest first. Each removal
repeats all the `destroy-btrfs` checks, including the mount, nested
subvolume, and fallback checks. If one environment cannot be removed, prune
reports it and continues with the others, and the command exits with a
nonzero status. Prune skips a root that is still the Btrfs filesystem default
subvolume, such as an installation's original `@`, and reports it until you
choose another default, as described above. Prune refuses to run while a
trial is selected for the next boot, because the destroy checks require a
known-good selection.

With `auto-prune` enabled, which is the default, `cps install`, `upgrade`,
and the other system commands prune before they stage a change and around
each [backup](#backups-before-live-changes), and `cps boot-env bless-uefi`
prunes after it promotes a trial. Automatic pruning reports problems as
warnings and never blocks the transaction. With `auto-prune=#false`, staging
still removes failed and abandoned trials but keeps every known-good
environment. Pruning also removes empty `upgrade-*` and `backup-*`
directories left in `environments`.

The prune before staging keeps the trial of an interrupted activation of the
same change, whose root is that change's `upgrade-*/root`, so that the retry
resumes it. An interrupted systemd-boot activation can leave the counted
entry without its `preferred` selection, and an interrupted direct UEFI
activation can leave the entry without `BootNext`. The trial is kept while
it has attempts left, or, with direct UEFI boot, while the firmware can
still schedule it in the current boot. `cps boot-env prune` run by hand
removes such a trial like any other abandoned or failed trial.

### Discarding a trial

To give up on a trial, run `cps boot-env discard-trial`. It reads the same
configuration as `prune`.

- If a trial is selected for the next boot, including a direct UEFI trial
  scheduled with `BootNext`, the command first selects its known-good
  fallback, as `rollback` does.
- If the system is running the trial, the command selects the fallback and
  keeps the running root. Reboot, and the next prune or staged change removes
  it.
- It then removes every failed or abandoned trial that is not running, and
  keeps all known-good environments.

While a trial is selected, system package commands refuse to change the
running root and point to this command.

### Free space checks

Before staging, CPS checks the Btrfs filesystem's free space with
`statvfs(3)`. Btrfs reports the space that new data can use under the data
profile, including unallocated device space. CPS requires the sizes of the
plan's payloads, the kernel artifacts it prepares, and a 32 MiB reserve for
metadata. When the plan installs a kernel or regenerates its initramfs, CPS
copies the kernel and initramfs into the image state, so each counts once
more. A generated initramfs is not a payload: its trigger writes it into the
root before CPS copies it, so it counts twice. Its size is known only after
the trigger runs, so CPS estimates it from the current initramfs at that
path, or else from the largest `usr/lib/modules/*/initrd` in the base. The
check is an early estimate. Btrfs can still run out of metadata space, which
`btrfs filesystem usage` shows.

Before activation copies boot files, CPS checks the ESP's free space against
the files it adds: the kernel and initramfs or the UKI, the entry, and a
temporary `loader.conf` copy, each rounded up to whole clusters, plus a 1 MiB
margin. Files already in place from an interrupted attempt do not count.

Both checks fail before any change and suggest `cps boot-env prune`. The
ESP check also applies to each [backup](#backups-before-live-changes).

## Testing in a VM

The [Btrfs VM tests](../developer/boot-tests.md#btrfs-snapshot-and-boot-tests)
exercise capture, staging, activation, blessing, and rollback. These settings
add scenarios for the features on this page.

| Setting | Scenario |
| --- | --- |
| `CPS_VM_BTRFS_HEALTH_FAIL=readiness` | A healthy package image whose readiness probe fails on both trial boots, so the trial stays unblessed. |
| `CPS_VM_BTRFS_DESTROY=1` | Stage and bless another root, test the destruction guards, recover from the kernel's default-subvolume refusal and an injected artifact-cleanup failure, and boot the retained fallback after cleanup. Needs a native C compiler for the failure-injection helper. |
| `CPS_VM_BTRFS_HEALTH_FAIL=1` with `CPS_VM_BTRFS_DESTROY_FAILED=1` | Discard an exhausted trial with damaged managed files. |
| `CPS_VM_STAGED_TRANSACTIONS=1` | Freeze, unfreeze, and update a private root before its first activation. |
