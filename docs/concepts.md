# CPS concepts and vocabulary

[Documentation](README.md) · [Quickstart](quickstart.md) ·
[CLI reference](reference/README.md)

## From a build to an installed package

A **package maintainer** describes the package and prepares its inputs.
A **build worker** runs a pinned recipe and can sign the resulting build
evidence. A **publisher** decides what enters its catalog and signs that
catalog. A **client** trusts selected publishers and plans changes for an
image. One person can fill all these roles in a local test setup.

| Term | Meaning in CPS |
| --- | --- |
| **Package name** | A name such as `apps/editor`. Slashes group related names. |
| **Package ID** | The publisher, name, and exact version, for example `cps://canopa/apps/editor@1.0.0-0`. |
| **Selector** | The name, version, or publisher qualification supplied to a command. An exact version such as `apps/editor@1.0.0-0` keeps publisher search order. |
| **Manifest** | Package metadata that describes dependencies, filesystem actions, and any systemd or kernel actions. The signed catalog authenticates it by digest. |
| **Action** | An operation declared by a manifest, such as creating a file, directory, symlink, or hard link at an image-relative path. |
| **Trigger** | A handler that a package ships and declares to rebuild derived files, such as a cache, when watched paths change in any package. CPS runs it once per transaction, confined to its declared **generated paths**, which the package owns. |
| **Prototype tree** | A staging directory laid out like the destination filesystem. `publish --proto` reads package file contents from this tree. |
| **Payload** | File contents identified by their SHA-256 digest. The manifest records the digest and length. |
| **Build recipe** | KDL that pins a manifest, sources, and a toolchain and supplies the build script. A **bundle** carries the recipe, manifest, and built payloads. |
| **Attestation** | A worker-signed statement covering the pinned build inputs, outputs, and published KDL manifest. |

[Manifest format and versions](maintainer/manifests.md) ·
[Pinned builds](maintainer/builds.md) · [Publication policy](publisher/publishing.md)

## Publishers, catalogs, and mirrors

A **repository** holds signed catalog metadata, manifests, and payload
objects for a publisher. The **catalog** is a signed snapshot of package
identities, their manifest digests, and the dependency summaries the planner
resolves with. A small signed **catalog root** commits by digest to
content-addressed **shards** that partition the records by package name, so a
publication rewrites only the shards it changes. Its sequence and trust root
history let clients detect replay and inconsistent successors. A **catalog
pin** is the SHA-256 of the exact signed root. Plans and profiles can keep
using a historical pin when its objects are available.

The publisher's **trust root** names its offline **root keys** and online
**catalog keys**, each with a signature threshold, and an expiry. A threshold of
root keys signs each trust root version, and each new version must also be
signed by the previous version's root keys. Exactly the catalog threshold of
catalog keys signs each catalog root, which carries the trust root history it
is signed under. Rotating or revoking a key, or recovering from a compromised
catalog key, is a new trust root version. A client pins one trust root version
for each publisher and records the highest version it has verified, so it
rejects catalogs signed under an older version.

A **mirror** distributes an exported repository. CPS verifies the transport
metadata using a provisioned **TUF root** (The Update Framework), then
verifies the catalog against the publisher's pinned **trust root**. TUF roles govern
freshness and authorization for mirror targets. Publisher signatures
authenticate the catalog and bind its package manifests. A **mirror cache**
is the local repository populated by sync. An **index-only cache** stores
metadata and downloads the needed payloads later during fetch.

**Publisher order** governs unqualified package selection. For a new package
CPS considers the first eligible publisher in the configured order.
Installed packages keep their publisher, a behavior called **stickiness**.
Use a qualified selector when you want a particular source for a new
package.

[Publisher configuration](client/publishers.md) ·
[Mirror sync and catalog checkpoints](client/mirrors.md)

## Images and transactions

An **image** is a managed filesystem root and its CPS state. The state
records installed packages, ownership, and the committed **revision**.
A **plan** resolves a request against that revision and records the catalog
pins, package changes, and actions. Fetch stages the required payloads in
the cache. Apply checks the plan and payloads before writing package files,
records a **transaction journal**, and commits the new image state when
the changes finish. Use resume to complete a journaled transaction.

