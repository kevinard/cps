# Build an offline Btrfs disk image

[Documentation](../README.md) · [Setup guides](README.md)

Installers and VM tests turn an inactive CPS root into a bootable GPT disk
file with `cps boot-env image`, without root privileges, mounts, or loop
devices. The disk gets the same layout as an installed
[Btrfs system](btrfs.md): a Btrfs top-level volume holding the root
subvolume and an `environments` subvolume, and a systemd-boot ESP with a
known-good entry for the root. After the first boot, the
[system commands](../client/system.md) stage, activate, and roll back
environments on it as on any installed Btrfs system.

## On this page

- [Assemble the volume directory](#assemble-the-volume-directory)
- [Prepare the disk template](#prepare-the-disk-template)
- [Write the image](#write-the-image)
- [Checks and failure cleanup](#checks-and-failure-cleanup)
- [Reproducible images](#reproducible-images)
- [Boot the image](#boot-the-image)

## Assemble the volume directory

Create a directory that stands for the Btrfs top level. It must contain only
the root directory and an empty environments directory, which become
subvolumes with those names. Assemble the root inside it as described in
[Assemble a bootable base root](boot-root.md).

```sh
mkdir -p volume/@environments
cps image-create --root volume/@ --config image.kdl
cps plan --root volume/@ --output base.json install-profile
cps fetch --root volume/@ --plan base.json
cps apply --root volume/@ base.json
cps verify --root volume/@ --pristine
```

Configure the root before writing the image. Its `/etc/cps/image.kdl` needs
a [`boot-btrfs`](btrfs.md#configure-live-system-commands) node for the
installed system, for example with `volume="/cps"`,
`environments="/cps/@environments"`, `esp="/efi"`, and the root partition's
`root-partuuid`. Its `/etc/fstab` or mount units mount the top level and the
ESP at those paths. Leave `subvol=` and `subvolid=` off any `/` line, because
each boot entry selects its subvolume.

The image keeps owners, modes, links, extended attributes, and timestamps
from the directory. Run as root to give the files their declared owners.
An unprivileged caller can only build roots whose files it owns, as the VM
tests do. The environments directory must be owned by root and writable only
by root on the installed system.

## Prepare the disk template

`boot-env image` writes into a copy of a GPT disk template. The template has
512-byte sectors, one EFI System Partition with systemd-boot 262 or newer,
and a Linux filesystem partition for the root. CPS formats that partition to
its full size, so size it for the root and the environments staged later.

```sh
truncate -s 8G template.raw
sfdisk template.raw <<PARTITIONS
label: gpt
start=1MiB, size=512MiB, type=uefi
start=513MiB, type=linux, uuid=$root_partuuid
PARTITIONS
truncate -s 512M esp.fat
mkfs.fat -F 32 esp.fat
mmd -i esp.fat ::/EFI ::/EFI/BOOT
mcopy -i esp.fat /usr/lib/systemd/boot/efi/systemd-bootx64.efi ::/EFI/BOOT/BOOTX64.EFI
dd if=esp.fat of=template.raw bs=1M seek=1 conv=notrunc
```

The ESP may already hold other files, such as firmware updates. It must not
hold CPS boot entries or CPS boot files.

## Write the image

```sh
cps boot-env image --btrfs-volume volume --root volume/@ \
    --environments volume/@environments --disk template.raw \
    --root-partuuid "$root_partuuid" --output disk.raw --options rw
```

The command prints the entry ID, `cps-IMAGE-REVISION`, and publishes the new
disk file. It leaves the template and the volume directory unchanged.

- The Btrfs filesystem UUID is the image identity. The root and environments
  are subvolumes of the top level, and the top level stays the default
  subvolume.
- The root carries the `user.cps.boot-environment` registration that
  [activation](btrfs.md#activating-btrfs-roots) writes on a mounted system.
  CPS sets it on the source directory while `mkfs.btrfs` copies the tree,
  then removes it.
- The ESP gets the root's kernel and initramfs under `EFI/cps/ID`, an entry
  that boots the root by `root=PARTUUID=` and its subvolume ID, and a
  `loader/loader.conf` with `default cps-*.conf`. The entry records the
  filesystem UUID, subvolume UUID and ID, and path, as an activated entry
  does.

A manifest-managed kernel supplies its prepared kernel-install artifacts.
For an image that ships boot files manually, pass `--kernel` and `--initrd`,
as for [`activate-btrfs`](btrfs.md#activating-btrfs-roots). `--options` adds
kernel arguments. CPS supplies `root=`, `rootfstype=`, and `rootflags=`.

The root is booted by its PARTUUID, so the kernel needs no initramfs to find
it. To boot by filesystem UUID instead, or with an encrypted root, register
the root on the installed system with `activate-btrfs`.

## Checks and failure cleanup

Before formatting, CPS checks the root like an activation: it requires an
inactive, clean, verified image that is not a staged trial, with its prepared
kernel artifacts. It rejects special files and nested mounts, and holds the
image lock while it works. CPS fingerprints the tree before formatting and
again after, and rejects a root that changed in between.

CPS reads the template's GPT with `sfdisk` and checks that the primary and
backup tables agree. It edits a private copy of the ESP with mtools, checks
it with `fsck.fat`, and reads back every file it writes. It checks the new
filesystem with `btrfs check --readonly`, then reads back the filesystem
UUID, the subvolume IDs and UUIDs, and the registration with
`btrfs inspect-internal`. The written partition must match the checked
filesystem's SHA-256. CPS writes the disk under a private `.cps-disk-*`
directory beside the output and publishes it with a rename that never
replaces an existing file. A failure removes the private copy and the
temporary registration.

The tools come from the system path: `mkfs.btrfs` and `btrfs` from
btrfs-progs with `--rootdir` and `--subvol` support, `sfdisk`, GNU `cp`,
dosfstools, and mtools. An unprivileged caller can set `CPS_DISK_TOOLS_DIR`
to the absolute path of a trusted directory with btrfs-progs, dosfstools,
and mtools.

## Reproducible images

Set `SOURCE_DATE_EPOCH` to repeat an image from the same root, template, and
tools. CPS passes the epoch and `DETERMINISTIC_SEED=1` to `mkfs.btrfs`, which
then uses the epoch for filesystem timestamps and derives its internal UUIDs
from the filesystem UUID. New and updated FAT entries get the epoch in UTC,
rounded down to two seconds, or January 1, 1980 for earlier epochs.

btrfs-progs 7.3 still gives the subvolumes it creates random UUIDs and the
current time as their creation time. Two images of the same root differ only
in those root items, in the filesystem's UUID tree, in the `# cps-btrfs-uuid`
line of the entry, and in the metadata checksums that cover them. File data,
inodes, and every other tree repeat.
[mkfs.btrfs manual](https://btrfs.readthedocs.io/en/latest/mkfs.btrfs.html)

## Boot the image

Write the disk to its target or boot it in a VM with UEFI firmware. The
first boot runs the known-good root. From there, use the
[system commands](../client/system.md#stage-changes-for-the-next-boot) and
[boot operations](boot-operations.md) as on any installed Btrfs system. The
[VM tests](../developer/boot-tests.md) build their disks this way.
