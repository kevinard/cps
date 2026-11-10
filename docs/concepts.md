# CPS concepts and vocabulary

[Documentation](README.md) · [Quickstart](quickstart.md) ·
[CLI reference](reference/README.md)

This page defines the terms the rest of the manual uses. Each section ends
with links to the guides that put them to work.

## Roles

A **package maintainer** describes a package and prepares its inputs. A
**build worker** runs a pinned recipe in a sandbox and can sign the
resulting build evidence. A **publisher** decides what enters its catalog and
signs that catalog. A **client** trusts selected publishers and plans changes
for an image. One person can fill all these roles in a local test setup, as
in the [quickstart](quickstart.md).

## Packages and dependencies

| Term | Meaning in CPS |
| --- | --- |
| **Package name** | A name such as `apps/editor`. Slashes group related names. |
| **Version** | `[epoch:]release[-revision]`, such as `1:2.4.1-3`. CPS orders versions the way dpkg does: epoch, then release, then revision, with digit runs compared as numbers and `~` sorting first. Constraints compare by meaning, so `2.0.0` satisfies `>=2.0.0-0`. |
| **Package ID** | The publisher, name, and exact version as written, for example `cps://canopa/apps/editor@1.0.0-0`. `@2.0.0` and `@2.0.0-0` are distinct IDs of the same release. |
| **Selector** | The name, version, or publisher qualification supplied to a command. An exact version such as `apps/editor@1.0.0-0` keeps publisher search order. |
| **Manifest** | The KDL description of one package version: its metadata, such as summary, license, and homepage, its dependencies, and its actions. The signed catalog authenticates it by digest. |
| **Action** | An operation declared by a manifest, such as creating a file, directory, symlink, or hard link at an image-relative path, or a systemd or kernel action. |
| **Dependency** | A `require`, `incorporate`, `exclude`, or `conditional` declaration. A `require-any` block lists interchangeable providers. The planner keeps a compatible installed provider, and otherwise tries them in the order written. |
| **Capability** | A virtual interface under `cap/` that packages provide and require instead of naming each other. File actions can separately grant Linux file capabilities such as `cap_net_raw`. |
| **Generated library dependency** | A `cap/elf/ABI/SONAME` capability that publication derives from ELF payloads. A package that ships a shared library provides it, and a package whose objects need that library requires it. Maintainers do not write these. |
| **Trigger** | A declaration that runs a handler the package ships when watched paths change, to rebuild derived files such as a cache or an initramfs. CPS has no package scripts. The paths a trigger generates belong to its package. |
| **Prototype tree** | A staging directory laid out like the destination filesystem. `publish --proto` reads package file contents from this tree. |
| **Payload** | File contents identified by their SHA-256 digest. The manifest records the digest and length. |

