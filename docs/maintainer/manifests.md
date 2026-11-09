# Write a package manifest

[Documentation](../README.md) · [Package maintainers](README.md)

A manifest describes one version of a package, including the paths it
owns and the dependencies it needs. Start with a prototype tree, review
the generated KDL, then test the package in an [inactive image](../client/images.md).

## On this page

- [Generating a manifest from a prototype](#generating-a-manifest-from-a-prototype)
  - [Suggesting runtime dependencies](#suggesting-runtime-dependencies)
- [Package metadata and version constraints](#package-metadata-and-version-constraints)
- [Mediated links](#mediated-links)
- [Virtual packages](#virtual-packages)
- [Recommended packages](#recommended-packages)
- [Filesystem permissions](#filesystem-permissions)
- [Linux file capabilities](#linux-file-capabilities)
- [Move preserved configuration](#move-preserved-configuration)
- [Require a trial boot](#require-a-trial-boot)

## Generating a manifest from a prototype

Stage an install into a private prototype tree, for example with `DESTDIR`,
then generate a KDL manifest for review.

```sh
cps manifest generate --proto proto --publisher canopa --version 1.0.0-0 demo/tool > tool.kdl
cps publish --repo repo --proto proto --signing-key private.der tool.kdl
```

The generator emits file, directory, symlink, and hard-link actions in
deterministic order without following symlinks or crossing mounts. For each
regular-file hard-link group it records one payload path and aliases within the
prototype. Groups extending outside the prototype are rejected. Ownership
defaults to UID and GID zero. Use `--uid` and `--gid` to set explicit owners.
The generator captures Linux file capabilities from executable inodes. It
rejects namespaced capability sets, hard-linked symlinks, unsupported permission bits and
filesystem objects, and other extended attributes on regular files or
directories. Review the manifest and add dependency, configuration, and
systemd declarations before publishing. Publication derives the payload
hashes and lengths. [IPS prototype manifest generation](https://docs.oracle.com/cd/E88353_01/html/E37839/pkgsend-1.html)

### Suggesting runtime dependencies

Review the generated manifest against the catalogs for the target image
before publishing it.

```sh
cps manifest dependencies --proto proto --config image.kdl \
    --catalog "canopa=$canopa_catalog_sha256" \
    --catalog "extras=$extras_catalog_sha256" \
    --library-dir /usr/lib --library-dir /lib64 tool.kdl
```

Supply one exact signed catalog digest per publisher in the image config.
The command uses the configured local repositories or metadata caches,
architecture, and facets. It considers the newest active version of each
package name from the first eligible publisher. It reads the pinned
catalogs even if the repositories have since advanced.

The scan reads ELF interpreters and needed libraries, including search
paths relative to `$ORIGIN`. For executable scripts it reads absolute
shebangs, including `env NAME` and simple unquoted `env -S NAME ARGS`
forms. Use repeated `--path-dir` options to set the target command search
order for `env`. The default is `/usr/bin`, then `/bin`. Paths delivered
by the prototype itself satisfy requirements locally.

Standard output contains KDL `require` suggestions with exact versions,
provider IDs, and the files that led to each suggestion. Review the lines
alongside existing dependencies before copying them into the package
block. Missing or ambiguous providers and unsupported inputs produce
diagnostics and a nonzero exit status, with any resolved suggestions still
printed. This is a direct path lookup against signed manifests. Use the
release profile ELF check to validate the final composition and ABI.

Pass `--check` in package CI before publishing to require a matching direct
`require` for every external provider found against the pinned catalogs.
The command prints only missing requirements and fails for missing or ambiguous
providers, unsupported inputs, and declared version ranges that exclude the
detected provider. Use the ELF profile gate to test runtime ABI compatibility.

## Package metadata and version constraints

Set `summary` on the package node and add an `optional` node for each
recommended add-on. Optional dependencies are advisory and can be installed
separately. Their version constraints describe compatible add-on versions.

```kdl
schema-version 1 kind="package-manifest"
package "apps/editor" publisher="canopa" version="1.0.0-0" summary="A lightweight text editor" {
    optional "apps/editor-spellcheck" version=">=1.0.0-0" description="Check spelling while typing"
}
```

Summaries and descriptions are single-line text, limited to 256 and 1024
UTF-8 bytes respectively. The catalog signs the summary and the manifest
digest, which covers the optional dependency descriptions. Publish a new
package version to add this metadata to a published package.

- Package manifests use KDL 2. The `schema-version 1` header requires `kind="package-manifest"`. Image configuration uses `schema-version 1 kind="image-config"`. Its paths resolve relative to the configuration file. It also accepts `architecture "x86_64"` and `facet "doc.man" #false`.
- Set `architecture="x86_64"` on the `package` node for a package that targets one architecture, or pass `--architecture x86_64` to `manifest generate`. Only matching packages participate in dependency resolution and publisher ordering. The signed catalog and manifest must agree, and a source build and its attestation must declare the same target. Omit the package property for portable packages or manifests with per-action architecture selectors. Package IDs are unique within a repository, so publishing the same name and version for two architectures requires separate repositories.
- Inside a package block, declare dependencies as `require "library/libdisplay" version=">=1.0.0-0, <2.0.0-0"` or `incorporate "library/libdisplay" version="=1.0.0-0"`. Use `exclude "library/libdisplay" version=">=2.0.0-0"` to reject a range when the package is present. Omit the version to exclude every version of that package.
- Package versions accept a digit-led release with ASCII letters and digits and single `.`, `+`, `_`, `~`, or `-` separators. An optional numeric epoch precedes `:`. A final dash followed by a number or dotted numbers specifies the packaging revision. When the upstream release itself has a dash before a number, as in `7.1.1-15`, spell the revision explicitly as `7.1.1-15-0`. Versions such as `1:1.3.2-4`, `262.1-1`, `7.2.14.arch1-1`, and `2.44+r50+g1848099f063e-1` can be used directly in package IDs and constraints.
- CPS compares epoch, release, and revision in that order. Omitted epochs and revisions count as zero. Digit runs compare numerically and ignore leading zeros, so `2.0.0`, `2.0.0-0`, and `0:02.0.0` are the same release and `rc10` follows `rc2`. Within the release and the revision, `~` sorts first. A `-` followed by a letter marks a pre-release and sorts next, then the end of the part, then letters, then `+`, a `-` followed by a digit, `.`, and `_`. So `1.0~rc1` < `1.0-rc1` < `1.0` < `1.0b3` < `1.0+b1` < `1.0-15-0` < `1.0.1` < `1.0_1`, and `7.1.1-15-0` follows `7.1.1-1`. Version constraints, upgrades, downgrade checks, obsoletion cutoffs, and snapshot allocation compare releases this way. A constraint accepts spaces after its operator, as in `>= 1.0`.
- Package IDs and `@version` selections keep the original spelling, so `@2.0.0` and `@2.0.0-0` name distinct packages of the same release. Updates never replace an installed package with another spelling of its release. Freezes and signed compositions also match the exact spelling. Use `upstream-version` to preserve a separate upstream label in the signed manifest when it differs from the package version.
- Use a `require-any` block with two or more `package` entries when interchangeable providers can satisfy a dependency. Each alternative accepts the same `version` constraints as `require`. The solver prefers a compatible installed provider that the transaction will retain, then tries the alternatives in the order listed, so list the preferred provider first. Offline removal checks that a compatible provider remains installed. [IPS dependency types](https://docs.oracle.com/cd/E53394_01/html/E54820/dependtypes.html)
- `conditional "desktop/plugin" version="=2.0.0-0" when="runtime/desktop" when-version=">=2.0.0-0"` requires the plugin when the resulting image includes a matching runtime. Omit `when-version` to match any installed version of the runtime. The predicate alone leaves the runtime optional.
- Plans use Resolvo to resolve compatible versions and report incompatible requirements. When selected file actions collide, CPS excludes the conflicting version or pair of versions and retries. Directory sharing still requires identical metadata. SAT resolution has a 60-second time limit. Saved plans record the selected catalog for each publisher and whether to install recommendations. Apply uses those inputs to verify the selection. [Resolvo](https://github.com/prefix-dev/resolvo)
- Actions support `file`, `dir`, `symlink`, and `hardlink`. A symlink specifies `target` in place of `mode`. Use `hardlink "bin/tool" target="usr/bin/tool"` to share the inode of an immutable regular file in the same package. The target is relative to the image root and supplies ownership, mode, and content. The optional `arch` and `facet` selectors match image policy. A hard link's target must be selected whenever the alias is selected. Facets default to enabled and can be set with `image-create --facet doc.man=false`. [IPS hard-link actions](https://docs.oracle.com/cd/E53394_01/html/E54820/pkgterms.html)
- Files marked `preserve=#true` keep local edits through an upgrade, including a package ownership transfer at the same path. Removing a managed action leaves a modified file in place and records its configuration identity so a reinstall can reclaim it. When the vendor file changes, find its new copy under `/var/lib/cps/image/config-updates/<revision>/` inside the image.
- The image must be inactive and reside on one filesystem. Existing explicit directory metadata must match either the installed declaration or the selected action. CPS can update directories in place when their metadata matches the installed declaration. Unmanaged or locally changed directory metadata must already match the selected action. Transitions between a directory and a file or symlink are rejected. When ownership ends, CPS prunes directories it recorded creating if they are empty and their metadata still matches. It keeps pre-existing directories and directories with local content or extended attributes.

## Mediated links

Packages can offer competing implementations of a shared command while
keeping their implementation binaries installed side by side. Mark the
package's normal symlink action with a mediator group.

```kdl
symlink "usr/bin/editor" target="my-editor" uid=0 gid=0
mediate "usr/bin/editor" group="editor" priority=20
```

Only the selected package contributes links for the group. A package can
declare several links for the same group with one priority. CPS selects
the highest priority by default, with package name breaking ties.

## Virtual packages

A mail transport package can declare its capability in the manifest.

```kdl
provides "cap/mail-transport" version="2.0.0-0"
```

A consumer can require the capability by name.

```kdl
require-capability "cap/mail-transport" version=">=2.0.0-0"
```

Capability names begin with `cap/`. The version on `provides` describes the
capability independently of the package version. A requirement with a version
constraint needs a matching versioned provider. Omit the constraint to accept
any declared version or an unversioned provider.

Capability providers have no declared order. CPS prefers providers that the
transaction already requests or retains, then compatible installed providers,
then the provider package name in ascending order. Use a [mediator](#mediated-links)
for a default command among coexisting providers. Plans, profiles, and installed
state record the concrete provider packages.
Multiple providers can coexist if their package constraints and files allow
it. Removing a provider requires another compatible installed provider if a
consumer remains. Install a concrete provider by package name to select it
explicitly. The build toolchain solver also handles capabilities.

## Recommended packages

Use `recommends` in a package manifest for a package that should usually be
installed alongside it.

```kdl
recommends "desktop/editor-help" version=">=2.0.0-0"
```

CPS resolves required packages first, then tries recommendations without
changing the selected versions. It skips unavailable recommendations and
ones that conflict with the selected packages. Newly added recommendations
are tried on upgrade. A previously skipped or removed recommendation stays
omitted unless its declaration changes or another package pulls it in.

Recommended packages are marked automatic unless explicitly requested.
They can be removed without removing the recommending package. Offline
`autoremove` keeps a compatible installed recommendation while its
recommending package is kept. Existing `optional` declarations are advisory.

Pass `--no-install-recommends` to `cps install`, `cps upgrade`, or `cps plan`
to skip this step. Saved plans record whether to install recommendations, so
apply uses the same policy. A signed profile follows its published
composition, which may include recommendations selected at publication.
The build recipe solver considers recommendations when resolving the toolchain.

## Filesystem permissions

Modes use four octal digits. Directories support sticky and setgid bits,
including `1777` for temporary directories and `2775` for shared group
directories. Setuid directories and sticky regular files are rejected.
Regular files can declare setuid or setgid modes such as `4755`, `2755`,
and `6755`. These files must be root-owned executables, with no group or
other write access, `preserve` disabled, and no file capabilities.

CPS sets ownership before permissions, checks that the filesystem retained
the requested values, and prepares privileged files privately before atomic
publication. It probes setid file and setgid directory support before
journaling a transaction. This catches Linux silently clearing setgid when
the caller lacks the required group membership or `CAP_FSETID`.
[Linux chmod behavior](https://man7.org/linux/man-pages/man2/chmod.2.html)

Upgrades can change an explicitly managed directory's ownership and modes
while its metadata still matches the installed declaration. CPS changes
that inode in place and keeps its contents. Local metadata changes stop the
upgrade for review. Use `plan repair` to restore the installed declaration.
Directory changes and privileged file replacements can resume offline from
the journal. Managed hard links follow their target's permissions.

Run `tests/file-modes-vm.sh` with the QEMU and kernel overrides to verify actual
setuid/setgid execution, sticky deletion rules, group inheritance, failure
without `CAP_FSETID`, interrupted offline recovery, repair, isolated boot
clones, and removal of privileges by an upgrade. `tests/boot-export.sh`
checks the bits in the exported ext4 image.

## Linux file capabilities

A signed file action can grant a Linux executable specific capabilities.

```kdl
file "usr/bin/ping" mode="0755" uid=0 gid=0 capabilities="cap_net_raw=ep"
```

Use comma-separated capability names followed by `=` and the permitted (`p`)
or inheritable (`i`) flags. Separate clauses with spaces when the sets differ.
The effective flag (`e`) applies to the entire file, so include it in every
clause when needed. Use `capabilities="="` for an explicit empty set. The
executable must be root-owned and nonwritable by its group and other users.
Hard links to the file share its capability metadata. [Linux capabilities](https://man7.org/linux/man-pages/man7/capabilities.7.html)

Reading or writing file capabilities requires an unmapped UID namespace.
Apply also needs `CAP_SETFCAP` and checks support in the image's staging
directory before recording the transaction. Verification checks the installed
set, including unexpected capabilities on ordinary files. Build workers can
attest manifests with capabilities, and they materialize their toolchains
without those privileges.

## Move preserved configuration

To move a preserved file, set `original-name` on the new file action.
Use the original package name and root-relative path and keep that identity
through later moves, including moves between packages of the same publisher.

```kdl
file "etc/canopa/session/preferences.conf" mode="0600" uid=0 gid=0 preserve=#true original-name="desktop/session:etc/canopa/preferences.conf"
```

The transaction must retire the old path and the new path must be unclaimed.
CPS carries edited contents and extended attributes to the new path with
the new package metadata. A locally deleted file stays absent at the new
path. In both cases CPS stages the new defaults there for review. An
untouched file takes the new defaults directly. In a staged upgrade the
previous boot environment retains its configuration at the old path. [IPS configuration migration](https://docs.oracle.com/cd/E26502_01/html/E21383/glyuz.html)

## Require a trial boot

Package authors can mark packages for staging in the signed manifest.

```kdl
schema-version 1 kind="package-manifest"
package "system/example-runtime" publisher="canopa" version="1.0.0-0" requires-reboot=#true {
    file "usr/lib/libexample.so.1" mode="0644" uid=0 gid=0
}
```

The system commands stage the whole transaction when they install, replace,
or remove a marked package, including dependency changes. For replacements,
CPS checks both the installed and selected manifests. An unchanged marked
package does not require staging. The flag defaults to false, and
distribution `stage-package` rules and boot path checks still apply.
