# Deploy ext4 boot environments

[Documentation](../README.md) · [Setup guides](README.md)

Start with an inactive root that contains the packages and boot files
for the target system. [Assemble a bootable base root](boot-root.md) if you
need to provision one.

## On this page

- [Prepare and deploy an offline GPT disk image](#prepare-and-deploy-an-offline-gpt-disk-image)
- [Provisioning the first root on an installed disk](#provisioning-the-first-root-on-an-installed-disk)
- [Activating a trial on an installed disk](#activating-a-trial-on-an-installed-disk)
- [Reusing a root slot](#reusing-a-root-slot)

## Prepare and deploy an offline GPT disk image

Boot environments use separate Linux roots and an EFI System Partition
(ESP). The systemd-boot workflow below starts with an inactive,
single-filesystem base root containing the kernel. GNU `cp` clones the
image and uses a reflink where supported. The destination must be a new
directory. Set `base_partuuid`, `staged_partuuid`, and `esp_partuuid` to the
GPT PARTUUIDs of the two root slots and the ESP in `template.raw`. Set each
export size to the size of its destination partition.

```sh
cps boot-env export --root base --output base.ext4 --size-mib 2048 --receipt > base.kdl
cps boot-env deploy --disk template.raw --root-image base.ext4 \
    --root-partuuid "$base_partuuid" --output base.raw
cps boot-env activate --known-good --root base --root-partuuid "$base_partuuid" \
    --disk base.raw --receipt base.kdl --esp-partuuid "$esp_partuuid" \
    --output-disk bootable-base.raw
cps plan --root base --output upgrade.json update
cps boot-env stage --source base --root staged upgrade.json
cps boot-env export --root staged --output staged.ext4 --size-mib 2048 --receipt > staged.kdl
cps boot-env deploy --disk bootable-base.raw --root-image staged.ext4 \
    --root-partuuid "$staged_partuuid" --output next.raw
cps boot-env activate --root staged --root-partuuid "$staged_partuuid" \
    --disk next.raw --receipt staged.kdl --esp-partuuid "$esp_partuuid" \
    --output-disk bootable-next.raw
```

`stage` verifies the new root and gives it a fresh image identity. Export
checks the root and its prepared kernel, then writes a new regular file.
Keep the source inactive and stable during export. CPS rejects nested
mounts and special files. The export includes local state and CPS metadata
alongside the package files. It uses `mke2fs` directory import to preserve
ownership, modes, links, and extended attributes. It runs `e2fsck` read-only
before publishing the image and refuses to replace an existing path. Use
e2fsprogs with support for `root_perms`. Filesystem timestamps and formatter
defaults affect the image bytes. [mke2fs manual](https://man7.org/linux/man-pages/man8/mke2fs.8.html)

Set `SOURCE_DATE_EPOCH` to export a byte-identical ext4 payload and receipt
from the same stable source tree, partition size, and e2fsprogs build. CPS
uses its own formatter profile and the image UUID as the directory hash
seed. The formatter clamps imported timestamps to the epoch and uses it
for filesystem creation. Keep source contents and metadata stable between
exports, including access times older than the epoch. A read-only source
mount preserves those access times. Use a decimal epoch from 0 to
4294967295 and e2fsprogs with `root_perms` and `SOURCE_DATE_EPOCH` support.
[e2fsprogs release notes](https://github.com/tytso/e2fsprogs/blob/v1.47.1/doc/RelNotes/v1.47.1.txt)

For an offline VM or disk image, provision a raw GPT template with 512-byte
sectors, the sector size CPS assumes for disk image files, one Linux filesystem partition per root, and a FAT EFI System
Partition containing systemd-boot. `deploy` uses GNU
`cp`, util-linux `sfdisk`, and `e2fsck`. It clones the source disk, checks
the GPT copies and partition ranges, and writes the payload to the selected
partition. The payload must match the partition size exactly. CPS checks
the deployed bytes by SHA-256 before publishing a new disk file. Keep the
inputs offline during the operation. The source disk is read-only, and the
new disk retains the previous contents outside the selected partition.
[sfdisk manual](https://man7.org/linux/man-pages/man8/sfdisk.8.html)

The ext4 filesystem UUID identifies the CPS image, and the label records
its revision in hexadecimal. Use the separate
**GPT partition UUID** for both `deploy` and `activate`. Keep the KDL export
receipt on the trusted host, outside the image. With `--disk` and
`--receipt`, activation checks the receipt against the root's identity,
revision, and CPS state. It checks the bytes in the named GPT partition
against the export's SHA-256 and uses `blkid` to confirm the ext4 UUID and
revision label. For a trial, it checks those two identifiers on the
registered fallback partition of the same disk. Keep the disk offline
through activation. The receipt describes the freshly deployed root,
whose bytes can change after boot.
For a detached ESP directory, activation accepts the operator's partition
mapping when both flags are omitted. Activation on a mounted VFAT ESP
requires `activate-device`.
[blkid manual](https://man7.org/linux/man-pages/man8/blkid.8.html)

## Provisioning the first root on an installed disk

Start with an inactive CPS base image and a dedicated GPT disk. CPS reads
the GPT with the disk's logical block size, so disks with 512-byte and
4096-byte (4Kn) sectors both work. Mount its writable VFAT ESP with systemd-boot
already installed and no boot entries registered. Choose an unmounted
Linux filesystem partition for the base. Set the export size to match that
partition exactly.

```sh
cps boot-env export --root /scratch/base --output /scratch/base.ext4 \
    --size-mib 2048 --receipt > /scratch/base.kdl
cps boot-env activate-device --known-good --root /scratch/base --esp /efi \
    --device "/dev/disk/by-partuuid/$base_partuuid" \
    --root-partuuid "$base_partuuid" \
    --receipt /scratch/base.kdl --root-image /scratch/base.ext4
```

The command writes the selected partition and verifies it before publishing
the first entry. `--known-good` makes that entry the initial default without
a trial counter. If the root was already written, omit `--root-image` to
verify its bytes against the receipt and finish registration.

## Activating a trial on an installed disk

You can plan and stage directly from the installed root during a rescue boot.
After a clean shutdown, mount that ext4 partition read-only and make the
image's configured repositories or synced caches available at their recorded
paths. Use writable scratch space outside the mounted root for the plan,
staged tree, and export. CPS holds a shared image lock while copying and
rejects nested mounts, including bind mounts.

`apply` requires an inactive working image. This command rejects transactions
against the running root and against a mounted ext4 root with a CPS revision
label, including during a rescue boot. Ext4 root labels consisting of exactly
16 lowercase hexadecimal digits are reserved for these deployments.

```sh
mount -t ext4 -o ro,noload "/dev/disk/by-partuuid/$base_partuuid" /mnt/canopa
cps plan --root /mnt/canopa --output /scratch/upgrade.json update
cps fetch --root /mnt/canopa --plan /scratch/upgrade.json
cps boot-env prepare --source /mnt/canopa --output /scratch/upgrade \
    --size-mib 2048 /scratch/upgrade.json
```

Use `ro,noload` on a clean ext4 filesystem to avoid journal writes to the
source partition. [Linux ext4 mount options](https://docs.kernel.org/admin-guide/ext4.html)

`prepare` creates a new directory containing `root/`, `root.ext4`, and
`receipt.kdl`. It uses cached plan payloads and the same checks as `stage`
and `export`, including the selected kernel and `SOURCE_DATE_EPOCH`
settings. CPS publishes the directory only after formatting and verifying
the image and writing its receipt. A formatting or verification failure
leaves the requested output path available for retry. An abrupt interruption
can leave a private `.cps-prepare-*` directory in the output parent.

Export receipts begin with `schema-version 1 kind="boot-export-receipt"`
and include a SHA-256 fingerprint of the source tree. CPS
checks the tree before and after formatting and checks it again during
activation. The fingerprint covers paths, file contents, modes, owners,
symbolic links, hard links within the root, and extended attributes. It is
independent of inode numbers and timestamps.

Use separate `stage` and `export` commands when you need to edit the staged
root or resolve configuration updates before exporting it.

A private staged environment can receive several saved transactions before
activation. For example, stage an `unfreeze` plan, then plan and apply the
package update against that new root. Its image identity stays the same and
its recorded boot parent remains the original known-good source. Export after
the last transaction and use the fresh receipt when activating it. Receipts
from an earlier revision remain invalid. Registered Btrfs roots and installed
ext4 roots retain their package-write protection.

Set `CPS_VM_STAGED_TRANSACTIONS=1` on the native Btrfs VM test to freeze, unfreeze,
and update a private root before its first activation. The signed-profile
rollback VM uses that option to apply a review transaction after staging,
reject the old receipt, and verify the reviewed root through blessing,
reboot, and rollback.

With a known-good base already registered, use a dedicated, unregistered
Linux filesystem partition on the same GPT disk as the mounted ESP for the
trial. The ext4 export must match the partition size.

```sh
cps boot-env activate-device --root /scratch/upgrade/root --esp /efi \
    --device "/dev/disk/by-partuuid/$trial_partuuid" \
    --root-partuuid "$trial_partuuid" \
    --receipt /scratch/upgrade/receipt.kdl --root-image /scratch/upgrade/root.ext4
```

CPS checks the GPT and compares the kernel's device extents to the requested
root PARTUUID and the writable VFAT mount. It opens the target exclusively,
checks the registered fallback, writes the export, and verifies the target
bytes and filesystem identity before publishing the counted boot entry.
Run with an account that can open the target and parent block devices and
write the ESP. Keep the root PARTUUIDs unique across attached disks.
[Linux block device open semantics](https://man7.org/linux/man-pages/man2/open.2.html)

Omit `--root-image` to verify an already deployed trial against its export
receipt and finish activation. Use the mounted ESP with `boot-env list`
and `boot-env rollback` as usual.

After a successful upgrade, you can reuse the older slot by passing
`--replace "$old_environment_id"` with `--root-image` and the older root's
device and PARTUUID. CPS verifies the new export, durably retires the old
boot entry and artifacts, and writes the root while the selected known-good
environment stays available. A mounted slot or a trial with attempts left
cannot be replaced.

If writing the slot is interrupted, rerun the replacement command with the
same `--replace` ID. CPS records that ID on the ESP until it has verified
the new root. If CPS reports that the replacement ID is unknown, use the
original command without `--replace` to finish deployment into the now
unregistered slot. If the new environment is already registered, omit
`--root-image` and `--replace` to verify it and finish activation.

Activation copies boot files from the selected root and publishes its Boot
Loader Specification entry last. Entries record SHA-256 digests of the
kernel and initramfs in comments. CPS checks those bytes before using an
entry as an activation fallback or rollback target.
With manually supplied boot files the kernel path defaults to
`boot/vmlinuz`. Use `--initrd` if the kernel needs
one to mount the root and `--options` for additional kernel arguments.

With `--esp-partuuid` and `--output-disk`, CPS edits a private copy of the
selected EFI partition on the same verified disk. It checks the FAT
filesystem, verifies the written files, and publishes a new disk image
with the remaining partitions intact. Boot the output disk. This mode
uses mtools and dosfstools and supports ESPs up to 1 GiB. An unprivileged
caller can set `CPS_ESP_TOOLS_DIR` to the absolute path of a trusted tool
directory. Use `activate --esp` for a detached ESP directory and
`activate-device --esp` for a mounted ESP. [fsck.fat manual](https://man7.org/linux/man-pages/man8/fsck.fat.8.html)

For repeatable offline activation and rollback, set `SOURCE_DATE_EPOCH`.
CPS sets new and updated FAT entries to that time in UTC, rounds down to
two-second precision, and uses January 1, 1980 for earlier epochs. It
reads back the timestamps and contents of written files before publishing.
Use the same source disk bytes, root export, command options, and mtools
build to reproduce the output disk. [GNU mtools release history](https://github.com/vapier/mtools/blob/main/NEWS)

For FAT32, CPS can clear a leftover dirty flag in its private copy
when the primary and backup boot sectors otherwise agree. The copy still
has to pass the read-only FAT check.

`--known-good` bootstraps the first entry. Later activations require the previous environment's known-good entry and boot files. A trial gets two attempts by default. Install [the `system/cps` package](../../packaging/cps.kdl) in the base image to make its health check part of `boot-complete.target`. The package ships the CPS binary, [the health unit](../../packaging/systemd/cps-boot-health.service), and the required dependency link. The unit runs `cps verify --quick --root /` after `multi-user.target`. Systemd blesses the entry when boot completion succeeds, removing the boot counter. If the attempts run out, systemd-boot selects the previous environment. CPS owns the boot selection in the ESP's [loader configuration](boot-operations.md#the-loader-configuration) and assumes no EFI default override. [systemd boot assessment](https://systemd.io/AUTOMATIC_BOOT_ASSESSMENT/)

## Reusing a root slot

After the trial becomes known-good, stage the next release from that
environment. On a two-slot disk, use `boot-env list` to identify the older
entry and its partition UUID. Deploy and activate in one command.

```sh
cps plan --root staged --output next-upgrade.json update
cps boot-env stage --source staged --root next-staged next-upgrade.json
cps boot-env export --root next-staged --output next-staged.ext4 \
    --size-mib 2048 --receipt > next-staged.kdl
cps boot-env activate --root next-staged --root-image next-staged.ext4 \
    --disk bootable-next.raw --receipt next-staged.kdl \
    --root-partuuid "$base_partuuid" --replace "$base_environment_id" \
    --esp-partuuid "$esp_partuuid" --output-disk next-release.raw
```

CPS checks that the entry being replaced owns the slot and that the
selected known-good environment is the staged root's parent. An inactive
known-good entry or an exhausted trial can be replaced. CPS verifies the
payload against its receipt, retires the old entry and its boot files,
and publishes the new disk with the selected root as the trial's fallback.
The source disk retains its original roots and entries. Omit `--replace`
to use the same transaction for an unregistered slot.
