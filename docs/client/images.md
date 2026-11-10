# Manage an inactive image

[Documentation](../README.md) · [Clients](README.md)

Use an image rooted in a directory to assemble packages or prepare a
bootable root. The image must be on a single filesystem for apply.
The [quickstart](../quickstart.md) creates a local repository and installs
a package into an image you can use with these commands.

This page also describes how every CPS transaction runs and how to recover
one, which applies equally to [the running system](system.md).

## On this page

- [Plan, fetch, validate, and apply](#plan-fetch-validate-and-apply)
- [How a transaction runs](#how-a-transaction-runs)
- [Recover an interrupted transaction](#recover-an-interrupted-transaction)
- [Abort a transaction](#abort-a-transaction)
- [Replace a directory with a file or symlink](#replace-a-directory-with-a-file-or-symlink)
- [Inspect transaction history](#inspect-transaction-history)
- [Verify an image and locate its state](#verify-an-image-and-locate-its-state)
- [Reconcile or remove packages](#reconcile-or-remove-packages)
- [Repair files or restore vendor defaults](#repair-files-or-restore-vendor-defaults)
- [Shared-library regressions](#shared-library-regressions)

## Plan, fetch, validate, and apply

A saved plan is a JSON document with `"format": 1`. It records the request,
the image revision, the selected signed catalogs, and every planned write.
Run the workflow from the directory containing the image.

```sh
cps plan --root image --output upgrade.json update
cps fetch --root image --plan upgrade.json
cps validate --root image upgrade.json
cps apply --root image upgrade.json
```

Publish a new package version or [synchronize your mirror](mirrors.md)
before planning an update. The quickstart repository needs a second package
version before the update plan can select a change.

`plan` prints the package changes and the effects that follow from them,
such as `Replace usr/share/demo with a symbolic link to demo2`,
`Recompose PACKAGE` for a [facet change](configuration.md#facets-and-variants-on-an-existing-image),
or `Run trigger PACKAGE:NAME`, and the payload bytes that it needs. `fetch`
fills the cache for the saved plan. `validate` is an optional preflight, and
`apply` repeats its checks before writing package files. Once CPS has
recorded the transaction, the verified staged payloads let it finish offline.

A plan applies only to the revision it was made for. After any other change
commits, apply reports `stale plan for revision N`, and you plan again.
Apply also refuses when a path that the plan leaves unchanged no longer
matches its manifest, and reports `pre-existing drift` for that path.
[Repair the image](#repair-files-or-restore-vendor-defaults), then plan again.

Package selectors in a plan name exact versions as they are spelled in the
package ID; see [package selectors](queries.md#package-selectors-and-versions).

## How a transaction runs

Apply, `cps install` and the other system commands, and `boot-env` staging
all run the same journaled transaction.

1. **Check.** CPS verifies the plan against the image revision and its
   catalog pins, stages and verifies every new payload, and checks the paths
   the plan leaves unchanged. On x86-64 and AArch64 it runs the
   [shared-library check](#shared-library-regressions).
2. **Record.** CPS writes the journal before changing any file. The journal
   holds the plan and the next state as changes to the committed revision,
   so its size follows the transaction rather than the image.
3. **Write files.** Each new file is synced before it is renamed into place,
   and every changed directory is synced once before the journal records
   progress.
4. **Run actions.** CPS runs the systemd actions (sysusers, tmpfiles, and
   presets), then the [triggers](#triggers) whose watched paths changed,
   then kernel-install for a changed kernel or a regenerated initramfs.
5. **Check again and commit.** CPS checks every managed path against the
   next state, then commits the changed package and path records in one
   SQLite transaction and advances the revision.

On the running system, CPS also stops services whose units the change
removes before it writes files, and reloads systemd and restarts affected
services after the check; see
[how live changes apply](system.md#how-live-changes-apply).

Resume rebuilds the next state from the committed revision and checks each
path against both its old and new contents, so CPS can resume or undo an
interruption at any point.

### Unchanged files are not hashed again

When CPS hashes a managed file, it records the file's inode, size,
timestamps, mode, and owner. A later transaction, and `verify --quick`,
trusts a file whose record still matches and hashes the rest, so checking an
unchanged file costs one `stat`. The cost of a transaction therefore follows
the packages it changes, not the size of the image.

Linux updates a file's change time on every write, metadata change, rename,
and extended attribute change, including file capabilities, and programs
cannot set it back. Files changed within the last two seconds are not
recorded, because filesystems take that time from a coarse clock. The
records carry over into a staged Btrfs snapshot, whose files keep their
inodes and change times. A copied root, such as an ext4 slot, has new
inodes, so CPS hashes its files once and records them again. After systemd
actions, triggers, and kernel preparation, CPS checks every managed path the
same way, which hashes the files the transaction wrote and any file the
actions changed. The shared-library check also reuses ELF metadata recorded
for unchanged files.

### Write order

Writes follow the packages' dependencies, so a program started during a
live transaction finds each new library before the new executables that
need it. Each package's files are written after those of the packages it
requires, including providers of required capabilities such as the
generated `cap/elf/...` sonames. Within a package, shared libraries go
first, then other files, and executables last. A symlink goes after the
write that creates its target, so an upgraded `libz.so.1` link never points
at a `libz.so.1.3.1` that is not there yet. Directories are created
first, and removals run after every new file is in place, dependents before
the packages they depend on. The order is part of the saved plan.

A directory that becomes a file or symlink, or the reverse, is swapped in
one rename after its new contents are complete; see
[Replace a directory with a file or symlink](#replace-a-directory-with-a-file-or-symlink).

### Triggers

Packages rebuild shared derived files, such as schema, icon, and font
caches, the hardware database, or an initramfs, with
[triggers](../maintainer/triggers.md) instead of package scripts. A trigger
runs once per transaction, after every file is written, when a package adds,
changes, or removes a path it watches, when its own package is installed or
upgraded, or when another trigger that runs generates a path it watches. A
facet or variant change that adds or removes watched files counts too. The
plan lists each run as `Run trigger PACKAGE:NAME`, in the order CPS runs
them.

- **Requirements.** CPS runs each handler under Bubblewrap and requires
  `/usr/bin/bwrap` on the host that runs CPS. Without root, as for an image
  you assemble as a regular user, the handler runs as root in a user
  namespace, so the host must allow unprivileged user namespaces, and the
  generated files belong to your user.
- **Confinement.** The handler sees the image read-only, without network,
  and can change only the outputs its trigger declares. A handler that
  writes anywhere else fails, and none of its changes reach the image. Its
  output appears on standard error.
- **Generated files.** The trigger's package owns its outputs. `verify`
  ignores their contents, and CPS removes them when the trigger goes away.
- **Failure.** A handler that exits with an error, or runs longer than its
  limit (300 seconds unless the trigger sets another), stops the
  transaction with a message such as
  `trigger test/summary:summary failed with exit status: 42`. Package files
  are already written, and `transaction status` reports
  `Offline actions pending`. Fix the cause, if it is in the image, and
  [resume](#recover-an-interrupted-transaction): CPS journals each finished
  trigger, so resume runs only the remaining ones. If the handler itself is
  broken, [abort](#abort-a-transaction) instead.

On the running system, a trigger whose outputs can cover a boot-sensitive
path, such as a generated initramfs under `usr/lib/modules`, makes the
change [staged for the next boot](system.md#stage-changes-for-the-next-boot).

## Recover an interrupted transaction

A transaction that stops before it commits stays pending. Its journal and
staged payloads remain in the image, the committed revision is unchanged,
and CPS refuses other plans until you finish it one way or the other.

```sh
cps transaction status --root image
cps transaction progress --root image upgrade.json
```

`transaction status` shows the pending plan and its phase: `File writes
pending` while CPS is writing files, `Offline actions pending` (or `Live
actions pending`) once only actions remain, or `Abort in progress`. It also
names the command that finishes the transaction. `transaction progress`
reports the phase of one saved plan as JSON:

| Phase | Meaning |
| --- | --- |
| `planned` | The plan can still be applied. `--validate` checks its cached payloads and reports `payload_validated`. |
| `applying` | This plan is pending. Resume it or abort it. |
| `aborting` | This plan is being aborted. Finish the abort. |
| `blocked` | A different plan is pending. |
| `superseded` | The image moved to another revision. Plan again. |
| `applied` | The plan committed. `revision` is the revision it created. |

The boot phases that `cps status --plan` adds are described in
[Manage a running system](system.md#follow-a-saved-plan).

### Choose a recovery command

| Situation | Running system | Inactive image |
| --- | --- | --- |
| The process was interrupted, or a trigger, systemd action, or kernel preparation failed and you fixed the cause | `sudo cps resume` | `cps transaction resume --root image`, or `apply` with the same plan |
| Resume reports that managed files are damaged | `sudo cps resume --repair` | `cps transaction resume --root image --repair` |
| You no longer want the change, or cannot fix its cause | `sudo cps transaction abort` | `cps transaction abort --root image` |
| Abort lists payloads it cannot find | add `--fetch` | add `--fetch` |
| An abort was interrupted or a trigger failed during it | run `sudo cps transaction abort` again | run `cps transaction abort --root image` again |
| No transaction is pending, but managed files are damaged | `sudo cps repair` | [`plan repair`](#repair-files-or-restore-vendor-defaults) |
| You want the vendor default of an edited configuration file | `sudo cps revert PATH` | [`plan revert`](#repair-files-or-restore-vendor-defaults) |
| A staged change waits for a reboot that you no longer want | `sudo cps boot-env discard-trial` | |

Resume is the usual choice: it finishes the change you planned, and works
offline from the staged payloads. Abort returns to the committed revision.
Repair and revert are ordinary transactions for an image with no pending
transaction. A staged boot change never touches the running root, so it has
nothing to abort; [discard the trial](../setups/btrfs.md#retention-and-pruning)
instead.

### Resume

```sh
cps transaction resume --root image
```

Resume uses the verified plan and staged payloads from the image journal.
You can also rerun `apply` with the same saved plan. If resume reports a
path whose contents differ, add `--repair`: CPS restores damaged paths in the
pending package state from verified staged or cached payloads, including
during the file-writing phase, and keeps local edits to preserved
configuration. After an `inconsistent image` report from offline actions,
CPS suggests the same option.

## Abort a transaction

When a transaction cannot finish, for example because a payload is gone or
an action keeps failing, or when you no longer want the change, return the
image to its committed revision instead.

```sh
cps transaction abort --root image
```

On the running system, omit `--root`; see
[aborting a live change](system.md#recover-an-interrupted-change).

Abort runs every journaled write backwards in reverse order. It restores
files, links, and directory metadata to their state before the transaction,
links hard link aliases to their restored targets, copies moved or adjusted
local configuration back from where the transaction put it, and removes
vendor proposals staged under `config-updates`. Completed transitions between
directories and files or symlinks are undone the same way they were made.
A path that already differed from its manifest when the transaction was
planned, such as a file whose mode was changed locally, returns to that
local state rather than to its manifest. `verify` then reports it as it did
before the transaction, and `repair` restores it.

Abort then runs the systemd actions and triggers of the change back to the
committed packages. A trigger whose watched paths the abort restores runs
again over the restored files, a trigger that the transaction removed runs
again for its restored package, and the outputs of triggers that the
transaction added are removed. When a trigger regenerates the committed
kernel's initramfs, abort prepares that kernel again. Finally it restores
the catalog archives and build receipts that the pending record replaced,
drops the file records of every journaled path, and deletes the pending
record. The committed revision and history do not change, so the same saved
plan can be applied again later.

### Missing payloads and `--fetch`

Before it changes anything, abort checks that it can restore every path
that is not already in its old state. Old file contents come from the staged
payloads, from paths it already restored, or from the payload cache of each
publisher, which keeps the payloads of installed packages and saved plans.
If any are missing, abort changes nothing and lists each path with the
payload digest, size, and the committed package that declares it.

```sh
cps transaction abort --root image --fetch
```

`--fetch` downloads those payloads from each publisher's mirror first. The
committed manifests pin their digests and sizes. Contents that no package
declares, such as a locally edited configuration file that a `revert` plan
already replaced, cannot be fetched. Restore those paths from a backup and
abort again, or finish the transaction with `transaction resume`.

### An interrupted abort

Abort is journaled like apply. It records the decision in the pending
record first, and from then on the transaction only goes back:
`transaction status` reports `Abort in progress`, `transaction progress`
reports the `aborting` phase, and `apply` with the same plan refuses. Run
`transaction abort` again to finish it. `transaction resume`, and
`cps resume` on the running system, also finish an abort in progress, but
only `transaction abort --fetch` downloads missing payloads.

Each restored path is checked against both its state after and before the
transaction, so finishing an abort is idempotent. Each trigger that the
abort finished is journaled, so a failing or interrupted handler leaves the
abort pending, and finishing it runs only the remaining triggers. After its
actions, abort checks every managed path: each journaled path against its
state before the transaction, and every other path against the committed
manifests. If a path matches neither, abort reports
`aborting left an inconsistent image`; correct that path and abort again.

### What abort cannot undo

Abort restores managed paths only. It cannot undo users that an offline
`sysusers` action already created, or remove a different kernel that the
transaction already prepared. Prepared kernels are stored by their packaged
files and remain unused. A committed kernel that the transaction prepared
again for a regenerated initramfs shares its artifacts with the committed
revision, so the abort prepares it once more.

## Replace a directory with a file or symlink

A release can turn a directory into a file or symlink, as in a merged `/usr`
where `bin` becomes a link to `usr/bin`, or turn a file or symlink into a
directory. Both happen in one transaction.

CPS never discards local content and does not move it elsewhere. The plan
fails, before anything changes, when the directory holds anything that the
transaction does not remove, and lists each path with the reason:

```text
cps: cannot replace directory usr/share/demo: this transaction would not remove these paths beneath it:
  usr/share/demo/local.txt (not managed by CPS)
Move or remove them, or restore vendor configuration with revert, then plan again
```

The reasons are `not managed by CPS`, `locally modified configuration`,
`retained local configuration` left by an earlier removal, and `kept by
this transaction`. Move or remove those paths, or
[revert](#repair-files-or-restore-vendor-defaults) the configuration to its
vendor default, and plan again. Locally edited configuration that the
release moves to a path outside the directory moves with its edits. Paths
that a [trigger](../maintainer/triggers.md) generated are no obstacle: they
leave with the old directory, and the trigger regenerates whatever it still
declares. The same holds when an abort turns a directory back into a file
or symlink after a trigger wrote into it.
[Change the file layout between versions](../maintainer/manifests.md#change-the-file-layout-between-versions)
describes what a package declares.

CPS builds the replacement in the image's private staging directory: the
new file or symlink, or the complete new directory tree with its files,
links, and subdirectories. It syncs it and swaps it with the old object with
`renameat2(RENAME_EXCHANGE)`, so the path names either the old or the new
object at every moment. The writes that the new target needs, such as the
files of `usr/bin`, come first. The old object, now in staging, is then
removed without following its symlinks. Paths are resolved beneath the
image root without following symlinks or crossing mounts, so no write can
pass through a symlink that a transition created, and a mount point inside a
replaced directory stops the plan.

Some filesystems do not support `RENAME_EXCHANGE`. There CPS first renames
the old object into staging, syncs, and then renames the replacement into
place, so the path is briefly absent. Each rename is synced before the next
step.

Resume and abort recognize every intermediate state:

| On-disk state | Resume | Abort |
| --- | --- | --- |
| Old object in place, partial replacement in staging | Rebuilds the replacement and swaps it in | Removes the staged replacement |
| New object in place, old object in staging | Removes the old object | Rebuilds the old object from payloads and swaps it back |
| Path absent, old object in staging (fallback only) | Renames the replacement into place | Renames the old object back |

## Inspect transaction history

```sh
cps transaction history --root image
cps transaction show --root image PLAN_SHA256
```

`transaction history` lists completed plans, newest first, with their
revision and operation. The default limit is 20 entries. `transaction show`
prints a JSON record of the request, package changes, catalog digests, and
planned action counts. The record commits with the image state, works
offline, and follows the source image's history when staging a boot
environment. History uses commit order within a revision and omits
wall-clock timestamps so independently provisioned images can still match
byte for byte. An aborted transaction leaves no history entry.

## Verify an image and locate its state

```sh
cps verify --root image
cps verify --root image --quick
cps verify --root image --pristine
```

`verify` hashes every managed file and compares every directory, link,
mode, and owner with the installed manifests, which it checks again against
their signed documents. It also checks the signed catalog archives. It
refuses while a transaction is pending.

`--quick` trusts the [file records](#unchanged-files-are-not-hashed-again)
instead of hashing every file. It still compares every directory, link,
mode, and owner and the path records, and hashes each file that changed or
that CPS has not recorded. It trusts the stored package manifests, which
CPS checked against their signed documents before committing them, instead
of parsing each one again. It reads the records but never writes them. The
boot health check and the promotion of a direct UEFI trial use quick
verification. Run full verification to detect changes below the filesystem,
such as edits to the block device, or changes made while the system clock
was set back.

`--pristine` checks a freshly assembled offline root. It checks all package
paths, including preserved configuration and implicit directories, and
reports unmanaged paths outside the image state. That includes anything in
`var`, which an inactive root leaves empty, and a missing `var` directory. It also rejects unexpected
hard links to regular files and undeclared extended attributes on managed
regular files and directories. Use regular `verify` after provisioning or
booting, when the system can generate files outside package ownership.
Trigger outputs count as owned paths, and their contents are not checked.

Use `verify --package NAME` or `verify --path /path` to limit on-disk
checks to the selected managed files while still authenticating image
metadata. Both also accept `--quick`.

Images keep the package database, transaction staging, prepared kernel
files, and [vendor configuration proposals](../maintainer/manifests.md#filesystem-actions)
under `/usr/lib/sysimage/cps/` inside the image, the location that other
distributions use for their package databases. The SQLite database
`state.sqlite` records image format 1 and uses the application ID `CPSI`.
The directory must be on the image root's own filesystem, and on Btrfs on
the root subvolume, so each boot environment snapshot carries the package
state that matches its files. After a transaction commits or aborts, CPS
clears its recovery copies in `/usr/lib/sysimage/cps/stage`.

`/var` is not part of an image. `image-create` makes `var` an empty
directory, the mount point of a `/var` that the
[boot environments share](../setups/btrfs.md#share-var-between-boot-environments),
and no package can ship anything in it. CPS never writes there in an
inactive image: offline systemd actions defer their tmpfiles rules for `/var`
to boot, and trigger handlers see an empty `/var`.
[Packages never ship /var](../maintainer/manifests.md#packages-never-ship-var-declare-it-with-tmpfilesd-and-sysusersd)
explains what packages declare instead. `image-create` needs an empty
directory, or one that holds only an empty `var` directory, such as a newly
created `/var` subvolume that an installer mounted there first.

## Reconcile or remove packages

Use `plan exact-install NAME ...` to reconcile an image to the named
packages and their required dependencies. The plan previews packages that
would be removed. Use `plan remove NAME ...` to remove selected packages
from committed image state while offline. Removal fails if a package that
remains still requires one of those packages. Both operations preserve
locally edited files declared with `preserve=#true`. A saved install,
exact-install, remove, or update plan can also be applied to a clone with
`boot-env stage`.

Use `cps plan --root image --output cleanup.json autoremove` to preview
removing unused dependencies. CPS solves from the explicitly installed
packages using their installed versions and the installed dependency
candidates. It keeps the dependencies needed by that solution, including a
compatible alternative and any conditional dependencies that apply. The
plan and apply work offline. Use `plan mark explicit` to keep an installed
dependency before planning cleanup.

## Repair files or restore vendor defaults

Use `cps plan --root image --output repair.json repair [NAME ...]` to repair
selected installed packages, or omit the names to check all installed packages.
Apply the saved plan with `cps apply --root image repair.json`. CPS restores
managed files, symlinks, and missing directories from the installed manifests.
It restores ownership and permissions on explicit directories in place.
Package payloads come from the configured local repository or cache and are
checked against the installed digests before any writes. Repair keeps local
edits and deletions on files declared with `preserve=#true` and restores the
installed permissions and ownership of files that are present. A repair that
only fixes preserved configuration metadata works offline. Resolve path
type conflicts involving directories or special files before retrying the plan.

Use `plan repair --path /path` for an exact path and any necessary parent
directories or hard links. To intentionally replace an edited or removed
preserved file with the defaults from its installed package, use
`plan revert /path` and apply the saved plan. Revert uses the installed
package payload and clears any pending vendor proposal for that path.

## Shared-library regressions

On x86-64 and AArch64, applying an install, upgrade, removal, autoremove,
or facet change checks whether any managed ELF file in the resulting image
would lose a compatible managed ELF interpreter or shared library that works
in the current image. For libraries, the check follows the object's `DT_RUNPATH`
or `DT_RPATH`, including `$ORIGIN`, then the applicable default directories
`/usr/lib`, `/lib`, `/usr/lib64`, and `/lib64`. It follows managed links and
checks ELF compatibility and required GNU version nodes.

CPS checks after staging verified payloads and before recording the transaction
or changing package files. A rejected transaction names each affected package,
ELF path, and missing or incompatible library. The check works for saved
plans, live updates, and staged upgrades. It models managed library resolution
and reports regressions from a verified baseline. Unmanaged libraries, loader
cache and environment settings, and runtime `dlopen` calls are outside its
scope. Plans and dry runs can still be generated before payloads are available.
Use `cps fetch --root image --plan upgrade.json` followed by
`cps validate --root image upgrade.json` to check a saved plan and its
cached payloads, filesystem metadata support, and ELF regressions before
apply. Validation uses disposable staging and leaves the image revision and
journal untouched. Apply repeats the checks against the current image.

`cps verify --root image --runtime` also audits the installed managed ELF
closure without a baseline. It checks the interpreter and shared libraries
for immutable managed ELF consumers, using the default search directories
or the ordered `--library-dir` arguments. It reports already missing
dependencies and works without the repository.

Packages carry [generated shared-library capabilities](../maintainer/manifests.md#generated-shared-library-dependencies),
so the solver already refuses a removal, autoremove, or upgrade that drops
the last provider of a required soname or GNU version node. This check
remains the safety net for interpreters, facet changes, libraries shadowed
through `DT_RUNPATH`, and consumers selected by a facet or variant.

Run `bash tests/elf-transactions.sh` to exercise solver refusals of
removals and soname bumps, an install that shadows a working library,
interpreter removal, a facet change, pre-existing drift, and a compatible
replacement.
