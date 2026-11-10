# Assemble a bootable base root

[Documentation](../README.md) · [Setup guides](README.md)

Start with a trusted repository or signed mirror that supplies a bootable
release profile for the target architecture. A CPS image becomes bootable
when its installed packages supply the filesystem layout, kernel and
initramfs, userspace, and boot health service for the chosen machine.
The one-file quickstart image is suitable for package testing.

## Assemble the package composition

Use an `image.kdl` that names the signed profile and its publishers, with
the profile's architecture and facets. Sync any mirrors before creating
the root. Keep the configuration, trust keys, and repository paths available
while assembling and verifying the image.
[Configure and sync the profile](../client/mirrors.md#synchronizing-a-signed-profile)

The root lives on Btrfs. Keep the CPS image state and managed paths in the
root itself, and mount shared data from other subvolumes or filesystems.

- On an installed system, use a new, empty named root subvolume under the
  mounted top level. CPS keeps the image state and managed paths in that
  subvolume. Create the `/var` subvolume beside it, as described in
  [Share /var between boot environments](btrfs.md#share-var-between-boot-environments),
  and add its line to the root's `/etc/fstab`. `image-create` makes `var`
  the empty mount point, or accepts the new, empty `/var` subvolume already
  mounted there. The packages' tmpfiles.d declarations populate it on the
  first boot.
- For an [offline disk image](disk-image.md), use an empty directory beside
  an empty environments directory, inside a volume directory that stands for
  the Btrfs top level.

Run the package operations with privileges to write the ownership, modes,
and capabilities declared in the base manifests. Set `root` to the absolute
path of the target directory or subvolume.

```sh
cps image-create --root "$root" --config image.kdl
cps plan --root "$root" --output base.json install-profile
cps fetch --root "$root" --plan base.json
cps apply --root "$root" base.json
cps verify --root "$root" --pristine
```

If you assemble the base directly from a local repository, use
`plan install PACKAGE...` with the set of packages that supplies your
bootable system. For a managed kernel, include the package that provides
the kernel actions, and a package with an
[initramfs trigger](../maintainer/triggers.md#generate-an-initramfs) if the
kernel package ships no initrd. CPS runs the host's `kernel-install` while
applying the plan and verifies the result.
[Kernel package declarations](../maintainer/systemd.md#kernel-packages)

## Configure and register the root

Before registering the root, configure its machine-specific mounts and
kernel command line. For live Btrfs system commands, provision
`/etc/cps/image.kdl` in the installed root with publisher, mirror, and
[`boot-btrfs`](btrfs.md#configure-live-system-commands) settings that resolve
on that system. Keep repository caches accessible
at stable paths across environments. The [Btrfs guide](btrfs.md#activating-btrfs-roots)
covers the root and ESP mounts and the health service.

Register the first known-good root using the guide for your layout.

- [A mounted Btrfs volume with systemd-boot or direct UEFI](btrfs.md#activating-btrfs-roots)
- [An offline Btrfs disk image](disk-image.md)
