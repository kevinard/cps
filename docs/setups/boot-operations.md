# Inspect and roll back boot environments

[Documentation](../README.md) · [Setup guides](README.md)

These commands work the same way for every boot mode once an ESP holds CPS
boot entries. For prerequisites and installation, see the
[Btrfs setup guide](btrfs.md) and [offline disk images](disk-image.md). The
terms are defined in [Boot environments](../concepts.md#boot-environments).

## On this page

- [List environments](#list-environments)
- [Trials and blessing](#trials-and-blessing)
- [Roll back](#roll-back)
- [Clean up environments](#clean-up-environments)
- [Compare environments](#compare-environments)
- [The loader configuration](#the-loader-configuration)

## List environments

```sh
cps boot list
```

`boot list`, `boot rollback`, and `boot destroy` read the ESP and the Btrfs
volume from the `boot-btrfs` node in `/etc/cps/image.kdl`. Pass `--config`
for another configuration, or `--esp` and `--btrfs-volume` to name them
directly.

Each line shows one environment, newest first.

| Column | Meaning |
| --- | --- |
| `DEFAULT` | `*` marks the environment selected for the next boot. |
| `STATUS` | `known-good`, `trial (N left)` for a trial with N boot attempts left, `exhausted` for a failed trial, or `retiring` for an environment whose removal was interrupted. |
| `ID` | `cps-IMAGE-REVISION`, from the root's image identity and revision. |
| `ROOT` | The PARTUUID of the root's Btrfs partition. When any entry selects its root by Btrfs filesystem UUID, the column shows `UUID=` or `PARTUUID=` before each identifier. |
| `TITLE` | The entry title, which includes the Linux release for manifest-managed kernels. A [backup](btrfs.md#backups-before-live-changes) of a running Btrfs root is titled `Canopa backup`. |

CPS entries declare their metadata format with `# cps-boot-entry-version 1`
and record SHA-256 digests of their boot files. CPS checks those files before
it uses an entry as a fallback or rollback target.

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

`rollback` selects the fallback of the environment currently selected for the
next boot, or of the running direct UEFI trial. Pass a known-good environment
ID to select a specific root, or when the selection has no recorded fallback.

```sh
cps boot rollback
# A specific known-good environment.
cps boot rollback "$environment_id"
```

Before changing the selection, CPS checks the target's boot files against
the digests in its entry. It then checks the registered filesystem and
subvolume identity on the mounted top-level volume, and the root's CPS image
identity and revision. For an entry that boots by PARTUUID, CPS reads the
parent GPT disk of the mounted ESP and checks that the Btrfs device is that
partition, so it needs read access to the disk.

For a Btrfs [backup](btrfs.md#backups-before-live-changes), rollback first
clears the read-only flag if an interrupted backup left it set. It then writes the target as
`default` in `loader.conf` and removes any `preferred` trial selection. With direct UEFI boot, it instead cancels a
pending `BootNext`, marks that trial as exhausted, and moves the target to the
front of `BootOrder`. Rollback keeps every environment and its files.

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
```

The diff reports changes to package identity, manifest digest, local
requests, and signed profile roles. `--paths` adds changes to managed file
actions and owners. It reads committed metadata, so use `cps verify` to check
the files on disk. The Btrfs rollback preview compares the environment
selected for the next boot with the rollback target and leaves the selection
unchanged. It accepts an environment ID, `--paths`, and `--json` as well.

## The loader configuration

CPS creates `loader/loader.conf` when it registers the first systemd-boot
environment on an ESP. Direct UEFI boot selects environments with firmware
variables instead and uses no loader configuration. A new file starts with the
following lines.

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
add keys such as `console-mode`. CPS rejects the file and names the key when
you set one of these:

- A `default` that is neither `cps-*.conf` nor a CPS entry ID followed by
  `.conf`. Restore the CPS value, or delete the line and select a known-good
  environment with `cps boot rollback ID`.
- A `preferred` that is not a CPS entry, a `preferred` with
  `default cps-*.conf`, or a `preferred` without a `default`. Delete the line
  to cancel the trial selection.
- More than one `default` or `preferred` line. Keep the one CPS wrote.
- A single-quoted `default` or `preferred` value. systemd-boot 260 and later
  remove matching single or double quotes, but earlier versions read single
  quotes literally. Remove the quotes.
- `timeout menu-force`, or the numbers `4294967295` and `4294967294`, which
  systemd-boot treats as `menu-force` and as a menu with a 136-year
  countdown. Use a number of seconds, `menu-hidden`, or `menu-disabled`.

These rules keep trial selection safe. Right after bootstrap,
`default cps-*.conf` selects the newest CPS entry whose attempts are not
exhausted, so the first trial boots and falls back to the bootstrapped root.
After a rollback pins a known-good environment as `default`, CPS publishes
each later trial and its files before it names the trial as `preferred`.
systemd-boot 262 boots the `preferred` entry while its boot counter has
attempts left, and otherwise boots `default`.
[systemd-boot selection](https://github.com/systemd/systemd/blob/v262/src/boot/boot.c#L1717)

Earlier systemd-boot releases ignore `preferred` and would boot the pinned
default, so the trial would never run. Before it writes `preferred`, CPS
reads the release from the `#### LoaderInfo: systemd-boot …` marker in each
systemd-boot binary under `EFI/systemd` and `EFI/BOOT` on the ESP, and
refuses the activation if any of them is older than 262. When the ESP holds
no systemd-boot binary, CPS uses the running loader's `LoaderInfo` EFI
variable instead. Update the loader with `bootctl update` and retry.
[`image disk`](disk-image.md) applies the same check to the template
ESP, so a new disk can take trials after its first rollback.

The automatic choice therefore depends only on the two owned keys and the CPS
entry files. A second `default` line would take precedence over the first, so
CPS rejects duplicates. `timeout menu-force` disables the timeout, so the
firmware would wait at the menu and an unattended trial or fallback would
never boot. Any other timeout only delays the automatic choice. The other keys
change the menu, its extra entries, or the console. Automatically detected
entries have their own IDs, such as `auto-windows`, which never match a CPS
selector or `cps-*.conf`. CPS rejects non-CPS Type #1 entries in
`loader/entries`. CPS writes `editor no` because the editor lets anyone at the
console change the kernel command line of a Type #1 entry. Enabling it does
not change automatic selection.

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
