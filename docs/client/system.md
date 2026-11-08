# Manage a running Btrfs system

[Documentation](../README.md) · [Clients](README.md)

Use these commands on an installed CPS-managed Btrfs system. For the
installer-side configuration, see [Set up Btrfs boot environments](../setups/btrfs.md).

## On this page

- [Search, install, and upgrade](#search-install-and-upgrade)
- [Check updates and diagnose held packages](#check-updates-and-diagnose-held-packages)
- [Remove packages and select publishers](#remove-packages-and-select-publishers)
- [Stage changes for the next boot](#stage-changes-for-the-next-boot)
- [Preview and resume changes](#preview-and-resume-changes)
- [Repair and restore defaults](#repair-and-restore-defaults)

## Search, install, and upgrade

On a distribution configured for CPS system commands, search by a name
fragment and use the package name from the result.

```sh
sudo cps search htop
sudo cps install apps/htop
htop
sudo cps check-updates --json
sudo cps upgrade
sudo cps status
```

`install` and `upgrade` select dependencies, preview the package changes,
service actions, and activation mode, and ask once before downloading and applying
them. Ordinary changes apply to the running system. Restart any open
applications to use updated files. CPS stops active services before
removing their files, reloads the systemd manager when unit files change,
and restarts affected active services. CPS waits for systemd's job results
before committing. A failed job or a job that takes longer than two minutes
leaves the transaction pending. Use `cps resume` after correcting the cause.
New services follow the image's preset policy.

On upgrade, an unavailable optional publisher is deferred while CPS
continues with the distribution repositories. CPS keeps installed packages
from the deferred publisher at their current versions. If an optional
publisher fails during payload download, CPS previews a new distribution-only
plan and asks for confirmation again unless you passed `--yes`. To use
distribution publishers from the start, pass `--base-only` to `upgrade` or
`check-updates`. This scope includes the primary publisher, signed profile
companions, and publishers declared in the system image configuration.
Package dependencies still apply, so an incompatible installed add-on can
hold back a base upgrade.

## Check updates and diagnose held packages

`check-updates --json` runs the upgrade planner on the running system.
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
deferred, the report names them in `deferred_publishers`. A check with no
actionable changes and only deferred publishers reports `deferred`. A check can report
both actionable changes and held-back packages. Use
`check-updates --explain library/libdisplay` to probe the newest held-back
version of one package with the same catalog pins. The report includes
the resulting solver or build-trust error, or indicates that the version
fits when explicitly required. The probe leaves the normal upgrade plan
as selected.

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
dependencies. `autoremove` removes dependencies that are no longer needed
by explicitly installed packages or the signed release profile. Both use
the same preview, confirmation, and reboot staging as `install`, and work
offline from the installed package state.

When a new package is available from several publishers, an interactive
`install` shows a choice with the newest matching version from each.
Press Enter for the first choice or enter a number. Use
`cps://PUBLISHER/NAME` to select a publisher explicitly. `--yes` and
`--dry-run` use the configured search order. Installed packages keep
their original publisher.

## Stage changes for the next boot

A transaction that changes the kernel, initramfs, other boot paths, or
a package marked for staging is applied in a new Btrfs environment.
CPS selects the new environment for a trial on the next reboot and
keeps the current one as fallback. CPS reports when a reboot is needed
and leaves the timing to you. Boot into the selected environment before
making more package changes so the next boot includes all of your work.

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

## Preview and resume changes

Use `--dry-run` to review the proposed transaction, `--yes` to confirm it
in a script, and `--no-refresh` on `install` or `upgrade` to use the
current cached catalog. Fetching uncached package files can still contact
the mirror. With configured
mirrors, `upgrade` checks for updates and `install` reuses recently
synchronized catalogs. `cps resume` completes an interrupted live
transaction using the files saved in its journal. If resume reports damage
to managed files, use `sudo cps resume --repair`. For an offline image use
`cps transaction resume --root IMAGE --repair`. These commands restore
damaged paths in the pending package state from verified staged or cached
payloads, including during the file-writing phase. They keep local edits to preserved
configuration. If a systemd helper still fails, correct its reported
problem and resume again. The explicit
`plan`, `fetch`, `apply`, and `boot-env` commands remain available for
administering images and boot environments.

Live transactions advance the package revision while preserving the boot
entry's identity. Changes to the running root use the same verified,
resumable package journal as offline image transactions.

Pass `--save-plan PATH` to a system package command to record the exact plan
it displays. This also works with `--dry-run`. Use
`cps status --plan PATH` to follow its SHA-256 in JSON. For an offline
image, use `cps transaction progress --root IMAGE PATH` with a saved plan.
The report includes the phase, revision, and the boot environment when
applicable. Use `--validate` to check cached payloads for a pending-free
planned change. `payload_validated` describes the check made during that
invocation. CPS rechecks the payloads at apply time.

Live changes report `applied` once committed. A boot transaction can report
`prepared_for_boot`, `staged_for_boot`, `trial_boot`, or
`booted_known_good`. A registered result outside the running and selected
roots reports `inactive_boot_environment`. The boot phases refer to the
plan that created that environment. A later live change reports its own
transaction phase.

## Repair and restore defaults

Use `cps repair --dry-run` to preview restoration of damaged managed files.
Pass package names to narrow the repair or use `--path /usr/bin/example`
to select specific paths. `cps revert /etc/example.conf` restores the
installed package's default configuration and clears the pending vendor
proposal for that file. Both commands use the installed manifests and exact
payload digests. Use `cps resume` if a live repair is interrupted.

Use `cps repair --prepare-only` to build and verify a recovery root from the
running root. CPS also takes this route for a repair or revert that touches
boot-sensitive files. The command prints the prepared root's path and leaves
boot selection alone. During trial activation CPS verifies the staged
root's recorded parent as the fallback. If the parent is damaged, the
prepared root is available for offline recovery.
