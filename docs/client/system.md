# Manage a running Btrfs system

[Documentation](../README.md) · [Clients](README.md)

Use these commands on an installed CPS-managed Btrfs system. For the
installer-side configuration, see [Set up Btrfs boot environments](../setups/btrfs.md).
Every change runs the same journaled transaction as an inactive image,
described in [How a transaction runs](images.md#how-a-transaction-runs).
This page covers what differs on the running root.

## On this page

- [Search, install, and upgrade](#search-install-and-upgrade)
- [Check updates and diagnose held packages](#check-updates-and-diagnose-held-packages)
- [Remove packages and select publishers](#remove-packages-and-select-publishers)
- [Freeze packages and change settings](#freeze-packages-and-change-settings)
- [How live changes apply](#how-live-changes-apply)
- [Stage changes for the next boot](#stage-changes-for-the-next-boot)
  - [Local changes before the reboot](#local-changes-before-the-reboot)
- [Recover an interrupted change](#recover-an-interrupted-change)
- [Follow a saved plan](#follow-a-saved-plan)
- [Repair, restore defaults, and verify](#repair-restore-defaults-and-verify)

## Search, install, and upgrade

On a distribution configured for CPS system commands, search by a name
fragment and use the package name from the result.

```sh
sudo cps search htop
sudo cps install apps/htop
htop
sudo cps upgrade --check --json
sudo cps upgrade
sudo cps status
```

`install` and `upgrade` select dependencies, preview the package changes,
service actions, triggers, and activation mode, and ask once before
downloading and applying them. Use `--dry-run` to review the proposed
transaction without applying it and `--yes` to confirm it in a script.
Ordinary changes apply to the running system. Restart any open applications
to use updated files.

With configured mirrors, `upgrade` refreshes the catalogs before planning
and `install` reuses recently synchronized catalogs. Pass `--no-refresh` to
`install` or `upgrade` to use the current cached catalogs. Fetching
uncached package files can still contact the mirror.

By default, a mirror that cannot be refreshed stops the upgrade, so the
system never keeps using an outdated catalog without notice. An optional
publisher whose [refresh policy](publishers.md#refresh-policy-for-mirror-publishers)
allows it is deferred instead. CPS then continues with the distribution
repositories and keeps installed packages from the deferred publisher at
their current versions. If an optional publisher fails during payload
download, CPS previews a new distribution-only plan and asks for
confirmation again unless you passed `--yes`. To use distribution
publishers from the start, pass `--base-only` to `upgrade` or
`upgrade --check`. This scope includes the primary publisher, signed profile
companions, and publishers declared in the system image configuration.
Package dependencies still apply, so an incompatible installed add-on can
hold back a base upgrade.

To build and install a package straight from a Git repository, see
[Install and track a branch](../maintainer/git.md#install-and-track-a-branch).

## Check updates and diagnose held packages

`upgrade --check --json` runs the upgrade planner on the running system.
It refreshes configured mirrors as `upgrade` does. Use `--no-refresh` to
check the cached catalogs. It only plans the transaction, so the installed
system and boot selection stay as they are while the mirror cache can advance.

The JSON `status` is `up_to_date`, `updates_available`, `deferred`, or
`blocked`. When a signed rollout defers an upgrade, `phased_update`
identifies the profile, target catalog, rollout percentage, and image cohort.
`changes` lists the planned package changes, including dependency installs
and removals. `held_back` lists packages for which the current catalogs and
profile offer a newer version than the plan selects. Each entry gives the
installed, newest available, and selected versions and the freeze reason
when applicable. The selected version is `null` if planning fails.
`blockers` contains errors that prevented a plan, such as a
pending trial boot or an unsatisfiable release profile. When publishers were
deferred, the report names them in `deferred_publishers`. `publisher_freshness`
reports the [refresh age and policy](publishers.md#refresh-policy-for-mirror-publishers)
of each mirror publisher. A check with no actionable changes and only
deferred publishers reports `deferred`. A check can report both actionable
changes and held-back packages.

Use `upgrade --check --explain library/libdisplay` to probe the newest
held-back version of one package with the same catalog pins. The report
includes the resulting solver or build-trust error, or indicates that the
version fits when explicitly required. The probe leaves the normal upgrade
plan as selected.

`reboot_required` is a boolean for the actionable plan and `reboot_reason`
explains a required reboot. Both fields are `null` when the upgrade is
blocked. The command exits with status 0 for a successful check with or
without updates, and status 1 if blocked. Refresh progress goes to stderr.

## Remove packages and select publishers

```sh
sudo cps remove --dry-run apps/htop
sudo cps remove apps/htop
sudo cps autoremove
```

`remove` checks that the remaining packages still have their required
dependencies, including the providers of generated shared-library
capabilities. `autoremove` removes dependencies that are no longer needed
by explicitly installed packages or the signed release profile. Both use
the same preview, confirmation, and reboot staging as `install`, and work
offline from the installed package state.

When a new package is available from several publishers, an interactive
`install` shows a choice with the newest matching version from each.
Press Enter for the first choice or enter a number. Use
`cps://PUBLISHER/NAME` to select a publisher explicitly, and
`NAME@VERSION` with the version spelled exactly as in the package ID to
select a version; see [package selectors](queries.md#package-selectors-and-versions).
`--yes` and `--dry-run` use the configured search order. Installed packages
keep their original publisher.

## Freeze packages and change settings

```sh
sudo cps freeze --reason "Wait for the driver update" library/libdisplay
cps list --frozen
sudo cps unfreeze library/libdisplay
```

`freeze`, `unfreeze`, and `mark` only change recorded package state, so
they apply at once and need no backup environment. `facet set`,
`variant set`, `mediator set`, and `config resolve` can change files, and
apply like `install`: live, or staged when a written path needs a reboot.
`--stage` stages any of them. See [Configure an image](configuration.md)
for what each one does.

## How live changes apply

The preview lists what the change does to running services:

- CPS stops active services whose unit files the change removes, before it
  removes their files, because `ExecStop` can need them.
- After the files are written and the [triggers](images.md#triggers) have
  run, CPS reloads the systemd manager when unit files change and restarts
  the active services whose packages changed. Then it reloads or restarts
  the active services that the triggers name.
- CPS waits for systemd's job results before committing. A failed job, or a
  job that takes longer than two minutes, leaves the transaction pending.
  Correct the cause and [resume](#recover-an-interrupted-change).
- New services follow the image's preset policy.
- New tmpfiles.d declarations create their paths in the mounted `/var` at
  once, so a newly installed service can start without a reboot. Packages
  never ship files in `/var`; see
  [shared /var](../setups/btrfs.md#share-var-between-boot-environments).

Live writes follow the packages' [dependency order](images.md#write-order),
so a program started during the transaction finds each new library before
the new executables that need it. A directory that becomes a symlink, such
as `/bin` turning into a link to `/usr/bin`, is swapped in one
`RENAME_EXCHANGE` rename after its target is complete, so the path never
disappears. CPS refuses the change before it starts if the directory holds
unmanaged files or edited configuration, and lists them; see
[path transitions](images.md#replace-a-directory-with-a-file-or-symlink).

A transaction's cost follows the packages it changes. CPS trusts recorded
[file fingerprints](images.md#unchanged-files-are-not-hashed-again) of
unchanged files instead of hashing the whole system. Live transactions
advance the package revision while preserving the boot entry's identity.

Before each live change, CPS saves the running root as a read-only
**backup** environment: a Btrfs snapshot registered as a known-good boot
entry titled `Canopa backup`, which the next boot does not select. If a
change breaks the system, select the backup and reboot:

```sh
sudo cps boot list
sudo cps boot rollback "$backup_id"
```

Backups count toward the
[retention policy](../setups/btrfs.md#retention-and-pruning) like other
known-good environments, and each one needs ESP space for its boot files.
Set `live-backup=#false` in `boot-btrfs` to change the running root without
them. See [Backups before live changes](../setups/btrfs.md#backups-before-live-changes).

## Stage changes for the next boot

A transaction that changes the kernel, initramfs, other boot paths, or
a package marked for staging is applied in a new Btrfs environment.
So is a change that runs a trigger whose outputs can cover boot paths, such
as an initramfs generator. CPS selects the new environment for a trial on
the next reboot and keeps the current one as fallback. CPS reports when a
reboot is needed and leaves the timing to you.

The new environment shares the running system's `/var`. Staging leaves it
untouched: the trial's own tmpfiles.d declarations take effect when it
boots. The trial's logs, including those of a failed trial, stay in `/var`
after a rollback.

Pass `--stage` to `install`, `upgrade`, `remove`, or `autoremove` to use the trial boot workflow
for an ordinary package change. The running root keeps its current
packages until you reboot into the trial. Combine `--stage` with
`--dry-run` to preview the changes and reboot requirement.

```sh
sudo cps install --stage --yes apps/htop
sudo cps upgrade --stage --dry-run
```

Package authors can use the
[`requires-reboot` manifest property](../maintainer/manifests.md#require-a-trial-boot)
to stage changes involving a package.

While a trial is pending, system package commands refuse to change the
running root, so the trial holds every package change. Boot into the
trial, or cancel it with `sudo cps boot discard-trial`, which selects
the known-good fallback again. Before staging, CPS checks that the Btrfs
filesystem has room for the new payloads, and by default it prunes old
boot environments under the `boot-btrfs` retention policy. See
[Retention and pruning](../setups/btrfs.md#retention-and-pruning).

### Local changes before the reboot

The trial starts as a snapshot of the running root, so it does not see
changes to `/etc` made after staging, such as a new password or user.
When the system shuts down cleanly, for example with `systemctl reboot`,
`cps-finalize-trial.service` runs `cps boot finalize-trial`. It compares
the running `/etc` and the trial's `/etc` with `/etc` as it was at staging,
and carries each local change into the trial:

- A path that the trial left as it was takes the running version: contents,
  mode, owner, extended attributes, symlink target, creation, or removal.
- `passwd`, `group`, `shadow`, `gshadow`, `subuid`, and `subgid` merge record
  by record when both sides changed different entries, so a user added
  locally and a system user added by the upgrade both stay. If both sides
  gave a new account the same UID or GID, the file is a conflict instead.
- A preserved file that both sides changed keeps the local edit, and the
  upgrade's version waits as a
  [pending configuration update](configuration.md#reviewing-configuration-updates),
  as if you had edited it before staging.
- Otherwise a path that both sides changed keeps the trial's version, and the
  running version is saved below `usr/lib/sysimage/cps/config-updates/etc-merge/`
  in the trial, in one directory per merge, so a later merge into the same
  trial keeps what an earlier one saved. A local change to a package file that is not preserved is
  [drift](images.md#repair-files-or-restore-vendor-defaults), so it is saved
  the same way, and a removed package file stays.

After booting the trial, review what the merge kept aside:

```sh
sudo cps config list
sudo cps config diff /etc/hostname
```

`config list` shows saved running versions as `saved local`, and
`config diff` compares the current file with the saved one. Copy what you
need, then delete the saved copy. `journalctl -b -1 -u cps-finalize-trial`
shows the merge report of the previous shutdown.

A crash, power loss, or `reboot --force` skips the merge, and the trial boots
with `/etc` as it was at staging. The merge builds the new `/etc` beside the
trial's and swaps them in one rename, so an interruption leaves the trial
with either `/etc`. If the merge fails, CPS leaves the trial's `/etc` as it
was and cancels the trial, so the next boot starts the fallback with all of
your changes. Stage the change again once the cause is fixed. See
[Merging /etc at shutdown](../setups/btrfs.md#merging-etc-at-shutdown).

## Recover an interrupted change

If a live change stops before it commits, because of a crash, a power loss,
or a failed trigger or service job, `cps status` reports it:

```text
Transaction 2fbac149… is incomplete. Run cps resume, or cps abort to undo it.
```

Other package commands refuse to run until you finish the transaction one
way or the other. The [recovery table](images.md#choose-a-recovery-command)
lists every case. In short:

```sh
sudo cps resume
sudo cps resume --repair
sudo cps abort
sudo cps abort --fetch
```

- `cps resume` completes the transaction using the files saved in its
  journal. Correct the reported problem first, for example a service that
  fails to start, and resume again if a systemd job still fails.
- `cps resume --repair` also restores damaged paths in the pending package
  state from verified staged or cached payloads, including during the
  file-writing phase. Use it when resume reports that managed files differ.
  It keeps local edits to preserved configuration.
- `cps abort` undoes the change and keeps the committed package
  revision. Use it when you cannot fix the cause or no longer want the
  change. Run it without `--root`: CPS refuses to abort a live transaction
  as an inactive image, and the reverse.
- Add `--fetch` when abort lists payloads it cannot find. It downloads them
  from each publisher's mirror.

On the running system, abort writes every journaled path back, then
restores services:

- It stops active services whose units it removes, before it removes their
  files.
- It runs the triggers that the restored files need, so generated caches
  and a generated initramfs match the committed packages.
- It reloads systemd and restarts the affected services that were running,
  including those that the triggers reload or restart.
- It starts again each service that the transaction stopped because it
  removed its unit. The transaction journals those services before it stops
  them, so an abort after any interruption knows them.

Once an abort starts, the transaction only goes back. `cps status` reports
`Transaction … is being aborted. Run cps abort.` Run that
command again to finish an interrupted or failed abort; `cps resume` also
finishes it, without fetching. See
[Abort a transaction](images.md#abort-a-transaction) for how abort restores
files and what it cannot undo.

## Follow a saved plan

Pass `--save-plan PATH` to a system package command to record the exact plan
it displays. This also works with `--dry-run`. Use
`cps status --plan PATH` to follow it, and add `--json` for a report that
scripts can read. The report includes the plan's SHA-256, its phase, the
revision, and the boot environment when applicable. The JSON report spells
phases in snake case, such as `prepared_for_boot`. Add `--validate` to check cached payloads for a plan that has
not been applied yet. `payload_validated` describes the check made during
that invocation. CPS rechecks the payloads at apply time.

A live change reports the [transaction phases](images.md#recover-an-interrupted-transaction)
and `applied` once committed. A staged change reports `prepared_for_boot`,
`staged_for_boot`, `trial_boot`, or `booted_known_good`. A registered
result outside the running and selected roots reports
`inactive_boot_environment`. The boot phases refer to the plan that created
that environment. A later live change reports its own transaction phase.

The same commands take `--root` to change an
[inactive image](images.md), where `cps fetch` and `cps apply` complete a
saved plan. `cps boot` manages boot environments.

## Repair, restore defaults, and verify

```sh
sudo cps verify --quick
sudo cps verify
sudo cps repair --dry-run
sudo cps revert /etc/example.conf
```

`cps verify --quick` checks the running system against its
recorded file fingerprints, as the boot health unit does. Without `--quick`
it hashes every managed file. See
[Verify an image](images.md#verify-an-image-and-locate-its-state).

Use `cps repair --dry-run` to preview restoration of damaged managed files.
Pass package names to narrow the repair or use `--path /usr/bin/example`
to select specific paths. `cps revert /etc/example.conf` restores the
installed package's default configuration and clears the pending vendor
proposal for that file. Both commands use the installed manifests and exact
payload digests. Use `cps resume` if a live repair is interrupted.

Use `cps repair --stage` to build and verify a recovery root from the
running root. CPS also takes this route for a repair or revert that touches
boot-sensitive files. The command prints the prepared root's path and leaves
boot selection alone. During trial activation CPS verifies the staged
root's recorded parent as the fallback. If the parent is damaged, the
prepared root is available for offline recovery.
