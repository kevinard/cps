# Setup guides

[Documentation](../README.md) · [Concepts](../concepts.md)

Choose how to assemble or run the image, then choose how to supply its
packages. An inactive directory is enough for package testing. A bootable
Canopa system keeps its root on Btrfs and boots with UEFI, systemd-boot, and
a VFAT ESP. Other filesystems can hold separately mounted data. Start by
[assembling a bootable base root](boot-root.md) if you are provisioning one.

| Image setup | Use it when | What you need |
| --- | --- | --- |
| [Local repository and directory image](../quickstart.md#try-cps-in-a-disposable-directory) | You want to try CPS or test a package without changing the running system | A Linux host, CPS, and an Ed25519 test key |
| [Installed Btrfs with systemd-boot](btrfs.md#activating-btrfs-roots) | You want live changes, staged trial boots from snapshots, and automatic pruning of old environments | An existing CPS-managed Btrfs root, writable top-level mount, and a dedicated ESP |
| [Btrfs with signed UKIs and systemd-boot](btrfs.md#signed-ukis-for-btrfs-roots) | You want systemd-boot to select a signed unified kernel image | A Btrfs boot setup, Secure Boot signing material, and the UKI and signing tools |
| [Btrfs with direct UEFI UKIs](btrfs.md#direct-uefi-boot-for-btrfs-roots) | You want firmware to select each signed UKI directly | A Btrfs boot setup, signed UKIs, writable EFI variables, and the CPS blessing service |
| [Offline Btrfs disk or VM image](disk-image.md) | You want an installer or VM test to write a bootable disk file without root | An inactive root in a volume directory, btrfs-progs, and a GPT template with a systemd-boot ESP |

Every bootable setup stages changes as [boot environments](../concepts.md#boot-environments)
that boot first as trials. See [boot operations](boot-operations.md) for
listing, trials, rollback, and cleanup, and [VM tests](../developer/boot-tests.md)
for reproducible boot and fallback scenarios.

## Supply packages and build inputs

| Setup | Guide |
| --- | --- |
| One or more local repositories with separately trusted publisher trust roots | [Configure publishers](../client/publishers.md) |
| One signed mirror with a full or index-only cache | [Sync a mirror](../client/mirrors.md#sync-a-single-publisher) |
| Signed profile across primary and companion publishers | [Sync a profile](../client/mirrors.md#synchronizing-a-signed-profile) and [install it](../client/profiles.md) |
| Disconnected provisioning of an exact composition | [Export and install an image bundle](../client/profiles.md#provisioning-the-same-composition-on-another-machine) |
| Publisher-operated HTTPS mirror | [Provision signing roles and serve a mirror](../publisher/mirrors.md#serving-a-basic-https-mirror) |
| Isolated build worker using one or more pinned toolchains | [Set up sources and build inputs](../maintainer/builds.md) and [configure worker limits](../maintainer/builds.md#worker-limits) |
| Local Git builds on an installed Btrfs system | [Configure the Git toolchain](../maintainer/git.md) |
