# Clients

[Documentation](../README.md) · [Quickstart](../quickstart.md) ·
[CLI reference](../reference/README.md)

Choose the workflow based on the root you want to change.

- **Running system.** Use the system commands on an installed, registered
  CPS-managed Btrfs root. [Manage a running system](system.md)
- **Inactive image.** Save a plan, optionally fetch and validate it, then
  apply it to a directory or prepare a new boot root. [Manage an inactive image](images.md)
- **Signed distribution.** Sync trusted mirrors, install the named profile, and
  follow it on upgrades. [Sync mirrors](mirrors.md) · [Install a profile](profiles.md)

Both workflows run the same journaled transaction. If one stops partway,
[choose a recovery command](images.md#choose-a-recovery-command): resume
it, repair damaged files, or abort it and keep the committed revision.

Use the [package queries](queries.md) to inspect what is installed and
why, and to learn how [selectors and versions](queries.md#package-selectors-and-versions)
work. Use [publisher configuration](publishers.md) to add or move a trusted
source and to understand how CPS follows its trust root through key
rotations. Use [image configuration](configuration.md) to adjust package
freezes, facets, variants, configuration proposals, or link providers.
