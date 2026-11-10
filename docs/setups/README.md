# Setup guides

[Documentation](../README.md) · [Concepts](../concepts.md)

Choose how to assemble or run the image, then choose how to supply its
packages. An inactive directory is enough for package testing. A bootable
system also needs a kernel, a root layout, and a boot mechanism. Start by
[assembling a bootable base root](boot-root.md) if you are provisioning one.

| Image setup | Use it when | What you need |
| --- | --- | --- |
| [Local repository and directory image](../quickstart.md#try-cps-in-a-disposable-directory) | You want to try CPS or test a package without changing the running system | A Linux host, CPS, and an Ed25519 test key |
| [Installed Btrfs with systemd-boot](btrfs.md#activating-btrfs-roots) | You want live changes, staged trial boots from snapshots, and automatic pruning of old environments | An existing CPS-managed Btrfs root, writable top-level mount, and a dedicated ESP |
| [Btrfs with signed UKIs and systemd-boot](btrfs.md#signed-ukis-for-btrfs-roots) | You want systemd-boot to select a signed unified kernel image | A Btrfs boot setup, Secure Boot signing material, and the UKI and signing tools |
| [Btrfs with direct UEFI UKIs](btrfs.md#direct-uefi-boot-for-btrfs-roots) | You want firmware to select each signed UKI directly | A Btrfs boot setup, signed UKIs, writable EFI variables, and the CPS blessing service |
| [Offline GPT disk or VM image](ext4.md#prepare-and-deploy-an-offline-gpt-disk-image) | You want to assemble and test a bootable disk file | An inactive root, a raw GPT template with separate ext4 root slots, and a systemd-boot ESP |
| [Installed ext4 disk with dedicated root slots](ext4.md#provisioning-the-first-root-on-an-installed-disk) | You want to provision and upgrade by writing inactive partitions | A dedicated GPT disk with a mounted systemd-boot ESP, unmounted target partition, and scratch storage |

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
