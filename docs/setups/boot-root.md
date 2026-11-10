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

The root layout depends on the deployment.

- For Btrfs, use a new, empty named root subvolume under the mounted
  top level. Keep the CPS image state and managed paths in that subvolume.
- For ext4, use an empty directory on a single filesystem. Export the
  assembled root to ext4 before writing the target partition.

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

- [Btrfs with systemd-boot or direct UEFI](btrfs.md#activating-btrfs-roots)
- [ext4 in a raw GPT disk image](ext4.md#prepare-and-deploy-an-offline-gpt-disk-image)
- [ext4 on an installed disk](ext4.md#provisioning-the-first-root-on-an-installed-disk)
