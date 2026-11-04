# Canopa Package System

This Rust prototype manages disposable Linux directory images from a signed local repository. It stores immutable package manifests and SHA-256 file payloads, plans changes across the image, and verifies installed files offline.

Repository reads cap catalogs at 64 MiB and manifests at 32 MiB. CPS streams payloads to the staging directory with the signed length as a bound and checks the digest before recording a transaction.

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

To upgrade, publish the revised recipe under a new version and run `cps plan --root image --output upgrade.json update`, then apply the saved plan. The plan pins the image revision and the signed catalogs used to resolve packages. Keep the saved plan until apply finishes. If interrupted, rerun `apply` with the same plan. Once writes begin, recovery uses the payloads staged inside the image and can finish offline.

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
File payloads come from the configured local repository or cache and are checked
against the installed digests before any writes. Repair preserves edited or
removed configuration files declared with `preserve=#true`. Resolve path type
conflicts involving directories or special files before retrying the plan.

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
rejects namespaced capability sets, hard-linked symlinks, special modes and
filesystem objects, and other extended attributes on regular files or
directories. Review the manifest and add dependency, configuration, and
systemd declarations before publishing. Publication derives the payload
hashes and lengths. [IPS prototype manifest generation](https://docs.oracle.com/cd/E88353_01/html/E37839/pkgsend-1.html)

## Finding and inspecting packages

```sh
cps list --root image
cps list --root image --available --all-versions
cps search --root image hello
cps info --root image demo/hello
cps info --root image --available demo/hello@1.0.0-0
```

`list` shows installed packages and their explicit or dependency status. With `--available` it shows the newest version of each package in the signed catalog. `search` matches package name fragments in that catalog, or in the installed set with `--installed`. `info` prints a verified manifest in KDL, including dependencies and file actions. It accepts a name, a name and version, or a full CPS package ID. Installed queries work offline. Available queries use the configured local repository or cache.

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

The fixture archives the selected Git commit and checks each cached crate
archive against `Cargo.lock` before vendoring from a fresh offline Cargo home.
It pins both archives, the package manifest, and a signed catalog containing
the worker's Rust and C toolchain. CPS builds SQLite and libseccomp twice from
the pinned upstream archives, then links its own two builds against those exact
libraries. All three packages carry signed build evidence. The solver installs
the declared runtime dependencies, and the fixture uses the installed loader
and libraries to run the installed CPS binary and verify the image. A separate
signed `runtime-seed` repository contains the worker's prebuilt ELF loader and
C and GCC runtime libraries. A release profile pins the signed seed catalog
and checks the complete CPS dependency set before publishing the three builds
in one catalog.

The output includes `source-revision`, the recipes, source cache, toolchain
repository, protected package repository, and runtime seed repository. Set
`CPS_VM_CPS_FIXTURE` to that output directory for a systemd VM boot test. The
VM fixture installs the exact protected CPS package and its dependencies from
the signed repositories. Set `CPS_BIN` to `image/usr/bin/cps` inside the output
directory to also use that executable to assemble the guest.

### Preparing a source cache

Add a `url="https://upstream.example.org/releases/hello.c"` property to a pinned `source` entry to fetch it. Use the final HTTPS URL. The fetcher checks the exact byte count and SHA-256 before publishing an object to the cache. It rejects redirects and rechecks existing objects before reuse. Recipes can share a cache by digest.

```sh
cps source fetch --cache source-cache hello.build.kdl
cps build --repo toolchain-cache --publisher canopa --trust-key public.der \
    --source-cache source-cache --output hello-build --check-reproducible hello.build.kdl
```

For local sources, `cps source import --from sources --cache source-cache hello.build.kdl` fills the same cache offline. Builds read the cache offline and verify each input again before staging it under its recipe path. The existing `--sources` option accepts a directory laid out by recipe path.

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

The optional worker key signs an in-toto statement in a DSSE envelope after both runs match. The statement records the recipe and source digests, pinned toolchain catalog and resolved package manifests, and the output hashes and lengths. Provision the worker public key and the expected recipe digest on the publishing machine from independently reviewed inputs. Keep the publishing key separate from the worker key. `publish-bundle` verifies the statement and streams the exact attested payloads into the repository. [in-toto statement specification](https://github.com/in-toto/attestation/blob/main/spec/v1/statement.md) · [DSSE signature protocol](https://github.com/secure-systems-lab/dsse/blob/master/protocol.md)

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

For independent rebuilds, set `minimum-builders=2` on the policy's `publisher` node and provision a `builder-key` for each worker. Run the same pinned recipe on each worker and pass the additional signed statements with `--witness`. The publishing policy defaults to one worker key.

```sh
cps publish-bundle --repo repo --signing-key private.der --builder-key worker-a-public.der \
    --recipe-sha256 "$reviewed_recipe_sha256" \
    --witness worker-b-public.der=worker-b-build/attestation.json worker-a-build
```

CPS counts distinct authorized keys whose signatures cover the same build statement, including the exact payload hashes and pinned inputs. The release stores those signatures together in the DSSE envelope. The threshold applies at publication. Administrators establish worker independence by assigning keys to separately operated workers.

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

Set `upgrade-from="<signed catalog SHA-256>"` on a profile to also test a rolling
upgrade from that historical catalog. CPS composes the baseline with the same
package names, architecture, and facets, choosing the newest baseline versions
for the profile roots. It runs the ordinary update solver with that installed
set. The result must reach the candidate profile's requested versions, retain
only packages available in the candidate, and pass the planner's directory and
kernel transition checks. This models the package-managed state of a clean
image. Live filesystem changes and boot health are checked separately during
staging and boot.

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
publisher. Otherwise both compositions use its `catalog` pin. The primary
catalog signs the checked architecture, facets, and ordered companion catalog
digests. A configured image can use those digests directly.

## Signed releases and mirrors

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

For an image that uses a small part of a mirror, add `--index-only` to `repo sync`.
This caches the catalog, manifests, and build evidence and verifies the signed
TUF target entries for payloads. Fetch the files for the resulting plan before
applying or staging it.

```sh
cps plan --root image --output upgrade.json update
cps fetch --root image --plan upgrade.json
cps apply --root image --plan upgrade.json
```

Fetch revalidates the plan against the image and its pinned catalogs. If a
required payload is missing or damaged, fetch refreshes TUF metadata using the
mirror endpoints saved by `repo sync` and verifies the exact target. Use the
sync account to fetch into its cache. Fetch leaves the image and the cache's
current catalog unchanged. A payload absent from the mirror must already be in
the local cache, which matters when an older plan references a withdrawn
version. Use full `repo sync` when provisioning a toolchain cache for offline
builds.

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

For a new package, the solver considers versions from the first publisher that
offers the package name. An installed package stays with its original
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
`publisher prefer --root image extras` to move it to the front.
`publisher remove --root image extras` requires that no installed package
comes from that publisher and at least one publisher remains.
These changes invalidate outstanding plans for the image and preserve its
boot environment identity. They are local to the named image root.

### Images that follow a signed release profile

Add `release-profile "desktop"` to the image KDL to follow that profile in the
first publisher's signed catalog. Configure the same architecture, facets, and
publisher order as the profile. Each companion repository must have the signed
catalog referenced by the profile. Publisher changes on a profiled image
require a new image configuration.

Available-package queries and new plans use the primary publisher's current
catalog and the companion digests it signs. Saved plans use their original
primary catalog and must match the companion digests signed in that catalog.
Local history keeps those plans usable after a release advances. Offline
installed-package queries, removal, and repair use committed image state.
Upgrade CPS before using catalogs with signed release profile records.

## Manifest and image behavior

- Manifests and image configuration use KDL 2 and begin with `schema-version 1`. Paths in image configuration resolve relative to the configuration file. It also accepts `architecture "x86_64"` and `facet "doc.man" #false`. Convert old TOML recipes before publishing new versions. Existing JSON manifest objects stay readable under their original digests.
- Versions have the numeric form `major.minor.patch-revision`. Inside a package block, declare dependencies as `require "library/libdisplay" version=">=1.0.0-0, <2.0.0-0"` or `incorporate "library/libdisplay" version="=1.0.0-0"`. Use `exclude "library/libdisplay" version=">=2.0.0-0"` to reject a range when the package is present. Omit the version to exclude every version of that package.
- Use a `require-any` block with two or more `package` entries when interchangeable providers can satisfy a dependency. Each alternative accepts the same `version` constraints as `require`. The solver prefers a compatible installed provider that the transaction will retain. Offline removal checks that a compatible provider remains installed. [IPS dependency types](https://docs.oracle.com/cd/E53394_01/html/E54820/dependtypes.html)
- `conditional "desktop/plugin" version="=2.0.0-0" when="runtime/desktop" when-version=">=2.0.0-0"` requires the plugin when the resulting image includes a matching runtime. Omit `when-version` to match any installed version of the runtime. The predicate alone leaves the runtime optional.
- New plans use Resolvo to resolve compatible versions and report incompatible requirements. When selected file actions collide, CPS excludes the conflicting version or pair of versions and retries. Directory sharing still requires identical metadata. SAT resolution has a 60-second time limit. Saved plans record their solver version so apply can verify the same selection against the pinned catalog. Plans created before solver versioning retain their original behavior. [Resolvo](https://github.com/prefix-dev/resolvo)
- Actions support `file`, `dir`, `symlink`, and `hardlink`. A symlink specifies `target` in place of `mode`. Use `hardlink "bin/tool" target="usr/bin/tool"` to share the inode of an immutable regular file in the same package. The target is relative to the image root and supplies ownership, mode, and content. The optional `arch` and `facet` selectors match image policy. A hard link's target must be selected whenever the alias is selected. Facets default to enabled and can be set with `image-create --facet doc.man=false`. [IPS hard-link actions](https://docs.oracle.com/cd/E53394_01/html/E54820/pkgterms.html)
- Files marked `preserve=#true` keep local edits through an upgrade, including a package ownership transfer at the same path. When the vendor file changes, find its new copy in `image/.cps/config-updates/<revision>/`.
- The image must be inactive and reside on one filesystem. Install and update require existing explicit directory metadata to match the selected action. Transitions between a directory and a file or symlink are rejected. When ownership ends, CPS prunes directories it recorded creating if they are empty and their metadata still matches. It keeps pre-existing directories and directories with local content or extended attributes.

Run `cargo test` for the desktop upgrade and interruption test.

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

Run `cargo build` followed by [`tests/systemd-actions.sh`](tests/systemd-actions.sh) to exercise the real helpers against a staged root. The test also checks recovery from a failed tmpfiles step after sysusers has run.

## Boot environments

Boot environments use separate Linux roots and a dedicated systemd-boot EFI System Partition (ESP). Start with an inactive, single-filesystem base root containing the kernel. GNU `cp` clones the image and uses a reflink where supported. The destination must be a new directory.

```sh
cps boot-env activate --known-good --root base --esp esp \
    --root-partuuid "$base_partuuid" --initrd boot/initramfs.img
cps plan --root base --output upgrade.json update
cps boot-env stage --source base --root staged upgrade.json
# Deploy the staged directory to its own GPT root partition before activation.
cps boot-env activate --root staged --esp esp \
    --root-partuuid "$staged_partuuid" --initrd boot/initramfs.img
```

`stage` verifies the new root and gives it a fresh image identity. Activation copies boot files from the selected root and publishes its Boot Loader Specification entry last. Supply the **partition UUID** of each deployed root and the root-relative boot file paths. The kernel path defaults to `boot/vmlinuz`. Omit `--initrd` for a kernel that can mount the root on its own. Use `--options` for additional kernel arguments. Provisioning the partitions and installing systemd-boot on the dedicated ESP are external to these commands.

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

The list marks the default configured on the ESP and shows each registered root's partition UUID, title, and boot status. The title includes the Linux release for manifest-managed kernels. Rollback selects the default environment's recorded parent unless an ID is supplied. Entries from earlier CPS versions use the previous known-good revision. Rollback checks the selected entry's boot files and updates `loader.conf` for the next boot. It keeps both environments available. Trials started from a pinned environment use the `preferred` setting in systemd-boot 262, with the pinned base as the fallback. [systemd-boot selection](https://github.com/systemd/systemd/blob/v262/src/boot/boot.c#L1717)

### VM boot tests

The [VM test](tests/systemd-boot-vm.sh) builds a GPT disk with two separate ext4 roots and a FAT ESP, then boots it through OVMF and systemd-boot under QEMU. The `fallback` scenario checks that staging leaves the ESP alone and that the previous root boots after two failed attempts. The `bless` scenario runs systemd as PID 1, installs the health gate through the CPS package, and checks that systemd removes the counter and boots the blessed root again. The `health-fail` scenario corrupts a managed file and checks that the health gate blocks blessing through both attempts. The `kernel-upgrade` scenario checks the kernel and initrd on the ESP and reports `uname -r` from both roots.

Build with `cargo build`. The test needs `qemu-system-x86_64`, `mkfs.fat`, mtools, `mkfs.ext4`, `sfdisk`, OpenSSL, systemd-boot, and matching non-Secure-Boot x86-64 OVMF code and variables files. Supply an EFI-stub Linux kernel with built-in virtio block, GPT, ext4, and serial console support, plus a compatible BusyBox with `sh`, `cat`, `reboot`, and `poweroff`.

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
an obsolete dependency. After systemd blesses the trial and reboots it as the
default, the test runs `boot-env list` and `boot-env rollback` against the ESP.
It boots the same VM disk again and checks that the previous kernel and retired
dependency are present and that the added package is absent. Both kernels need
VFAT support. To load modules from extracted kernel packages, set
`CPS_VM_KERNEL_MODULES_ROOT` and `CPS_VM_TRIAL_MODULES_ROOT` to the roots
containing their indexed `lib/modules` trees.

#### Testing a source-built BusyBox

On an x86-64 Arch Linux worker with a native GCC toolchain, run
`tests/build-busybox.sh target/busybox-fixture` with a new output directory.
It signs a seed catalog containing the worker's compiler and utilities, then
pins that catalog and the upstream BusyBox archive in the recipe. CPS compiles
the static binary twice in fresh sandboxes and publishes it to an
attestation-required test repository. Set `CPS_BUSYBOX_SOURCE_DIR` to a
directory containing the pinned archive to run without network access.
The seed catalog identifies the prebuilt compiler used by this test.
[BusyBox release archives](https://busybox.net/downloads/)

To use that package in the VM, set `CPS_VM_BUSYBOX_REPO` to
`target/busybox-fixture/repo` and `CPS_VM_BUSYBOX_TRUST_KEY` to
`target/busybox-fixture/release-public.der`. The fixture installs the exact
package from that publisher and the systemd guest reports the BusyBox version
on each boot.
