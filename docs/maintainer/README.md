# Package maintainers

[Documentation](../README.md) · [Concepts](../concepts.md) ·
[CLI reference](../reference/README.md)

A maintainer owns the package definition and its build inputs. The publisher
reviews the resulting package and controls admission to a signed catalog.

1. Install your program into a private prototype tree. Generate and review its
   package manifest, including dependencies, ownership, and file modes.
   [Write a manifest](manifests.md)
2. Declare service presets, account or state creation, or kernel artifacts if
   your package needs them. [Declare systemd and kernel actions](systemd.md)
   If your package ships a tool that rebuilds a cache from files that other
   packages install, declare a trigger.
   [Regenerate shared caches with triggers](triggers.md)
3. Pin the manifest, sources, and toolchain. Build twice in isolated sandboxes
   and sign the matching build evidence when your publisher requires it.
   [Build from pinned sources](builds.md)
4. Test the package in a disposable image. Check installation, upgrades,
   and removal, including any preserved configuration.
   [Manage an inactive image](../client/images.md)
5. Send the reviewed recipe and bundle to the publisher.
   [Publication policy](../publisher/publishing.md#publishing-policy)

When you maintain many recipes, order their builds, list the rebuilds that
a soname bump requires, and move them to a new toolchain catalog together.
[Plan builds across recipes](orchestration.md)

For projects users install directly from a Git commit or branch, put a
combined package manifest and build recipe at the repository root.
[Package a project for Git installation](git.md)
