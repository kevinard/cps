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

## The loader configuration

CPS creates `loader/loader.conf` when it registers the first environment on
an ESP. A new file starts with the following lines.

```
default cps-*.conf
timeout 0
editor no
auto-entries no
auto-firmware no
```

CPS owns only the `default` and `preferred` keys. It changes them in place
and keeps every other line, including comments, in its original order. You
can change the menu timeout, enable the editor or the automatic entries, or
add keys such as `console-mode`. CPS rejects the file and names the key
when you set one of these:

- A `default` that is neither `cps-*.conf` nor a CPS entry ID followed by
  `.conf`. Restore the CPS value, or delete the line and select a known-good
  environment with `cps boot-env rollback --esp esp ID`.
- A `preferred` that is not a CPS entry, a `preferred` with
  `default cps-*.conf`, or a `preferred` without a `default`. Delete the line
  to cancel the trial selection.
- More than one `default` or `preferred` line. Keep the one CPS wrote.
- `timeout menu-force`. Use a number of seconds, `menu-hidden`, or
  `menu-disabled`.

These rules keep trial selection safe. systemd-boot 262 boots the
`preferred` entry while its boot counter has attempts left, and otherwise
boots `default`. CPS writes a known-good environment as `default` and
publishes a trial and its files before it names the trial as `preferred`.
Right after bootstrap, `default cps-*.conf` selects the newest CPS entry
whose attempts are not exhausted, so a first trial falls back the same way.
The boot counter is part of the entry's file name. systemd-boot decrements it
on each attempt, and systemd removes it when the health checks pass. So the
automatic choice depends only on the two owned keys and the CPS entry files.
A second `default` line would take precedence over the first, so CPS
rejects duplicates. `timeout menu-force` disables the timeout. The firmware
would then wait at the menu, so an unattended trial or fallback never
boots. Any other timeout only delays the automatic choice. The other keys
change the menu, its extra entries, or the console. Automatically detected
entries have their own IDs, such as `auto-windows`. These never match a CPS
selector or `cps-*.conf`, and CPS rejects non-CPS Type #1 entries in
`loader/entries`. CPS writes `editor no` because the editor lets anyone at
the console change the kernel command line of a Type #1 entry. Enabling it
does not change automatic selection.

The EFI variables `LoaderEntryDefault` and `LoaderEntryOneShot`, set with
`bootctl set-default` and `bootctl set-oneshot`, override `default` from
outside the file. Likewise, `LoaderConfigTimeout`, set with
`bootctl set-timeout`, overrides `timeout`. CPS assumes that none of them is
set on a managed system.

## Cleaning up environments

Old environments, their roots, and their boot files stay until they are
removed. On Btrfs systems, `cps boot-env prune` and the automatic
retention policy remove them, and `cps boot-env discard-trial` cancels a
pending trial. See [Retention and pruning](btrfs.md#retention-and-pruning).
Activation checks the ESP's free space before it copies boot files, and
fails with a message that suggests pruning.

## Comparing environments

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
