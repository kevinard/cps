# Canopa Package System

CPS is an image-oriented package manager for Linux, written in Rust and inspired by IPS. It manages signed repositories and release profiles, plans recoverable image transactions, stages upgrades into systemd boot environments, and verifies installed files offline. Its build and publishing tools support pinned inputs, isolated reproducible builds, and independently signed build evidence.

Repository reads cap catalogs at 64 MiB and manifests at 32 MiB. CPS streams payloads to the staging directory with the signed length as a bound and checks the digest before recording a transaction.

## Using CPS on an installed Btrfs system

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
applications to use updated files. CPS reloads the systemd manager when
system unit files change and queues restarts for active services owned
by updated packages. New services follow the package's preset policy.

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
pending trial boot or an unsatisfiable release profile. A check can report
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

Package authors can mark packages for staging in the signed manifest.

```kdl
schema-version 1
package "system/example-runtime" publisher="canopa" version="1.0.0-0" requires-reboot=#true {
    file "usr/lib/libexample.so.1" mode="0644" uid=0 gid=0
}
```

The system commands stage the whole transaction when they install, replace,
or remove a marked package, including dependency changes. For replacements,
CPS checks both the installed and selected manifests. An unchanged marked
package does not require staging. The flag defaults to false, and
distribution `stage-package` rules and boot path checks still apply.

Use `--dry-run` to review the proposed transaction, `--yes` to confirm it
in a script, and `--no-refresh` on `install` or `upgrade` to use the
current cached catalog. Fetching uncached package files can still contact
the mirror. With configured
mirrors, `upgrade` checks for updates and `install` reuses recently
synchronized catalogs. `cps resume` completes an interrupted live
transaction using the files saved in its journal. The explicit
`plan`, `fetch`, `apply`, and `boot-env` commands remain available for
administering images and boot environments.

For distribution installers, put the usual publisher and mirror
configuration in `/etc/cps/image.kdl` and add, for example:

```kdl
boot-btrfs volume="/cps" environments="/cps/@environments" esp="/efi" root-partuuid="7adff456-af50-498f-b39a-dcf87cb4a7c1" options="rw"
stage-package "system/elf-runtime"
stage-package "system/systemd"
```

Use the real partition UUID and package names. Mount the Btrfs top-level
subvolume read-write at `volume`, create a separate, root-owned subvolume
for `environments` below it, and mount the writable EFI System Partition
at `esp`. Register the initial root with `boot-env activate-btrfs --known-good`
and install the CPS health service for trial boot assessment. Keep the
repository caches at stable paths outside the boot roots. Use `options`
for any kernel arguments the installation requires. Use `kernel` and
`initrd` only for images that supply boot files manually.

The first live transaction upgrades the image state format so the boot
entry keeps its identity while the package revision advances. Use the
current CPS for managing and verifying those roots. Changes to the running
root use the same verified, resumable package journal as image transactions.

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

## Build and try it

Build with `cargo build --locked`. CPS needs Linux with `openat2` and `/proc`, plus the SQLite and libseccomp development libraries. The example uses OpenSSL to generate a local Ed25519 signing key. Run it as your regular user, with package UID and GID matching that user.

```sh
cps="$PWD/target/debug/cps"
work=$(mktemp -d)
cd "$work"
mkdir -p proto/usr/share/cps-demo
printf 'Hello from Canopa\n' > proto/usr/share/cps-demo/hello

openssl genpkey -algorithm ED25519 -outform DER -out private.der
openssl pkey -inform DER -in private.der -pubout -outform DER -out public.der

cat > hello.kdl <<MANIFEST
schema-version 1
package "demo/hello" publisher="canopa" version="1.0.0-0" {
    file "usr/share/cps-demo/hello" mode="0644" uid=$(id -u) gid=$(id -g)
}
MANIFEST

cat > image.kdl <<'CONFIG'
schema-version 1
publisher "canopa" {
    repository "repo"
    trust-key "public.der"
}
CONFIG

"$cps" publish --repo repo --proto proto --signing-key private.der hello.kdl
"$cps" image-create --root image --config image.kdl
"$cps" plan --root image --output install.json install demo/hello
"$cps" apply --root image install.json
"$cps" verify --root image
```

To upgrade, publish the revised recipe under a new version and run `cps plan --root image --output upgrade.json update`, then apply the saved plan. The plan pins the image revision and the signed catalogs used to resolve packages. Once CPS has staged the payloads and recorded the transaction, it can finish offline.

```sh
cps transaction status --root image
cps transaction resume --root image
```

Status shows the pending plan and whether CPS is still writing files or has
reached the offline systemd and kernel actions. Resume uses the verified
plan and staged payloads from the image journal. You can also rerun `apply`
with the same saved plan. Transactions recorded by earlier CPS versions
require their original saved plan. Use the current CPS to resume a new journal.

Use `cps transaction history --root image` to list completed plans, newest
first. The default limit is 20 entries. Use
`cps transaction show --root image PLAN_SHA256` for a JSON record of the
request, package changes, catalog digests, and planned action counts. The record
commits with the image state, works offline, and follows the source image's
history when staging a boot environment. Older entries show their plan digest
and revision. History uses commit order within a revision and omits wall-clock
timestamps so independently provisioned images can still match byte for byte.

Run `cps verify --root image --pristine` to check a freshly assembled offline
root. It checks all package paths, including preserved configuration and
implicit directories, and reports unmanaged paths outside the reserved
`.cps` state directory. It also rejects unexpected hard links to regular files
and undeclared extended attributes on managed regular files and directories.
Use regular `verify` after provisioning or booting, when the system can
generate files outside package ownership.

Use `plan exact-install NAME ...` to reconcile an image to the named packages and their required dependencies. The plan previews packages that would be removed. Use `plan remove NAME ...` to remove selected packages from committed image state while offline. Removal fails if a package that remains still requires one of those packages. Both operations preserve locally edited files declared with `preserve=#true`. A saved install, exact-install, remove, or update plan can also be applied to a clone with `boot-env stage`.

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
metadata. Use `plan repair --path /path` for an exact path and any necessary
parent directories or hard links. To intentionally replace an edited or
removed preserved file with the defaults from its installed package, use
`plan revert /path` and apply the saved plan. Revert uses the installed
package payload and clears any pending vendor proposal for that path.

### Shared-library regressions

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

Run `bash tests/elf-transactions.sh` to exercise an install that shadows a
working library, an incompatible upgrade, removals, a facet change, and a
compatible replacement.

## Generating a manifest from a prototype

Stage an install into a private prototype tree, for example with `DESTDIR`,
then generate a KDL manifest for review.

```sh
cps manifest generate --proto proto --publisher canopa --version 1.0.0-0 demo/tool > tool.kdl
cps publish --repo repo --proto proto --signing-key private.der tool.kdl
```

