# Install or replicate a signed composition

[Documentation](../README.md) · [Clients](README.md)

A [release profile](../concepts.md#release-profiles-and-locks) is a signed,
tested composition that an image can follow. An image lock captures one
image's exact composition so that you can reproduce it.

## On this page

- [Images that follow a signed release profile](#images-that-follow-a-signed-release-profile)
- [Install a particular release](#install-a-particular-release)
- [Update a profile image](#update-a-profile-image)
- [Add local publishers to a profile image](#add-local-publishers-to-a-profile-image)
- [Facet rules on profile images](#facet-rules-on-profile-images)
- [Provisioning the same composition on another machine](#provisioning-the-same-composition-on-another-machine)

## Images that follow a signed release profile

Add `release-profile "desktop"` to the image KDL to follow that profile in the
first publisher's signed catalog. Configure the same architecture, image-wide
facet rules, variants, and publisher order as the profile. Each companion
repository must have the signed catalog referenced by the profile. The signed
publishers keep their signed membership and order. Repository paths can be
relocated in place. To fetch the profile from mirrors, see
[Synchronizing a signed profile](mirrors.md#synchronizing-a-signed-profile).

On an image with no installed packages, use the profile's checked roots and
dependencies directly.

```sh
cps install --root image --profile
```

CPS checks every manifest digest against the signed composition, then
checks dependencies, paths, and configured build trust before using the
normal transaction. It marks the profile roots as explicitly installed.
The image records the manifest digests of the applied composition. Local
installs, exact installs, removals, and dependency cleanup preserve that
composition. Exact installs reconcile local additions around the protected
packages. Only a signed profile update can replace the protected set.

## Install a particular release

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
looking up available packages, and planning `install --profile`. Cache
pruning with `--keep-image` retains the pinned catalogs' payloads. The first
committed `install --profile` consumes the pin, and subsequent plans follow the
current signed release.

For reproducible release images, also set `image-id` to the release image's
UUID. This fixes the initial image identity used in saved plans, offline
disk images, and boot entries.

```kdl
image-id "a47593c2-e985-42d2-92b6-44c7057e1e96"
```

To compare independent builds, use the same signed bootstrap catalog, image
configuration, canonical repository paths, builder UID and GID, and
`SOURCE_DATE_EPOCH`. Use the same GPT template for byte-identical disk
images. A staged upgrade receives a fresh image identity.

## Update a profile image

On a profiled image with installed packages, `cps upgrade` reconciles the image to the current signed composition
plus the packages the operator explicitly requested and their dependencies.
A previous profile default leaves the image if the new profile and local
packages no longer require it. The package can remain published for manual
installation. Use `cps mark explicit` to keep an installed default as a
local choice. `cps install --exact` replaces the local choices while
preserving the signed composition. Signed obsoletion rules still apply. A
profile update fails if the signed set requires a downgrade or a publisher
switch, or if it conflicts with a [freeze](configuration.md#freezing-package-versions).

When the publisher rolls an update out in phases, an image outside the
current rollout keeps its composition, and `upgrade --check` reports the
rollout in `phased_update`. Pass `--force-rollout` to `upgrade` to take the
update anyway.

Apply the upgrade to an image with `--root`, or save its plan with
`-n --save-plan` and stage it in a boot environment.
The image tracks local requests separately from the signed profile roots.

Available-package queries and new plans use the primary publisher's current
catalog and the companion digests it signs. Saved plans use their original
primary catalog and must match the companion digests signed in that catalog.
Local history keeps those plans usable after a release advances. Offline
installed-package queries, removal, and repair use committed image state.

## Add local publishers to a profile image

After installing the signed composition, you can add publishers of your
own, from a [mirror URL](publishers.md#add-a-publisher-from-an-https-mirror)
or a local repository.

```sh
sudo cps publisher add https://packages.example.org/extras/
sudo cps install cps://extras/apps/editor
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

## Facet rules on profile images

The signed profile fixes the image-wide facet rules and variants that its
publisher checked, so `facet set FACET=...` and `variant set` are
refused on a profile image. Per-package rules (`PACKAGE:FACET=...`) follow
one rule:

- A rule may reach packages you installed locally.
- A rule that reaches a profile-protected package is refused, unless the
  profile declares `allow-facet-override` for a pattern that covers the
  rule's facet. A rule reaches a package when its package name or glob
  matches it, or when `--with-dependencies` carries it there from a local
  package.

CPS checks the rule on every plan, so a later profile update that starts
protecting a package your rule reaches is refused until you reset the rule.
The publisher tested the composition with the profile's facets. A permitted
override changes files inside protected packages, which the publisher
accepted by allowing it, and never changes the protected package set.

```sh
cps facet set --root image tools/app:devel=true
cps facet set --root image 'library/sqlite:debug.symbols=true'  # needs allow-facet-override
```

## Provisioning the same composition on another machine

```sh
cps image lock --root installed > desktop.lock.json
cps image create --root replica --config image.kdl --lock desktop.lock.json
cps install --root replica -y --lock desktop.lock.json
cps image diff installed replica
```

An image lock is a JSON document with `"format": 1` and
`"kind": "image-lock"`. It records the exact package composition, publisher
order and key fingerprints, catalog pins, architecture, image-wide facet
rules, per-package facet rules, variants, and local package requests. The
replica gets a fresh image identity. The target's KDL configuration supplies
its repository locations, trust roots, and build trust policies. Give it the
locked architecture, image-wide facet rules, variants, profile name, and
publisher order, including any locally added publishers. The configuration
may omit per-package facet rules, and the replica then adopts the lock's
rules. If it lists any, they must equal the lock's. Retain the pinned signed
catalogs and their selected payloads in the configured repositories or
caches. The target's pinned trust roots must verify the locked catalogs, so
pin a version no later than the one those catalogs are signed under; see
[pinned trust roots](publishers.md#rotation-and-revocation).

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
cps image lock --root installed --export desktop.bundle
cps image create --root replica --config image.kdl --bundle desktop.bundle
cps install --root replica -y --lock desktop.bundle/image.lock.json
```

Export verifies every copied object and publishes the new directory only
after the copy succeeds. The bundle contains the pinned signed catalogs,
their manifest indexes, and the selected attestations and payloads. Its
repositories supply the historical pins. The target configuration still
supplies publisher trust roots and build trust. Keep the bundle at a stable path
for offline repair, or configure regular repositories to resume updates.
Before initializing the requested root, `image create --bundle` replays
the lock with the local trust policy and reports missing or damaged
payloads from the bundle.
