# Package maintainers

[Documentation](../README.md) · [Concepts](../concepts.md) ·
[CLI reference](../reference/README.md)

A maintainer owns the package definition and its build inputs. The publisher
reviews the resulting package and controls admission to a signed catalog.

1. Write the package manifest: its name, version, license and homepage, its
   dependencies, and the paths it installs. Put optional content such as
   headers, manual pages, and debug symbols in facets of the package.
   [Write a manifest](manifests.md)
2. Declare service presets, account or state creation, or kernel artifacts if
   your package needs them. [Declare systemd and kernel actions](systemd.md)
   If your package ships a tool that rebuilds a cache from files that other
   packages install, declare a trigger.
   [Regenerate shared caches with triggers](triggers.md)
3. Write a build recipe that pins the toolchain, sources, and a template for
   each package the build produces. Build twice in isolated sandboxes and
   sign the matching build evidence when your publisher requires it.
   [Build from pinned sources](builds.md)
4. Test the package in a disposable image. Check installation, upgrades,
   and removal, including any preserved configuration and changes to the
   file layout. [Manage an inactive image](../client/images.md)
5. Send the reviewed recipe and bundle to the publisher.
   [Publication policy](../publisher/publishing.md#publishing-policy)

| Task | Page |
| --- | --- |
| Choose between a facet, a separate package, and a variant | [Manifests](manifests.md#choosing-between-a-facet-a-package-and-a-variant) |
| Write versions and version constraints | [Manifests](manifests.md#versions) |
| Inspect generated `cap/elf/` library dependencies | [Manifests](manifests.md#generated-shared-library-dependencies) |
| Replace a directory with a symlink in a new version | [Manifests](manifests.md#change-the-file-layout-between-versions) |
| Collect installed files with patterns | [Builds](builds.md#collecting-installed-files) |
| Publish several packages from one build | [Builds](builds.md#building-several-packages-from-one-recipe) |

When you maintain many recipes, order their builds, list the rebuilds that
a soname bump requires, and move them to a new toolchain catalog together.
[Plan builds across recipes](orchestration.md)

For projects users install directly from a Git commit or branch, put a
combined package manifest and build recipe at the repository root.
[Package a project for Git installation](git.md)
