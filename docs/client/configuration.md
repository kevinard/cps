# Manage image configuration and package preferences

[Documentation](../README.md) · [Clients](README.md)

Each change on this page applies to an inactive image with `--root`, or as
a saved plan that you can [stage in a boot environment](../setups/btrfs.md#capturing-a-running-btrfs-root).
The examples use an image in the `image` directory. Each change command
prints its plan and asks before it applies it; `-y` skips the prompt and
`-n --save-plan PLAN` only saves the plan, as described in
[Save, review, and apply a plan](images.md#save-review-and-apply-a-plan).

## On this page

- [Freezing package versions](#freezing-package-versions)
- [Facets and variants on an existing image](#facets-and-variants-on-an-existing-image)
  - [Facet rules and precedence](#facet-rules-and-precedence)
  - [Per-package facet rules](#per-package-facet-rules)
  - [Changing variants](#changing-variants)
- [Reviewing configuration updates](#reviewing-configuration-updates)
- [Choose the provider of a mediated link](#choose-the-provider-of-a-mediated-link)

## Freezing package versions

Keep a package at its installed version until explicitly unfreezing it.
Freeze and unfreeze changes work offline.

```sh
cps freeze --root image --reason "Validate graphics compatibility" library/libdisplay
cps list --root image --frozen
cps unfreeze --root image library/libdisplay
```

A freeze constrains the exact package version during installs, dependency
resolution, and updates. Other packages can still advance when their
dependencies allow it. A conflicting signed profile update fails with the
freeze's reason and keeps the constraint in place. Freezes follow staged
roots and are recorded in transaction history without timestamps. Removal
and dependency cleanup remain allowed. The freeze survives removal and
constrains later reinstallation.

Specify `NAME@VERSION` to freeze a package that is not installed. A freeze
matches the version exactly as it is spelled in the package ID, so
`library/libdisplay@2.0.0` does not freeze `2.0.0-0`; see
[package selectors](queries.md#package-selectors-and-versions). For an
installed package, an explicit version must match the installed version.
Publisher selectors check the selected installed package, while the stored
constraint applies to the package name and version. This follows
[IPS package freezes](https://docs.oracle.com/cd/E26502_01/html/E28984/gilfr.html)
with CPS's exact package versions.

Run `tests/package-freezes.sh` for the CLI workflow. It also accepts
`CPS_TEST_BINARY` for a packaged executable.

## Facets and variants on an existing image

A **facet** is optional content inside a package, such as headers and `.so`
links (`devel`), manual pages (`doc.man`), locales (`locale.fr`), debug
symbols (`debug.symbols`), or 32-bit multilib objects (`isa.i686`). A
**variant** is a mutually exclusive image property, such as `arch` or
`container`, that selects between alternative content. Neither changes
which packages are installed, and neither adds dependencies. Content that
must be installed on its own, or that needs other packages, is a separate
package. See
[facets, packages, and variants](../concepts.md#facets-separate-packages-and-variants).

```sh
cps facet list --root image
cps facet list --root image locale.fr debug.symbols
cps facet list --root image --package library/sqlite
cps facet set --root image doc.man=true 'locale.*=false' locale.fr=true
```

Without arguments, `facet list` lists every rule with its source: `local` for a
rule set on the image and `default` for a built-in default that no local
rule replaces. With facet names it prints each effective value and the rule
that decided it, such as `debug.* (default)` or `unset` for a facet no rule
names. With `--package` it resolves the facets of one installed package,
including per-package rules, and lists the facets its actions use unless you
name some. Add `--json` for machine-readable output.

A facet or variant change uses the installed manifests and leaves package
versions and catalog checkpoints in place, so it plans offline. The plan
lists `Recompose PACKAGE` for each installed package whose selected actions
change, and writes only the paths of those packages. Triggers that watch
the added or removed files run as part of the change. Enabling an action
may require its payload from the repository or cache, which `facet set`
fetches before it applies the change. The change can be staged in a boot
environment and resumed like an ordinary plan. Rules also apply to packages
that you install later.

The running system does not support `facet set` or `variant set`. To
change facets or variants on a running Btrfs system, capture the root, save
a plan against the capture, and stage it, as described in
[Capturing a running Btrfs root](../setups/btrfs.md#capturing-a-running-btrfs-root).

Disabling a facet records the identity and fingerprint of any edited or
locally deleted configuration it leaves behind. Re-enabling can reclaim the
matching file, restore a pending vendor proposal, or follow a configuration
rename declared with `original-name`. If a file at the disabled path has
changed, CPS reports a conflict. To install the vendor copy in that case,
move the changed file aside and generate a fresh plan.

### Facet rules and precedence

A rule names a facet exactly (`doc.man`), with a trailing wildcard
(`locale.*`, which matches `locale.fr` and `locale.fr.ca` but not `locale`),
or as `*`. Each rule is `NAME=true` or `NAME=false`, and `NAME=default`
removes a rule. CPS decides each facet of each package in this order,
following IPS:

1. The package's own rules. An exact package name beats a `PREFIX/*` glob,
   and a longer glob prefix beats a shorter one. The first package pattern
   with a matching rule decides.
2. Per-package rules that a package it is a dependency of carries with
   `--with-dependencies`.
3. The image-wide rules.
4. The built-in defaults `debug.*=false` and `isa.*=false`.
5. Otherwise the facet is enabled.

Within one set of rules, the most specific match wins: an exact name beats
any wildcard, and a longer wildcard prefix beats a shorter one. Debug
symbols and multilib objects are therefore off on a new image. Enable them
like any other facet, for example `facet set 'debug.*=true'` or
`facet set isa.i686=true`. The defaults are built in, like IPS's implicit
`facet.debug.*` rule, and `cps facet list` shows them. `facet set
'debug.*=default'` returns to the built-in default.

Set initial rules with `image create --facet` and `--variant`, or in the
image configuration:

```kdl
facet "devel" #false
facet "locale.*" #false
facet "locale.fr" #true
facet "devel" #true package="library/sqlite" with-dependencies=#true
variant "container" #true
```

### Per-package facet rules

Write `PACKAGE:FACET=VALUE` to override a facet for one package, or
`PREFIX/*:FACET=VALUE` for every package under a prefix. Package names
contain `/` and facet names contain `.`, and neither contains `:`. In the
image configuration, write the package as a `package` property, as above.

```sh
cps facet set --root image devel=false library/sqlite:devel=true
cps facet set --root image --with-dependencies library/sqlite:devel=true
cps facet set --root image 'apps/editor:debug.symbols=true'
cps facet set --root image library/sqlite:devel=default
```

A per-package rule suits content you want for a few packages only, such as
the debug symbols of the program you are debugging, or the headers of one
library on an image that otherwise drops `devel`.

Headers often `#include` the headers of their dependencies, so enabling
`devel` for one library alone can leave its headers unusable. Add
`--with-dependencies`, or `with-dependencies=#true` in the image
configuration, to apply the rule to the package's installed dependency
closure as well. The closure follows `require`, `require-capability`,
`require-any`, and `conditional` dependencies to installed packages. It
skips recommendations, incorporations, and exclusions. A dependency's own
rule beats an inherited one, and `cps facet list --package` shows an inherited
rule as `RULE (dependency of PACKAGE)`. The closure follows later installs
and removals, so the packages a plan selects decide which rules reach which
dependencies. When the files of two packages collide only under the rules
they inherit, a plan looks for another choice of versions or alternatives
that keeps them apart. It never adds a package that nothing requires just
to hide a collision. Two inherited rules that disagree about the same dependency
stop the plan; add a rule for that dependency to settle it.

On an image that follows a signed release profile, image-wide facets and
variants stay fixed by the profile. You can add per-package rules for
packages you installed locally. A rule that reaches a package the profile
protects, directly, through a glob, or through `--with-dependencies`, is
refused unless the profile allows overriding that facet. See
[profile images](profiles.md#facet-rules-on-profile-images).

### Changing variants

```sh
cps variant list --root image
cps variant set --root image container=true
cps variant set --root image --unset container
```

`variant list` lists the image's values, including the built-in `arch`. Set
other variants with `image create --variant NAME=VALUE`, a `variant` node in
the image configuration, or `variant set`. A variant change works like a
facet change: it reuses the installed manifests and plans offline. The
architecture is fixed when the image is created, and `variant set`
refuses it. Installing a package that selects content by a variant the image
does not set fails until you set it; the error names the variant to set.

## Reviewing configuration updates

When package permissions or ownership change on an edited file, CPS
publishes a new inode with the local contents and the package metadata.
It copies extended attributes before applying permissions, so the POSIX
ACL mask follows the new mode. Existing hard-link aliases retain their
original inode. [Linux access control lists](https://man7.org/linux/man-pages/man5/acl.5.html)

When an upgrade keeps an edited or locally removed file and stages new
vendor defaults, `cps config list` shows the pending file and its vendor
copy. Compare the two copies or merge changes by editing the local file.
Then resolve the update on the inactive root.

```sh
cps config list --root staged
cps config diff --root staged /etc/canopa/session/preferences.conf
cps config resolve --root staged --take vendor etc/canopa/session/preferences.conf
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

An edited configuration file inside a directory that a release replaces
with a file or symlink stops the plan; see
[path transitions](images.md#replace-a-directory-with-a-file-or-symlink).
Maintainers can [declare a move for a preserved file](../maintainer/manifests.md#move-preserved-configuration).

## Choose the provider of a mediated link

A mediator selects which installed package supplies shared symlinks such as
`usr/bin/editor`. See [how maintainers declare a mediator](../maintainer/manifests.md#mediated-links).

```sh
cps mediator list --root image
cps mediator set --root image editor editors/basic
```

Omit the provider to restore automatic selection. Removing the preferred
package makes the next provider active. The preference survives removal
and becomes active again if the package returns. Image locks capture the
preferences, and staged roots carry them through boot and rollback.