The generator emits file, directory, symlink, and hard-link actions in
deterministic order without following symlinks or crossing mounts. For each
regular-file hard-link group it records one payload path and aliases within the
prototype. Groups extending outside the prototype are rejected. Ownership
defaults to UID and GID zero. Use `--uid` and `--gid` to set explicit owners.
The generator captures Linux file capabilities from executable inodes. It
rejects namespaced capability sets, hard-linked symlinks, unsupported permission bits and
filesystem objects, and other extended attributes on regular files or
directories. Review the manifest and add dependency, configuration, and
systemd declarations before publishing. Publication derives the payload
hashes and lengths. [IPS prototype manifest generation](https://docs.oracle.com/cd/E88353_01/html/E37839/pkgsend-1.html)

### Suggesting runtime dependencies

Review the generated manifest against the catalogs for the target image
before publishing it.

```sh
cps manifest dependencies --proto proto --config image.kdl \
    --catalog "canopa=$canopa_catalog_sha256" \
    --catalog "extras=$extras_catalog_sha256" \
    --library-dir /usr/lib --library-dir /lib64 tool.kdl
```

Supply one exact signed catalog digest per publisher in the image config.
The command uses the configured local repositories or metadata caches,
architecture, and facets. It considers the newest active version of each
package name from the first eligible publisher. It reads the pinned
catalogs even if the repositories have since advanced.

The scan reads ELF interpreters and needed libraries, including search
paths relative to `$ORIGIN`. For executable scripts it reads absolute
shebangs, including `env NAME` and simple unquoted `env -S NAME ARGS`
forms. Use repeated `--path-dir` options to set the target command search
order for `env`. The default is `/usr/bin`, then `/bin`. Paths delivered
by the prototype itself satisfy requirements locally.

Standard output contains KDL `require` suggestions with exact versions,
provider IDs, and the files that led to each suggestion. Review the lines
alongside existing dependencies before copying them into the package
block. Missing or ambiguous providers and unsupported inputs produce
diagnostics and a nonzero exit status, with any resolved suggestions still
printed. This is a direct path lookup against signed manifests. Use the
release profile ELF check to validate the final composition and ABI.

Pass `--check` in package CI before publishing to require a matching direct
`require` for every external provider found against the pinned catalogs.
The command prints only missing requirements and fails for missing or ambiguous
providers, unsupported inputs, and declared version ranges that exclude the
detected provider. Use the ELF profile gate to test runtime ABI compatibility.

## Finding and inspecting packages

To measure catalog operations on a disposable, signed repository, run:

```sh
cargo build --locked --bin cps --example tuf-fixture
python3 tests/benchmark-catalog.py --packages 1000 --versions 2 --closure 128
```

The benchmark emits JSON wall-clock timings. It publishes version history,
installs and upgrades a dependency closure with conflicting alternatives,
verifies the installed root, and compares file lookup before and after
publishing a signed index. It also measures TUF export and metadata refresh
into empty and existing caches using a local mirror. The package count,
version history, and closure size are configurable. Use release binaries
via `--cps` and `--tuf-fixture` for production measurements. The runner uses
the local filesystem and does not flush the operating system's file cache.

```sh
cps list --root image
cps list --root image --intent
cps list --root image --available --all-versions
cps search --root image hello
cps search --root image --file hello
cps search --root image --file /usr/share/cps-demo/hello
cps info --root image demo/hello
cps info --root image --available demo/hello@1.0.0-0
cps policy --root image --probe demo/hello@1.0.0-0
cps owner --root image /usr/share/cps-demo/hello
cps contents --root image demo/hello
cps deps --root image demo/hello
cps rdeps --root image demo/hello
cps why --root image demo/hello
```

`list` shows installed packages and their explicit or dependency status. With `--available` it shows the newest version of each package in the signed catalog. `search` matches name and summary fragments case-insensitively and shows the signed summary when present. Use `--installed` to search installed packages. `info` prints the verified manifest in KDL, including optional dependency descriptions, requirements, and file actions. It accepts a name, a name and version, or a full CPS package ID. Installed queries work offline. Available queries use the configured local repository or cache.

`policy` lists candidate versions in publisher order with their verified
catalog pins, the preferred source, active freeze, signed profile manifests,
and whether each source requires build trust. Use `--json` for scripting.
`--probe` runs the normal install planner for the exact selector and
reports its result. Use `check-updates --explain` for a probe in the
system upgrade context.

`search --file` finds which available packages ship a file. Use an exact
basename or a path within the image, with or without a leading slash. It
prints the package ID and root-relative path for each match and exits with
status 1 if none match. It checks the newest version of each package for the
image's architecture in each publisher's verified catalog and honors the
image's facets and signed profile catalog pins. The search uses cached or
local manifests and works with an index-only mirror cache. Publishers can
enable indexed lookups with `cps repo index-files --repo REPO --publisher NAME
--signing-key KEY`. This publishes a successor catalog whose signature binds
a basename index. Subsequent releases regenerate it. Searches open only
candidate manifests and still check their file actions and digests. Mirror
export and sync carry the index alongside the signed catalog. Upgrade
clients before publishing a catalog with the index enabled.

`owner` prints one installed package ID per line for an exact path. Use
`/usr/…` or `usr/…` to address a path within the selected image. Shared
directories can have multiple owners. The command exits with an error
for paths with no owner, including implicit parent directories.

`contents` accepts the same installed package selectors as `info` and
lists owned paths in sorted order, relative to the image root. Both
commands use committed ownership records offline, so a locally deleted
file still shows up and actions excluded by the image's facets or
architecture are omitted.

`deps` lists installed packages that satisfy the selected package's direct
requirements. `rdeps` lists installed packages with a direct requirement the
selected package satisfies. Both accept the same installed package selectors
as `info` and work offline from the committed image. They include all
matching installed providers for `require-any` and `require-capability`, and
conditional requirements whose predicate matches the installed image. Each
match appears once as a package ID, sorted by package name. An empty result
produces no output.

`why` uses the same selectors and works offline. It shows the package's
effective explicit local request, its role in the applied signed profile,
and its matching direct dependents together. Profile roles are `root`,
`dependency`, or `protected` for legacy images with an unknown exact role.
A listed dependent can also have another installed provider for an alternative
or capability. Installed packages with a matching `recommends` declaration
appear separately under `Recommended by`. For a dependency package, `why` also
shows a shortest path of active requirements from a local request or signed
profile root when one exists. It labels alternative requirements and counts
matching installed providers.

`list --intent` shows effective local requests separately from the roles in
the applied signed profile. It reports `protected` for legacy profile
packages whose exact roles cannot be recovered from the image archive.

```sh
cps plan --root image --output mark.json mark explicit demo/hello
cps apply --root image mark.json
```

Use `mark automatic` to clear a local request. Both operations accept
installed package IDs or names and work with the repository offline.
They change the marks in the image database and record them in transaction
history. Package files and versions stay in place. Automatic packages can
be removed later by `autoremove` or signed profile reconciliation if the
resulting image has no use for them. A signed profile root stays protected
even after its local request is cleared. Apply marks to an inactive image
or stage the saved plan in a boot environment.

## Building packages from source

Run `cps build` as an unprivileged user on an x86-64 or AArch64 Linux worker. It needs Linux 5.11 or newer, unprivileged user namespaces, seccomp, and a current Bubblewrap at `/usr/bin/bwrap`. Supply a local CPS repository or synchronized cache containing a signed toolchain catalog. The recipe pins that catalog, the package manifest, and each source file.

```kdl
schema-version 1
build architecture="x86_64" epoch=1700000000 {
    toolchain "<catalog SHA-256>" solver="sat-v1" {
        package "devel/base"
    }
    manifest "hello.kdl" sha256="<manifest SHA-256>"
    source "hello.c" sha256="<source SHA-256>" size=123
    script """
    mkdir -p "$DESTDIR/usr/bin"
    cc -ffile-prefix-map=/build=. "$CPS_SOURCES/hello.c" -o "$DESTDIR/usr/bin/hello"
    """
}
```

Use the digest of the selected repository catalog, such as the digest recorded in its `current` file at recipe preparation. Replace the manifest and source hashes with their SHA-256 digests and set `size` to the source byte count. The manifest path is relative to the recipe. Source paths are relative to `--sources`. The example toolchain package must provide the shell, compiler, and utilities through its dependencies.

`solver="sat-v1"` pins the toolchain selection algorithm. Recipes written before this setting was introduced retain the original resolver when the setting is absent.

### Setting build options

Declare each configurable option and its default inside the `build` block.
A recipe that uses Make can pass compiler flags like this.

```kdl
option "CFLAGS" "-O2"
script """
make "CFLAGS=$CPS_BUILD_CFLAGS"
make "CFLAGS=$CPS_BUILD_CFLAGS" DESTDIR="$DESTDIR" install
"""
```

CPS exposes a declared option named `NAME` as `CPS_BUILD_NAME` inside the
sandbox. Option names start with an uppercase ASCII letter and can also
contain digits and underscores. The recipe decides how to use each value.
Pass `--option NAME=VALUE` for each override.

```sh
cps build --toolchain-config toolchain.kdl --source-cache source-cache \
    --output custom --check-reproducible --option 'CFLAGS=-O3 -g' hello.build.kdl
```

Both reproducibility runs use the same effective values. CPS saves them
in `custom/build.kdl`, and any builder attestation signs that recipe's
digest. Review the effective recipe before approving its digest for
publication. A changed option produces a different recipe digest from
the publisher's original, so an exact rebuild uses the publisher's
archived values.

### Debugging a recipe in its build environment

```sh
cps develop --toolchain-config toolchain.kdl --source-cache source-cache \
    --workspace . hello.build.kdl
cps develop --toolchain-config toolchain.kdl --source-cache source-cache \
    --workspace . hello.build.kdl -- make check
```

The shell or command sees the same verified toolchain, pinned sources,
build options, and offline sandbox as the recipe. It starts in writable
scratch space by default. `--workspace` instead mounts the selected
directory read-write at `/build/workspace` and starts there. The recipe
script is available at `/recipe/build.sh` for explicit execution. Build
outputs in `$DESTDIR` are temporary. Commands inherit the caller's standard
streams and return their exit status.

### Building with several publishers

Use a toolchain block with publisher pins when a compiler and its dependencies
come from separate signed catalogs. Each pin gives the catalog digest and
the SHA-256 of the raw 32-byte Ed25519 public key. With a CPS DER public
key, calculate the fingerprint with `tail -c 32 public.der | sha256sum`.

```kdl
toolchain solver="sat-v1" {
    publisher "canopa" catalog="<catalog SHA-256>" key-sha256="<key SHA-256>"
    publisher "tools" catalog="<catalog SHA-256>" key-sha256="<key SHA-256>"
    package "devel/base"
    package "cps://tools/devel/compiler@1.0.0-0"
}
```

Unqualified packages use the first pinned publisher that offers the name.
A qualified request can select another pinned publisher. Give the build
worker a KDL file with locations for those same publishers. Relative paths
resolve against the worker's configuration file.

```kdl
schema-version 1
publisher "canopa" {
    repository "cache/canopa"
    trust-key "keys/canopa.der"
}
publisher "tools" {
    repository "cache/tools"
    trust-key "keys/tools.der"
}
```

```sh
cps build --toolchain-config toolchain.kdl --source-cache source-cache \
    --output bundle --check-reproducible --builder-signing-key builder.der example.build.kdl
```

CPS verifies each key and reads the exact signed catalog from its local
repository. Build evidence for this recipe form records the ordered pins
and each selected package's publisher, version, and manifest digest.

### Building CPS itself

On an x86-64 Arch Linux worker with Rust, the native GCC toolchain, and Python
3.11 or newer, build the fixture in a new output directory. The worker needs a
sparse crates.io cache populated for the selected revision's `Cargo.lock` and a
directory with the SQLite 3.54.1 autoconf archive and the libseccomp 2.6.1 release
archive. The recipes verify the archive lengths and digests on import.
[SQLite downloads](https://www.sqlite.org/download.html) · [libseccomp release](https://github.com/seccomp/libseccomp/releases/tag/v2.6.1)

```sh
cargo fetch --locked
CPS_RUNTIME_SOURCE_DIR=/path/to/runtime-sources tests/build-cps.sh target/cps-fixture HEAD
```

The fixture takes the package manifests, build recipes, and worker seed scripts
from the selected Git commit. It checks each cached crate
archive against `Cargo.lock` before vendoring from a fresh offline Cargo home.
It builds SQLite and libseccomp twice offline and publishes both to an initial
protected catalog. Their packages carry the headers, pkg-config files, and
linker symlinks under the `devel` facet. The `cps-build-libs` profile
validates both packages with that facet enabled.

The x86-64 CPS package declares its target in the signed manifest. Its
recipe pins that library catalog, the Rust and C toolchain catalog,
and the `runtime-seed` catalog with the worker's prebuilt ELF loader and C
and GCC runtime libraries. The seed provides both loader paths used by CPS
and glibc as hard links to one inode. After two matching builds, the release
adds CPS to the catalog and validates the `cps-runtime` profile with `devel`
disabled.
The fixture installs that profile and uses its own loader and libraries to
run the installed CPS binary and verify the image.

The output includes `source-revision`, the recipes, source cache, toolchain
repository, protected package repository, and runtime seed repository. Set
`CPS_VM_CPS_FIXTURE` to that output directory for a systemd VM boot test. The
VM fixture installs the exact protected CPS package and its dependencies from
the signed repositories. Set `CPS_BIN` to `image/usr/bin/cps` inside the output
directory to also use that executable to assemble the guest. The VM defaults
to package revision `0.1.0-38`. Use the current host and guest CPS to assemble
and boot images with protected profiles.

### Preparing a source cache

Add a `url="https://upstream.example.org/releases/hello.c"` property to a pinned `source` entry to fetch it. Use the final HTTPS URL. The fetcher checks the exact byte count and SHA-256 before publishing an object to the cache. It rejects redirects and rechecks existing objects before reuse. Recipes can share a cache by digest.

```sh
cps source fetch --cache source-cache hello.build.kdl
cps build --repo toolchain-cache --publisher canopa --trust-key public.der \
    --source-cache source-cache --output hello-build --check-reproducible hello.build.kdl
```

Use `cps source fetch --from sources --cache source-cache hello.build.kdl`
when a recipe combines checked-in patches with upstream downloads. CPS checks
the local directory for uncached inputs and uses the recipe URLs for missing
files. A local file that fails verification stops the fetch.

`cps source import --from sources --cache source-cache hello.build.kdl` fills
the same cache offline. Builds read the cache offline and verify each input
again before staging it under its recipe path. The `--sources` build option
accepts a directory laid out by recipe path.

### Building and publishing with signed evidence

```sh
cps build --repo toolchain-cache --publisher canopa --trust-key public.der \
    --sources sources --output hello-build --check-reproducible \
    --builder-signing-key builder-private.der hello.build.kdl
cps publish-bundle --repo repo --signing-key private.der --builder-key builder-public.der \
    --recipe-sha256 "$reviewed_recipe_sha256" hello-build
```

CPS resolves build dependencies against the pinned catalog and verifies their payloads. The toolchain and source mounts are read-only. The recipe gets a fixed user, hostname, locale, timezone, umask, paths, and `SOURCE_DATE_EPOCH` with fresh writable scratch space. Bubblewrap isolates namespaces and seccomp limits sockets to local Unix sockets. Package signing is a separate step with its own inputs. [Bubblewrap sandboxing](https://github.com/containers/bubblewrap/blob/main/README.md) · [SOURCE_DATE_EPOCH](https://reproducible-builds.org/docs/source-date-epoch/)

The bundle contains the recipe, package manifest, and the declared regular file outputs. The manifest supplies the packaged modes, owners, directory entries, and symlinks. `--check-reproducible` repeats the build in a fresh sandbox and requires identical file hashes before making the bundle available. Both runs use the same worker kernel and CPU.

### Worker limits

Each sandbox run has a wall-clock deadline, bounded scratch mounts, a per-file size limit, and a log limit. CPS also caps the total size of declared outputs it collects. Pass `--worker-config worker.kdl` to set limits on the build worker. These settings are separate from the package recipe.

```kdl
schema-version 1
worker timeout-seconds=3600 scratch-bytes=8589934592 file-bytes=8589934592 output-bytes=17179869184 log-bytes=16777216
```

The example shows the defaults. `scratch-bytes` applies to each of `/build` and `/tmp`, with a separate cap of 64 MiB for `/run`. On deadline expiry CPS terminates the sandbox, including its descendants, and the bundle stays unpublished. The writable output filesystem uses a worker-provided disk quota when aggregate disk use during a build must be bounded.

For CPU, memory, and task limits, add a delegated cgroup v2 directory to the worker configuration.

```kdl
schema-version 1
worker timeout-seconds=3600 {
    cgroup "/sys/fs/cgroup/path/to/cps-worker.service" cpu-percent=200 memory-bytes=8589934592 tasks=256
}
```

For a systemd worker service, delegate `cpu memory pids` and place CPS in a leaf such as the subgroup selected by `DelegateSubgroup=supervisor`. Enable the three controllers in the delegated parent's `cgroup.subtree_control` before building. CPS creates a fresh child cgroup per sandbox run, applies the limits before execution, disables swap for the child, and removes it after success, deadline expiry, or memory exhaustion. Configured cgroup limits are required for the build to start. [systemd cgroup delegation](https://systemd.io/CGROUP_DELEGATION/) · [Linux cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html)

The optional worker key signs an in-toto statement in a DSSE envelope after both runs match. The statement records the recipe and source digests, pinned toolchain catalog and resolved package manifests, and the output hashes and lengths. Version 3 also signs the exact published KDL manifest, which binds package metadata to the same evidence as the payloads. Upgrade the publisher before accepting version 3 bundles. Provision the worker public key and the expected recipe digest on the publishing machine from independently reviewed inputs. Keep the publishing key separate from the worker key. `publish-bundle` verifies the statement and streams the exact attested payloads into the repository. [in-toto statement specification](https://github.com/in-toto/attestation/blob/main/spec/v1/statement.md) · [DSSE signature protocol](https://github.com/secure-systems-lab/dsse/blob/master/protocol.md)

Run [`tests/build.sh`](tests/build.sh) on a worker with Bubblewrap, Bash, a C compiler with static libc, binutils, Python 3, and OpenSSL. It exercises isolation, reproducibility, signed provenance, and package installation. [`tests/systemd-build-worker.sh`](tests/systemd-build-worker.sh) also checks resource enforcement and cleanup in a temporary delegated user scope. [`tests/sources.sh`](tests/sources.sh) uses a local TLS server to exercise source downloads and cache integrity.

### Publishing policy

To require attested builds for a release repository, install `publish-policy.kdl` in the repository directory before its first publication. Builder key paths resolve relative to that directory. Approve the reviewed recipe separately for each package version.

```kdl
schema-version 1
publisher "canopa" {
    builder-key "builder-public.der"
    approve "demo/hello" version="1.0.0-0" sha256="<reviewed recipe SHA-256>"
}
```

Protected repositories accept packages through `publish-bundle` and `repo publish-release`. CPS checks the verified worker key and recipe digest against the policy, independently of the command-line arguments. The publisher signs the protection flag into the catalog, and later publication fails if the policy file is missing. Keep bootstrap packages in a separate toolchain repository. Export carries package attestations and leaves the publisher's local policy and key files on the publishing machine.

For distribution repositories, enable toolchain verification before the
first publication.

```kdl
schema-version 1
publisher "canopa" verify-toolchains=#true {
    builder-key "builder-public.der"
    approve "demo/hello" version="1.0.0-0" sha256="<reviewed recipe SHA-256>" toolchain-config="../review-toolchain.kdl"
}
```

The signer uses its own KDL toolchain configuration and authenticated local
catalogs to resolve every requested tool and transitive dependency. It uses
the recipe's pinned catalog digests and solver version, then compares the
complete selection with the version 3 build statement. This works offline
with signed catalogs and manifests and uses any builder trust configured for
those toolchain publishers. The signer can perform the check without
installing the tools or fetching their payloads.

The `toolchain-config` path is relative to the publishing repository. It
accepts the same publisher blocks as the build command, including a single
publisher for a legacy one-catalog recipe. A repository can also set it on
individual approvals. With `verify-toolchains=#true`, every approval needs a
configuration and the signed catalog preserves the requirement across
releases. Upgrade clients before distributing catalogs that enable it.

For independent rebuilds, set `minimum-builders=2` on the policy's `publisher` node and provision a `builder-key` for each worker. Run the same pinned recipe on each worker and pass the additional signed statements with `--witness`. The publishing policy defaults to one worker key.

```sh
cps publish-bundle --repo repo --signing-key private.der --builder-key worker-a-public.der \
    --recipe-sha256 "$reviewed_recipe_sha256" \
    --witness worker-b-public.der=worker-b-build/attestation.json worker-a-build
```

CPS counts distinct authorized keys whose signatures cover the same build statement, including the exact payload hashes and pinned inputs. The release stores those signatures together in the DSSE envelope. Administrators establish worker independence by assigning keys to separately operated workers.

### Rebuilding a published package

Set `archive-build-inputs=#true` on the publishing policy's `publisher` node
to make reviewed recipes and their input manifests available to rebuilders.
CPS requires version 3 build evidence and signs the digests of both exact
documents into the catalog. Once the archive is in use, later releases keep
existing records and include inputs for new packages. For a previously
published version, republish the original bundle with its original
attestation to add its inputs. Clients need archive support to consume a
catalog containing these records.

```sh
recipe=$(cps repo rebuild-inputs --repo /var/lib/cps/canopa --publisher canopa \
    --trust-key canopa.der --output reviewed cps://canopa/demo/hello@1.0.0-0)
cps source fetch --cache source-cache "$recipe"
cps repo fetch-toolchain --toolchain-config toolchain.kdl "$recipe"
cps build --toolchain-config toolchain.kdl --source-cache source-cache \
    --check-reproducible --output rebuilt "$recipe"
cps repo verify-rebuild --repo /var/lib/cps/canopa --publisher canopa \
    --trust-key canopa.der --bundle rebuilt cps://canopa/demo/hello@1.0.0-0
```

When the source files are available for redistribution, the publisher can
include them in the publication using its build source cache. Set
`archive-build-inputs=#true require-source-archives=#true` on the publish
policy's `publisher` node to require the complete archive for every
published version.

```sh
cps repo publish-release --repo repo --signing-key release-private.der \
    --source-cache publisher-source-cache release.kdl
```

`publish-bundle` accepts the same `--source-cache` option. CPS verifies
the source bytes against the reviewed recipe and advances the catalog
after the entire release passes validation. It reuses verified sources
already stored in the repository. A recipe without sources needs no
cache. With `archive-build-inputs=#true` alone, supplying a cache archives
sources for that publication.

Once a signed catalog requires archives, later releases preserve that
requirement. To enable it in an existing repository, first backfill
every published version, including withdrawn versions. For packages
with archived recipes and manifests, use `repo archive-sources` to
attach the sources.

```sh
cps repo archive-sources --repo repo --signing-key release-private.der \
    --source-cache publisher-source-cache cps://canopa/demo/hello@1.0.0-0
cps repo export --repo repo --publisher canopa --trust-key canopa.der \
    --include-sources --output source-mirror-targets
```

Sign those targets through the same TUF release process. The source archive
is available after withdrawal. A repository signs the archive record only
after verifying every pinned source, and later catalogs preserve the record.
Catalogs with source archive records require CPS `0.1.0-20` or later.
Catalogs that require source archives need CPS `0.1.0-22` or later.
For those catalogs, `repo export` includes the source targets automatically.
Sync checks the signed target digests and sizes before promoting the catalog.
Rebuilders download the files explicitly with `repo fetch-sources`.

From a local repository or a synchronized source-inclusive mirror, prepare
the cache without contacting the recipe's upstream URLs.

```sh
cps repo fetch-sources --repo /var/lib/cps/canopa --publisher canopa \
    --trust-key canopa.der --source-cache source-cache cps://canopa/demo/hello@1.0.0-0
```

Use this command in place of `cps source fetch` in the rebuild workflow.
Both full and index-only sync leave source targets for this explicit fetch.
CPS checks the TUF target digest and size against the verified recipe before
writing to the worker's source cache. Valid cached sources can be reused
offline. The same `--catalog` pin selects a historical archived release.

`repo rebuild-inputs` verifies the signed catalog, archived bytes, and
their binding to the admitted attestation before writing a new directory.
The command prints the recipe path, preserves the manifest's relative path,
and writes the original attestation alongside the recipe directory. Use
`--catalog SHA256` to select a cached historical catalog.

Both full and index-only mirror sync carry the archived inputs, including
those for withdrawn versions. The recipe pins the source archives and
toolchain catalogs. Supply source archives without URLs through
`cps source import` or `cps source fetch --from`, and configure local
toolchain repositories with the pinned catalogs and provisioned trust keys.
For a new mirror cache, sync each toolchain publisher with its catalog
digest from the recipe.

```sh
cps repo sync --repo /var/lib/cps/build/canopa --root tuf-root.json \
    --metadata "$metadata_url" --targets "$targets_url" --publisher canopa \
    --trust-key canopa.der --index-only --require-catalog "$catalog_sha256"
```

Repeat `--require-catalog` when provisioning one cache for several recipes.
The signed mirror release must advertise each digest as current or include
it through `repo export --retain-catalog`. Point the toolchain configuration
at the synchronized caches. `repo fetch-toolchain` resolves the pinned
selection with the same solver and builder trust policy as the build. It
downloads and verifies the selected payloads across publishers. The build
verifies those payloads again and runs offline.

For pins outside the live mirror's retention window, publish a separate TUF
archive for the exact catalog. Export from a repository that still has the
catalog and its objects. CPS checks an older catalog against the publisher's
current catalog before exporting it.

```sh
cps repo export --repo toolchain-repo --publisher tools --trust-key tools.der \
    --archive-catalog "$catalog_sha256" --output archive-targets
```

Sign the targets as a separate TUF repository and provision its trusted
root to rebuilders. Use one archive cache per catalog pin.

```sh
cps repo sync --repo "cache/tools/$catalog_sha256" --root archive-root.json \
    --metadata "$archive_metadata_url" --targets "$archive_targets_url" \
    --publisher tools --trust-key tools.der --index-only \
    --archive-catalog "$catalog_sha256"
```

Point the toolchain configuration at that cache. The archive has no live
catalog pointer and uses independent TUF replay state. CPS checks the exact
pin in the signed archive metadata on sync and on later downloads.
`repo rebuild-inputs` and `repo fetch-sources` can also use an archive cache
with an explicit `--catalog` pin. The archive includes required source
targets, and `--include-sources` adds optional sources recorded by the
archived catalog.

`repo verify-rebuild` compares the exact recipe and reviewed manifest in
the bundle, the reconstructed published manifest, and every payload's
length and digest against the signed release. It rejects undeclared
entries in the payload tree. It works offline with the catalog and its
version 3 attestation, including for packages published before recipe
archiving. It accepts the same `--catalog` pin as input extraction.

To verify a signed rebuild, build with `--builder-signing-key` and pass that
builder's provisioned public key to `repo verify-rebuild --witness-key`.
CPS verifies the witness signature over the same statement, including the
resolved toolchain and source pins. It checks that the key is distinct
from the publisher and the builders who signed the admitted statement.

### Verifying builds on images and workers

Add `build-trust` inside a `publisher` block in the image or toolchain
configuration to require builder signatures independently of publication
policy. Provision the worker keys separately from the repository.

```kdl
schema-version 1
publisher "canopa" {
    repository "/var/lib/cps/canopa"
    trust-key "/etc/cps/keys/canopa.der"
    build-trust minimum-builders=2 {
        builder-key "/etc/cps/keys/worker-a.der"
        builder-key "/etc/cps/keys/worker-b.der"
    }
}
```

The threshold defaults to one. CPS verifies evidence from the pinned catalog
for each package entering or changing in an image. It repeats the check when
applying a saved plan. With `--toolchain-config`, CPS checks selected inputs
before materializing the toolchain. The signatures must be from distinct
configured builders, separate from the publisher's current and retired keys.
The evidence must be version 3 and cover the exact published manifest and
payload digests. Cached evidence works offline.

Use `image-create --config` to provision this policy on a new image. CPS
stores the public keys in the image state. Older CPS binaries cannot apply
plans to an image with build trust. Existing version 1 and 2 bundles remain
publishable, and guarded consumers require packages built with version 3.

To enable or replace build trust on an existing image, put the policy in
its own KDL file. Relative key paths resolve from the file's directory.

```kdl
schema-version 1
build-trust minimum-builders=2 {
    builder-key "worker-a.der"
    builder-key "worker-b.der"
}
```

```sh
cps publisher set-build-trust --root image --policy builders.kdl canopa
cps publisher audit-builds --root image canopa
```

Applying a guarded plan archives each admitted package's signed attestation,
catalog digest, and publisher key history in the image database. The archive
follows the installed versions in each boot environment. `audit-builds`
rechecks those signatures against the installed manifest, the recorded
publisher key history, and the image's current builder policy, even without
the repository. Where the image has a signed catalog archive, the envelope
must match the attestation digest admitted by the publisher for that exact
manifest. The same check applies when reusing archived evidence during a
signed profile upgrade.

By default, CPS checks the current or profile-pinned catalog's publisher key
history and audits every installed package before saving the policy. For a
package without an archived receipt, it checks the installed manifest
against the signed catalog and reads the referenced evidence from the
repository cache. This covers withdrawn packages whose evidence is still
cached. Run `set-build-trust` with the same policy to archive that evidence
for future offline audits. When this records evidence from a later catalog,
CPS keeps one additional signed catalog per publisher to authenticate the
receipt offline. The installed image's catalog checkpoint stays at its
applied revision.

For immediate key revocation, use `set-build-trust --future-only` to change
admission policy even with the repository offline. Use `audit-builds` to
identify installed packages whose evidence fails the new policy.
CPS requires a clean transaction state before changing the policy. A change
invalidates unapplied saved plans and preserves the image's boot identity.
It applies to the named root.

Use `publisher add --build-trust builders.kdl` to set build trust as part of
adding a publisher.

### Checking release composition

Add a `profile` inside the policy's `publisher` block to require a representative
image composition before changing the catalog.

```kdl
profile "desktop" architecture="x86_64" {
    package "desktop/base"
    package "system/kernel@7.2.11-0"
    facet "doc.man" #false
}
```

CPS resolves each profile against the candidate catalog using the image solver
and the specified architecture and facets. An unversioned profile package
selects its newest active version. Dependencies can select any compatible active
version. The check covers dependencies, file ownership, and the consistency of
selected kernel actions. Publish coordinated changes in one batch when the
individual changes would break a profile. The first catalog that passes these
checks records the requirement in its signature. Subsequent publications and
withdrawals require a policy with at least one profile. Profile roots come from
the release's publisher. Clients need support for profile enforcement to read
catalogs that record this requirement.

Once a profile has a signed composition, CPS checks an upgrade from the
current release before publishing its successor. Set
`upgrade-from="<signed catalog SHA-256>"` to check an additional historical
release, or to provide a baseline before the first composition is signed.
For a fleet with several supported deployed releases, add one
`upgrade-from "<catalog SHA-256>"` child per historical composition inside
the profile block. Each child requires a signed composition for that profile
and uses its recorded companion pins. The current release is still checked.

Use `repo publish-release --prepare` to stage the signed candidate and its
objects while keeping the current catalog pointer on the previous release.
Test the candidate digest in a private repository used by your upgrade and
boot checks. Then rerun publication with `--expect-catalog SHA256` to
advance the pointer only if the release base is still current and the
recomputed catalog equals the tested digest.

New signed profiles record the exact versions of the requested roots and
the manifest digest of every package in the checked composition. When
the baseline has the same named profile, CPS
replays its recorded composition using its companion pins. With older
profiles that record only root versions, it resolves dependencies from
those roots. The architecture, facets, and publisher order must match the
baseline image configuration. For catalogs without recorded roots, CPS
selects the newest baseline versions of the current profile's root names.

CPS runs the update planner with the resulting installed set. For a signed
baseline profile, the candidate's signed package set supplies exact
constraints. A baseline catalog without a signed profile uses the ordinary
solver with the candidate's root versions as the acceptance check. The
result must retain only packages available in the candidate and pass the
planner's directory and kernel transition checks. This models the managed
package state of a clean image. Live changes and boot health are checked during
staging and boot. A profile that records its roots retains its image
configuration across releases. Use a separate profile name for a new
configuration. Upgrade clients before publishing profiles with signed
compositions.

For a native x86-64 or AArch64 profile, add `elf-check` with the image's
ordered default library directories.

```kdl
profile "desktop" architecture="x86_64" {
    package "desktop/base"
    elf-check {
        library-dir "/usr/lib"
        library-dir "/lib64"
    }
}
```

CPS hashes the selected payloads and reads ELF program headers as data. It
checks `PT_INTERP` and `DT_NEEDED` against the signed file map, including
symlinks, hard links, and pinned companion packages. Each object can use its
own `DT_RUNPATH` or `DT_RPATH`, with absolute paths and `$ORIGIN` supported.
The check covers file availability, ELF class and machine compatibility,
and the GNU version nodes required by each direct dependency. It follows
version-table pointers in the dynamic segment, so it works on objects with
section headers removed. The selected library must define each non-weak
required version with its recorded name and hash. Symbol-by-symbol
relocation, libraries loaded with `dlopen`, inherited search paths, and
loader state from the running system are outside its scope.

Set `require-declarations=#true` on the `elf-check` block to also require
a matching direct `require` for each external package supplying a managed
ELF interpreter or shared library. The publisher checks the actual provider
in the signed composition. This requirement carries forward to later
catalogs for the profile.

The gate applies to the fresh composition and the result of a checked
upgrade. The signed profile records its library directories and version
checking requirement. Later catalogs must retain those checks for that
profile. Upgrade clients before consuming catalogs that record version
checking. Run
[`tests/elf-profile.sh`](tests/elf-profile.sh) to exercise the publication
gate with a compiler-generated executable and shared library and GNU
`objcopy` with `--strip-section-headers` support.

List companion publishers inside a profile when dependencies are supplied by
another repository.

```kdl
profile "cps-runtime" architecture="x86_64" {
    package "system/cps"
    publisher "canopa-runtime-seed" catalog="<signed seed catalog SHA-256>" {
        repository "../runtime-seed"
        trust-key "../runtime-seed-public.der"
    }
}
```

Paths resolve relative to the directory containing `publish-policy.kdl`. The
signer checks each pinned catalog against its configured trust key. It uses the
release publisher first, then companions in declaration order. During an
upgrade check, installed packages keep their original publishers. Set
`upgrade-from` on a companion to pin a different baseline catalog for that
publisher when the baseline lacks a signed record for the profile. If the
baseline has that record, any companion `upgrade-from` must match its signed
pin. A baseline without a profile record or a companion override uses the
candidate's `catalog` pin. The primary
catalog signs the checked architecture, facets, and ordered companion catalog
digests. For each companion it also records the signed catalog sequence and
the fingerprint of the provisioned trust anchor. Images verify both against
the pinned companion catalog and their configured key.

When advancing a profile's companion pin, keep the previous catalog object in
the publishing machine's companion repository or cache. CPS checks that the
new companion catalog advances the sequence and preserves known package
identities, withdrawals, attestations, and key handoffs. Primary mirror sync
rejects successors that drop recorded companion lineage, replace its trust
anchor, or reuse a sequence with another digest. A signing-key rotation through
an authenticated handoff still uses the original anchor. An older primary
catalog with no lineage remains readable, and publishing an updated profile
records lineage for its pins. Upgrade clients before distributing those
profiles. Saved plans continue to use their original catalog set.

Add a `build-trust` block inside a companion publisher to require version 3
build evidence for the companion packages in the fresh composition and in
the result of a checked upgrade. This includes packages the update solver
retains from the baseline. Builder keys must be separate from the primary
and companion publishers' current and retired signing keys.

```kdl
build-trust minimum-builders=2 {
    builder-key "../keys/worker-a.der"
    builder-key "../keys/worker-b.der"
}
```

The primary signs the companion's builder keys and threshold with its
catalog pin. Images following the profile verify that policy for all
companion packages selected by an install or update, including unchanged
packages and packages outside the release's tested composition. For
unchanged packages, CPS reuses archived evidence that satisfies the signed
policy or checks the attestation in the pinned catalog. New and changed
packages must also satisfy any independently configured image policy for
their publisher. Applying a guarded plan also archives the primary catalog
that signed the companion policy. Use
`cps publisher audit-builds --root image seed` to verify installed packages
against that signed policy and any local policy for the companion, even when
the repositories are offline. Images created by older clients can archive
the signed policy by applying a new guarded plan. Later catalogs can rotate
the signed policy and must retain a builder policy for the guarded companion
in that profile. Upgrade clients before consuming profiles with this
requirement.

## Signed releases and mirrors

Each committed repository-backed transaction records the selected catalog
digest, sequence, and publisher key history in the image. Subsequent plans,
saved-plan checks, and available-package queries reject a lower sequence, a
different catalog at the same sequence, or a signing history that drops a
known rotation. The check works with a replacement cache that lacks the
previous catalog. An unsequenced legacy catalog starts providing sequence
protection when the image first applies a sequenced catalog.

CPS also archives one signed catalog per applied publisher. A different
catalog must preserve published manifests, withdrawals, obsoletions, and
profile safeguards, even when the cache lacks the earlier catalog. This
applies to both profile companions and independently configured publishers.
Offline verification checks archived signatures, catalog checkpoints, and
the recorded manifest digest for each installed package. A withdrawal in the
archive still authenticates a package that was installed before withdrawal.
An interrupted apply checks the pending catalog archives before resuming.
For an image with only a compact checkpoint, CPS checks the earlier
catalog if it is still cached. The next repository-backed transaction
creates the per-publisher archives. Upgrade CPS in the target image before
that transaction so it can read the new image format.

Each boot environment keeps its own checkpoints. An older release can still
be installed into a new image using its signed bootstrap catalog or booted
from its existing environment. Upgrade CPS in the target image before its
first checkpointed transaction. Older CPS versions reject the new image
state and cannot resume checkpointed transactions.

Group related package changes and withdrawals in a KDL release file. Use the
reviewed catalog digest from `repo/current` as `base`, or `base="empty"` when
initializing a repository. Bundle, builder key, and witness paths are relative
to the release file's directory.

```kdl
schema-version 1
release "canopa" base="<reviewed catalog SHA-256>" {
    bundle "cps://canopa/demo/hello@2.0.0-0" path="hello-build" builder-key="worker-a-public.der" recipe-sha256="<reviewed recipe SHA-256>" {
        witness "worker-b-build/attestation.json" key="worker-b-public.der"
    }
    withdraw "cps://canopa/demo/hello@1.0.0-0"
}
```

```sh
cps repo publish-release --repo repo --signing-key private.der release.kdl
```

CPS checks the base under the repository lock and uses one policy snapshot for
all bundles. Readers see a single new signed catalog after every action succeeds.
If verification fails, the current catalog stays unchanged. Content-addressed
objects may remain on disk. Historical catalogs remain available for pinned plans.

When only the release profiles change, update the publish policy and publish a
release with `refresh-profiles=#true` and the current `base` digest. The signer
reruns the profile checks and publishes a new catalog if the signed profile
records changed. This lets a primary publisher advance a companion pin without
rebuilding its own packages.

`publish` assembles the local CPS repository using a publisher signing key. Export a snapshot to a new directory before signing it with [The Update Framework](https://theupdateframework.github.io/specification/latest/).

```sh
cps repo export --repo repo --publisher canopa --trust-key public.der --output release-targets
```

The export includes the catalog pointer, signed catalog, and the manifests and payloads for active packages. Sign the exported files with a TUF repository tool such as [`tuftool`](https://github.com/awslabs/tough/tree/develop/tuftool), using the export directory as `--add-targets`. Keep root signing keys offline, assign separate keys to the release roles, enable consistent snapshots, and advance the relevant metadata versions whenever their contents change. Publish the versioned root files, targets, and versioned metadata before publishing the new `timestamp.json`.

Export compresses each distinct payload with Zstandard when the result is
smaller. The mirror carries either `blobs-SHA256.zst` or `blobs-SHA256`,
where `SHA256` identifies the original file. Sync and on-demand fetch verify
the TUF target, then check the original length and digest against the signed
manifest before caching the file. Upgrade clients before serving an export
with compressed payloads.

When a signed release profile still references an older companion catalog,
include that digest in the companion mirror's export with
`--retain-catalog SHA256`. Repeat the option for up to 32 historical catalogs.
Sign the `retained` file and the exported objects together as TUF targets. Sync
verifies the retained catalogs and their relation to the current catalog, then
caches their objects without advancing the pointer or replay checkpoint to a
historical digest. Index-only sync leaves their payloads for plan-specific fetch.
Move primary profiles to an updated companion catalog when rolling out a fix.
The old catalog remains selectable by profiles and saved plans that pin it while
its objects are available.

Provision the initial TUF root and the CPS publisher public key through a trusted channel. Sync into a new cache on a Linux filesystem that supports atomic directory exchange.

```sh
cps repo sync --repo /var/lib/cps/canopa --root canopa-root.json \
    --metadata https://packages.example.org/metadata/ \
    --targets https://packages.example.org/targets/ \
    --publisher canopa --trust-key public.der
```

Point `image-create` or the KDL image configuration at this cache. Give write access to the account that runs sync and keep the cache outside all boot roots on persistent storage. CPS uses the original provisioned root to identify the repository on each sync and stores authenticated root rotations and replay state under `.tuf`. Sync accepts HTTPS or a local `file:///` mirror, with trailing slashes on both URLs.

Full sync checks TUF signatures, versions, and expiration, then verifies the CPS publisher signature and downloads the complete release. A failed download leaves the previous cache available. Sync can repair a damaged cache object when the mirror release includes that exact digest. The cache retains previous content-addressed objects for saved plans. Planning and applying can work offline once the required metadata and payloads are cached.

For smaller upgrade downloads, include byte-level patches when exporting a
mirror release. Pass `--delta-from` with a previously signed catalog digest
that your users are likely to have installed. Repeat the option for other
baselines.

```sh
cps repo export --repo repo --publisher canopa --trust-key public.der \
    --delta-from "$previous_catalog" --output release-targets
```

The export compares new versions against the newest earlier version of the
same package and architecture in each supplied catalog. It includes patches
that save space and keeps the full payloads. Sign the entire export with TUF.
During `cps upgrade` or `cps fetch`, CPS can use the old installed or cached
file as a base when the patch is smaller than the full mirror target.
It checks the patch's TUF metadata and verifies the complete result against
the signed package manifest. If the patch fails, CPS fetches the full
target. CPS streams files when generating and applying patches. When either
version of a file exceeds 64 MiB, the export uses a v2 patch with 64-bit
offsets and lengths.

When preparing a cache for an existing image, add `--image /path/to/image`
to `repo sync`. CPS verifies the candidate against the installed publisher
key, checkpoint, and signed catalog archive before advancing the cache's CPS
checkpoint. This works with an empty cache. If the image has only a compact
checkpoint, CPS needs the last applied catalog in the cache or as a signed
target in the mirror. TUF metadata still records authenticated progress when
a candidate fails the CPS check. The image is opened read-only and keeps
its existing repository paths until an explicit relocation.

### Synchronizing a signed profile

For a profile that spans mirrors, add one `mirror` entry per publisher to
the same KDL file used by `image-create`. The `root` path names that mirror's
provisioned TUF root. Relative paths resolve from the KDL file.

```kdl
schema-version 1
release-profile "desktop"
publisher "canopa" {
    repository "/var/lib/cps/desktop"
    trust-key "canopa.der"
}
publisher "runtime" {
    repository "/var/lib/cps/runtime"
    trust-key "runtime.der"
}
mirror "canopa" root="canopa-root.json" metadata="https://packages.example.org/canopa/metadata/" targets="https://packages.example.org/canopa/targets/"
mirror "runtime" root="runtime-root.json" metadata="https://packages.example.org/runtime/metadata/" targets="https://packages.example.org/runtime/targets/"
```

```sh
cps repo sync-profile --config image.kdl --index-only
cps image-create --root image --config image.kdl
```

CPS verifies the primary release and requires each companion mirror to
advertise the exact signed pin as its current or retained catalog. It
prepares all caches before advancing the primary pointer. If the profile
records a package composition, CPS checks its exact manifests in publisher
order, dependencies, selected paths, kernel actions, and builder trust from
the signed profile and supplied KDL. These checks also run in index-only
mode. Older profiles without a recorded composition can still synchronize
their catalog pins.

Use `--profile-payloads` to fetch the effective files selected by the signed
composition before the profile becomes current. It includes a configured
bootstrap composition and leaves payloads for unrelated packages and
disabled facets for on-demand fetch.

```sh
cps repo sync-profile --config image.kdl --profile-payloads --image image
```

CPS verifies these downloads against the same TUF snapshots used to stage
the catalogs. If any required download fails, the primary pointer stays on
the previous release. The flag requires a recorded signed composition.
Plans that change packages outside that composition can still need
`cps fetch`.

If a companion mirror is behind or the staged composition fails validation,
clients can still select the previous primary catalog and its cached
objects. Replay checkpoints still remember newer verified metadata.

Use `repo sync-profile --config image.kdl --image /path/to/image` to guard
the whole profile with an existing image. The supplied configuration must
match the installed profile, architecture, facets, publisher order, and
publisher keys. CPS checks the current catalog from each mirror and the
exact companion catalogs pinned by the new primary profile. It advances
the CPS checkpoints only after the guarded profile passes validation.

The first successful coordinated sync binds the primary cache to that
profile and its ordered cache directories. Use `repo sync-profile` for
subsequent updates to that primary cache. Give each profile its own primary
cache. Companion current pointers can advance independently, and profile
clients continue to use the exact companion pins recorded by their primary
catalog.

For an image that uses a small part of a mirror, add `--index-only` to `repo sync`.
This caches the catalog, manifests, and build evidence and verifies the signed
TUF target entries for payloads. Fetch the files for the resulting plan before
applying or staging it.

```sh
cps plan --root image --output upgrade.json update
cps fetch --root image --plan upgrade.json
cps apply --root image upgrade.json
```

Fetch revalidates the plan against the image and its pinned catalogs. If a
required payload is missing or damaged, fetch refreshes TUF metadata using the
mirror endpoints saved by `repo sync` and verifies the exact target. Use the
sync account to fetch into its cache. Fetch leaves the image and the cache's
current catalog unchanged. A cached copy or a historical archive can supply
payloads the live mirror has dropped. To recover a missing payload for a saved
plan or an installed package, provision a historical cache with
`repo sync --archive-catalog` and pass it to fetch.

```sh
cps plan --root image --output repair.json repair system/kernel
cps fetch --root image --plan repair.json --archive-cache canopa=/var/lib/cps/archive/kernel
cps apply --root image repair.json
```

Repeat `--archive-cache PUBLISHER=PATH` for multiple archives. CPS checks
the archive with the image's publisher key and uses the first supplied
archive that lists the exact package manifest. It verifies the required
payloads and copies them into the configured repository's blob cache
without changing catalog pointers or the image. Other payloads use the
configured mirror. An archive that already has verified bytes works offline.

When an archive matches an exact catalog pin in a saved plan, fetch also
restores the signed catalog, manifests, build evidence, and archived rebuild
inputs before replaying the plan. This can recover a plan after the cache is
rebuilt at its configured path. CPS still checks the image revision, publisher configuration, catalog
history, and the complete plan before fetching payloads or applying it.

### Reclaiming mirror cache space

`repo prune-cache` lists unneeded payload files and their lengths. Run it
once to review the list, then repeat with `--apply` to remove those files.
It operates on TUF mirror caches. A cache created by an earlier CPS version
needs a successful sync to record which historical catalogs its mirror
retains.

```sh
cps repo prune-cache --repo /var/lib/cps/canopa --publisher canopa \
    --trust-key public.der --keep-image current-root --keep-image fallback-root \
    --keep-plan upgrade.json
```

CPS keeps payloads referenced by the current catalog and by catalogs retained
when that release was synchronized. Coordinated profile sync registers the
visible profile's companion pins, including its configured bootstrap pin.
Pruning keeps those payloads even when a standalone companion sync updates
the mirror's retention list. If promotion stops partway through, pruning
still follows the primary cache's visible catalog. If that pointer is
unavailable or unknown, it conservatively keeps both registered selections.
Run a coordinated sync with CPS `0.1.0-24` or later to register an existing
profile, and use that version or later to prune its caches.

On an installed Btrfs system, add `--system-config /etc/cps/image.kdl` to
discover registered roots that use the selected cache. The preview reports
each retained boot entry, its package payload count, and any profile
catalog it keeps. The boot entries and image states stay locked while CPS
computes and applies the retention set.

Repeat `--keep-image` for each root that needs offline repair, including
fallback roots. An image following a signed
profile also keeps the profile's current pin for that cache. Repeat
`--keep-plan` for saved plans and `--keep-catalog SHA256` for other historical
catalogs needed by builds or releases. Supply the references for each cache
you prune. Catalogs, manifests, build evidence, and replay checkpoints stay
available.

Use `--keep-lock image.lock.json` to retain the locked packages' payloads
for that cache's publisher without retaining every payload in the pinned
catalog. CPS checks the lock's publisher key, catalog, and manifest digests
before pruning.

Each new publication or withdrawal advances a sequence signed by the CPS
publisher. Sync remembers the newest authenticated catalog even if downloading
its payloads fails. It rejects older publisher sequences and different catalogs
with a sequence it has already seen, including when those catalogs arrive with
newer TUF metadata. Sync can retry the same catalog and can skip sequences. A
successor must preserve known package identities, build evidence, and
withdrawals. Once a repository requires build evidence, sync also enforces that
requirement on successors. Older catalogs remain readable, and the first change
made to a legacy repository starts its sequence at one. Upgrade clients before
publishing sequenced catalogs.

The publisher signs the association between each package and its build evidence. Export and sync carry the evidence with the release. To retrieve it, use `cps repo attestation --repo /var/lib/cps/canopa --publisher canopa --trust-key public.der cps://canopa/demo/hello@1.0.0-0`. Clients need a CPS version that supports attestations before they can read catalogs containing that field.

### Publisher signing key rotation

Rotate a publisher's catalog signing key on the publishing repository with both
private keys and the reviewed current catalog digest.

```sh
cps repo rotate-key --repo repo --publisher canopa --base "$reviewed_catalog_sha256" \
    --signing-key old-private.der --new-signing-key new-private.der
```

Both keys sign the handoff, including the base catalog digest and the sequence
where the new key takes effect. The new key signs the successor catalog and all
subsequent publications and withdrawals. The catalog carries the handoff
history, so clients can skip releases and still verify the current signing key.
TUF sync remembers authenticated handoffs even if a payload download fails. It
rejects releases that drop or rewrite known handoffs or backdate a new handoff
into an already authenticated catalog sequence.

Keep the original publisher public key as the trust anchor for export, sync,
images, and pinned build toolchains. It verifies historical catalogs and anchors
newer ones. Export and sign the rotated catalog with the repository's TUF roles
as usual. The publisher and TUF roles have separate keys and rotation procedures.
Upgrade clients before distributing the rotated catalog. If the current private
key is lost, recovery requires a new trust bootstrap.

### Withdrawing and obsoleting packages

To withdraw an exact version, use the publisher signing key on the publishing
repository, then export and sign a new TUF release.

```sh
cps repo withdraw --repo repo --signing-key private.der cps://canopa/demo/hello@1.0.0-0
cps repo withdrawals --repo repo --publisher canopa --trust-key public.der
```

The signed withdrawal records the original manifest digest and prevents reuse of
that package ID. A cache that has accepted the withdrawal rejects later releases
that omit or change the record. New installs use the remaining available versions.
An installed withdrawn version remains eligible for ordinary retention until it
can be upgraded or is explicitly removed. Saved plans still use their pinned
catalogs. The publishing repository and existing caches retain previously fetched
payloads and build evidence for recovery and auditing. A fresh cache only fetches
objects for active versions. Upgrade clients before distributing a catalog with
withdrawals.

To retire a package name as part of a rolling release, add an `obsolete` action
to the KDL file for `repo publish-release`.

```kdl
schema-version 1
release "canopa" base="<reviewed catalog SHA-256>" {
    obsolete "cps://canopa/library/legacy@2.0.0-0"
}
```

The cutoff must be newer than every previously published version of that
package. CPS withdraws its active versions and signs the cutoff into the same
catalog. New install and update plans cannot select versions through the cutoff.
If a selected publisher has obsoleted an installed package and offers no newer
version, resolving a transaction removes it when remaining dependencies allow
that removal. Coordinate changes to dependents in the same release. Removal
preserves locally edited configuration under the ordinary image rules.

An obsoletion reserves the name at its publisher's position in the search order.
A higher version can reintroduce the package, while the cutoff and earlier
withdrawals remain in the signed history. Saved plans still use their pinned
catalogs. Inspect the cutoffs with `cps repo obsoletions --repo repo --publisher
canopa --trust-key public.der`. Clients need obsoletion support to read catalogs
containing these records.

### Renaming packages within a publisher

Publish the successor before the rename or include its build bundle in the
same release. Add a `rename` action to the reviewed release document.

```kdl
schema-version 1
release "canopa" base="<reviewed catalog SHA-256>" {
    rename "cps://canopa/apps/old-editor@2.0.0-0" to="apps/editor"
}
```

The source version is an obsoletion cutoff and must be newer than every
published version of the old name. The publisher signs the rename and
permanently retires the old name. Later releases can rename the successor
again, and clients that skip releases follow the chain to its final name.
Update clients before publishing a catalog with rename records.

`cps upgrade` removes an installed source and selects its successor from
the same publisher in one transaction. It carries forward explicit
installation intent and keeps automatic dependencies automatic unless
another explicit request or the signed profile marks the successor as
explicit. Update packages that depend on the old name as part of the release.
If the installed source is frozen, the successor cannot satisfy a freeze
or other constraints, or the successor is installed from another publisher,
CPS blocks the upgrade. If the source belongs to the protected signed
composition, the new signed profile must include the successor.

Use `cps upgrade --dry-run` or `cps check-updates --json` to preview the
transaction. Other catalog-based package changes wait until the rename
has been applied.

### Publisher order

An image configuration can declare multiple publisher blocks in search order.
Give each publisher its own repository or synchronized cache and trust key.

```kdl
schema-version 1
publisher "canopa" {
    repository "/var/lib/cps/canopa"
    trust-key "/etc/cps/keys/canopa.der"
}
publisher "extras" {
    repository "/var/lib/cps/extras"
    trust-key "/etc/cps/keys/extras.der"
}
```

By default, for a new package, the solver considers versions from the first
publisher that offers the package name. An installed package stays with its original
publisher. Each plan that resolves packages pins a signed catalog per publisher,
and apply checks the same catalog set and stages the selected payloads before
changing the image. [IPS publisher search order and stickiness](https://docs.oracle.com/cd/E53394_01/html/E54739/gijmo.html)

`list --available` shows the newest version from the selected publisher for each
name. Use `list --available --all-versions` to see every publisher's catalog
entries, or a full package ID with `info --available` to inspect a particular one.

`plan install` and `plan exact-install` accept names, `NAME@VERSION`,
`cps://PUBLISHER/NAME`, and full package IDs. A publisher-qualified request can
select a lower-priority publisher for a new package. To change the publisher of
an installed package, remove the package first. An exact version can select an
older version from the package's own publisher when the resulting image
satisfies all dependencies. A version on its own keeps normal publisher order.

On an existing image, use `publisher list` to inspect search order.
`publisher add --root image --repo /var/lib/cps/extras --trust-key extras.der extras`
verifies the signed catalog before appending it. Use
`publisher prefer --root image extras` to move it to the front of its group.
`publisher remove --root image extras` requires that no installed package
comes from that publisher and at least one publisher remains.
These changes invalidate outstanding plans for the image and preserve its
boot environment identity. They are local to the named image root.

Use `publisher set-repository` to move configured publishers to replacement
local repositories or caches. Supply multiple `--repo` arguments to move
the paths together in one transaction.

```sh
cps publisher set-repository --root image \
    --repo canopa=/srv/cps/canopa --repo extras=/srv/cps/extras
```

CPS verifies the replacement catalogs with the installed trust keys and
checks them against the image's catalog checkpoints. Signed profiles validate
their primary catalog and every pinned companion together. A legacy image
with only a compact checkpoint requires the last applied catalog in the
replacement cache if the candidate is newer. An index-only cache works once
it contains the required signed catalogs. Package payloads are fetched during
the normal plan and apply workflow. Relocation invalidates outstanding plans
and preserves the installed packages, boot identity, and applied checkpoints.
Use the new paths in the KDL configuration used for subsequent mirror syncs.

### Images that follow a signed release profile

Add `release-profile "desktop"` to the image KDL to follow that profile in the
first publisher's signed catalog. Configure the same architecture, facets, and
publisher order as the profile. Each companion repository must have the signed
catalog referenced by the profile. The signed publishers keep their signed
membership and order. Repository paths can be relocated in place.

On an image with no installed packages, use the profile's checked roots and
dependencies directly.

```sh
cps plan --root image --output install.json install-profile
cps fetch --root image --plan install.json  # for an index-only mirror cache
cps apply --root image install.json
```

CPS checks every manifest digest against the signed composition, then
checks dependencies, paths, and configured build trust before using the
normal transaction. It marks the profile roots as explicitly installed.
The image records the manifest digests of the applied composition. Local
installs, exact installs, removals, and dependency cleanup preserve that
composition. Exact installs reconcile local additions around the protected
packages. Only a signed profile update can replace the protected set.
Older profiles can still be used with ordinary `plan install`. Use
`repo publish-release` with `refresh-profiles=#true` to record the
composition before using `install-profile`.

After installing the signed composition, you can add a locally trusted
publisher directly from a TUF mirror URL. Obtain its TUF root metadata and
CPS publisher key through a trusted channel before adding it.

```sh
sudo cps publisher add extras --url https://packages.example.org/extras/ \
    --tuf-root /etc/cps/keys/extras-root.json \
    --trust-key /etc/cps/keys/extras.der
sudo cps install cps://extras/apps/editor
sudo cps upgrade
```

The URL must serve `metadata/` and `targets/` beneath it. CPS verifies
and registers the mirror in one command, and `upgrade` refreshes it
automatically using the saved trust root. The default cache lives in
`repositories/NAME` under the configured Btrfs environments directory so
it is available across boots. Use `--cache PATH` for a different location
or when adding to an offline image. Use `--no-refresh` on `upgrade` to
use already cached catalogs.

For a local repository or a manually managed mirror cache, use `--repo`.

```sh
sudo cps publisher add extras --repo /var/lib/cps/extras \
    --trust-key /etc/cps/keys/extras.der
```

The image stores local publishers after the signed ones. `publisher prefer`
reorders local publishers within that group. `publisher remove` removes a
local publisher once its packages have been removed. A local package can
depend on the protected base, and CPS rejects package selections that would
replace a protected manifest or conflict with its files. On an installed
Btrfs system, publisher membership edits use the boot configuration in
`/etc/cps/image.kdl` and apply to the running known-good environment.

Keep the signed publishers and their mirror definitions in the profile's
image configuration. CPS refreshes URL-added publishers independently of
the signed profile sync. A profile update uses the signed companion pins
and each local publisher's current verified catalog. For manually managed
mirrors, run `repo sync --image /` with the mirror options to check the
new catalog against the installed system.

To install a particular published release, set `bootstrap-catalog` to the
primary publisher's signed catalog digest.

```kdl
release-profile "desktop" bootstrap-catalog="0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
```

Keep that catalog and its signed companion catalogs in the configured local
repositories. With mirrors, `repo sync-profile --config image.kdl` requires
both the current and bootstrap catalog sets to be advertised as current or
retained TUF targets. It verifies both compositions before promoting the
current release. CPS uses the bootstrap pin when creating the image,
looking up available packages, and planning `install-profile`. Cache
pruning with `--keep-image` retains the pinned catalogs' payloads. The first
committed `install-profile` consumes the pin, and subsequent plans follow the
current signed release.

For reproducible release images, also set `image-id` to the release image's
UUID. This fixes the initial image identity used in saved plans, ext4 exports,
and boot entries.

```kdl
image-id "a47593c2-e985-42d2-92b6-44c7057e1e96"
```

To compare independent builds, use the same signed bootstrap catalog, image
configuration, canonical repository paths, builder UID and GID, and
`SOURCE_DATE_EPOCH`. Use the same GPT template for byte-identical disk
images. A staged upgrade receives a fresh image identity.

On a profiled image with installed packages, `plan update` reconciles the
image to the current signed composition plus the packages the operator
explicitly requested and their dependencies. A previous profile default
leaves the image if the new profile and local packages no longer require it.
The package can remain published for manual installation. Use
`plan mark explicit` to keep an installed default as a local choice.
`plan exact-install` replaces the local choices while preserving the signed
composition. Signed obsoletion rules still apply. A profile update fails if
the signed set requires a downgrade or a publisher switch.

Save and apply the plan normally, or stage it in a boot environment.
Existing saved updates retain their original package selection rules.
When an earlier image lacks separate local and profile marks, CPS uses
the archived signed roots if they match its applied composition and
otherwise keeps its existing explicit marks as local choices.
Use a CPS release that supports reconciliation before publishing a profile
that depends on it to remove old defaults. Images that need to record
overlapping local and profile choices, or keep roots after the catalog
has advanced, use image format 8.

Available-package queries and new plans use the primary publisher's current
catalog and the companion digests it signs. Saved plans use their original
primary catalog and must match the companion digests signed in that catalog.
Local history keeps those plans usable after a release advances. Offline
installed-package queries, removal, and repair use committed image state.
Upgrade CPS before using catalogs with signed release profile records.

### Phased profile updates

Set `rollout-percentage` on a profile in the publisher's release policy.
The value is signed with the checked composition.

```kdl
profile "desktop" architecture="x86_64" rollout-percentage=10 {
    package "desktop/base@2.0.0-0"
}
```

An image outside the rollout defers the entire composition-changing upgrade,
including the packages from pinned companion catalogs. A percentage of
`0` pauses these upgrades and `100` makes the profile available to every
cohort. Omitting the property also makes the profile available to everyone.
New images can install a profile at any percentage. Mirror refreshes can
continue while the package upgrade is deferred.

CPS assigns a cohort from 0 to 99 using the image's original identity and the
target profile's roots, manifest digests, and companion catalog pins.
Boot environments inherit their source image's cohort for the same target.
Images cloned from the same provisioned image share a cohort. Independently
created images get separate identities by default.

To widen the rollout, change the percentage and publish with
`refresh-profiles=#true` using the current catalog as the release base.
Keeping the same composition, roots, and companion pins keeps the same
cohorts. If an earlier composition is still deployed, the publish policy's
`upgrade-from` can also check the transition from that catalog.

A deferred `plan update` reports the reason without saving a plan. Operators
can override the gate explicitly.

```sh
sudo cps upgrade --force-phased-update
cps plan --root image --output upgrade.json update --force-phased-update
```

The override is recorded in the saved plan and transaction history. The
normal signature, dependency, and image checks still apply. Upgrade clients
before publishing profiles that use `rollout-percentage`.

Attach an advisory to the profile that contains the fix. The publisher
checks that the package is in the selected composition and that its
selected version falls outside the affected range.

```kdl
advisory "CPS-2026-01" package="library/runtime" affected="<2.0.0-0" summary="Runtime security fix" action="Restart dependent applications"
```

`check-updates` reports matching advisories for installed packages,
including when the target profile is deferred. The signed advisory reports
the fixed version and any operator action. Later releases carry the advisory
forward while the profile still contains the affected package from the
same publisher. Publication rejects a new profile version that matches the
affected range. Publishers can revise a notice by declaring it again for
the same ID and package. Upgrade clients before publishing profiles with advisories.

### Running release promotion in CI

The GitHub Actions release workflow runs on a dedicated, trusted Linux
runner labeled `cps-release` and uses the `cps-release` environment. Restrict
the environment and runner to the protected `main` branch. Install the CPS
build dependencies, populate the locked Cargo dependency cache, and install
GNU coreutils and `jq` on the runner.

Configure the environment variables below as absolute runner paths unless
the value is a publisher name. Put the local path of the protected publisher
key in the environment secret `CPS_RELEASE_SIGNING_KEY_PATH`.

| Variable | Value |
| --- | --- |
| `CPS_RELEASE_REPO` | Writable publishing repository |
| `CPS_RELEASE_PUBLISHER` | Publisher name |
| `CPS_RELEASE_INPUTS` | Trusted parent directory of release requests |
| `CPS_RELEASE_SOURCE_CACHE` | Optional source cache for release build bundles |
| `CPS_RELEASE_VM_GATE` | Executable that boots and smoke-tests an actual rehearsed root |
| `CPS_RELEASE_ROLLOUT_GATE` | Executable that checks health of the deployed cohort |
| `CPS_RELEASE_ROLLOUT_PLAN` | JSON policy for scheduled rollout advancement |
| `CPS_RELEASE_DEPLOY` | Executable that deploys the signed catalog to the serving mirror |

Each request contains `release.kdl` and a `cases/` directory. Each named
case contains an `image.kdl` using trusted publisher keys and the intended
profile and architecture. Add a `baselines` file with one additional
historical catalog digest per line. An optional executable
`before-upgrade` receives the CPS binary, baseline root, baseline digest,
and candidate digest. Use it to install representative third-party
software or prepare service state. The runner rehearses each profile's
fresh install and upgrade from the current signed profile, plus the
specified historical baselines. List every supported historical composition
in the relevant cases. Each rehearsal checks the applied catalog pin and
calls the VM gate with mode (`fresh` or `upgrade`), profile, candidate
digest, baseline digest, resulting root, and previous root. The baseline
digest and previous root are empty for a fresh install.

Dispatch `promote` with a request name. A failed rehearsal leaves the
publishing pointer in place. Dispatch `widen` with the deployed catalog
digest, profile, and new percentage. It calls the rollout gate with the
repository, publisher, digest, profile, and percentage before signing a
successor. Widening changes only the signed rollout percentage and
catalog sequence. The existing cohort assignment stays stable.

For scheduled advancement, set the repository variable
`CPS_RELEASE_AUTOROLLOUT=true` and configure a protected runner policy file.
Set `catalog` to the initial promoted digest, `profile` to its profile name,
and `percentages` to increasing approved steps beginning at the signed
initial percentage. For example, a release admitted at 10 percent can use
`"percentages": [10, 25, 50, 100]`. The workflow checks twice an hour
and can also be dispatched with `advance`. Each run can widen by one step.
The health adapter must confirm the serving mirror exposes the requested
digest and check the cohort's health. Exit 75 to wait for more observations
or a longer soak. Any other nonzero exit fails the run.

The deployment adapter receives the repository, publisher, and exact
catalog digest. It should check the digest before exporting and signing
TUF targets, publish the new timestamp last, and safely accept retries.
The workflow records successful deployment in `.ci-deployed-catalog`
in the publishing repository. If the approved current digest has not
been recorded, a scheduled run retries its deployment before widening.
If mirror deployment fails after local promotion, dispatch
`deploy-current` with the current digest to retry that deployment.
`bash tests/release-ci.sh` exercises the workflow with a local repository
and test adapters.

### Provisioning the same composition on another machine

```sh
cps image-lock --root installed > desktop.lock.json
cps image-create --root replica --config image.kdl --lock desktop.lock.json
cps plan --root replica --output install.json install-lock desktop.lock.json
cps fetch --root replica --plan install.json
cps apply --root replica install.json
cps image-diff installed replica
```

The lock records the exact package composition, publisher order and key
fingerprints, catalog pins, architecture, facets, and local package
requests. The replica gets a fresh image identity. The target's KDL
configuration supplies its repository locations, trust keys, and build
trust policies. Give it the locked architecture, facets, profile name,
and publisher order, including any locally added publishers. Retain the
pinned signed catalogs and their selected payloads in the configured
repositories or caches.

Capture a profiled image after its applied base matches its archived
primary catalog. If a local install advanced that catalog to a different
profile composition, apply the signed profile update before capturing.
A lock can provision a fresh image from historical catalogs. Future
updates use the installed image's usual catalog checkpoints and profile
policy.

For disconnected provisioning, export the lock together with the repository
objects it needs. Populate the source caches with the selected package
payloads before exporting.

```sh
cps image-lock --root installed --export desktop.bundle
cps image-create --root replica --config image.kdl --bundle desktop.bundle
cps plan --root replica --output install.json install-lock desktop.bundle/image.lock.json
cps apply --root replica install.json
```

Export verifies every copied object and publishes the new directory only
after the copy succeeds. The bundle contains the pinned signed catalogs,
their manifest indexes, and the selected attestations and payloads. Its
repositories supply the historical pins. The target configuration still
supplies publisher keys and build trust. Keep the bundle at a stable path
for offline repair, or configure regular repositories to resume updates.
Before initializing the requested root, `image-create --bundle` replays
the lock with the local trust policy and reports missing or damaged
payloads from the bundle.

## Manifest and image behavior

Set `summary` on the package node and add an `optional` node for each
recommended add-on. Optional dependencies are advisory and can be installed
separately. Their version constraints describe compatible add-on versions.

```kdl
schema-version 1
package "apps/editor" publisher="canopa" version="1.0.0-0" summary="A lightweight text editor" {
    optional "apps/editor-spellcheck" version=">=1.0.0-0" description="Check spelling while typing"
}
```

Summaries and descriptions are single-line text, limited to 256 and 1024
UTF-8 bytes respectively. The catalog signs the summary and the manifest
digest, which covers the optional dependency descriptions. Publish a new
package version to add this metadata to an existing package and update
clients before publishing packages that use it.

- Manifests and image configuration use KDL 2 and begin with `schema-version 1`. Paths in image configuration resolve relative to the configuration file. It also accepts `architecture "x86_64"` and `facet "doc.man" #false`. Convert old TOML recipes before publishing new versions. Existing JSON manifest objects stay readable under their original digests.
- Set `architecture="x86_64"` on the `package` node for a package that targets one architecture, or pass `--architecture x86_64` to `manifest generate`. Only matching packages participate in dependency resolution and publisher ordering. The signed catalog and manifest must agree, and a source build and its attestation must declare the same target. Omit the package property for portable packages or manifests with per-action architecture selectors. Upgrade clients before publishing targeted packages. Package IDs are unique within a repository, so publishing the same name and version for two architectures requires separate repositories.
- Versions have the numeric form `major.minor.patch-revision`. Inside a package block, declare dependencies as `require "library/libdisplay" version=">=1.0.0-0, <2.0.0-0"` or `incorporate "library/libdisplay" version="=1.0.0-0"`. Use `exclude "library/libdisplay" version=">=2.0.0-0"` to reject a range when the package is present. Omit the version to exclude every version of that package.
- Set `upstream-version="1:1.3.2-4"` on an imported package to retain its original label. The signed manifest preserves the label and `cps info` displays it. The package ID, dependency constraints, and ordering still use the numeric CPS version. Importers must assign increasing CPS versions when upstream ordering depends on epochs, prereleases, or other qualifiers. The package identity fixture covers numeric versions, distro suffixes, VCS snapshots, epochs, and architecture selection.
- Use a `require-any` block with two or more `package` entries when interchangeable providers can satisfy a dependency. Each alternative accepts the same `version` constraints as `require`. The solver prefers a compatible installed provider that the transaction will retain. Offline removal checks that a compatible provider remains installed. [IPS dependency types](https://docs.oracle.com/cd/E53394_01/html/E54820/dependtypes.html)
- `conditional "desktop/plugin" version="=2.0.0-0" when="runtime/desktop" when-version=">=2.0.0-0"` requires the plugin when the resulting image includes a matching runtime. Omit `when-version` to match any installed version of the runtime. The predicate alone leaves the runtime optional.
- New plans use Resolvo to resolve compatible versions and report incompatible requirements. When selected file actions collide, CPS excludes the conflicting version or pair of versions and retries. Directory sharing still requires identical metadata. SAT resolution has a 60-second time limit. Saved plans record their solver version so apply can verify the same selection against the pinned catalog. Plans created before solver versioning retain their original behavior. [Resolvo](https://github.com/prefix-dev/resolvo)
- Actions support `file`, `dir`, `symlink`, and `hardlink`. A symlink specifies `target` in place of `mode`. Use `hardlink "bin/tool" target="usr/bin/tool"` to share the inode of an immutable regular file in the same package. The target is relative to the image root and supplies ownership, mode, and content. The optional `arch` and `facet` selectors match image policy. A hard link's target must be selected whenever the alias is selected. Facets default to enabled and can be set with `image-create --facet doc.man=false`. [IPS hard-link actions](https://docs.oracle.com/cd/E53394_01/html/E54820/pkgterms.html)
- Files marked `preserve=#true` keep local edits through an upgrade, including a package ownership transfer at the same path. When the vendor file changes, find its new copy in `image/.cps/config-updates/<revision>/`.
- The image must be inactive and reside on one filesystem. Existing explicit directory metadata must match either the installed declaration or the selected action; matching installed directories can be updated in place. Unmanaged or locally changed directory metadata must already match the selected action. Transitions between a directory and a file or symlink are rejected. When ownership ends, CPS prunes directories it recorded creating if they are empty and their metadata still matches. It keeps pre-existing directories and directories with local content or extended attributes.

Use `cps plan --root image --output cleanup.json autoremove` to preview
removing unused dependencies. CPS solves from the explicitly installed
packages using their installed versions and the installed dependency
candidates. It keeps the dependencies needed by that solution, including a
compatible alternative and any conditional dependencies that apply. The
plan and apply work offline. Use `plan mark explicit` to keep an installed
dependency before planning cleanup.

### Mediated links

Packages can offer competing implementations of a shared command while
keeping their implementation binaries installed side by side. Mark the
package's normal symlink action with a mediator group.

```kdl
symlink "usr/bin/editor" target="my-editor" uid=0 gid=0
mediate "usr/bin/editor" group="editor" priority=20
```

Only the selected package contributes links for the group. A package can
declare several links for the same group with one priority. CPS selects
the highest priority by default, with package name breaking ties.

```sh
cps mediator --root image
cps plan --root image --output editor.json set-mediator editor --provider editors/basic
cps apply --root image editor.json
```

Omit `--provider` to restore automatic selection. Removing the preferred
package makes the next provider active. The preference survives removal
and becomes active again if the package returns. Image locks capture the
preferences, and staged roots carry them through boot and rollback.

### Virtual packages

A mail transport package can declare its capability in the manifest.

```kdl
provides "cap/mail-transport" version="2.0.0-0"
```

A consumer can require the capability by name.

```kdl
require-capability "cap/mail-transport" version=">=2.0.0-0"
```

Capability names begin with `cap/`. The version on `provides` describes the
capability independently of the package version. A requirement with a version
constraint needs a matching versioned provider. Omit the constraint to accept
any declared version or an unversioned provider.

CPS prefers compatible providers that the transaction already requests or
retains, then compatible installed providers. Plans, profiles, and installed
state record the concrete provider packages.
Multiple providers can coexist if their package constraints and files allow
it. Removing a provider requires another compatible installed provider if a
consumer remains. Install a concrete provider by package name to select it
explicitly. Use `solver="sat-v1"` if a build toolchain depends on a capability,
and upgrade clients before publishing manifests that use these declarations.

### Recommended packages

Use `recommends` in a package manifest for a package that should usually be
installed alongside it.

```kdl
recommends "desktop/editor-help" version=">=2.0.0-0"
```

CPS resolves required packages first, then tries recommendations without
changing the selected versions. It skips unavailable recommendations and
ones that conflict with the selected packages. Newly added recommendations
are tried on upgrade. A previously skipped or removed recommendation stays
omitted unless its declaration changes or another package pulls it in.

Recommended packages are marked automatic unless explicitly requested.
They can be removed without removing the recommending package. Offline
`autoremove` keeps a compatible installed recommendation while its
recommending package is kept. Existing `optional` declarations are advisory.

Pass `--no-install-recommends` to `cps install`, `cps upgrade`, or `cps plan`
to skip this step. Saved plans record the solver revision, so apply uses the
same policy. A signed profile follows its published composition, which may
include recommendations selected at publication. Build recipes can opt into
recommendations with `solver="sat-v2"`. Upgrade clients before publishing
manifests with `recommends` or applying saved plans that use `sat-v2`.

### Filesystem permissions

Modes use four octal digits. Directories support sticky and setgid bits,
including `1777` for temporary directories and `2775` for shared group
directories. Setuid directories and sticky regular files are rejected.
Regular files can declare setuid or setgid modes such as `4755`, `2755`,
and `6755`. These files must be root-owned executables, with no group or
other write access, `preserve` disabled, and no file capabilities.

CPS sets ownership before permissions, checks that the filesystem retained
the requested values, and prepares privileged files privately before atomic
publication. It probes setid file and setgid directory support before
journaling a transaction. This catches Linux silently clearing setgid when
the caller lacks the required group membership or `CAP_FSETID`.
[Linux chmod behavior](https://man7.org/linux/man-pages/man2/chmod.2.html)

Upgrades can change an explicitly managed directory's ownership and modes
while its metadata still matches the installed declaration. CPS changes
that inode in place and keeps its contents. Local metadata changes stop the
upgrade for review; `plan repair` can restore the installed declaration.
Directory changes and privileged file replacements can resume offline from
the journal. Managed hard links follow their target's permissions.

Use CPS `0.1.0-38` or later for package operations once a release uses these
permission bits. Roll out the updated client using compatible metadata before
publishing a release that needs the new bits; earlier clients reject those
manifests. Run
`tests/file-modes-vm.sh` with the QEMU and kernel overrides to verify actual
setuid/setgid execution, sticky deletion rules, group inheritance, failure
without `CAP_FSETID`, interrupted offline recovery, repair, isolated boot
clones, and removal of privileges by an upgrade. `tests/boot-export.sh`
checks the bits in the exported ext4 image.

### Freezing package versions

Keep a package at its installed version until explicitly unfreezing it.
Freeze and unfreeze changes use saved plans and work offline.

```sh
cps plan --root image --output freeze.json freeze --reason "Validate graphics compatibility" library/libdisplay
cps apply --root image freeze.json
cps freeze --root image
cps plan --root image --output unfreeze.json unfreeze library/libdisplay
cps apply --root image unfreeze.json
```

A freeze constrains the complete `major.minor.patch-revision` version during
installs, dependency resolution, and updates. Other packages can still advance
when their dependencies allow it. A conflicting signed profile update fails
with the freeze's reason; it cannot silently lift the constraint. Freezes
follow staged roots and are recorded in transaction history without timestamps.
Images with freezes require image format 9 and a CPS release that supports it.

Removal and dependency cleanup remain allowed. The freeze survives removal and
constrains later reinstallation. Specify `NAME@VERSION` to freeze a package
that is not installed. For an installed package, an explicit version must
match the installed version. Publisher selectors check the selected installed
package, while the stored constraint applies to the package name and version.
This follows [IPS package freezes](https://docs.oracle.com/cd/E26502_01/html/E28984/gilfr.html)
with CPS's complete numeric versions.

Run `tests/package-freezes.sh` for the CLI workflow. It also accepts
`CPS_TEST_BINARY` for a packaged executable and `CPS_TEST_PREVIOUS_BINARY`
to verify that an earlier client rejects the new image format. The signed
profile VM test freezes SQLite in both independently assembled base images,
checks a conflicting update, and verifies the retained freeze on the trial,
default reboot, and rollback boots.

Run `cargo test` for the desktop upgrade and interruption tests.

### Changing facets on an existing image

```sh
cps facet --root image
cps plan --root image --output facets.json change-facet doc.man=true devel=false
cps fetch --root image --plan facets.json
cps apply --root image facets.json
```

`facet` lists explicit overrides. Use `NAME=default` to remove an override and
restore the default enabled selection. A facet change uses the installed
manifests and leaves package versions and catalog checkpoints in place. It
can be staged in a boot environment and resumed like an ordinary plan.
Enabling an action may require its payload from the repository or cache.
Disabling a facet records the identity and fingerprint of any edited or
locally deleted configuration it leaves behind. Re-enabling can reclaim the
matching file, restore a pending vendor proposal, or follow a configuration
rename declared with `original-name`. If a file at the disabled path has
changed, CPS reports a conflict. To install the vendor copy in that case,
move the changed file aside and generate a fresh plan. Images with disabled
configuration records require a CPS version that can read image format 7.
Signed release profiles fix their facet policy.

### Reviewing configuration updates

When package permissions or ownership change on an edited file, CPS
publishes a new inode with the local contents and the package metadata.
It copies extended attributes before applying permissions, so the POSIX
ACL mask follows the new mode. Existing hard-link aliases retain their
original inode. [Linux access control lists](https://man7.org/linux/man-pages/man5/acl.5.html)

To move a preserved file, set `original-name` on the new file action.
Use the original package name and root-relative path and keep that identity
through later moves, including moves between packages of the same publisher.

```kdl
file "etc/canopa/session/preferences.conf" mode="0600" uid=0 gid=0 preserve=#true original-name="desktop/session:etc/canopa/preferences.conf"
```

The transaction must retire the old path and the new path must be unclaimed.
CPS carries edited contents and extended attributes to the new path with
the new package metadata. A locally deleted file stays absent at the new
path. In both cases CPS stages the new defaults there for review. An
untouched file takes the new defaults directly. In a staged upgrade the
previous boot environment retains its configuration at the old path.
Deploy a CPS version that understands `original-name` before using it in
packages offered to existing images. [IPS configuration migration](https://docs.oracle.com/cd/E26502_01/html/E21383/glyuz.html)

When an upgrade keeps an edited or locally removed file and stages new
vendor defaults, `cps config list` shows the pending file and its vendor
copy. Compare the two copies or merge changes by editing the local file.
Then save and apply a review plan on the inactive root.

```sh
cps config list --root staged
cps config diff --root staged /etc/canopa/session/preferences.conf
cps plan --root staged --output config-review.json \
    resolve-config etc/canopa/compositor.conf --take vendor
cps apply --root staged config-review.json
```

`config diff` compares local contents with the authenticated pending vendor
copy using `/usr/bin/diff`. It returns status 1 for differences and treats a
locally removed file as empty.

`--take vendor` replaces the local file with the staged defaults and
restores a file that was locally removed. CPS checks the copy against the
installed manifest and uses it directly, so review works with the
repository offline. Use `--take local` to keep an edit, a merged file, or
a local deletion and dismiss that proposal. A newer vendor change can
queue another proposal. Older vendor copies stay in the archive.

On images made before review tracking, CPS finds vendor copies that match
the current installed defaults. Review can run on a staged boot environment
before export and keeps its package revision and boot lineage.

### Linux file capabilities

A signed file action can grant a Linux executable specific capabilities.

```kdl
file "usr/bin/ping" mode="0755" uid=0 gid=0 capabilities="cap_net_raw=ep"
```

Use comma-separated capability names followed by `=` and the permitted (`p`)
or inheritable (`i`) flags. Separate clauses with spaces when the sets differ.
The effective flag (`e`) applies to the entire file, so include it in every
clause when needed. Use `capabilities="="` for an explicit empty set. The
executable must be root-owned and nonwritable by its group and other users.
Hard links to the file share its capability metadata. [Linux capabilities](https://man7.org/linux/man-pages/man7/capabilities.7.html)

Reading or writing file capabilities requires an unmapped UID namespace.
Apply also needs `CAP_SETFCAP` and checks support in the image's staging
directory before recording the transaction. Verification checks the installed
set, including unexpected capabilities on ordinary files. Build workers can
attest manifests with capabilities, and they materialize their toolchains
without those privileges.

## Offline systemd actions

A manifest can request an operation for a vendor file it delivers. The operation is selected with that file's architecture and facet settings.

```kdl
schema-version 1
package "service/example" publisher="canopa" version="1.0.0-0" {
    file "usr/lib/sysusers.d/example.conf" mode="0644" uid=0 gid=0
    sysusers "example.conf"
    file "usr/lib/tmpfiles.d/example.conf" mode="0644" uid=0 gid=0
    tmpfiles "example.conf"
    file "usr/lib/systemd/system/example.service" mode="0644" uid=0 gid=0
    preset "example.service"
}
```

CPS shows the actions in the plan and invokes the host's systemd tools with the inactive image as their alternate root. It installs the package files first, then provisions accounts, creates state, and applies presets. New or changed sysusers and tmpfiles declarations are processed by name, with target-local replacements and masks taking precedence. For example, a tmpfiles rule of `d /var/lib/example 0750 example example -` can use the account created earlier in the same transaction. Offline tmpfiles supports `d`, `f`, and `L` creation rules. Boot-only rules and paths under `/dev`, `/proc`, `/run`, and `/sys` are deferred to boot. [systemd-tmpfiles](https://www.freedesktop.org/software/systemd/man/latest/systemd-tmpfiles.html)

Presets apply the image's own enablement policy to newly installed units. CPS preserves existing enablement and unit overrides, including masks. The preset step only writes enablement links in the image. [systemd presets](https://www.freedesktop.org/software/systemd/man/latest/systemd.preset.html)

`hwdb "90-device.hwdb"` declares an input to the hardware database.
Deliver the corresponding immutable file under `usr/lib/udev/hwdb.d`
in the same package. CPS queues the rebuild when an active input is added,
changed, or removed. Multiple changed inputs run one
`systemd-hwdb --strict update` against the target root after file writes.
It can run again when an interrupted transaction resumes. The generated
`etc/udev/hwdb.bin` belongs to the system rather than a package.

Run `cargo build` followed by [`tests/systemd-actions.sh`](tests/systemd-actions.sh) to exercise the real helpers against a staged root. The test also checks recovery from a failed tmpfiles step after sysusers has run.

## Boot environments

Boot environments use separate Linux roots and a dedicated systemd-boot EFI System Partition (ESP). Start with an inactive, single-filesystem base root containing the kernel. GNU `cp` clones the image and uses a reflink where supported. The destination must be a new directory.

```sh
cps boot-env export --root base --output base.ext4 --size-mib 2048 --receipt > base.kdl
cps boot-env deploy --disk template.raw --root-image base.ext4 \
    --root-partuuid "$base_partuuid" --output base.raw
cps boot-env activate --known-good --root base --root-partuuid "$base_partuuid" \
    --disk base.raw --receipt base.kdl --esp-partuuid "$esp_partuuid" \
    --output-disk bootable-base.raw
cps plan --root base --output upgrade.json update
cps boot-env stage --source base --root staged upgrade.json
cps boot-env export --root staged --output staged.ext4 --size-mib 2048 --receipt > staged.kdl
cps boot-env deploy --disk bootable-base.raw --root-image staged.ext4 \
    --root-partuuid "$staged_partuuid" --output next.raw
cps boot-env activate --root staged --root-partuuid "$staged_partuuid" \
    --disk next.raw --receipt staged.kdl --esp-partuuid "$esp_partuuid" \
    --output-disk bootable-next.raw
```

`stage` verifies the new root and gives it a fresh image identity. Export
checks the root and its prepared kernel, then writes a new regular file.
Keep the source inactive and stable during export. CPS rejects nested
mounts and special files. The export includes local state and CPS metadata
alongside the package files. It uses `mke2fs` directory import to preserve
ownership, modes, links, and extended attributes. It runs `e2fsck` read-only
before publishing the image and refuses to replace an existing path. Use
e2fsprogs with support for `root_perms`. Filesystem timestamps and formatter
defaults affect the image bytes. [mke2fs manual](https://man7.org/linux/man-pages/man8/mke2fs.8.html)

Set `SOURCE_DATE_EPOCH` to export a byte-identical ext4 payload and receipt
from the same stable source tree, partition size, and e2fsprogs build. CPS
uses its own formatter profile and the image UUID as the directory hash
seed. The formatter clamps imported timestamps to the epoch and uses it
for filesystem creation. Keep source contents and metadata stable between
exports, including access times older than the epoch. A read-only source
mount preserves those access times. Use a decimal epoch from 0 to
4294967295 and e2fsprogs with `root_perms` and `SOURCE_DATE_EPOCH` support.
[e2fsprogs release notes](https://github.com/tytso/e2fsprogs/blob/v1.47.1/doc/RelNotes/v1.47.1.txt)

For an offline VM or disk image, provision a raw GPT template with 512-byte
sectors, one Linux filesystem partition per root, and a FAT EFI System
Partition containing systemd-boot. `deploy` uses GNU
`cp`, util-linux `sfdisk`, and `e2fsck`. It clones the source disk, checks
the GPT copies and partition ranges, and writes the payload to the selected
partition. The payload must match the partition size exactly. CPS checks
the deployed bytes by SHA-256 before publishing a new disk file. Keep the
inputs offline during the operation. The source disk is read-only, and the
new disk retains the previous contents outside the selected partition.
[sfdisk manual](https://man7.org/linux/man-pages/man8/sfdisk.8.html)

The ext4 filesystem UUID identifies the CPS image, and the label records
its revision in hexadecimal. Use the separate
**GPT partition UUID** for both `deploy` and `activate`. Keep the KDL export
receipt on the trusted host, outside the image. With `--disk` and
`--receipt`, activation checks the receipt against the root's identity,
revision, and CPS state. It checks the bytes in the named GPT partition
against the export's SHA-256 and uses `blkid` to confirm the ext4 UUID and
revision label. For a trial, it checks those two identifiers on the
registered fallback partition of the same disk. Keep the disk offline
through activation. The receipt describes the freshly deployed root,
whose bytes can change after boot.
For a detached ESP directory, activation accepts the operator's partition
mapping when both flags are omitted. Activation on a mounted VFAT ESP
requires `activate-device`.
[blkid manual](https://man7.org/linux/man-pages/man8/blkid.8.html)

### Provisioning the first root on an installed disk

Start with an inactive CPS base image and a dedicated GPT disk with
512-byte logical sectors. Mount its writable VFAT ESP with systemd-boot
already installed and no boot entries registered. Choose an unmounted
Linux filesystem partition for the base. Set the export size to match that
partition exactly.

```sh
cps boot-env export --root /scratch/base --output /scratch/base.ext4 \
    --size-mib 2048 --receipt > /scratch/base.kdl
cps boot-env activate-device --known-good --root /scratch/base --esp /efi \
    --device "/dev/disk/by-partuuid/$base_partuuid" \
    --root-partuuid "$base_partuuid" \
    --receipt /scratch/base.kdl --root-image /scratch/base.ext4
```

The command writes the selected partition and verifies it before publishing
the first entry. `--known-good` makes that entry the initial default without
a trial counter. If the root was already written, omit `--root-image` to
verify its bytes against the receipt and finish registration.

### Capturing a running Btrfs root

`boot-env capture` takes a read-only snapshot of a CPS root subvolume and
verifies it before publishing a new directory containing `root/`.
Use a writable sibling subvolume on the same Btrfs filesystem for captures,
mounted separately from the root subvolume.

```sh
cps boot-env capture --source / --output /var/lib/cps/captures/before-upgrade
cps plan --root /var/lib/cps/captures/before-upgrade/root \
    --output /scratch/upgrade.json update
cps fetch --root /var/lib/cps/captures/before-upgrade/root --plan /scratch/upgrade.json
cps boot-env prepare --source /var/lib/cps/captures/before-upgrade/root \
    --output /scratch/upgrade --size-mib 2048 /scratch/upgrade.json
```

The capture retains the image identity and revision, so a saved plan for
that revision also applies to the captured baseline. Snapshot creation
holds the shared CPS image lock. Btrfs supplies a point-in-time view of the
root subvolume while applications continue to write to the running root.
Shared data mounted from other filesystems has its own lifecycle.
CPS rejects nested subvolume stubs in the capture and verifies the package
database, installed files, and catalog archives.
[Btrfs subvolumes](https://btrfs.readthedocs.io/en/latest/Subvolumes.html)

Run with privileges for Btrfs snapshot creation and deletion. A failed
validation removes the private snapshot. An abrupt interruption can leave
a `.cps-capture-*` directory containing `root/` in the capture parent.
To remove a capture, run `btrfs subvolume delete CAPTURE/root`, followed
by `rmdir CAPTURE`.

Use `stage-btrfs` to apply a saved plan in a writable snapshot of the
read-only baseline on the same Btrfs filesystem.

```sh
cps boot-env stage-btrfs --source /var/lib/cps/captures/before-upgrade/root \
    --output /var/lib/cps/captures/trial /scratch/upgrade.json
```

The command verifies the baseline and its plan, applies the transaction in
a private subvolume, prepares the selected kernel, and assigns a fresh CPS
image identity with its recorded parent. It publishes `trial/root/` after
verification. A failed transaction deletes the private subvolume. The new
root is writable for configuration review and boot.

Both the captured baseline and a staged Btrfs root can be exported as ext4.

Run `tests/boot-capture-vm.sh` with `CPS_VM_BTRFS` and
`CPS_VM_MKFS_BTRFS` set to trusted Btrfs utilities, QEMU, and a kernel
with built-in Btrfs and ext4 support. The test captures its running root,
rejects nested subvolumes, verifies that later source writes leave the
read-only capture intact, and checks failed and concurrent staging. It boots
the staged Btrfs subvolume, the previous root, and the exported ext4 root.
It uses the same `CPS_BIN`, `CPS_VM_QEMU`, `CPS_VM_QEMU_DATA`,
`CPS_VM_KERNEL`, and `CPS_VM_BUSYBOX` overrides as the boot fixture.
For native boot with a custom kernel, set `CPS_VM_KERNEL_RELEASE` to
the guest kernel's release. Set `CPS_VM_FAT_BUILTIN=1` if the guest kernel
has built-in VFAT and NLS support for the EFI partition.

### Activating Btrfs roots

Use a single-device Btrfs filesystem directly on a GPT Linux filesystem
partition, with the writable VFAT ESP on the same disk. Mount the Btrfs
top-level subvolume (ID 5) read-write so CPS can verify and register named
roots beneath it.
The commands use the disk's 512-byte logical sector layout.

```sh
mount -t btrfs -o subvolid=5 "/dev/disk/by-partuuid/$root_partuuid" /mnt/cps-volume
cps boot-env activate-btrfs --known-good --root /mnt/cps-volume/@ \
    --btrfs-volume /mnt/cps-volume --esp /efi --root-partuuid "$root_partuuid"
cps boot-env activate-btrfs --root /mnt/cps-volume/@captures/trial/root \
    --btrfs-volume /mnt/cps-volume --esp /efi --root-partuuid "$root_partuuid"
cps boot-env list --esp /efi
cps boot-env rollback --esp /efi --btrfs-volume /mnt/cps-volume
```

Register the original known-good root before activating its staged upgrade.
Bootstrap can read the running root through its top-level path. Each entry
records the filesystem UUID, subvolume UUID and ID, and root path. CPS
compares the mounted filesystem's device with the GPT root partition and
ESP, checks the CPS image identity and revision, and verifies the fallback
root and its boot artifacts. The kernel receives the recorded subvolume ID
through `rootflags`. Keep registered paths stable so rollback can check
their identities and installed contents.
[Btrfs ioctl documentation](https://btrfs.readthedocs.io/en/stable/btrfs-ioctl.html)

Native activation marks the root with the `user.cps.boot-environment`
extended attribute before publishing its boot entry. CPS checks that mark
under the image's exclusive lock before applying package transactions.
`stage-btrfs` clears the inherited mark on its private writable snapshot.
Activation retries accept the matching registration. If activation fails
after marking the root, rerun it to finish registration.

The package's health service and systemd blessing apply to Btrfs trials.
Set `CPS_VM_BTRFS_BOOT=1` on `tests/boot-capture-vm.sh`, provide
`CPS_VM_OVMF_CODE`, `CPS_VM_OVMF_VARS`, and a trusted
`CPS_VM_SYSTEMD_BOOT` EFI binary, and use the installed systemd runtime.
This mode tests activation, a transaction waiting while registration
completes, blessing, the trial's default on reboot, rejection of altered
subvolume metadata, and rollback to the previous root. It also fills the
EFI partition, checks that a failed activation keeps the known-good boot
selection, and retries activation after freeing space.
Add `CPS_VM_BTRFS_HEALTH_FAIL=1` to change a managed file's metadata at
trial boot and verify that two unblessed attempts fall back to the original
subvolume.

Distributions can extend the blessing gate with a systemd drop-in for
`cps-boot-health.service`. For example, install the following at
`/etc/systemd/system/cps-boot-health.service.d/readiness.conf` in the image.

```ini
[Unit]
Wants=canopa-app.service
After=canopa-app.service

[Service]
ExecStartPost=/usr/local/libexec/canopa-check-readiness
TimeoutStartSec=90s
```

The readiness command runs after package verification. Give it a bounded
probe of the service itself, and return nonzero if the service cannot
serve requests. Systemd treats a failing post-start command as a failed
health service, so the trial stays unblessed. The VM fixture's
`CPS_VM_BTRFS_HEALTH_FAIL=readiness` mode exercises a healthy package image
whose readiness probe fails on both trial boots.

Boot rollback selects the previous root and its package state. Data mounted
from a sibling subvolume, another device, or a network service has its own
lifecycle. Keep shared state compatible with the previous system until the
trial is accepted, or coordinate its backup and restore with the application.

Run `tests/boot-profile-btrfs-vm.sh` to test a native Btrfs root assembled
entirely through a signed release profile. Set `CPS_VM_CPS_FIXTURE`,
`CPS_VM_KERNEL_FIXTURE`, `CPS_VM_SYSTEMD_FIXTURE`,
`CPS_VM_USR_BASE_FIXTURE`, `CPS_VM_UTIL_LINUX_FIXTURE`, and
`CPS_VM_BUSYBOX_FIXTURE` to the corresponding build outputs. Also set
`CPS_VM_NEXT_KERNEL_FIXTURE` to a distinct signed kernel build and
`CPS_VM_UTILITIES_FIXTURE` to the GNU tools build below, and provide
the QEMU, OVMF, and trusted `CPS_VM_MKFS_BTRFS` overrides. The test installs
the source-built CPS, systemd, util-linux, BusyBox, and Btrfs-capable kernel
with builder trust enforced by the signed profile. The ELF runtime comes
from the signed merged `/usr` seed. It keeps signing keys outside the guest
and mounts its repositories read-only. It captures the running root,
stages an unfreeze, applies the profile and kernel upgrade to that private root, boots
and blesses it, checks the default on reboot, and explicitly rolls back to
the previous root with its original freeze and kernel. It imports both
verified kernel bundles under a temporary release key and pins their builder
keys in the signed profile. The staged guest runs its packaged kernel-install
and GNU utilities to prepare fresh artifacts. The test checks distinct kernel
bytes, prepared artifacts, and the running release on both trial boots and
after rollback.

Set `CPS_VM_KERNEL_FIXTURE` to the output from `tests/build-kernel.sh`
to stage the signed Btrfs-capable kernel as part of the upgrade. The image
requires its builder signature, audits the installed evidence after boot,
and checks the running kernel in both the trial and the previous root.

Set `CPS_VM_SYSTEM_COMMANDS=1` with `CPS_VM_BTRFS_BOOT=1` in
`tests/boot-capture-vm.sh` to exercise the everyday commands. The guest
installs a package live, upgrades its running service, boots a staged
upgrade, and rolls back to the system with the live package still installed.
It then removes the package live and boots a staged removal of its unused
dependency with the repository unavailable.

### Reclaiming old Btrfs environments

Retire an obsolete registered Btrfs root after its replacement has booted
successfully. Use the host's mount namespace and unmount the old root first.

```sh
cps boot-env list --esp /efi
cps boot-env destroy-btrfs --esp /efi --btrfs-volume /mnt/cps-volume --dry-run "$old_id"
cps boot-env destroy-btrfs --esp /efi --btrfs-volume /mnt/cps-volume "$old_id"
```

The command keeps the selected known-good environment, its recorded fallback,
the running environment's recorded fallback, and the loader's configured default.
It rejects the running root, mounted roots,
roots with nested subvolumes or mounts, and trials with attempts left. It checks
the registered filesystem and subvolume identity, then deletes by subvolume ID.
Only that subvolume and its private CPS boot artifacts are removed. Other
captures and shared subvolumes keep their own contents and lifecycle.

CPS first renames the boot entry to a durable `.cps-destroy` retirement record,
then commits subvolume deletion before removing boot artifacts and the record.
`boot-env list` shows interrupted cleanup as `retiring`. Retry the same command
to finish; activation of that identity stays blocked until cleanup completes.
Keep the recorded root path stable during this operation.

Btrfs refuses to delete its filesystem default subvolume. If it is the obsolete
root, choose another filesystem default, such as top-level ID 5 with
`btrfs subvolume set-default 5 /mnt/cps-volume`, then retry. CPS boot entries
select their subvolume explicitly. Storage reclamation continues in the
background after the deletion transaction commits.
[Btrfs subvolume deletion](https://btrfs.readthedocs.io/en/latest/btrfs-subvolume.html)

Set `CPS_VM_BTRFS_DESTROY=1` for the native VM test to stage and bless another
root, test the destruction guards, recover from the kernel's default-subvolume
refusal and an injected artifact-cleanup failure, and boot the retained fallback
after cleanup. This fixture uses a native C compiler for its failure-injection
helper. Combine
`CPS_VM_BTRFS_HEALTH_FAIL=1` and `CPS_VM_BTRFS_DESTROY_FAILED=1` to verify
that an exhausted trial with damaged managed files can be discarded.

### Activating a trial on an installed disk

You can plan and stage directly from the installed root during a rescue boot.
After a clean shutdown, mount that ext4 partition read-only and make the
image's configured repositories or synced caches available at their recorded
paths. Use writable scratch space outside the mounted root for the plan,
staged tree, and export. CPS holds a shared image lock while copying and
rejects nested mounts, including bind mounts.

`apply` requires an inactive working image. This command rejects transactions
against the running root and against a mounted ext4 root with a CPS revision
label, including during a rescue boot. Ext4 root labels consisting of exactly
16 lowercase hexadecimal digits are reserved for these deployments.

```sh
mount -t ext4 -o ro,noload "/dev/disk/by-partuuid/$base_partuuid" /mnt/canopa
cps plan --root /mnt/canopa --output /scratch/upgrade.json update
cps fetch --root /mnt/canopa --plan /scratch/upgrade.json
cps boot-env prepare --source /mnt/canopa --output /scratch/upgrade \
    --size-mib 2048 /scratch/upgrade.json
```

Use `ro,noload` on a clean ext4 filesystem to avoid journal writes to the
source partition. [Linux ext4 mount options](https://docs.kernel.org/admin-guide/ext4.html)

`prepare` creates a new directory containing `root/`, `root.ext4`, and
`receipt.kdl`. It uses cached plan payloads and the same checks as `stage`
and `export`, including the selected kernel and `SOURCE_DATE_EPOCH`
settings. CPS publishes the directory only after formatting and verifying
the image and writing its receipt. A formatting or verification failure
leaves the requested output path available for retry. An abrupt interruption
can leave a private `.cps-prepare-*` directory in the output parent.

New export receipts include a SHA-256 fingerprint of the source tree. CPS
checks the tree before and after formatting and checks it again during
activation. The fingerprint covers paths, file contents, modes, owners,
symbolic links, hard links within the root, and extended attributes. It is
independent of inode numbers and timestamps. Older receipts still verify
the package database and exported partition bytes.

Use separate `stage` and `export` commands when you need to edit the staged
root or resolve configuration updates before exporting it.

A private staged environment can receive several saved transactions before
activation. For example, stage an `unfreeze` plan, then plan and apply the
package update against that new root. Its image identity stays the same and
its recorded boot parent remains the original known-good source. Export after
the last transaction and use the fresh receipt when activating it. Receipts
from an earlier revision remain invalid. Registered Btrfs roots and installed
ext4 roots retain their package-write protection.

Use the current CPS for boot operations after activating an environment with
several transactions. Earlier clients reject its parent metadata. Set
`CPS_VM_STAGED_TRANSACTIONS=1` on the native Btrfs VM test to freeze, unfreeze,
and update a private root before its first activation. The signed-profile
rollback VM uses that option to apply a review transaction after staging,
reject the old receipt, and verify the reviewed root through blessing,
reboot, and rollback.

With a known-good base already registered, use a dedicated, unregistered
Linux filesystem partition on the same GPT disk as the mounted ESP for the
trial. The disk must use 512-byte logical sectors, and the ext4 export must
match the partition size.

```sh
cps boot-env activate-device --root /scratch/upgrade/root --esp /efi \
    --device "/dev/disk/by-partuuid/$trial_partuuid" \
    --root-partuuid "$trial_partuuid" \
    --receipt /scratch/upgrade/receipt.kdl --root-image /scratch/upgrade/root.ext4
```

CPS checks the GPT and compares the kernel's device extents to the requested
root PARTUUID and the writable VFAT mount. It opens the target exclusively,
checks the registered fallback, writes the export, and verifies the target
bytes and filesystem identity before publishing the counted boot entry.
Run with an account that can open the target and parent block devices and
write the ESP. Keep the root PARTUUIDs unique across attached disks.
[Linux block device open semantics](https://man7.org/linux/man-pages/man2/open.2.html)

Omit `--root-image` to verify an already deployed trial against its export
receipt and finish activation. Use the mounted ESP with `boot-env list`
and `boot-env rollback` as usual.

After a successful upgrade, you can reuse the older slot by passing
`--replace "$old_environment_id"` with `--root-image` and the older root's
device and PARTUUID. CPS verifies the new export, durably retires the old
boot entry and artifacts, and writes the root while the selected known-good
environment stays available. A mounted slot or a trial with attempts left
cannot be replaced.

If writing the slot is interrupted, rerun the replacement command with the
same `--replace` ID. CPS records that ID on the ESP until it has verified
the new root. If CPS reports that the replacement ID is unknown, use the
original command without `--replace` to finish deployment into the now
unregistered slot. If the new environment is already registered, omit
`--root-image` and `--replace` to verify it and finish activation.

Activation copies boot files from the selected root and publishes its Boot
Loader Specification entry last. New entries record SHA-256 digests of the
kernel and initramfs in comments. CPS checks those bytes before using an
entry as an activation fallback or rollback target. Older entries retain
the path and file checks available from their original metadata.
With manually supplied boot files the kernel path defaults to
`boot/vmlinuz`. Use `--initrd` if the kernel needs
one to mount the root and `--options` for additional kernel arguments.

With `--esp-partuuid` and `--output-disk`, CPS edits a private copy of the
selected EFI partition on the same verified disk. It checks the FAT
filesystem, verifies the written files, and publishes a new disk image
with the remaining partitions intact. Boot the output disk. This mode
uses mtools and dosfstools and supports ESPs up to 1 GiB. An unprivileged
caller can set `CPS_ESP_TOOLS_DIR` to the absolute path of a trusted tool
directory. Use `activate --esp` for a detached ESP directory and
`activate-device --esp` for a mounted ESP. [fsck.fat manual](https://man7.org/linux/man-pages/man8/fsck.fat.8.html)

For repeatable offline activation and rollback, set `SOURCE_DATE_EPOCH`.
CPS sets new and updated FAT entries to that time in UTC, rounds down to
two-second precision, and uses January 1, 1980 for earlier epochs. It
reads back the timestamps and contents of written files before publishing.
Use the same source disk bytes, root export, command options, and mtools
build to reproduce the output disk. [GNU mtools release history](https://github.com/vapier/mtools/blob/main/NEWS)

For FAT32, CPS can clear a leftover dirty flag in its private copy
when the primary and backup boot sectors otherwise agree. The copy still
has to pass the read-only FAT check.

`--known-good` bootstraps the first entry. Later activations require the previous environment's known-good entry and boot files. A trial gets two attempts by default. Install [the `system/cps` package](packaging/cps.kdl) in the base image to make its health check part of `boot-complete.target`. The package ships the CPS binary, [the health unit](packaging/systemd/cps-boot-health.service), and the required dependency link. The unit runs `cps verify --root /` after `multi-user.target`. Systemd blesses the entry when boot completion succeeds, removing the boot counter. If the attempts run out, systemd-boot selects the previous environment. The managed ESP uses its own loader configuration and assumes no EFI default override. [systemd boot assessment](https://systemd.io/AUTOMATIC_BOOT_ASSESSMENT/)

### Kernel packages

An image can select one kernel from its package manifests. The kernel release identifies the `usr/lib/modules` directory and the packaged kernel image. An optional prebuilt initrd goes in the same directory.

```kdl
schema-version 1
package "system/kernel" publisher="canopa" version="7.2.11-1" {
    file "usr/lib/modules/7.2.11-arch1-1/vmlinuz" mode="0644" uid=0 gid=0
    file "usr/lib/modules/7.2.11-arch1-1/initrd" mode="0644" uid=0 gid=0
    kernel-install "7.2.11-arch1-1" initrd=#true
}
```

Installing or changing the selected kernel runs the host's `kernel-install add` with systemd's Type #1 BLS plugin and a private boot tree inside the image. CPS verifies the resulting kernel and initrd against the signed manifest. A staged upgrade prepares its files before publishing the new root. Later, `boot-env activate` takes the prepared files from that root and publishes them alongside its boot entry. Omit `--kernel` and `--initrd` for manifest-managed kernels. Images with manually supplied boot files can still use the options above. [systemd kernel-install](https://www.freedesktop.org/software/systemd/man/260/kernel-install.html)

### Listing and rolling back

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

The list marks the default configured on the ESP and shows each registered root's partition UUID, title, and boot status. The title includes the Linux release for manifest-managed kernels. Rollback selects the default environment's recorded parent unless an ID is supplied. Entries from earlier CPS versions use the previous known-good revision. Rollback checks the selected entry's boot files and updates `loader.conf` for the next boot. It keeps both environments available. Trials started from a pinned environment use the `preferred` setting in systemd-boot 262, with the pinned base as the fallback. [systemd-boot selection](https://github.com/systemd/systemd/blob/v262/src/boot/boot.c#L1717)

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

### Reusing a root slot

After the trial becomes known-good, stage the next release from that
environment. On a two-slot disk, use `boot-env list` to identify the older
entry and its partition UUID. Deploy and activate in one command.

```sh
cps plan --root staged --output next-upgrade.json update
cps boot-env stage --source staged --root next-staged next-upgrade.json
cps boot-env export --root next-staged --output next-staged.ext4 \
    --size-mib 2048 --receipt > next-staged.kdl
cps boot-env activate --root next-staged --root-image next-staged.ext4 \
    --disk bootable-next.raw --receipt next-staged.kdl \
    --root-partuuid "$base_partuuid" --replace "$base_environment_id" \
    --esp-partuuid "$esp_partuuid" --output-disk next-release.raw
```

CPS checks that the entry being replaced owns the slot and that the
selected known-good environment is the staged root's parent. An inactive
known-good entry or an exhausted trial can be replaced. CPS verifies the
payload against its receipt, retires the old entry and its boot files,
and publishes the new disk with the selected root as the trial's fallback.
The source disk retains its original roots and entries. Omit `--replace`
to use the same transaction for an unregistered slot.

### VM boot tests

The [VM test](tests/systemd-boot-vm.sh) deploys two CPS exports to a GPT disk
and activates them in its FAT ESP, then boots through OVMF and systemd-boot
under QEMU.
The `fallback` scenario checks that staging leaves the ESP alone and that the
previous root boots after two failed attempts. The `bless` scenario runs
systemd as PID 1, installs the health gate through the CPS package, and checks
that systemd removes the counter and boots the blessed root again. The
`health-fail` scenario corrupts a managed file in the exported partition and
checks that the health gate blocks blessing through both attempts.
`catalog-health-fail` uses `sqlite3` and `debugfs` to corrupt the archived
catalog in the trial partition after activation. It checks for the catalog
digest error and verifies that systemd-boot falls back after both attempts. The
`kernel-upgrade` scenario checks the kernel and initrd on the ESP and reports
`uname -r` from both roots.
The `facet-change` scenario stages documentation from the installed manifest
and preserves edited configuration while disabling its facet. It checks the
blessed trial after reboot and rolls back to the original facet selection.

Build with `cargo build`. Run [the export test](tests/boot-export.sh) with
e2fsprogs, OpenSSL, and `setfattr` to check the payload and failure cleanup.
[The deployment test](tests/boot-deploy.sh) also uses `sfdisk` and `blkid`
to check partition isolation and reject damaged or ambiguous layouts,
stale receipts, modified payloads, and missing fallback roots.
Run it with `--esp` and mtools and dosfstools installed to exercise raw-disk
activation and rollback, including preservation of unrelated EFI files.
The VM test additionally needs `qemu-system-x86_64`, `mkfs.fat`, mtools,
systemd-boot, and matching non-Secure-Boot x86-64 OVMF code and
variables files. Supply an EFI-stub Linux kernel with built-in virtio block,
GPT, ext4, and serial console support, plus a compatible BusyBox with `sh`,
`cat`, `reboot`, and `poweroff`.

```sh
CPS_VM_OVMF_CODE=/path/to/OVMF_CODE.fd \
CPS_VM_OVMF_VARS=/path/to/OVMF_VARS.fd \
CPS_VM_KERNEL=/path/to/vmlinuz \
CPS_VM_BUSYBOX=/path/to/busybox \
tests/systemd-boot-vm.sh bless
```

The script defaults to Arch Linux paths for the kernel, BusyBox, and systemd-boot. Set `CPS_VM_SYSTEMD_BOOT` if the EFI binary is elsewhere. It uses regular files and software emulation.

The fixture assembles each root from signed packages and runs `verify --pristine`
before packing it into the VM disk. Its runtime package imports prebuilt host
binaries, libraries, and VFAT and ISO-8859-1 modules into a prototype, then
generates the manifest and publishes it through CPS. The blessing fixture needs
the local systemd userspace, util-linux, binutils `readelf` and `strip`, kmod, and kernel
support for EFI variables and FAT. Set `CPS_VM_KERNEL_RELEASE` for another
installed kernel, or `CPS_VM_FAT_BUILTIN=1` if VFAT and its character sets are
built in.

For `kernel-upgrade`, provide the original kernel as `CPS_VM_KERNEL` with `CPS_VM_KERNEL_RELEASE`, and the upgrade as `CPS_VM_TRIAL_KERNEL` with `CPS_VM_TRIAL_KERNEL_RELEASE`. Both releases must be distinct, and both kernels need the built-in storage and console support listed above. This scenario also uses `cpio` and the host's `kernel-install` and `90-loaderentry.install` plugin.

The `capabilities` scenario also requires a C compiler with static libc,
`setpriv`, `getcap`, and `setcap`. It tests a signed capability-bearing
package inside the disposable guest, including failed preflight without
`CAP_SETFCAP`, raw socket access as an unprivileged user, repair, cloning,
and capability removal during an upgrade.

The `rollback` scenario adds another signed publisher after activating the base
environment. The upgrade installs a dependency from that publisher and removes
an obsolete dependency. The staged root rejects a replay of the base
catalog while the base root still accepts that catalog. After systemd
blesses the trial and reboots it as the default, the test runs
`boot-env list` and `boot-env rollback` against the disk.
It boots the resulting disk and checks that the previous kernel and retired
dependency are present and that the added package is absent. Both kernels need
VFAT support. To load modules from extracted kernel packages, set
`CPS_VM_KERNEL_MODULES_ROOT` and `CPS_VM_TRIAL_MODULES_ROOT` to the roots
containing their indexed `lib/modules` trees.

Set `CPS_VM_RECYCLE_SLOT=1` with `rollback` to stage and bless a second
upgrade on the original root slot, then roll it back to the first upgrade.
The scenario accepts a vendor copy for one setting and keeps another
administrator edit with tighter permissions. It checks both settings in
the guest across rollback.

Set `CPS_VM_RELEASE_PROFILE=1` in the rollback test to assemble the roots from
a signed release profile. Use a current CPS build on the host and provide the
signed CPS and BusyBox fixtures. The test installs the signed base
composition, publishes the companion update, and checks that the profile
still selects the old kernel. It advances the profile through the automatic
upgrade check before staging and booting the trial. In the local repository
scenario with slot recycling disabled, the test also moves the primary and
companion repositories together and checks that the previous saved plan is
stale.

Build the disposable mirror signer with
`cargo build --locked --example tuf-fixture`. With the same signed profile
fixtures, set `CPS_VM_TUF_MIRRORS=1` for the rollback scenario. The test signs
separate local TUF mirrors with throwaway keys, syncs the base profile
index-only, and fetches its planned payloads. It advances a companion while
the primary still pins the previous catalog, and checks that pruning keeps
the previous kernel after its mirror drops the pin. It then repairs a copy
of the base root from a signed archive after deleting that cached kernel.
It also restores a missing catalog from the archive and replays the saved
profile installation against the original cache paths.
The test rejects a replayed TUF timestamp and verifies that missing or corrupt
payloads block promotion with `--profile-payloads`. After a successful retry,
it takes the mirrors offline and prepares the upgrade in one command. It
deploys that bundle and boots through blessing and rollback.
This scenario runs without slot reuse.

With the signed kernel fixture, set `CPS_VM_DEVICE_ACTIVATION=1` to bootstrap
the base and activate the trial from a rescue VM with a writable virtio disk.
The test checks rejection of unchecked ESP writes, mounted or registered root
targets, and incorrect partition or receipt identities. It then verifies the
written partitions and boots the result through the blessing and rollback
checks. Rollback runs
against the mounted ESP in the rescue VM and rejects a mismatched root
before selecting and booting the original kernel.
Combine it with `CPS_VM_RECYCLE_SLOT=1` to also exercise direct replacement,
recovery from a partial write after retirement, and boot and rollback of
the recovered upgrade.
With those options and `CPS_VM_RELEASE_PROFILE=1`, set
`CPS_VM_INSTALLED_STAGE=1` to plan and stage the second upgrade inside the
rescue VM from the first upgrade's read-only root partition. This variant
checks that state written during the first boot reaches the staged tree and
that the fallback partition stays byte-for-byte intact during deployment. It
also checks that `apply` rejects a writable copy of the deployed root.

With `CPS_VM_RELEASE_PROFILE=1`, offline disk activation, and
`SOURCE_DATE_EPOCH` set, use `CPS_VM_REPROVISION=1` to install the signed
profile in two fresh roots and compare the saved plans, ext4 exports, KDL
receipts, and full GPT disks before booting the upgrade scenario.

#### Testing a source-built Linux kernel

On the x86-64 Arch Linux build worker, provide GCC, binutils, libelf headers,
`bc`, Bison, Flex, M4, XZ, and the usual native build tools. Verify the Linux 6.12.115
release signature and place `linux-6.12.115.tar.xz` in a local source directory.
[Linux release signatures](https://www.kernel.org/signature.html)

```sh
CPS_KERNEL_SOURCE_DIR=/path/to/verified-sources tests/build-kernel.sh target/kernel-fixture
```

The recipe pins the archive, VM configuration, and signed worker toolchain.
CPS builds the EFI kernel twice in fresh offline sandboxes, verifies the matching
outputs, and publishes `system/kernel-qemu` with signed build evidence. Set
`CPS_KERNEL_BC` to an extracted trusted `bc` binary if the worker does not have
it installed. The worker toolchain still comes from prebuilt Arch packages.

Set `CPS_KERNEL_VARIANT=next` with a new output directory to build package
revision `6.12.115-2`. That variant uses release `6.12.115-cps-vm-next` and
`CONFIG_HZ_1000=y`; the base variant uses `6.12.115-cps-vm`. Both use the same
verified upstream archive, and each has its own pinned manifest and configuration.
Use the base and next outputs as `CPS_VM_KERNEL_FIXTURE` and
`CPS_VM_NEXT_KERNEL_FIXTURE` for the signed native Btrfs test.

For the signed-profile rollback test, also set `CPS_VM_KERNEL_FIXTURE` to the
build output directory. The signed trial installs the kernel and its build
configuration directly from the fixture repository. This kernel has built-in
virtio, ext4, Btrfs, and VFAT support and boots the QEMU test root without an initrd.
The previous root keeps its original kernel and initrd.

#### Testing source-built systemd and systemd-boot

On the x86-64 Arch worker, provide Meson, Ninja, Python, Jinja2, pyelftools,
gperf, and patchelf. The standalone fixture also uses the worker's libmount
and libblkid headers. Place the
`v262.1` source archive in the source directory as `systemd-262.1.tar.gz`.
[systemd v262.1 release](https://github.com/systemd/systemd/releases/tag/v262.1)

```sh
CPS_SYSTEMD_SOURCE_DIR=/path/to/verified-sources \
CPS_SYSTEMD_CPS_FIXTURE=target/cps-fixture \
tests/build-systemd.sh target/systemd-fixture
```

The fixture pins the archive and the signed worker toolchain. It builds the
VM's systemd components twice offline and publishes the matching outputs with
signed build evidence. The standalone build publishes systemd revision
`262.1.0-3` and a separate seed for the worker's prebuilt libmount and
libblkid. Set `CPS_SYSTEMD_TOOLS_ROOT` to a trusted
extracted Arch package tree if gperf, patchelf, Jinja2, and pyelftools are
available there instead of on the worker.

Set `CPS_VM_SYSTEMD_FIXTURE` to the build output for signed-profile rollback.
The test installs systemd from the signed repository, copies the installed EFI
loader to the ESP, and checks the packaged PID 1 on each boot. Combine this
with `CPS_VM_KERNEL_FIXTURE` to test the source-built trial kernel. The signed
util-linux and BusyBox fixtures provide the guest's mount and module-loading
commands. Offline staging uses the host's systemd helpers.

#### Testing a merged `/usr` image

Build the merged base from the CPS fixture's signed runtime catalog and build
BusyBox for canonical installation under `/usr/bin`.

```sh
tests/build-usr-base.sh target/usr-base-fixture target/cps-fixture
CPS_BUSYBOX_MERGED_USR=1 tests/build-busybox.sh target/busybox-usr-fixture
```

The signed `system/filesystem` package owns the compatibility links. The ELF
runtime package places the x86-64 interpreter under `/usr/lib`. Applications
can still start through `/lib64`, and scripts can use `/bin/sh`. Package file
actions use canonical paths. CPS rejects file actions beneath compatibility
links and can verify their exact targets with `verify --pristine`.
[systemd filesystem hierarchy requirements](https://systemd.io/SYSTEMD_FILE_HIERARCHY_REQUIREMENTS/)

Set `CPS_VM_USR_BASE_FIXTURE` to the merged base output alongside the signed
systemd fixture, and point the BusyBox repository and key at the merged
BusyBox output. Both roots are provisioned with the layout before booting.
The test checks the aliases, shell, and interpreter on the trial and rollback
boots and fails if systemd reports an unmerged `/usr`.

#### Building util-linux for the boot image

On the x86-64 Arch worker, verify the util-linux 2.42.4 release signature and
place `util-linux-2.42.4.tar.xz` in a local source directory.
[util-linux release archive and signature](https://www.kernel.org/pub/linux/utils/util-linux/v2.42/)

```sh
CPS_UTIL_LINUX_SOURCE_DIR=/path/to/verified-sources \
tests/build-util-linux.sh target/util-linux-fixture target/usr-base-fixture
```

The fixture builds `mount`, `umount`, `blkid`, `sfdisk`, and their libraries twice
offline. It publishes the matching outputs with signed build evidence, then
installs the package into the signed merged base and runs the tools with its
loader. The package also delivers the headers and linker inputs under the
`devel` facet.

Set `CPS_SYSTEMD_UTIL_LINUX_FIXTURE=target/util-linux-fixture` when running
`tests/build-systemd.sh`. Systemd package revision `262.1.0-5` uses util-linux
`2.42.4-1`, including the GPT tools needed for native boot activation. The
earlier systemd revision `262.1.0-4` uses util-linux `2.42.4-0`. Both use the
util-linux, libseccomp, and ELF runtime packages directly from their pinned
publisher catalogs. The libseccomp headers, pkg-config metadata, and library
come from the same signed package. Set `CPS_SYSTEMD_LIBSECCOMP_VERSION=2.6.1-0`
to build against an earlier CPS fixture.

For signed-profile rollback, set `CPS_VM_UTIL_LINUX_FIXTURE` to the util-linux
output, `CPS_VM_SYSTEMD_FIXTURE` to the matching systemd build, and
`CPS_VM_USR_BASE_FIXTURE` to the merged base. The release profile pins the
util-linux catalog with the `devel` facet disabled. The guest mounts the ESP
with the packaged `mount` and probes it with `blkid` on the trial, default
reboot, and rollback. The VM defaults to the matching systemd revision for
the selected CPS fixture. Set `CPS_VM_SYSTEMD_VERSION=262.1.0-1` to reuse
that earlier systemd revision.

#### Building GNU tools for boot preparation

Place the verified `coreutils-9.12.tar.xz` and `grep-3.12.tar.xz` releases in
a local source directory. Verify their detached signatures using the GNU
keyring and the expected release keys: coreutils uses
`6C37DC12121A5006BC1DB804DF6FD971306037D9`, and grep uses
`155D3FC500C834486D1EEA677FD9FCCB000BEEEE`.
[Coreutils sources and signature](https://ftp.gnu.org/gnu/coreutils/)
· [Grep release announcement](https://lists.gnu.org/archive/html/info-gnu/2025-04/msg00008.html)

```sh
CPS_GNU_SOURCE_DIR=/path/to/verified-sources \
tests/build-gnu-tools.sh target/gnu-tools-fixture target/usr-base-fixture
```

The KDL recipes pin each archive's size and SHA-256 digest. The fixture
builds the GNU coreutils commands as one binary with command symlinks and
builds GNU grep separately. Both builds run twice in the isolated CPS
environment before publication with manifest-bound builder evidence.
The installation check resolves libraries from the signed merged base and
exercises `install`, file modes, and the GNU grep `-G` behavior used by
systemd's kernel-install plugin. Package dependencies require the ELF
runtime; the boot profile explicitly selects these utility packages.

#### Testing a source-built BusyBox

On an x86-64 Arch Linux worker with a native GCC toolchain, run
`tests/build-busybox.sh target/busybox-fixture` with a new output directory.
It signs a seed catalog containing the worker's compiler and utilities, then
pins that catalog, the upstream BusyBox 1.38.0 archive, and the two ash fixes
for CVE-2026-38754 and CVE-2026-38755 in the recipe. CPS compiles
the static binary twice in fresh sandboxes and publishes it to an
attestation-required test repository. Set `CPS_BUSYBOX_SOURCE_DIR` to a
directory containing the pinned archive to run without network access.
The seed catalog identifies the prebuilt compiler used by this test.
[BusyBox release archives](https://busybox.net/downloads/) ·
[IFS cleanup patch](https://lists.busybox.net/pipermail/busybox/2026-June/092360.html) ·
[Function recursion patch](https://lists.busybox.net/pipermail/busybox/2026-June/092354.html)

To use that package in the VM, set `CPS_VM_BUSYBOX_REPO` to
`target/busybox-fixture/repo` and `CPS_VM_BUSYBOX_TRUST_KEY` to
`target/busybox-fixture/release-public.der`. The fixture installs the exact
package from that publisher. The systemd guest loads its prepared modules
with the packaged `insmod` applet, checks the loaded modules in sysfs, and
reports the version and loaded module names on each boot. The host still prepares
the module files for kernels with modular FAT support.

To use an earlier fixture, set `CPS_VM_BUSYBOX_VERSION=1.37.0-0` for the
split layout or `CPS_VM_BUSYBOX_VERSION=1.37.0-1` for merged `/usr`. These
fixtures use the worker's `insmod` in the guest.
