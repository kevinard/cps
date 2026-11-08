# Inspect and roll back boot environments

[Documentation](../README.md) · [Setup guides](README.md)

Use the command form for your boot mode. For prerequisites and
installation, see the [Btrfs](btrfs.md) and [ext4](ext4.md) setup guides.

## Listing and rolling back

```sh
cps boot-env list --esp esp
cps boot-env rollback --esp esp
# Or select a specific known-good ID from the list.
cps boot-env rollback --esp esp "$environment_id"
```

For an offline disk, list the entries that systemd-boot actually sees and
publish rollback to a new disk file.

```sh
cps boot-env list --disk bootable-next.raw --esp-partuuid "$esp_partuuid"
cps boot-env rollback --disk bootable-next.raw --esp-partuuid "$esp_partuuid" \
    --output-disk rolled-back.raw
```

Rollback on a disk image or a mounted VFAT ESP checks that the selected
root's ext4 UUID and revision match its registered entry before changing
the default. For a mounted ESP, CPS reads the parent GPT disk and locates the
root by PARTUUID on that disk. The command requires read access to the disk
and write access to the ESP. For a detached ESP directory, the operator
supplies the partition mapping.

The list marks the default configured on the ESP and shows each registered root's partition or Btrfs filesystem UUID, title, and boot status. The title includes the Linux release for manifest-managed kernels. CPS entries declare their metadata format with `# cps-boot-entry-version 1`. Rollback selects the default environment's recorded parent. Supply a known-good environment ID to select a specific root or when the default has no recorded parent. Rollback checks the selected entry's boot files and updates `loader.conf` for the next boot. It keeps both environments available. Trials started from a pinned environment use the `preferred` setting in systemd-boot 262, with the pinned base as the fallback. [systemd-boot selection](https://github.com/systemd/systemd/blob/v262/src/boot/boot.c#L1717)

Compare two committed image states offline, including independently created roots.

```sh
cps image-diff base staged
cps image-diff base staged --paths --json
cps boot-env rollback --esp /efi --btrfs-volume /cps --dry-run
```

The diff reports changes to package identity, manifest digest, local requests,
and signed profile roles. `--paths` adds changes to managed file actions and
owners. It reads committed metadata. Use `cps verify` to check the files on
disk. The Btrfs rollback preview compares the configured default with the
selected known-good root and leaves the boot selection in place.
