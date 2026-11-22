# Inspect and roll back boot environments

[Documentation](../README.md) · [Setup guides](README.md)

These commands work the same way for every boot mode once an ESP holds CPS
boot entries. For prerequisites and installation, see the
[Btrfs setup guide](btrfs.md) and [offline disk images](disk-image.md). The
terms are defined in [Boot environments](../concepts.md#boot-environments).

## On this page

- [List environments](#list-environments)
- [Manage named environments](#manage-named-environments)
- [Trials and blessing](#trials-and-blessing)
- [Roll back](#roll-back)
- [Clean up environments](#clean-up-environments)
- [Compare environments](#compare-environments)
- [The loader configuration](#the-loader-configuration)

## List environments

```sh
cps boot list
```

Boot management commands use the `boot-btrfs` node in `/etc/cps/image.kdl`
by default. Pass `--config` for another configuration. Commands that work
offline also accept `--esp` and, where needed, `--btrfs-volume`.

Each line shows one environment, newest first.

| Column | Meaning |
| --- | --- |
| `DEFAULT` | `*` marks the environment selected for the next boot. |
| `STATUS` | `known-good`, `trial (N left)` for a trial with N boot attempts left, `exhausted` for a failed trial, or `retiring` for an environment whose removal was interrupted. |
| `ID` | `cps-IMAGE-REVISION`, from the root's image identity and revision. |
| `ROOT` | The PARTUUID of the root's Btrfs partition. When any entry selects its root by Btrfs filesystem UUID, the column shows `UUID=` or `PARTUUID=` before each identifier. |
| `TITLE` | The entry title, which includes the Linux release for manifest-managed kernels. A [backup](btrfs.md#backups-before-live-changes) of a running Btrfs root is titled `Canopa backup`. |
| `NAME` | The user-assigned name, or `-` for an unnamed environment. |

CPS entries declare their metadata format with `# cps-boot-entry-version 1`
and record SHA-256 digests of their boot files. CPS checks those files before
it uses an entry as a fallback or rollback target.

## Manage named environments

```sh
sudo cps boot create before-upgrade
sudo cps boot rename before-upgrade baseline
sudo mkdir /mnt/baseline
sudo cps boot mount baseline /mnt/baseline
sudo cps boot unmount baseline /mnt/baseline
sudo cps boot activate baseline
```

`create` snapshots the running, registered known-good root and publishes it
without changing which environment boots next. It requires that the running
root is already selected for the next boot and no trial is pending. The name
must be 1–48 lowercase ASCII letters, digits, hyphens, or underscores, start
with a letter or digit, and not start with `cps-`. Like a
[backup](btrfs.md#backups-before-live-changes), `create` prunes first with
`auto-prune` to make room on the ESP, and keeps the snapshot read-only until
it is registered. A retry after an interruption reuses the snapshot only while
the running root is at the same revision. `rename` can also assign a
first name to a known-good environment by ID. It changes neither the boot ID
nor the directory or loader selection. You can continue to use the ID in
place of the name.

`mount` exposes a read-only view of the registered root, without mounting
shared filesystems such as `/var`. The target must be an existing empty
directory; it and its parent must be owned and writable only by root.
`unmount` takes the same environment and directory and refuses a busy or
nonmatching mount. It matches the mount against the entry's Btrfs subvolume
only, so it works even when the root fails the checks that `mount` runs.
Both commands need Linux 5.12 or newer.

`activate NAME` verifies and selects an existing known-good environment for
the next boot; it does not reboot the system. `activate --dry-run NAME`
previews the change, as described in [Roll back](#roll-back). To publish a
root prepared by `boot stage`, continue to use `activate --root` with the
explicit boot paths.

Named environments are retained outside the pruning count. To remove one,
use `sudo cps boot destroy --dry-run NAME` first, then `sudo cps boot destroy
NAME` when the environment is no longer selected, running, mounted or needed
as a fallback.

## Trials and blessing

A staged environment boots first as a **trial**. Its fallback is the
known-good environment it was staged from.

- With systemd-boot, the trial is a counted entry such as
  `cps-IMAGE-REVISION+2.conf`. It gets two attempts unless `--tries` or the
  `tries` key in `boot-btrfs` sets another number, from 1 to 99. systemd-boot
  decrements the counter on each attempt, and boots the fallback once the
  attempts are used up.
- With [direct UEFI boot](btrfs.md#direct-uefi-boot-for-btrfs-roots),
  CPS sets the firmware's `BootNext` to the trial for exactly one
  attempt. The firmware consumes `BootNext`, so any reset after that boots
  the fallback.

The trial is blessed only when the boot reaches `boot-complete.target`, which
requires `cps-boot-health.service`. That unit runs
`cps verify --quick --root /` after `multi-user.target`. With systemd-boot,
systemd then removes the boot counter from the entry's file name. With direct
UEFI boot, `cps-uefi-bless.service` runs `cps boot bless`, which
verifies the trial and its fallback and makes the trial the firmware default.
Install the health unit with [the `system/cps` package](../../packaging/cps.kdl),
and extend it with readiness checks as described in
[Health checks and shared state](btrfs.md#health-checks-and-shared-state).
[systemd boot assessment](https://systemd.io/AUTOMATIC_BOOT_ASSESSMENT/)

On a running system, while a trial is selected for the next boot,
system package commands such as `cps install` refuse to change the running
root. Boot the trial, or cancel it with `cps boot discard-trial`. When
the running system shuts down cleanly, `cps-finalize-trial.service`
[merges the `/etc` changes](btrfs.md#merging-etc-at-shutdown) made since
staging into the trial.

## Roll back

`activate` selects a known-good environment, by name or ID, for the next
boot. `rollback` is a shortcut that activates the fallback of the environment
currently selected for the next boot, or of the running direct UEFI trial.
When the selection has no recorded fallback, activate an environment by name
or ID instead.

```sh
cps boot rollback
# A specific known-good environment.
cps boot activate "$environment"
```

Before changing the selection, CPS checks the target's boot files against
the digests in its entry. It then checks the registered filesystem and
subvolume identity on the mounted top-level volume, and the root's CPS image
identity and revision, and verifies the root's packages. Files whose
fingerprints match the ones CPS recorded are trusted without hashing them
again, so a rollback stays fast on a large root; `cps verify` hashes every
file. For an entry that boots by PARTUUID, CPS reads the
parent GPT disk of the mounted ESP and checks that the Btrfs device is that
partition, so it needs read access to the disk.

For a Btrfs [backup](btrfs.md#backups-before-live-changes), activation first
clears the read-only flag if an interrupted backup left it set. It then writes the target as
`default` in `loader.conf` and removes any `preferred` trial selection. With direct UEFI boot, it instead cancels a
pending `BootNext`, marks that trial as exhausted, and moves the target to the
front of `BootOrder`. Activation keeps every environment and its files.

## Clean up environments

Each environment keeps its root and its boot files on the ESP until it is
removed. Before activation copies boot files, CPS checks that they fit on the
ESP with a 1 MiB margin, and fails before any change with a pointer to
pruning. `cps boot prune` applies the retention policy, system package
commands prune automatically before they stage a change, and
`cps boot discard-trial` cancels a trial and removes failed ones. See
[Retention and pruning](btrfs.md#retention-and-pruning).

## Compare environments

Compare the committed package state of two roots offline, including roots
created independently.

```sh
cps image diff base staged
cps image diff base staged --paths --json
cps boot rollback --dry-run
cps boot activate --dry-run "$environment"
```

The diff reports changes to package identity, manifest digest, local
requests, and signed profile roles. `--paths` adds changes to managed file
actions and owners. It reads committed metadata, so use `cps verify` to check
the files on disk. The Btrfs preview compares the environment
selected for the next boot with the target and leaves the selection
unchanged. Both `rollback` and `activate` accept `--paths` and `--json` with
`--dry-run`.

## The loader configuration

CPS creates `loader/loader.conf` when it registers the first systemd-boot
environment on an ESP. Direct UEFI boot selects environments with firmware
variables instead and uses no loader configuration. A new file starts with the
following lines.

```
default cps-IMAGE-REVISION.conf
timeout 0
editor no
auto-entries no
auto-firmware no
```

CPS owns only the `default` and `preferred` keys. It changes them in place
and keeps every other line, including comments, in its original order. You
can change the menu timeout, enable the editor or the automatic entries, or
add keys such as `console-mode`. CPS rejects the file and names the key when
you set one of these:

- A `default` that is not a CPS entry ID followed by `.conf`, including a
  pattern such as `cps-*.conf`. Restore the CPS value, or delete the line and
  select a known-good environment with `cps boot activate ID`.
- A `preferred` that is not a CPS entry, or a `preferred` without a
  `default`. Delete the line to cancel the trial selection.
- More than one `default` or `preferred` line. Keep the one CPS wrote.
- A single-quoted `default` or `preferred` value. systemd-boot 260 and later
  remove matching single or double quotes, but earlier versions read single
  quotes literally. Remove the quotes.
- `timeout menu-force`, or the numbers `4294967295` and `4294967294`, which
  systemd-boot treats as `menu-force` and as a menu with a 136-year
  countdown. Use a number of seconds, `menu-hidden`, or `menu-disabled`.

These rules keep trial selection safe. `default` always names a known-good
entry: the first environment when CPS bootstraps the ESP, and the target of
each rollback or activation after that. CPS publishes each trial and its
files before it names the trial as `preferred`. systemd-boot 262 boots the
`preferred` entry while its boot counter has attempts left, and otherwise
boots `default`. The selection never depends on how systemd-boot sorts
entries.
[systemd-boot selection](https://github.com/systemd/systemd/blob/v262/src/boot/boot.c#L1717)

Earlier systemd-boot releases ignore `preferred` and would boot the
default, so a trial would never run. CPS therefore requires systemd-boot 262
or newer. Before every systemd-boot activation, CPS reads the release from
the `#### LoaderInfo: systemd-boot …` marker in each systemd-boot binary
under `EFI/systemd` and `EFI/BOOT` on the ESP, and refuses the activation if
any of them is older than 262. When the ESP holds
no systemd-boot binary, CPS uses the running loader's `LoaderInfo` EFI
variable instead. Update the loader with `bootctl update` and retry.
[`image disk`](disk-image.md) applies the same check to the template
ESP.

The choice therefore depends only on the two owned keys and the CPS entry
files. A second `default` line would take precedence over the first, so
CPS rejects duplicates. `timeout menu-force` disables the timeout, so the
firmware would wait at the menu and an unattended trial or fallback would
never boot. Any other timeout only delays the choice. The other keys
change the menu, its extra entries, or the console. Automatically detected
entries have their own IDs, such as `auto-windows`, which never match a CPS
selector. CPS rejects non-CPS Type #1 entries in
`loader/entries`. CPS writes `editor no` because the editor lets anyone at the
console change the kernel command line of a Type #1 entry. Enabling it does
not change the selection.

### Firmware variable overrides

systemd-boot also reads selection and timeout settings from EFI variables,
and these take precedence over `loader.conf`.
[systemd-boot variables](https://www.freedesktop.org/software/systemd/man/latest/systemd-boot.html#EFI%20Variables)
When systemd-boot booted the host from the ESP that CPS manages, as
`LoaderDevicePartUUID` reports, CPS reads these variables from efivarfs each
time it reads the boot selection. It rejects the ESP, names the variable,
and gives the `bootctl` command that clears it in these cases:

- `LoaderEntryPreferred`, set with `bootctl set-preferred`, names anything
  other than the CPS `preferred` entry. systemd-boot tries it before both
  `loader.conf` keys.
- `LoaderEntryDefault`, set with `bootctl set-default` or with the `d` key
  in the menu, names anything other than the CPS `default`. It replaces the
  known-good fallback once a trial's attempts are exhausted.
- `LoaderConfigTimeout`, set with `bootctl set-timeout`, holds a timeout
  that `loader.conf` cannot have, such as `menu-force`.
- `LoaderConfigTimeoutOneShot`, set with `bootctl set-timeout-oneshot` or
  `systemctl reboot --boot-loader-menu`, holds anything but a positive number
  of seconds. A one-shot timeout always shows the menu, so `0`,
  `menu-hidden`, and `menu-disabled` wait at it as `menu-force` does.

Empty variables, and variables that repeat the CPS selection, are accepted.
Without efivarfs, without systemd-boot, or for an ESP that did not boot the
host, CPS skips the check. `LoaderEntryOneShot`,
set with `bootctl set-oneshot` or `systemctl reboot --boot-loader-entry`,
selects an entry for the next boot only and is left to the administrator.
The SMBIOS OEM string `io.systemd.boot.timeout=`, which a virtual machine
manager can set, also overrides `timeout` and is not checked.
