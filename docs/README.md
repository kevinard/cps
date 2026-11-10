# CPS documentation

[Project README](../README.md) · [Quickstart](quickstart.md) ·
[Concepts and vocabulary](concepts.md) · [Setup guides](setups/README.md) ·
[CLI reference](reference/README.md)

Start with the [quickstart](quickstart.md) to install a package in a disposable
image. If CPS already manages your machine, go directly to the
[running system guide](client/system.md).

## [Clients](client/README.md)

- [Install, upgrade, and recover a running system](client/system.md)
- [Plan and apply changes to an inactive image](client/images.md)
- [Find packages, inspect ownership, and explain dependencies](client/queries.md)
- [Manage freezes, facets, variants, configuration updates, and mediators](client/configuration.md)
- [Configure publishers and their search order](client/publishers.md)
- [Install a signed profile or replicate an exact composition](client/profiles.md)
- [Sync mirrors and manage cached payloads](client/mirrors.md)

## [Package maintainers](maintainer/README.md)

- [Generate and review a package manifest](maintainer/manifests.md)
- [Pin sources and build a reproducible package](maintainer/builds.md)
- [Enable direct installation from Git](maintainer/git.md)
- [Declare systemd and kernel actions](maintainer/systemd.md)
- [Regenerate shared caches with triggers](maintainer/triggers.md)
- [Order recipe builds, plan rebuilds, and repin toolchains](maintainer/orchestration.md)

## [Publishers](publisher/README.md)

- [Set publication policy and publish a catalog](publisher/publishing.md)
- [Check release compositions and phase upgrades](publisher/profiles.md)
- [Archive inputs and verify independent rebuilds](publisher/rebuilds.md)
- [Export and serve signed mirrors](publisher/mirrors.md)
- [Rehearse and promote releases](publisher/release-process.md)

## [Developers](developer/README.md)

- [Build CPS, navigate the code, and run tests](developer/README.md)
- [Exercise boot, health checks, and rollback in QEMU](developer/boot-tests.md)
- [Build signed packages and toolchains for test fixtures](developer/build-fixtures.md)
- [Regenerate and check the CLI man pages](reference/README.md)
