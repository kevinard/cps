# Manage an inactive image

[Documentation](../README.md) · [Clients](README.md)

Use an image rooted in a directory to assemble packages or prepare a
bootable root. The image must be on a single filesystem for apply.
The [quickstart](../quickstart.md) creates a local repository and installs
a package into an image you can use with these commands.

## On this page

- [Plan, fetch, validate, and apply](#plan-fetch-validate-and-apply)
- [Resume a transaction and inspect its history](#resume-a-transaction-and-inspect-its-history)
- [Verify an image and locate its state](#verify-an-image-and-locate-its-state)
- [Reconcile or remove packages](#reconcile-or-remove-packages)
- [Repair files or restore vendor defaults](#repair-files-or-restore-vendor-defaults)
- [Shared-library regressions](#shared-library-regressions)

## Plan, fetch, validate, and apply

A saved plan is a JSON document with `"format": 1`. It records the request,
the image revision, and the selected signed catalogs. Run the workflow from
the directory containing the image.

```sh
cps plan --root image --output upgrade.json update
cps fetch --root image --plan upgrade.json
cps validate --root image upgrade.json
cps apply --root image upgrade.json
```

Publish a new package version or [synchronize your mirror](mirrors.md)
before planning an update. The quickstart repository needs a second package
version before the update plan can select a change. `fetch` fills the cache for the saved plan.
`validate` is an optional preflight, and `apply` repeats the checks before
writing package files. Once CPS has recorded the transaction, the verified
staged payloads let it finish offline.

## Resume a transaction and inspect its history

If an apply stops partway through, inspect its journal and resume.

```sh
cps transaction status --root image
cps transaction resume --root image
```

Status shows the pending plan and whether CPS is still writing files or has
reached the offline systemd and kernel actions. Resume uses the verified
plan and staged payloads from the image journal. You can also rerun `apply`
with the same saved plan.

The journal records the plan and the next state as changes to the committed
revision, so its size follows the transaction rather than the image. CPS
writes the journal before changing any file. Each new file is synced before
it is renamed into place, and every changed directory is synced once before
the journal records progress or the transaction commits. The commit writes
only the changed package and path records, in one SQLite transaction. Resume
rebuilds the next state from the committed revision and checks each path
against both its old and new contents, so an interruption at any point can
be resumed or repaired.

Before writing, apply checks that the paths the plan leaves unchanged still
match their manifests. When CPS hashes a managed file, it records the file's
inode, size, timestamps, mode, and owner. A later transaction trusts a file
whose record still matches and hashes the rest, so the check costs one
`stat` per unchanged file. Linux updates a file's change time on every
write, metadata change, rename, and extended attribute change, including
file capabilities, and programs cannot set it back. Files changed within the
last two seconds are not recorded, because filesystems take that time from
a coarse clock. The record carries over into a staged Btrfs snapshot, whose
files keep their inodes and change times. A copied root, such as an ext4
slot, has new inodes, so CPS hashes its files once and records them again.
After offline or live systemd and kernel actions, CPS checks every managed
path the same way, which hashes the files the transaction wrote and any file
the actions changed. The shared-library check also reuses ELF metadata
recorded for unchanged files.

Use `cps transaction history --root image` to list completed plans, newest
first. The default limit is 20 entries. Use
`cps transaction show --root image PLAN_SHA256` for a JSON record of the
request, package changes, catalog digests, and planned action counts. The record
commits with the image state, works offline, and follows the source image's
history when staging a boot environment. History uses commit order within a revision and omits wall-clock
timestamps so independently provisioned images can still match byte for byte.

## Verify an image and locate its state

Run `cps verify --root image --pristine` to check a freshly assembled offline
root. It checks all package paths, including preserved configuration and
implicit directories, and reports unmanaged paths outside the image state.
It also rejects unexpected hard links to regular files
and undeclared extended attributes on managed regular files and directories.
Use regular `verify` after provisioning or booting, when the system can
generate files outside package ownership.

Regular and pristine verification hash every managed file. Add `--quick` to
trust the file records described in
[resuming a transaction](#resume-a-transaction-and-inspect-its-history)
instead. A quick check still compares every directory, link, mode, and owner
and the path records, and hashes each file that changed or that CPS has not
recorded. It trusts the stored package manifests, which CPS checked against
their signed documents before committing them, instead of parsing each one
again. It reads the records but never writes them. The boot
health check and the promotion of a direct UEFI trial use quick
verification. Run full verification to detect changes below the filesystem,
such as edits to the block device, or changes made while the system clock
was set back.

Images keep the package database, transaction staging, and prepared kernel
files under `/var/lib/cps/image/` inside the image. The SQLite database
`state.sqlite` records image format 1 and uses the application ID `CPSI`.
On Btrfs, this directory and its ancestors must be part of the root subvolume.
Keep `/var` on that subvolume so each boot environment carries the package
state that matches its files.

## Reconcile or remove packages

Use `plan exact-install NAME ...` to reconcile an image to the named packages and their required dependencies. The plan previews packages that would be removed. Use `plan remove NAME ...` to remove selected packages from committed image state while offline. Removal fails if a package that remains still requires one of those packages. Both operations preserve locally edited files declared with `preserve=#true`. A saved install, exact-install, remove, or update plan can also be applied to a clone with `boot-env stage`.

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

Use `verify --package NAME` or `verify --path /path` to limit on-disk
checks to the selected managed files while still authenticating image
metadata. Both also accept `--quick`. Use `plan repair --path /path` for an exact path and any necessary
parent directories or hard links. To intentionally replace an edited or
removed preserved file with the defaults from its installed package, use
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