After writing files, a transaction runs the triggers whose watched paths
changed, in an order derived from what each one generates and watches. A
trigger replaces a package script: it runs a fixed command under Bubblewrap,
and CPS publishes only the paths it declares as outputs. CPS records each
finished trigger in the journal, so resume runs only the remaining ones.
[Regenerate shared caches with triggers](maintainer/triggers.md)

An **explicit request** records that the user asked to keep a package.
An **automatic** package was selected to satisfy dependencies or
recommendations. Dependency cleanup can remove automatic packages when
local requests and the release profile no longer need them.

| Selection term | Effect |
| --- | --- |
| **Architecture** | Restricts eligible package versions or actions to the target CPU architecture. It is the built-in `arch` variant. |
| **Facet** | Optional content inside one package, such as `doc.man` or `devel`, that the image enables or disables. |
| **Variant** | A mutually exclusive image property, such as `arch` or `container`, that selects between alternative content. |
| **Freeze** | A persistent constraint on a package's exact version. |
| **Mediator** | Picks one provider for shared symlinks, such as the default editor command. |
| **Capability** | In dependency declarations, a virtual package interface under `cap/`. File actions can separately grant Linux executable capabilities such as `cap_net_raw`. |
| **Preserved configuration** | A file marked `preserve=#true`. CPS carries local edits through an upgrade and can stage changed vendor defaults for review. |

[Inactive image transactions](client/images.md) ·
[Package selection and configuration](client/configuration.md)

### Facets, separate packages, and variants

CPS follows the IPS split between three ways of shaping what an image
contains. Each problem has one mechanism.

- A **facet** is optional content inside one package. The image chooses it
  for every package, and can override the choice for a single package. A
  facet never adds dependencies. Typical facets are headers and `.so`
  links (`devel`), manual pages and documentation (`doc.man`, `doc.*`),
  locales (`locale.fr`), static libraries, debug symbols (`debug.*`), and
  32-bit multilib objects (`isa.i686`), the way illumos ships 32- and
  64-bit objects in one package. Debug symbols and multilib objects are off
  unless the image enables them.
- A **separate package** is needed when content must be installed or
  removed on its own, or has its own dependency closure. A runtime library
  and a plugin that needs another service are separate packages.
- A **variant** is a mutually exclusive property of the image that selects
  between alternative content. Today the built-in variant is `arch`. Other
  examples are a container or bare-metal variant, like the IPS zone
  variant, or a debug build. An image has exactly one value per variant, so
  one path can carry different actions for different values.

Facets and variants select files within the packages that dependency
resolution already chose. They never change which packages are installed,
so dependency declarations cannot carry them.

## Release profiles and locks

A **release profile** is a named, signed composition for a target
architecture and set of facets and variants. A **primary publisher** signs the
profile. The profile can pin signed catalogs from **companion publishers**.
The publisher tests the selected roots and their dependency closure
before signing it. A profile-following image protects that composition
while letting the client install compatible local packages. A **phased
rollout** makes a composition-changing update eligible for a percentage of
image cohorts.

An **image lock** captures one installed image's exact composition, including
its publisher pins, facet and variant policy, and local requests. Use it to provision another image with
that composition. An **image bundle** carries the lock and the verified
repository objects it needs for disconnected provisioning.

[Install a profile or replicate an image](client/profiles.md) ·
[Define release profiles](publisher/profiles.md)

## Boot environments

A **boot environment** pairs a root with its boot artifacts and registered
boot entry. A staged change prepares a separate root, using a Btrfs snapshot
or a copy deployed to a dedicated ext4 slot. A **trial** is selected for
the next boot with an attempt limit or a one-shot boot selection, depending on
the boot mode. A successful health check lets the boot mechanism **bless**
the trial as **known good**. **Rollback** selects a previous known-good root
for the next boot. The boot entry uses the root's committed package state
and configuration together.

The **EFI System Partition** (ESP) holds the boot files. **systemd-boot**
can load separate kernel and initramfs files or a **unified kernel image**
(UKI). CPS also supports Btrfs UKIs that UEFI firmware launches directly.
A filesystem UUID identifies a filesystem, while a **PARTUUID** identifies
a partition in the partition table. Use the identifier requested by each
setup or command.

[Boot setup guides](setups/README.md) · [Boot operations](setups/boot-operations.md)
