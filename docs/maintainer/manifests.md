# Write a package manifest

[Documentation](../README.md) · [Package maintainers](README.md)

A manifest describes one version of one package: its metadata, its
dependencies, and the paths it installs. Write it by hand, generate it from
a prototype tree, or let a [build recipe](builds.md) expand it from file
patterns. Then test the package in an [inactive image](../client/images.md).

## On this page

- [Manifest structure](#manifest-structure)
- [Package metadata](#package-metadata)
- [Versions](#versions)
- [Dependencies](#dependencies)
  - [Version constraints](#version-constraints)
  - [Alternatives and conditions](#alternatives-and-conditions)
  - [Virtual packages](#virtual-packages)
  - [Recommended packages](#recommended-packages)
- [Generating a manifest from a prototype](#generating-a-manifest-from-a-prototype)
- [Generated shared-library dependencies](#generated-shared-library-dependencies)
  - [Reviewing runtime dependencies](#reviewing-runtime-dependencies)
- [Filesystem actions](#filesystem-actions)
  - [Filesystem permissions](#filesystem-permissions)
  - [Linux file capabilities](#linux-file-capabilities)
- [Facets and variants](#facets-and-variants)
  - [Choosing between a facet, a package, and a variant](#choosing-between-a-facet-a-package-and-a-variant)
  - [Example: debug symbols](#example-debug-symbols)
- [Mediated links](#mediated-links)
- [Change the file layout between versions](#change-the-file-layout-between-versions)
- [Move preserved configuration](#move-preserved-configuration)
- [Require a trial boot](#require-a-trial-boot)

## Manifest structure

A manifest is a KDL 2 document with a `schema-version 1
kind="package-manifest"` header and one `package` node. The package node's
properties carry the metadata. Its children declare dependencies,
filesystem actions, and any [systemd and kernel actions](systemd.md) or
[triggers](triggers.md).

```kdl
schema-version 1 kind="package-manifest"
package "apps/editor" publisher="canopa" version="2.1.0-1" upstream-version="2.1" summary="A lightweight text editor" license="MIT OR Apache-2.0" homepage="https://editor.example.org/" source-url="https://editor.example.org/releases/editor-2.1.tar.gz" {
    require "library/libdisplay" version=">=1.4.0-0, <2.0.0-0"
    recommends "apps/editor-help" version=">=2.1.0-0"
    optional "apps/editor-spellcheck" version=">=2.0.0-0" description="Check spelling while typing"
    dir "usr/share/editor" mode="0755" uid=0 gid=0
    file "usr/bin/editor" mode="0755" uid=0 gid=0
    symlink "usr/bin/edit" target="editor" uid=0 gid=0
    file "usr/share/editor/themes.conf" mode="0644" uid=0 gid=0
    file "etc/editor.conf" mode="0644" uid=0 gid=0 preserve=#true
    file "usr/share/man/man1/editor.1" mode="0644" uid=0 gid=0 facet="doc.man"
}
```

| Property | Meaning |
| --- | --- |
| `package NAME` | Slash-separated components of lowercase ASCII letters, digits, `.`, `_`, and `-`, each starting with a letter or digit. |
| `publisher` | The publisher whose catalog will carry the package. Required. |
| `version` | The package [version](#versions). Required, except in a build template whose recipe supplies it. |
| `upstream-version` | A free-form upstream label of at most 256 bytes, shown by `cps info`. |
| `architecture` | Restricts the whole package to one architecture, such as `x86_64`. |
| `summary`, `license`, `homepage`, `source-url` | [Descriptive metadata](#package-metadata). |
| `requires-reboot` | `#true` to [require a trial boot](#require-a-trial-boot). |

Set `architecture` for a package that targets one architecture, or pass
`--architecture` to `manifest generate`. Only matching packages take part in
dependency resolution and publisher ordering, and a source build must
declare the same target. Omit it for portable packages and for manifests
that select actions per architecture with `arch`. Package IDs are unique
within a repository, so publishing one name and version for two
architectures needs two repositories.

Publication derives each file's payload digest and length from the
prototype tree, so an authored manifest never contains `sha256` or `size`.

## Package metadata

| Property | Rules |
| --- | --- |
| `summary` | One line of at most 256 UTF-8 bytes. |
| `license` | An [SPDX license expression](https://spdx.github.io/spdx-spec/v2.3/SPDX-license-expressions/) of at most 1024 bytes, such as `GPL-2.0-or-later WITH Linux-syscall-note` or `(MIT OR Apache-2.0) AND BSD-3-Clause`. |
| `homepage` | The upstream project's HTTPS URL. |
| `source-url` | The HTTPS URL of the upstream release. A [build recipe](builds.md) still pins every source it uses by digest. |

CPS checks the SPDX grammar: identifiers with an optional `+`,
`LicenseRef-` and `DocumentRef-` references, uppercase `AND`, `OR`, and
`WITH`, and parentheses. It does not check identifiers against the SPDX
license list, so new identifiers work. URLs must not carry credentials and
must be written in normalized form, such as `https://www.sqlite.org/` with
its trailing slash.

`optional` nodes name advisory add-ons. Each has a `description` of at most
1024 bytes, and its version constraint describes compatible add-on versions.
CPS never installs an optional package on its own.

The catalog signs each package's summary and manifest digest, and the digest
covers every other field. To change the metadata of a published package,
publish a new version. `cps info NAME` prints the whole manifest.

## Versions

A version has the form `[EPOCH:]RELEASE[-REVISION]`, up to 256 bytes.

- `EPOCH` is a number. Omitted, it is zero.
- `RELEASE` starts with a digit and contains ASCII letters and digits
  joined by single `.`, `+`, `_`, `~`, or `-` separators.
- `REVISION` is the packaging revision: a final `-` followed by a number or
  dotted numbers. Omitted, it is zero.

Because a final dash followed by a number is always the revision, give an
upstream release that ends that way, such as ImageMagick's `7.1.1-15`, an
explicit revision: `7.1.1-15-0`. Distribution versions such as `1:1.3.2-4`,
`262.1-1`, `7.2.14.arch1-1`, and `2.44+r50+g1848099f063e-1` work as written.

CPS orders versions the way dpkg does. It compares the epoch, then the
release, then the revision. Each part is read from the left. Two digit runs
compare numerically, ignoring leading zeros, and letters compare in ASCII
order, uppercase first. Otherwise the first difference decides, in this
order:

| Order | At the first difference | Example |
| --- | --- | --- |
| 1 | `~` | `1.0~rc1` precedes `1.0` |
| 2 | `-` before a letter, a pre-release | `1.0-rc1` precedes `1.0` |
| 3 | The end of the part | `1.0` |
| 4 | A digit run | `1.0.5` precedes `1.0.a` |
| 5 | A letter | `1.0b3` follows `1.0` |
| 6 | `+` | `1.0+b1` follows `1.0b3` |
| 7 | `-` before a digit | `1.0-15-0` follows `1.0+b1` |
| 8 | `.` | `1.0.1` follows `1.0-15-0` |
| 9 | `_` | `1.0_1` follows `1.0.1` |

So `1.0~rc1` < `1.0-rc1` < `1.0` < `1.0b3` < `1.0+b1` < `1.0-15-0` <
`1.0.1` < `1.0_1`, `rc10` follows `rc2`, `1.0` precedes `1.0.0`, and
`7.1.1-15-0` follows `7.1.1-1`.

Versions have a meaning and a spelling. `2.0.0`, `2.0.0-0`, and `0:02.0.0`
have the same meaning: one release. Every newer-or-older decision uses the
meaning: version constraints, upgrades, downgrade checks, obsoletion
cutoffs, and the revisions that Git builds allocate. Identity uses the
spelling: a package ID, an `@VERSION` selector, a freeze, and a signed
composition name one exact spelling, so `@2.0.0` and `@2.0.0-0` are distinct
packages. An upgrade never replaces an installed package with another
spelling of the same release. Pick one spelling convention, such as always
writing the revision, and keep it.

## Dependencies

Declare dependencies as children of the package node.

| Node | Effect |
| --- | --- |
| `require NAME` | The package must be installed. |
| `require-capability CAP` | A package that [provides](#virtual-packages) the capability must be installed. |
| `require-any { package NAME ... }` | One of two or more [alternatives](#alternatives-and-conditions) must be installed. |
| `conditional NAME when=OTHER` | NAME is required when the resulting image contains OTHER. |
| `incorporate NAME version=...` | If NAME is installed, its version must match. Does not install NAME. The constraint is required. |
| `exclude NAME` | NAME must not be installed in the matching versions, or at all without `version`. |
| `recommends NAME` | NAME is [installed when possible](#recommended-packages) and can be removed. |
| `optional NAME description=...` | An advisory add-on that CPS never installs on its own. |

Dependencies cannot carry facet or variant selectors. Facets and variants
choose files inside packages that resolution already selected. Content with
its own dependencies belongs in a [separate package](#choosing-between-a-facet-a-package-and-a-variant).

Plans resolve dependencies with [Resolvo](https://github.com/prefix-dev/resolvo),
a SAT solver with a 60-second time limit, and report incompatible
requirements. When selected file actions collide, CPS excludes the
conflicting version or pair of versions and retries. Saved plans record the
selected catalog for each publisher and whether to install recommendations,
and apply checks the selection against them.
[IPS dependency types](https://docs.oracle.com/cd/E53394_01/html/E54820/dependtypes.html)

### Version constraints

A `version` property holds one or more clauses separated by commas, and a
version must satisfy all of them. Each clause is an operator followed by a
version: `>=`, `<=`, `>`, `<`, or `=`. Spaces may follow the operator.

```kdl
require "library/libdisplay" version=">=1.4.0-0, <2.0.0-0"
require "library/libfont" version="= 3.2.1-2"
exclude "library/libdisplay" version="<1.2.0-0"
```

Clauses compare by meaning, never by spelling, so `2.0.0` satisfies
`>=2.0.0-0` and `=2.0.0-0`, while `2.0.0-0` does not satisfy `<2.0.0`.
`=1.0` does not match `1.0.0`, because those are different releases. A
constraint without an operator, `==`, and `!=` are errors. Omit `version`
to accept every version.

A pre-release sorts before its release, so `>=2.0.0-0` excludes
`2.0.0-rc1` and `2.0.0~rc1`. To admit pre-releases, name the earliest one,
as in `>=2.0.0~rc1`.

### Alternatives and conditions

Use `require-any` when interchangeable packages satisfy a dependency. Each
`package` alternative takes the same `version` constraints as `require`.

```kdl
require-any {
    package "library/spell-hunspell" version=">=1.7.0-0"
    package "library/spell-aspell"
}
conditional "desktop/editor-plugin" version="=2.1.0-1" when="runtime/desktop" when-version=">=46.0-0"
```

The solver keeps a compatible installed alternative that the transaction
retains. Otherwise it tries the alternatives in the written order, so list
the preferred one first. Offline removal checks that a compatible
alternative remains installed.

`conditional` requires the plugin when the resulting image includes a
matching runtime. Omit `when-version` to match any version of the runtime.
The condition never installs the runtime itself.

### Virtual packages

A package can provide a named capability, and a consumer can require it
without naming a package.

```kdl
provides "cap/mail-transport" version="2.0.0-0"
require-capability "cap/mail-transport" version=">=2.0.0-0"
```

Capability names begin with `cap/` and follow the package name rules.
Names under `cap/elf/` are reserved for
[generated shared-library dependencies](#generated-shared-library-dependencies).
The version on `provides` describes the capability, independently of the
package version. A requirement with a version constraint needs a matching
versioned provider. Omit the constraint to accept any declared version or
an unversioned provider.

Capability providers have no declared order. CPS prefers providers that the
transaction already requests or retains, then compatible installed
providers, then the provider name in ascending order. Several providers can
coexist when their constraints and files allow it, and removing one requires
another compatible provider while a consumer remains. Plans, profiles, and
installed state record the concrete provider. Install a provider by name to
choose it, and use a [mediator](#mediated-links) for a default command among
coexisting providers.

### Recommended packages

```kdl
recommends "desktop/editor-help" version=">=2.0.0-0"
```

CPS resolves required packages first, then tries recommendations without
changing the selected versions. It skips recommendations that are
unavailable or conflict with the selection. Recommendations added in a new
version are tried on upgrade. One that was skipped or removed stays omitted
until its declaration changes or another package pulls it in.

Recommended packages are automatic unless the user requests them, and they
can be removed without removing the recommending package. Offline
`autoremove` keeps a compatible recommendation while its recommending
package is kept. `cps install`, `cps upgrade`, and `cps plan` accept
`--no-install-recommends`, and a saved plan records the choice. A signed
profile follows its published composition, which may include
recommendations selected at publication. The build toolchain solver also
considers recommendations.

## Generating a manifest from a prototype

Stage an install into a private prototype tree, for example with `DESTDIR`,
then generate a KDL manifest for review. For packages built from a recipe,
[collect rules](builds.md#collecting-installed-files) let `cps build` write
the file list from what the build installed.

```sh
cps manifest generate --proto proto --publisher canopa --version 1.0.0-0 demo/tool > tool.kdl
cps publish --repo repo --proto proto --signing-key private.der tool.kdl
```

The generator emits file, directory, symlink, and hard-link actions in
deterministic order without following symlinks or crossing mounts. For each
regular-file hard-link group it records one payload path and aliases within
the prototype, and it rejects groups that extend outside the prototype.
Ownership defaults to UID and GID zero; `--uid` and `--gid` set other owners.
It captures Linux file capabilities from executable inodes. It rejects
namespaced capability sets, hard-linked symlinks, unsupported permission
bits and filesystem objects, and other extended attributes on regular files
or directories. Add dependencies, metadata, facets, configuration, and
systemd declarations before publishing.
[IPS prototype manifest generation](https://docs.oracle.com/cd/E88353_01/html/E37839/pkgsend-1.html)

## Generated shared-library dependencies

Publication reads each package's ELF payloads and adds its shared-library
declarations to the signed manifest. You do not write them, and you cannot:
publication rejects an authored manifest that declares a `cap/elf/` name.

- A package that ships a shared object at `DIR/SONAME`, where `SONAME` is
  the object's `DT_SONAME` and `DIR` is a library directory, provides
  `cap/elf/ABI/SONAME`, and `cap/elf/ABI/SONAME/VERSION` for each GNU
  version node that the object defines.
- A package whose ELF objects name a `DT_NEEDED` library that the package
  does not supply itself requires the matching capability. When the object
  requires GNU version nodes from that library, the package requires
  `cap/elf/ABI/SONAME/VERSION` for each non-weak node instead, because a
  version node implies its soname. A library that the object finds in its
  own package, through `DT_RUNPATH`, `DT_RPATH`, or the library directories,
  needs no declaration.

A published manifest for a program linked against SQLite and glibc, and the
SQLite library package, would contain lines such as these:

```text
provides "cap/elf/x86_64/libsqlite3.so.0"
require-capability "cap/elf/x86_64/libc.so.6/GLIBC_2.34"
require-capability "cap/elf/x86_64/libsqlite3.so.0"
```

`ABI` names the machine, class, and byte order of the ELF object:
`x86_64`, `i686`, `x32`, `aarch64`, `aarch64_be`, `arm`, `riscv64`,
`riscv32`, `ppc64le`, `ppc64`, `s390x`, or `loongarch64`. Other objects use
`elfCLASSORDER-MACHINE`, such as `elf32be-8`. A 32-bit and a 64-bit library
with one soname therefore never satisfy each other.

The library directories are `/usr/lib`, `/lib`, `/usr/lib64`, `/lib64`,
`/usr/lib32`, `/lib32`, and the `x86_64-linux-gnu`, `i386-linux-gnu`,
`aarch64-linux-gnu`, and `arm-linux-gnueabihf` directories under `/usr/lib`
and `/lib`. The set is fixed so that every builder and the publisher derive
the same declarations.

Only actions without a facet or variant take part. The package-level
architecture counts as unconditional, so `arch="x86_64"` on an action in an
`x86_64` package does too. Content that a facet or variant selects neither
provides nor requires a capability. A file that starts with the ELF magic
but does not parse as an ELF executable or shared object, such as a
malformed test fixture, is data and takes no part either. `cps manifest
dependencies` reports such files as diagnostics.

Preview the declarations for a manifest and its prototype tree. Each
requirement is followed by a comment listing the files that need it.

```sh
cps manifest generated --proto proto tool.kdl
```

To change what is generated, change the content:

| Situation | What to do |
| --- | --- |
| A library outside the library directories, such as `/usr/lib/app/`, provides nothing, and a consumer in another package that reaches it through `DT_RUNPATH` gets a requirement nothing provides. | Ship the library in a library directory, or ship it and its consumers in one package. |
| A library has no `DT_SONAME`. | It provides nothing. Link it with `-Wl,-soname,NAME`. |
| An interpreter, or a `DT_NEEDED` entry spelled as a path, needs one file at one path. | Declare `require` on the package that ships it. [`manifest dependencies`](#reviewing-runtime-dependencies) suggests the line. |
| A program needs a minimum version of a library package. | Add `require "library/sqlite" version=">=3.54.1-1"` beside the generated requirement. |
| Content selected by a facet or variant needs a library. | Put it in a separate package with its own dependencies. |

A build generates the declarations from its output tree and signs the
digest of the resulting published manifest in its attestation. Publication
generates them again from the stored payloads and requires the same
digest, and the signed catalog binds the manifest and its dependency
summary. The solver uses these capabilities like any other. It refuses to
remove or replace the last provider of a required soname or version node,
and an update keeps a provider until a replacement provides the same
capabilities. The [transaction check](../client/images.md#shared-library-regressions)
covers what the capabilities cannot: interpreters, facet changes, shadowing
through `DT_RUNPATH`, and content selected by facets or variants. When a
soname changes, [`cps rebuild-plan`](orchestration.md#plan-rebuilds-after-a-change)
lists the packages to rebuild.

### Reviewing runtime dependencies

Review the generated manifest against the catalogs for the target image
before publishing it.

```sh
cps manifest dependencies --proto proto --config image.kdl \
    --catalog "canopa=$canopa_catalog_sha256" \
    --catalog "extras=$extras_catalog_sha256" tool.kdl
```

Supply one exact signed catalog digest for each publisher in the image
configuration. The command uses the configured local repositories or
metadata caches, architecture, variants, and facet rules. It considers the
newest active version of each package name from the first eligible
publisher, and it reads the pinned catalogs even if the repositories have
since advanced.

Standard output starts with comments listing the generated declarations
and, for each generated requirement, the catalog packages that provide it.
Do not copy those lines. A generated requirement without a provider in the
pinned catalogs produces a diagnostic, which names any catalog package that
ships a matching file outside the library directories.

The command then suggests path-based `require` lines for what publication
cannot generate. It reads ELF interpreters, `DT_NEEDED` names spelled as
paths, and the needed libraries of content selected by a facet or variant,
with search paths relative to `$ORIGIN`. For executable scripts it reads
absolute shebangs, including `env NAME` and simple unquoted
`env -S NAME ARGS` forms. Repeat `--path-dir` to set the target command
search order for `env`; the default is `/usr/bin`, then `/bin`. Paths that
the prototype itself delivers satisfy requirements locally. Interpreters
are files at fixed paths, so a direct `require` on the package that ships
the file is the declaration.

Each suggestion has an exact version, the provider ID, and the files that
led to it. Review the lines alongside existing dependencies before copying
them into the package node. Missing or ambiguous providers and unsupported
inputs produce diagnostics and a nonzero exit status, and the resolved
suggestions are still printed.

Pass `--check` in package CI before publishing. It requires a matching
direct `require` for every external path-based provider and a provider for
every generated requirement. It prints only what is missing, and fails for
missing or ambiguous providers, unsupported inputs, and declared version
ranges that exclude the detected provider. The release profile ELF check
then validates the final composition and ABI.

## Filesystem actions

Paths are relative to the image root, without a leading `/`. Every action
other than `hardlink` sets `uid` and `gid`.

| Action | Properties |
| --- | --- |
| `file PATH` | `mode`, and optionally `preserve=#true` for [configuration](#move-preserved-configuration), `original-name`, and `capabilities`. |
| `dir PATH` | `mode`. Declare a directory when it needs explicit metadata or must exist while empty. |
| `symlink PATH` | `target`, written as the link's contents. |
| `hardlink PATH` | `target`, an image-relative path to an immutable `file` of the same package, which supplies ownership, mode, and content. |

Every action also accepts the `facet`, `arch`, and `variant.NAME`
[selectors](#facets-and-variants). A hard link's target must be selected
whenever the link is, so the target has no facet or the same facet, and its
variants are a subset of the link's.
[IPS hard-link actions](https://docs.oracle.com/cd/E53394_01/html/E54820/pkgterms.html)

A file marked `preserve=#true` keeps local edits through an upgrade,
including a package ownership transfer at the same path. Removing the
package leaves a modified file in place and records its identity, so a
reinstall can reclaim it. When the vendor file changes, CPS stages the new
copy under `/var/lib/cps/image/config-updates/<revision>/` inside the image.

Directories that several packages declare must have identical metadata.
Directories that no package declares are created as needed. When ownership
ends, CPS prunes directories that it created if they are empty and their
metadata still matches, and keeps pre-existing directories and directories
with local content or extended attributes. CPS can update an explicitly
declared directory in place while its metadata matches the installed
declaration. Unmanaged or locally changed directory metadata must already
match the selected action.

A transaction writes each package's files after the files of the packages it
depends on, including providers of `require-capability` and generated
`cap/elf/` requirements. Within a package, shared libraries come first and
executables last, and removals run in reverse order after every new file is
in place. Declare runtime dependencies completely, so that a program
started during a live upgrade finds the libraries it needs.

### Filesystem permissions

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
upgrade for review; `plan repair` restores the installed declaration.
Directory changes and privileged file replacements can resume offline from
the journal. Managed hard links follow their target's permissions.

Run `tests/file-modes-vm.sh` with the QEMU and kernel overrides to verify
setuid and setgid execution, sticky deletion rules, group inheritance,
failure without `CAP_FSETID`, interrupted offline recovery, repair,
isolated boot clones, and removal of privileges by an upgrade.
`tests/boot-export.sh` checks the bits in the exported ext4 image.

### Linux file capabilities

A file action can grant a Linux executable specific capabilities.

```kdl
file "usr/bin/ping" mode="0755" uid=0 gid=0 capabilities="cap_net_raw=ep"
```

Use comma-separated capability names followed by `=` and the permitted
(`p`) or inheritable (`i`) flags. Separate clauses with spaces when the sets
differ. The effective flag (`e`) applies to the entire file, so include it in
every clause when needed. Use `capabilities="="` for an explicit empty set.
The executable must be root-owned and not writable by its group or other
users. Hard links to the file share its capability metadata.
[Linux capabilities](https://man7.org/linux/man-pages/man7/capabilities.7.html)

Reading or writing file capabilities requires an unmapped UID namespace.
Apply also needs `CAP_SETFCAP` and checks support in the image's staging
directory before recording the transaction. Verification checks the
installed set, including unexpected capabilities on ordinary files. Build
workers can attest manifests with capabilities, and they materialize their
toolchains without those privileges.

## Facets and variants

Mark optional content inside a package with a `facet`, and alternative
content with variants. Both apply to `file`, `dir`, `symlink`, and
`hardlink` actions.

```kdl
file "usr/include/sqlite3.h" mode="0644" uid=0 gid=0 facet="devel"
symlink "usr/lib/libsqlite3.so" target="libsqlite3.so.0" uid=0 gid=0 facet="devel"
file "usr/share/locale/fr/LC_MESSAGES/sqlite.mo" mode="0644" uid=0 gid=0 facet="locale.fr"
file "usr/lib32/libsqlite3.so.0" mode="0755" uid=0 gid=0 facet="isa.i686"
symlink "lib64" target="usr/lib" uid=0 gid=0 arch="x86_64"
symlink "etc/init.conf" target="../usr/lib/init/container.conf" uid=0 gid=0 variant.container="true"
symlink "etc/init.conf" target="../usr/lib/init/metal.conf" uid=0 gid=0 variant.container="false"
```

- A facet name is one or more dot-separated components of lowercase ASCII
  letters, digits, `_`, and `-`, such as `devel`, `doc.man`, `locale.pt_br`,
  or `debug.symbols`. It starts with a letter or digit. An action
  has at most one facet. Images choose facets by exact name or with a
  trailing wildcard such as `locale.*`, so name related facets under a
  common prefix.
- A facet that no rule matches is enabled, except `debug.*` and `isa.*`,
  which are off unless the image enables them.
- `variant.NAME="VALUE"` selects the action only when the image sets that
  variant to that value. `#true` and `#false` stand for `"true"` and
  `"false"`. `arch="x86_64"` is shorthand for `variant.arch="x86_64"`. An
  action can carry several variants and is selected when all of them match.
- Installing a package that selects content by a variant the image does not
  set fails, and the solver tries other versions. Publish a variant only
  together with the images that set it.
- Several actions can share a path only when every pair requires different
  values of one variant, so no image can select two of them. A facet
  difference is not enough, because both facets can be enabled. Publication
  takes a file's payload from the prototype at its path, so alternatives at
  one path share a payload. Put distinct payloads at distinct paths and
  select a symlink by variant, as above.

An image chooses facets for all packages and can override the choice for one
package or a prefix of packages. Facet and variant changes recompose the
installed packages offline without resolving again. See
[facets and variants on an existing image](../client/configuration.md#facets-and-variants-on-an-existing-image).

### Choosing between a facet, a package, and a variant

Each problem has one mechanism. Pick it with this rule:

| Use | When | Examples |
| --- | --- | --- |
| Facet | Optional content inside one package. It never adds dependencies. | `devel` headers and `.so` links, `doc.man`, `locale.fr`, static libraries, `debug.symbols`, `isa.i686` multilib objects |
| Separate package | The content is installed or removed on its own, or has its own dependencies. | A runtime library split from its compiler, a plugin that requires another service, a daemon split from its client library |
| Variant | A mutually exclusive property of the image selects between alternatives. | `arch`, a container or bare-metal layout |

Ship 32-bit multilib objects as `isa.*` facets of the 64-bit package, the
way illumos ships both in one package. A header set whose headers include
the headers of other packages is still a facet: a client applies a
per-package facet rule to the package's dependencies too, with
[`--with-dependencies`](../client/configuration.md#per-package-facet-rules).
CPS has no subpackages. When one build must produce two installable units,
the recipe [publishes two ordinary packages](builds.md#building-several-packages-from-one-recipe).

### Example: debug symbols

Debug information is optional content of the package that ships the
stripped program. It needs no other package and no dependencies, so it is
the `debug.symbols` facet, not a separate debug package and not a variant.

```kdl
file "usr/bin/editor" mode="0755" uid=0 gid=0
file "usr/lib/debug/.build-id/87/8fa85a7e413fb286f8caa40d1d5d63f1680a04.debug" mode="0644" uid=0 gid=0 facet="debug.symbols"
```

`cps build` writes these actions itself when it
[strips the shipped objects](builds.md#stripping-and-debug-information), so
a recipe author writes neither the debug path nor the facet. Images leave
`debug.*` off by default. A developer turns the symbols on for one package,
or for all packages, without changing what is installed:

```sh
cps plan --root image --output debug.json change-facet apps/editor:debug.symbols=true
cps plan --root image --output debug-all.json change-facet 'debug.*=true'
```

A debug build of the whole system, in contrast, would replace content
rather than add to it, and would be a variant.

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
the highest priority by default, with the package name breaking ties.
Users inspect the choice with `cps mediator` and change it with
[`plan set-mediator`](../client/configuration.md#choose-the-provider-of-a-mediated-link).

## Change the file layout between versions

A new version can change any path's type. A directory can become a file
or symlink, such as `bin` becoming a link to `usr/bin` in a merged `/usr`,
and a file or symlink can become a directory. Clients apply the change in
one transaction and swap the old and new objects atomically; see
[path transitions](../client/images.md#replace-a-directory-with-a-file-or-symlink).

```kdl
schema-version 1 kind="package-manifest"
package "system/filesystem" publisher="canopa" version="2.0.0-0" {
    dir "usr/bin" mode="0755" uid=0 gid=0
    symlink "bin" target="usr/bin" uid=0 gid=0
    file "usr/bin/sh" mode="0755" uid=0 gid=0
}
```

Version `1.0.0-0` of this package shipped `dir "bin"` and `file "bin/sh"`.
Version `2.0.0-0` moves the file to `usr/bin/sh` and declares the symlink.
The plan shows `Replace bin with a symbolic link to usr/bin`. When you
replace a directory:

- Stop shipping every path inside it. When another package also ships
  files there, publish a version of that package that moves them. The
  solver treats the old version as a path conflict with the new layout, so
  it updates both packages together, or keeps the old layout and reports
  the conflict when you request the new version explicitly.
- Expect the plan to refuse when the directory holds unmanaged files,
  locally modified preserved configuration, or retained configuration. CPS
  never discards local content and does not relocate it. It lists each
  path, and the administrator moves or removes them, or restores vendor
  configuration with `revert`, and plans again.
- To carry edited configuration out of the directory, give it a new path
  outside it with [`original-name`](#move-preserved-configuration).

When a file becomes a directory, a preserved file at that path must still
hold its vendor contents, or the plan stops for the same reason.

## Move preserved configuration

To move a preserved file, set `original-name` on the new file action. Use
the original package name and root-relative path, and keep that identity
through later moves, including moves between packages of the same
publisher.

```kdl
file "etc/canopa/session/preferences.conf" mode="0600" uid=0 gid=0 preserve=#true original-name="desktop/session:etc/canopa/preferences.conf"
```

The transaction must retire the old path, and the new path must be
unclaimed. CPS carries edited contents and extended attributes to the new
path with the new package metadata. A locally deleted file stays absent at
the new path. In both cases CPS stages the new defaults there for review.
An untouched file takes the new defaults directly. In a staged upgrade the
previous boot environment keeps its configuration at the old path.
[IPS configuration migration](https://docs.oracle.com/cd/E26502_01/html/E21383/glyuz.html)

## Require a trial boot

Mark a package whose changes must be tested by booting them.

```kdl
schema-version 1 kind="package-manifest"
package "system/example-runtime" publisher="canopa" version="1.0.0-0" requires-reboot=#true {
    file "usr/lib/libexample.so.1" mode="0644" uid=0 gid=0
}
```

The system commands stage the whole transaction, including dependency
changes, in a new boot environment when they install, replace, or remove a
marked package. For a replacement, CPS checks both the installed and the
selected manifests. An unchanged marked package does not require staging.
The flag defaults to `#false`, and distribution `stage-package` rules and
boot path checks still apply. See [Boot operations](../setups/boot-operations.md).
