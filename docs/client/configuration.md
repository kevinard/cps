# Manage image configuration and package preferences

[Documentation](../README.md) · [Clients](README.md)

## On this page

- [Freezing package versions](#freezing-package-versions)
- [Changing facets on an existing image](#changing-facets-on-an-existing-image)
- [Reviewing configuration updates](#reviewing-configuration-updates)
- [Choose the provider of a mediated link](#choose-the-provider-of-a-mediated-link)

## Freezing package versions

Keep a package at its installed version until explicitly unfreezing it.
Freeze and unfreeze changes use saved plans and work offline.

```sh
cps plan --root image --output freeze.json freeze --reason "Validate graphics compatibility" library/libdisplay
cps apply --root image freeze.json
cps freeze --root image
cps plan --root image --output unfreeze.json unfreeze library/libdisplay
cps apply --root image unfreeze.json
```

A freeze constrains the exact package version during
installs, dependency resolution, and updates. Other packages can still advance
when their dependencies allow it. A conflicting signed profile update fails
with the freeze's reason and keeps the constraint in place. Freezes
follow staged roots and are recorded in transaction history without timestamps.
Removal and dependency cleanup remain allowed. The freeze survives removal and
constrains later reinstallation. Specify `NAME@VERSION` to freeze a package
that is not installed. For an installed package, an explicit version must
match the installed version. Publisher selectors check the selected installed
package, while the stored constraint applies to the package name and version.
This follows [IPS package freezes](https://docs.oracle.com/cd/E26502_01/html/E28984/gilfr.html)
with CPS's exact package versions.

Run `tests/package-freezes.sh` for the CLI workflow. It also accepts
`CPS_TEST_BINARY` for a packaged executable. The signed
profile VM test freezes SQLite in both independently assembled base images,
checks a conflicting update, and verifies the retained freeze on the trial,
default reboot, and rollback boots.

Run `cargo test` for the desktop upgrade and interruption tests.

## Changing facets on an existing image

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
move the changed file aside and generate a fresh plan.
Signed release profiles fix their facet policy.

## Reviewing configuration updates

When package permissions or ownership change on an edited file, CPS
publishes a new inode with the local contents and the package metadata.
It copies extended attributes before applying permissions, so the POSIX
ACL mask follows the new mode. Existing hard-link aliases retain their
original inode. [Linux access control lists](https://man7.org/linux/man-pages/man5/acl.5.html)

When an upgrade keeps an edited or locally removed file and stages new
vendor defaults, `cps config list` shows the pending file and its vendor
copy. Compare the two copies or merge changes by editing the local file.
Then save and apply a review plan on the inactive root.

```sh
cps config list --root staged
cps config diff --root staged /etc/canopa/session/preferences.conf
cps plan --root staged --output config-review.json \
    resolve-config etc/canopa/session/preferences.conf --take vendor
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

Maintainers can [declare a move for a preserved file](../maintainer/manifests.md#move-preserved-configuration).

## Choose the provider of a mediated link

A mediator selects which installed package supplies shared symlinks such as
`usr/bin/editor`. See [how maintainers declare a mediator](../maintainer/manifests.md#mediated-links).

```sh
cps mediator --root image
cps plan --root image --output editor.json set-mediator editor --provider editors/basic
cps apply --root image editor.json
```

Omit `--provider` to restore automatic selection. Removing the preferred
package makes the next provider active. The preference survives removal
and becomes active again if the package returns. Image locks capture the
preferences, and staged roots carry them through boot and rollback.