[Write a manifest](maintainer/manifests.md) ·
[Generated shared-library dependencies](maintainer/manifests.md#generated-shared-library-dependencies) ·
[Triggers](maintainer/triggers.md)

## Builds

A **build recipe** is KDL that pins one or more manifest templates, the
sources, and a toolchain, and supplies the build script. The build runs
offline in a sandbox and installs into a destination tree. Each template
claims its files, explicitly or with `collect` patterns, and the recipe can
`discard` others. A file that the build installs but no template claims fails
the build, and so does a file that two templates claim.

One recipe can produce several packages. Each output is an ordinary package
with its own name, dependencies, and metadata, and clients cannot tell it
from a separately built package. Use separate outputs only for content that
is installed on its own or has its own dependencies, as described in
[facets, separate packages, and variants](#facets-separate-packages-and-variants).
Debug information that the build splits from stripped objects stays in the
same package, under the `debug.symbols` facet.

A **bundle** carries the recipe, the expanded manifests, and the built
payloads. An **attestation** is a worker-signed statement covering the
pinned inputs, the outputs, and each manifest as it will be published,
including its generated library dependencies. Publishers and clients decide
separately which build workers they trust.

Across many recipes, `cps build-order` prints the order in which to build
them, `cps rebuild-plan` lists what to rebuild after packages change, and
`cps recipe repin` moves recipes to new toolchain catalogs.

[Pinned builds](maintainer/builds.md) ·
[Several packages from one recipe](maintainer/builds.md#building-several-packages-from-one-recipe) ·
[Plan builds across recipes](maintainer/orchestration.md) ·
[Verify rebuilds](publisher/rebuilds.md)

## Publishers, catalogs, and trust

A **repository** holds a publisher's signed catalogs, manifests, and payload
objects. The **catalog** is a signed snapshot of package IDs, their manifest
digests, and the dependency summaries the planner resolves with. It consists
of a small signed **catalog root** that commits by digest to up to 256
content-addressed **shards**. Each shard holds the records of the package
names whose hash shares a prefix, so one publication can add many packages
and still rewrites only the shards it changes. A **catalog pin** is the
SHA-256 of the exact signed catalog root. Plans, locks, and profiles can keep
using a historical pin while its objects are available. The catalog
**sequence** increases with each publication, which lets clients detect
replay and inconsistent successors.

The publisher's **trust root** names its offline **root keys** and online
**catalog keys**, each with a signature threshold, and an expiry. A threshold
of root keys signs each trust root version, and each later version must also
be signed by the previous version's root keys. Exactly the catalog threshold
of catalog keys signs each catalog root, which carries the trust root history
it is signed under. Rotating or revoking a key, renewing the expiry, and
recovering from a compromised catalog key are each a new trust root version.
A client pins one trust root version for each publisher and records the
highest version it has verified, so it rejects catalogs signed under an older
version. Any signing key can be a file or an **external signer**, a command
that signs with a key held in an HSM or cloud KMS.

A **mirror** distributes an exported repository. CPS verifies the
transport metadata with a provisioned **TUF root** (The Update Framework),
then verifies the catalog against the publisher's pinned trust root. TUF roles
govern the freshness and authorization of mirror files, and their keys are
separate from the publisher's catalog keys. A routine refresh of mirror
metadata needs only the timestamp key. A **mirror cache** is the local
repository that sync populates. An **index-only cache** stores metadata and
downloads payloads later during fetch. When a mirror cannot be refreshed, the
update fails unless the image's **refresh policy** for that publisher allows
deferring it for a limited time.

**Publisher order** governs unqualified package selection. For a new package,
CPS considers the first eligible publisher in the configured order.
Installed packages keep their publisher, a behavior called **stickiness**.
Use a qualified selector to choose a particular source for a new package.

[Publish and retire packages](publisher/publishing.md) ·
[Publisher trust root](publisher/publishing.md#publisher-trust-root) ·
[Configure publishers](client/publishers.md) ·
[Serve a mirror](publisher/mirrors.md) · [Sync mirrors](client/mirrors.md)

## Images and transactions

An **image** is a managed filesystem root and its CPS state. The state records
installed packages, path ownership, and the committed **revision**. It lives
in `/usr/lib/sysimage/cps` inside the image, next to the files it describes.
Packages never ship anything in `/var`: an image starts with `var` as an
empty mount point, and the packages' tmpfiles.d and sysusers.d declarations
populate `/var` at boot. Every change goes through the same steps:

| Term | Meaning in CPS |
| --- | --- |
| **Plan** | A request resolved against the committed revision. It records the catalog pins, package changes, and the ordered writes. |
| **Fetch** | Downloads and verifies the payloads a plan needs into the cache. |
| **Apply** | Checks the plan and payloads, records a **transaction journal**, writes the changes, runs triggers and systemd and kernel actions, and commits the next revision. |
| **Pending transaction** | A journaled transaction that has not committed. `cps transaction status` shows it. |
| **Resume** | Finishes a pending transaction from its journal. |
| **Abort** | Undoes a pending transaction by applying its inverse, reruns the triggers and kernel preparation for the committed packages, and leaves the committed revision unchanged. |
| **Verify** | Checks an image against its installed manifests offline. `--quick` trusts recorded file fingerprints that still match instead of hashing every file. |

Writes follow the dependency graph. A package's files are written after those
of the packages it requires, so libraries land before the executables that
load them, and removals run in the reverse order. A **filesystem transition**
replaces a directory with a file or symlink, or the reverse, within one
transaction. CPS builds the replacement aside and swaps it into place
atomically. It refuses the plan if the directory holds anything the
transaction does not remove, such as unmanaged files.

After the files are written, CPS runs each trigger whose watched paths
changed, once, in an order derived from what each trigger generates and
watches. The handler runs under Bubblewrap with the image read-only, and only
changes to its declared outputs reach the image. The journal records each
finished trigger, so resume and abort run only the remaining ones.

An **explicit request** records that the user asked for a package. An
**automatic** package was selected to satisfy dependencies or
recommendations. Dependency cleanup removes automatic packages that local
requests and the release profile no longer need.

[Manage an inactive image](client/images.md) ·
[Abort a transaction](client/images.md#abort-a-transaction) ·
[Replace a directory with a file or symlink](client/images.md#replace-a-directory-with-a-file-or-symlink) ·
[Manage a running system](client/system.md)

## Selecting content

| Selection term | Effect |
| --- | --- |
| **Facet** | Optional content inside one package, such as `doc.man` or `devel`, that the image enables or disables, with optional per-package overrides. |
| **Variant** | A mutually exclusive image property, such as `arch` or `container`, that selects between alternative content. |
| **Architecture** | The target CPU architecture. It restricts eligible package versions, and as the built-in `arch` variant it selects actions. |
| **Freeze** | A persistent constraint on a package's exact version. |
| **Mediator** | Picks one provider for shared symlinks, such as the default editor command. |
| **Preserved configuration** | A file marked `preserve=#true`. CPS carries local edits through an upgrade and can stage changed vendor defaults for review. |

### Facets, separate packages, and variants

CPS follows the IPS split between three ways of shaping what an image
contains. Each problem has one mechanism.

- A **facet** is optional content inside one package. The image chooses it
  for every package and can override the choice for a single package. A
  facet never adds dependencies. Typical facets are headers and `.so` links
  (`devel`), manual pages and documentation (`doc.man`, `doc.*`), locales
  (`locale.fr`), static libraries, debug symbols (`debug.symbols`), and
  32-bit multilib objects (`isa.i686`), the way illumos ships 32- and 64-bit
  objects in one package. `debug.*` and `isa.*` facets are off unless the
  image enables them. Every other facet is on by default.
- A **separate package** is needed when content must be installed or removed
  on its own, or has its own dependencies. A runtime library and a plugin that
  needs another service are separate packages, even when one recipe builds
  them.
- A **variant** is a mutually exclusive property of the image that selects
  between alternative content. The built-in variant is `arch`. Other examples
  are a container or bare-metal variant, like the IPS zone variant, or a
  debug build. An image has exactly one value for each variant, so one path
  can carry different actions for different values.

Facets and variants select files within the packages that dependency
resolution chose. They never change which packages are installed, so
neither dependency declarations nor generated library dependencies depend on
them.

[Facets and variants on an image](client/configuration.md#facets-and-variants-on-an-existing-image) ·
[Facets and variants in a manifest](maintainer/manifests.md#facets-and-variants)

## Release profiles and locks

A **release profile** is a named, signed composition for a target
architecture and a set of facets and variants. A **primary publisher** signs
the profile, which can pin signed catalogs from **companion publishers**. The
publisher tests the selected roots and their dependency closure before
signing. A profile-following image protects that composition while letting
the client install compatible local packages. A **phased rollout** makes a
composition-changing update eligible for a percentage of image cohorts.

An **image lock** captures one installed image's exact composition, including
its publisher pins, facet and variant policy, and local requests. Use it to
provision another image with that composition. An **image bundle** carries
the lock and the verified repository objects it needs for disconnected
provisioning.

[Install a profile or replicate an image](client/profiles.md) ·
[Define release profiles](publisher/profiles.md)

## Boot environments

A **boot environment** pairs a root with its boot artifacts and its
registered boot entry on the **EFI System Partition** (ESP). The entry
records the root's image identity and revision, so the boot always uses the
root's committed package state and configuration together. A staged Btrfs
environment is a writable snapshot of its parent root. Every Btrfs
environment mounts the same `/var` subvolume, so logs and application data
survive rollback and pruning. An ext4 environment is
an exported root written to a dedicated partition, its **root slot**.

| Term | Meaning in CPS |
| --- | --- |
| **Staging** | Applying a plan to a new root instead of the running one, for changes that need a reboot. |
| **Trial** | A staged environment selected for the next boot with a limited number of attempts: two by default with systemd-boot, and one with direct UEFI boot. |
| **Known good** | An environment that booted and passed its health checks, or that was registered with `--known-good` to bootstrap the ESP. A healthy boot **blesses** a trial. Otherwise the next boot returns to the trial's **fallback**, the known-good parent it was staged from. |
| **Rollback** | Selects a known-good environment, by default the fallback of the current selection, for the next boot. |
| **Failed trial** | A trial whose attempts are exhausted. An **abandoned trial** still has attempts left but is no longer selected, for example after a rollback. |
| **Discard** | On Btrfs, `cps boot-env discard-trial` cancels a pending or running trial, selects its fallback, and removes failed and abandoned trials. |
| **Prune** | On Btrfs, removes failed and abandoned trials and the known-good environments that the retention policy no longer keeps. System package commands prune automatically before staging. An ext4 slot is reused by replacing its environment instead. |

**systemd-boot** loads either separate kernel and initramfs files or a signed
**unified kernel image** (UKI). CPS owns only the boot selection keys in its
`loader.conf`. On Btrfs, UEFI firmware can also launch each signed UKI
directly, without a boot loader. A filesystem UUID identifies a filesystem,
while a **PARTUUID** identifies a partition in the partition table. Use the
identifier each setup or command asks for.

[Boot setup guides](setups/README.md) ·
[Boot operations](setups/boot-operations.md) ·
[Retention and pruning](setups/btrfs.md#retention-and-pruning)
