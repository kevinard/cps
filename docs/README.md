# CPS documentation

[Project README](../README.md) · [Quickstart](quickstart.md) ·
[Concepts and vocabulary](concepts.md) · [Setup guides](setups/README.md) ·
[CLI reference](reference/README.md)

Start with the [quickstart](quickstart.md) to install a package in a disposable
image, and keep the [concepts page](concepts.md) at hand for the vocabulary the
guides use. If CPS already manages your machine, go directly to the
[running system guide](client/system.md).

## Start here

- [Publish and install a package in a disposable image](quickstart.md)
- [Concepts and vocabulary](concepts.md)

## [Setup guides](setups/README.md)

- [Assemble a bootable base root](setups/boot-root.md)
- [Set up Btrfs boot environments](setups/btrfs.md)
- [Deploy ext4 boot environments](setups/ext4.md)
- [List, roll back, and clean up boot environments](setups/boot-operations.md)

## [Clients](client/README.md)

- [Install, upgrade, and recover a running system](client/system.md)
- [Plan, apply, resume, and abort changes to an inactive image](client/images.md)
- [Find packages, inspect ownership, and explain dependencies](client/queries.md)
- [Manage freezes, facets, variants, configuration updates, and mediators](client/configuration.md)
- [Configure publishers, trust roots, and refresh policy](client/publishers.md)
- [Install a signed profile or replicate an exact composition](client/profiles.md)
- [Sync mirrors and manage cached payloads](client/mirrors.md)

## [Package maintainers](maintainer/README.md)

- [Write a manifest and review its generated dependencies](maintainer/manifests.md)
- [Build one or more packages from a pinned recipe](maintainer/builds.md)
- [Order recipe builds, plan rebuilds, and refresh recipe pins](maintainer/orchestration.md)
- [Enable direct installation from Git](maintainer/git.md)
- [Declare systemd and kernel actions](maintainer/systemd.md)
- [Regenerate shared caches with triggers](maintainer/triggers.md)

## [Publishers](publisher/README.md)

- [Publish packages and manage the trust root](publisher/publishing.md)
- [Check release compositions and phase upgrades](publisher/profiles.md)
- [Archive inputs and verify independent rebuilds](publisher/rebuilds.md)
- [Export and serve signed mirrors](publisher/mirrors.md)
- [Rehearse and promote releases](publisher/release-process.md)

## [Developers](developer/README.md)

- [Build CPS, navigate the code, and run tests](developer/README.md)
- [Exercise boot, health checks, and rollback in QEMU](developer/boot-tests.md)
- [Build signed packages and toolchains for test fixtures](developer/build-fixtures.md)

## [Reference](reference/README.md)

- [Commands by task, man pages, and data formats](reference/README.md)
- [Regenerate and check the CLI man pages](reference/README.md#regenerate-the-man-pages)
