# Build a package from pinned sources

[Documentation](../README.md) · [Package maintainers](README.md)

A build recipe pins everything a build reads: a signed toolchain catalog
and its publisher trust root, a package manifest template for each package
it produces, and each source file. `cps build` runs the recipe's script
offline in a sandbox, turns what the script installed into explicit package
manifests, and writes a bundle that a publisher can verify and publish.

Run `cps build` as an unprivileged user on an x86-64 or AArch64 Linux
worker with Linux 5.11 or newer, unprivileged user namespaces, seccomp, and
a current Bubblewrap at `/usr/bin/bwrap`. Supply a local CPS repository or
synchronized cache that holds the signed toolchain catalog.

## On this page

- [A first recipe](#a-first-recipe)
- [Recipe structure](#recipe-structure)
- [Collecting installed files](#collecting-installed-files)
- [Building several packages from one recipe](#building-several-packages-from-one-recipe)
- [Stripping and debug information](#stripping-and-debug-information)
- [Parallel jobs](#parallel-jobs)
- [Setting build options](#setting-build-options)
- [Debugging a recipe in its build environment](#debugging-a-recipe-in-its-build-environment)
- [Building with several publishers](#building-with-several-publishers)
- [Preparing a source cache](#preparing-a-source-cache)
- [Preparing a snapshot from Git](#preparing-a-snapshot-from-git)
- [Building and publishing with signed evidence](#building-and-publishing-with-signed-evidence)
- [Worker limits](#worker-limits)

## A first recipe

```kdl
schema-version 1 kind="build-recipe"
build architecture="x86_64" epoch=1700000000 {
    toolchain solver="sat-v2" {
        publisher "canopa" catalog="<catalog SHA-256>" key-sha256="<trust root SHA-256>"
        package "devel/base"
    }
    version "1.0.0-1" upstream="1.0.0"
    manifest "hello.kdl" sha256="<manifest SHA-256>"
    source "hello.c" sha256="<source SHA-256>" size=123
    script """
    mkdir -p "$DESTDIR/usr/bin"
    cc -ffile-prefix-map=/build=. -Wl,--build-id "$CPS_SOURCES/hello.c" -o "$DESTDIR/usr/bin/hello"
    """
}
```

The recipe pins its package template, `hello.kdl`, by SHA-256:

```kdl
schema-version 1 kind="package-manifest"
package "demo/hello" publisher="canopa" summary="Print a greeting" license="MIT" {
    collect "usr/bin/*"
}
```

`catalog` is the digest of the toolchain repository's signed catalog root,
such as the digest in its `current` file when you prepare the recipe.
`key-sha256` is the SHA-256 of the publisher trust root file that the worker
pins, such as `sha256sum canopa.trust.json`. Replace the template and
source hashes with their SHA-256 digests and set `size` to the source's
byte count. The template path is relative to the recipe, and source paths
are relative to `--sources` or the source cache. The toolchain package must
provide the shell, compiler, and utilities through its dependencies.

```sh
cps build --repo toolchain-cache --publisher canopa --trust-root canopa.trust.json \
    --sources sources --output hello-build --check-reproducible hello.build.kdl
```

## Recipe structure

A recipe has a `schema-version 1 kind="build-recipe"` header and one `build`
node. `architecture` names the target, and `epoch` sets
`SOURCE_DATE_EPOCH` and the timestamps of the outputs.

| Node | Meaning |
| --- | --- |
| `toolchain solver="sat-v2"` | Required, once. At least one `publisher NAME catalog=... key-sha256=...` pin and at least one `package` request. |
| `manifest PATH sha256=...` | A package template. One or more, up to 64; each produces one package. |
| `version VERSION upstream=...` | The version of every produced package. Optional; without it, each template declares its own `version`. |
| `source PATH sha256=... size=...` | A pinned input, staged under `$CPS_SOURCES`. Add `url` to [fetch it](#preparing-a-source-cache). |
| `script """..."""` | Required. Runs with `/bin/sh -eu` in `/build`, umask `022`. |
| `discard PATTERN` | Installed paths that no package claims and the build may drop. See [collecting](#collecting-installed-files). |
| `strip #false` | Ship ELF objects unstripped. See [debug information](#stripping-and-debug-information). |
| `option NAME DEFAULT` | A [build option](#setting-build-options). |

The script runs with the toolchain as its root filesystem, read-only, and
this environment:

| Variable | Value |
| --- | --- |
| `DESTDIR` | `/out`, where the script installs the package files |
| `CPS_SOURCES` | `/sources`, the pinned sources at their recipe paths, read-only |
| `SOURCE_DATE_EPOCH` | The recipe's `epoch` |
| `CPS_BUILD_JOBS`, `MAKEFLAGS` | The worker's [job count](#parallel-jobs), and `-jN` |
| `CPS_BUILD_NAME` | The value of each declared option `NAME` |
| `PATH` | `/usr/bin:/bin` |
| `HOME`, `TMPDIR` | `/build/home` and `/tmp` |
| `USER`, `LOGNAME`, `HOSTNAME` | `builder`, `builder`, and `cps-build` |
| `LC_ALL`, `LANG`, `TZ` | `C`, `C`, and `UTC` |

A template is a package manifest whose file list can be patterns. It may
omit `version` when the recipe has a `version` node, and it may contain
`collect` rules. The manifests the build publishes are always explicit and
list every path, mode, owner, and payload digest.

## Collecting installed files

Let the build install normally, for example with `make install`, and
describe the file list with patterns. This recipe for SQLite pins its
version in the recipe and its template by digest.

```kdl
schema-version 1 kind="build-recipe"
build architecture="x86_64" epoch=1791504000 {
    toolchain solver="sat-v2" {
        publisher "canopa" catalog="<catalog SHA-256>" key-sha256="<trust root SHA-256>"
        package "devel/base"
    }
    version "3.54.1-1" upstream="3.54.1"
    manifest "sqlite.kdl" sha256="<template SHA-256>"
    discard "usr/lib/*.la"
    discard "usr/share/doc/**"
    source "sqlite-autoconf-3540100.tar.gz" size=3332984 sha256="926ef8d3eab93fbf6e06011901273eec076b25594a4e1cdadd50c987f4f6d64e" url="https://www.sqlite.org/2026/sqlite-autoconf-3540100.tar.gz"
    script """
    tar -xzf "$CPS_SOURCES/sqlite-autoconf-3540100.tar.gz"
    cd sqlite-autoconf-3540100
    ./configure --prefix=/usr --disable-static
    make
    make DESTDIR="$DESTDIR" install
    """
}
```

```kdl
schema-version 1 kind="package-manifest"
package "system/sqlite" publisher="canopa" summary="SQL database engine library" license="blessing" homepage="https://www.sqlite.org/" {
    require "system/elf-runtime"
    collect "usr/lib/libsqlite3.so*"
    collect "usr/include/**"
    collect "usr/lib/pkgconfig/*"
}
```

`cps build` turns each installed regular file and symlink that a rule
matches into an explicit `file` or `symlink` action. A file that shares its
inode with an earlier collected file of the same package becomes a
`hardlink` when its own rule gives it the same mode, owner, facet, and
variants as that file and does not preserve it. Otherwise it stays a `file`
with its own rule's metadata. Explicit actions in the template keep their exact form and take
precedence over patterns, so a template can collect `usr/bin/*` and still
declare `file "usr/bin/su" mode="4755" uid=0 gid=0`. Within one template the
first matching rule wins. Directories are never collected; declare `dir`
actions for directories that need explicit metadata.

### Patterns

Patterns are root-relative paths. A `**` component matches any number of
whole components, and at least one at the end of a pattern, so
`usr/include/**` matches every path under `usr/include` but not the
directory itself. Inside a component, `*` matches any run of characters and
`?` matches one. Every other character is literal. `discard` and trigger
`watch` and `output` patterns use the same syntax.

### Collect rule properties

| Property | Default |
| --- | --- |
| `mode` | `"0755"` when the installed file has an execute bit, otherwise `"0644"` |
| `uid`, `gid` | `0` |
| `preserve` | `#false` |
| `facet` | The built-in facet below. `facet=#null` ships the paths without a facet. |
| `arch`, `variant.NAME` | None |

Without a `facet` property, a collected path gets the first matching
built-in facet:

| Paths | Facet |
| --- | --- |
| `usr/include/**`, `usr/lib/pkgconfig/**`, `usr/share/pkgconfig/**`, `usr/lib/cmake/**`, `usr/share/aclocal/**` | `devel` |
| A symlink matching `usr/lib/*.so` | `devel` |
| `usr/share/man/**` | `doc.man` |
| `usr/share/info/**` | `doc.info` |
| `usr/share/doc/**` | `doc` |
| `usr/lib/debug/**` | `debug.symbols` |
| `usr/share/locale/LANG/**` | `locale.LANG`, lowercased, with characters other than letters, digits, `_`, and `-` replaced by `-`, so `pt_BR` becomes `locale.pt_br` |

### Every installed file needs an owner

Every regular file and symlink that the build installs must belong to
exactly one package or match a `discard` pattern. Otherwise the build fails
and lists the unclaimed paths, as `rpmbuild` does for installed but
unpackaged files:

```text
cps: the build installed 1 file(s) that no output claims; add collect or file actions, or a recipe discard pattern:
  usr/lib/liblz.a
```

Claim the path with a `collect` rule or a `file` action, or add a `discard`
pattern for content that no image should receive. Discard patterns apply
only to paths that no package claims. A `collect` rule that matches nothing
also fails the build, which catches stale templates, and an explicit `file`
action requires an installed regular file at its path.

### Versions in templates

The recipe pins each template by SHA-256, and the build attestation binds
the exact expanded manifest. With a `version` node, templates must omit
`version` and `upstream-version`, so a release update edits only the
recipe's version and source pins, not the templates or their digests. A
template dependency can write `{version}` in its constraint, as in
`require "library/liblz" version="={version}"`, to require the package of
the same build. Without a `version` node, each template declares its own
version.

## Building several packages from one recipe

One build can publish several ordinary packages. Use this when the build
produces content that must be installed on its own or has its own
dependencies, such as a library that other programs link against and the
command-line tools that come with it. Each package has its own name,
metadata, and dependencies, and clients see no difference from packages
built separately. There are no subpackages. Optional content, such as
headers, manual pages, and debug information, stays in its package as a
[facet](manifests.md#choosing-between-a-facet-a-package-and-a-variant).

```kdl
schema-version 1 kind="build-recipe"
build architecture="x86_64" epoch=1767225600 {
    toolchain solver="sat-v2" {
        publisher "canopa" catalog="<catalog SHA-256>" key-sha256="<trust root SHA-256>"
        package "devel/base"
    }
    version "2.4.0-1" upstream="2.4.0"
    manifest "liblz.kdl" sha256="<template SHA-256>"
    manifest "lz.kdl" sha256="<template SHA-256>"
    discard "usr/lib/liblz.a"
    source "lz-2.4.0.tar.gz" sha256="<source SHA-256>" size=481920 url="https://lz.example.org/releases/lz-2.4.0.tar.gz"
    script """
    tar -xzf "$CPS_SOURCES/lz-2.4.0.tar.gz"
    cd lz-2.4.0
    make PREFIX=/usr
    make PREFIX=/usr DESTDIR="$DESTDIR" install
    """
}
```

`liblz.kdl` claims the library, its headers, and its `pkg-config` file:

```kdl
schema-version 1 kind="package-manifest"
package "library/liblz" publisher="canopa" summary="LZ compression library" license="BSD-3-Clause" homepage="https://lz.example.org/" {
    collect "usr/lib/liblz.so*"
    collect "usr/include/**"
    collect "usr/lib/pkgconfig/*"
}
```

`lz.kdl` claims the tools and their manual pages, and requires the library
from the same build:

```kdl
schema-version 1 kind="package-manifest"
package "apps/lz" publisher="canopa" summary="LZ compression tools" license="BSD-3-Clause OR GPL-2.0-only" homepage="https://lz.example.org/" {
    require "library/liblz" version="={version}"
    collect "usr/bin/*"
    collect "usr/share/man/**"
}
```

`cps build` prints one line per package and writes both expanded manifests
to the bundle. The library package gets `usr/lib/liblz.so.1.0.0` and its
`liblz.so.1` link unconditionally, the headers, the `pkg-config` file, and
the unversioned `liblz.so` link in the `devel` facet, and its debug file in
`debug.symbols`:

```kdl
schema-version 1 kind="package-manifest"
package "library/liblz" publisher="canopa" version="2.4.0-1" upstream-version="2.4.0" summary="LZ compression library" license="BSD-3-Clause" homepage="https://lz.example.org/" {
    file "usr/include/lz.h" mode="0644" uid=0 gid=0 facet="devel"
    symlink "usr/lib/liblz.so" target="liblz.so.1" uid=0 gid=0 facet="devel"
    symlink "usr/lib/liblz.so.1" target="liblz.so.1.0.0" uid=0 gid=0
    file "usr/lib/liblz.so.1.0.0" mode="0755" uid=0 gid=0
    file "usr/lib/pkgconfig/lz.pc" mode="0644" uid=0 gid=0 facet="devel"
    file "usr/lib/debug/.build-id/3d/ff3394e4b04b36c16dbd201c952eecdb4e9d3f.debug" mode="0644" uid=0 gid=0 facet="debug.symbols"
}
```

Publication then adds the [generated library dependencies](manifests.md#generated-shared-library-dependencies):
`library/liblz` provides `cap/elf/x86_64/liblz.so.1`, and `apps/lz`
requires it. `cps manifest generated --proto bundle/proto
bundle/manifests/apps/lz.kdl` previews them.

The rules for several packages:

- Every package of one recipe belongs to one publisher, and no name repeats.
- One sandboxed build runs, and its installed tree is partitioned among the
  templates. A path belongs to one package only: an explicit action or
  collect rule in two templates for the same path fails the build.
- Two packages cannot ship ELF objects with the same build ID, because their
  debug information can belong to only one package.
- The bundle holds every expanded manifest, one attestation covers them
  all, and `cps publish-bundle` publishes all of them in one signed catalog
  update. The publish policy must approve each package with the same recipe
  digest. See [Publish and retire packages](../publisher/publishing.md#publishing-policy).

## Stripping and debug information

By default, `cps build` strips the ELF executables and shared objects that
the packages ship and keeps their debug information in the same package.
For each object with symbols or DWARF sections and a GNU build ID, CPS runs
the toolchain's `objcopy` inside the sandbox after the script finishes.
`objcopy --only-keep-debug` writes
`usr/lib/debug/.build-id/xx/yyyy.debug`, where `xxyyyy` is the build ID in
hex, and `objcopy --strip-unneeded` strips the shipped object in place. The
package gains a `file` action for the debug file with mode `0644`, the
object's owner and variants, and facet `debug.symbols`. Debug information is
a facet, not a separate package, because it is optional content that adds
no dependencies; see [debug symbols](manifests.md#example-debug-symbols).

`debug.*` facets are off unless the image enables them, for example with
`facet "debug.symbols" #true` in the image configuration or with a
per-package rule such as `change-facet apps/lz:debug.symbols=true`.
Debuggers find the files by build ID. `objcopy` keeps the build ID note and
the section order, so stripped outputs stay reproducible.

CPS skips files that are not native ELF executables or shared objects, such
as scripts, static archives, relocatable objects and kernel modules, EFI
images, and objects for another architecture. An object with symbols but no
build ID fails the build; link it with `-Wl,--build-id`. The toolchain must
provide `objcopy` when any shipped object needs stripping. Write
`strip #false` in the `build` block to ship objects as installed.

## Parallel jobs

The sandbox exports the worker's job count as `CPS_BUILD_JOBS` and as
`MAKEFLAGS=-jN`, so plain `make` runs in parallel. Pass it to other build
tools explicitly, as in `ninja -j "$CPS_BUILD_JOBS"` or
`cargo build --jobs "$CPS_BUILD_JOBS"`. The count is the `jobs` setting of
the [worker configuration](#worker-limits), or else the CPUs available to
CPS, capped by a configured cgroup CPU quota.

The job count is a worker property, not a recipe input, and the attestation
does not record it. Outputs must not depend on it. `--check-reproducible`
runs the second build with a different job count and fails if any output or
expanded manifest differs. The option name `JOBS` is reserved.

## Setting build options

Declare each configurable option and its default inside the `build` block.
A recipe that uses Make can pass compiler flags like this.

```kdl
option "CFLAGS" "-O2"
script """
make "CFLAGS=$CPS_BUILD_CFLAGS"
make "CFLAGS=$CPS_BUILD_CFLAGS" DESTDIR="$DESTDIR" install
"""
```

CPS exposes a declared option named `NAME` as `CPS_BUILD_NAME` inside the
sandbox. Option names have at most 64 characters, start with an uppercase
ASCII letter, and contain only uppercase letters, digits, and underscores.
Values are at most 16 KiB. The recipe decides how to use each value. Pass
`--option NAME=VALUE` for each override.

```sh
cps build --toolchain-config toolchain.kdl --source-cache source-cache \
    --output custom --check-reproducible --option 'CFLAGS=-O3 -g' hello.build.kdl
```

Both reproducibility runs use the same effective values. CPS saves them in
`custom/build.kdl`, and any builder attestation signs that recipe's digest.
Review the effective recipe before approving its digest for publication. A
changed option produces a different recipe digest from the publisher's
original, so an exact rebuild uses the publisher's archived values.

## Debugging a recipe in its build environment

```sh
cps develop --toolchain-config toolchain.kdl --source-cache source-cache \
    --workspace . hello.build.kdl
cps develop --toolchain-config toolchain.kdl --source-cache source-cache \
    --workspace . hello.build.kdl -- make check
```

The shell or command sees the same verified toolchain, pinned sources,
build options, and offline sandbox as the recipe. It starts in writable
scratch space by default. `--workspace` instead mounts the selected
directory read-write at `/build/workspace` and starts there. The recipe
script is available at `/recipe/build.sh` for explicit execution. Build
outputs in `$DESTDIR` are temporary. Commands inherit the caller's standard
streams and return their exit status.

## Building with several publishers

Pin each publisher when a compiler and its dependencies come from separate
signed catalogs. Each pin gives the catalog digest and the SHA-256 of the
publisher trust root file the worker pins for that publisher, such as
`sha256sum tools.trust.json`.

```kdl
toolchain solver="sat-v2" {
    publisher "canopa" catalog="<catalog SHA-256>" key-sha256="<trust root SHA-256>"
    publisher "tools" catalog="<catalog SHA-256>" key-sha256="<trust root SHA-256>"
    package "devel/base"
    package "cps://tools/devel/compiler@1.0.0-0"
}
```

Unqualified packages use the first pinned publisher that offers the name. A
qualified request can select another pinned publisher. Give the build
worker a KDL file with locations for those same publishers. Relative paths
resolve against the worker's configuration file.

```kdl
schema-version 1 kind="toolchain-config"
publisher "canopa" {
    repository "cache/canopa"
    trust-root "keys/canopa.trust.json"
}
publisher "tools" {
    repository "cache/tools"
    trust-root "keys/tools.trust.json"
}
```

```sh
cps build --toolchain-config toolchain.kdl --source-cache source-cache \
    --output bundle --check-reproducible --builder-signing-key builder.der example.build.kdl
```

CPS verifies each trust root against its pin and reads the exact signed
catalog from its local repository. Build evidence records the ordered pins
and each selected package's publisher, version, and manifest digest. When a
new toolchain catalog is published,
[`cps recipe repin`](orchestration.md#repin-toolchain-catalogs) moves
recipes to it.

## Preparing a source cache

Add a `url="https://upstream.example.org/releases/hello.c"` property to a
pinned `source` entry to fetch it. Use the final HTTPS URL, without
credentials or a fragment. The fetcher checks the exact byte count and
SHA-256 before publishing an object to the cache. It rejects redirects and
rechecks existing objects before reuse. Recipes can share a cache by digest.

```sh
cps source fetch --cache source-cache hello.build.kdl
cps build --repo toolchain-cache --publisher canopa --trust-root canopa.trust.json \
    --source-cache source-cache --output hello-build --check-reproducible hello.build.kdl
```

Use `cps source fetch --from sources --cache source-cache hello.build.kdl`
when a recipe combines checked-in patches with upstream downloads. CPS
checks the local directory for uncached inputs and uses the recipe URLs for
missing files. A local file that fails verification stops the fetch.

`cps source import --from sources --cache source-cache hello.build.kdl`
fills the same cache offline. Builds read the cache offline and verify each
input again before staging it under its recipe path. The `--sources` build
option accepts a directory laid out by recipe path.

## Preparing a snapshot from Git

Use `cps source git-snapshot` to build from a branch, tag, or commit. Supply
a build recipe with its normal manifest and toolchain pins. The recipe
builds exactly one package and has no `version` node, because CPS assigns
the version. The template can use `version="0.0.0-0"` as a placeholder and
can contain `collect` rules, which the snapshot keeps. Write the script to
unpack `$CPS_SOURCES/git.tar` before building.

```sh
cps source git-snapshot --git-repo https://git.example.org/hello.git --branch main \
    --repo packages --trust-root canopa.trust.json --output snapshot hello.build.kdl
cps source fetch --from snapshot/sources --cache source-cache snapshot/build.kdl
```

Pass exactly one of `--branch main`, `--tag v1.2.0`, or
`--commit <full SHA-1 or SHA-256>`. A remote must allow fetching the
requested commit by SHA. Both lightweight and annotated tags resolve to
their commit.

The command writes a tar archive of the resolved commit to
`snapshot/sources/git.tar`. It records `branch@commit`, `tag:NAME@commit`,
or the full commit ID as the manifest's `upstream-version` in
`snapshot/manifest.kdl`, and pins the new manifest and archive by SHA-256 in
`snapshot/build.kdl`. Git submodule contents need separate source pins. Use
the generated recipe and cache with the usual build and publication
commands.

When the tag name is a valid CPS version after removing an optional
lowercase `v`, CPS uses that version directly. For example, `v1.2.0`
becomes `1.2.0`, and `1.3.0-rc.1` keeps its spelling. The manifest still
records the original tag and resolved commit.

Branches, commit SHAs, and other tags reserve increasing versions in the
`0.0.0-N` series. CPS accounts for published and withdrawn versions in that
series independently of numbered releases. A release such as `1.2.0` sorts
above these snapshots in automatic version selection. Concurrent
preparations for the same package fetch and reserve in order. Repeating the
latest snapshot preparation with the same inputs reuses its version while
it remains eligible.

## Building and publishing with signed evidence

```sh
cps build --repo toolchain-cache --publisher canopa --trust-root canopa.trust.json \
    --sources sources --output hello-build --check-reproducible \
    --builder-signing-key builder-private.der hello.build.kdl
cps publish-bundle --repo repo --signing-key private.der --builder-key builder-public.der \
    --recipe-sha256 "$reviewed_recipe_sha256" hello-build
```

CPS resolves build dependencies against the pinned catalog and verifies
their payloads. The toolchain and source mounts are read-only. The recipe
gets a fixed user, hostname, locale, timezone, umask, paths, and
`SOURCE_DATE_EPOCH` with fresh writable scratch space. Bubblewrap isolates
namespaces and seccomp limits sockets to local Unix sockets. Package signing
is a separate step with its own inputs.
[Bubblewrap sandboxing](https://github.com/containers/bubblewrap/blob/main/README.md) ·
[SOURCE_DATE_EPOCH](https://reproducible-builds.org/docs/source-date-epoch/)

`--check-reproducible` repeats the build in a fresh sandbox with a different
job count and requires identical file hashes and expanded manifests before
making the bundle available. Both runs use the same worker kernel and CPU.
`--builder-signing-key` requires it. The key accepts `PATH`, `file:PATH`, or
`command:DESCRIPTOR`, so it can stay in a hardware token or KMS behind an
[external signer](../publisher/mirrors.md#external-signer-protocol). The
helper receives `CPS_SIGNER_CONTEXT=attestation`.

The bundle contains:

| Path | Contents |
| --- | --- |
| `build.kdl` | The effective recipe, including option overrides |
| `templates/` | The pinned templates at their recipe paths |
| `manifests/NAME.kdl` | Each package's expanded manifest, such as `manifests/apps/lz.kdl` |
| `proto/` | The packaged regular-file payloads |
| `attestation.json` | The signed build statement, with `--builder-signing-key` |

The expanded manifests supply the packaged modes, owners, directory
entries, and symlinks.

The worker key signs an in-toto statement in a DSSE envelope after both runs
match. The predicate type is
`urn:canopa:cps:published-build-provenance:v1`. One statement covers every
package of the build. It lists their IDs and binds the recipe and source
digests, pinned publisher catalogs and keys, resolved toolchain manifests,
every payload's hash and length, and each package's exact published KDL
manifest, including its generated library dependencies, as the subject
`manifest/NAME`. One statement per build makes the build the unit of trust:
builders and witnesses sign one run once, and no package can be paired with
payloads from another run. Each package's catalog entry names the same
attestation, and clients verify each package against its own manifest and
payloads in that statement.

`publish-bundle` verifies the statement and checks that each expanded
manifest is what its pinned template produces: the same metadata,
dependencies, and explicit actions, followed only by collected and debug
actions that follow its rules. It then streams the exact attested payloads
into the repository. Provision the worker public key and the expected recipe
digest on the publishing machine from independently reviewed inputs, and
keep the publishing key separate from the worker key.
[in-toto statement specification](https://github.com/in-toto/attestation/blob/main/spec/v1/statement.md) ·
[DSSE signature protocol](https://github.com/secure-systems-lab/dsse/blob/master/protocol.md)

For a package intended for a protected repository, give the publisher the
bundle and the information needed to
[review its recipe and worker evidence](../publisher/publishing.md#publishing-policy).

## Worker limits

Each sandbox run has a wall-clock deadline, bounded scratch mounts, a
per-file size limit, and a log limit. CPS also caps the total size of the
outputs it collects. Pass `--worker-config worker.kdl` to set limits on the
build worker. These settings are separate from the package recipe.

```kdl
schema-version 1 kind="build-worker"
worker timeout-seconds=3600 scratch-bytes=8589934592 file-bytes=8589934592 output-bytes=17179869184 log-bytes=16777216 jobs=8
```

The example shows the defaults, except `jobs`, which defaults to the
available CPUs and accepts 1 to 1024. `scratch-bytes` applies to each of
`/build` and `/tmp`, with a separate cap of 64 MiB for `/run`. On deadline
expiry CPS terminates the sandbox, including its descendants, and the
bundle stays unpublished. The writable output filesystem uses a
worker-provided disk quota when aggregate disk use during a build must be
bounded.

For CPU, memory, and task limits, add a delegated cgroup v2 directory to the
worker configuration.

```kdl
schema-version 1 kind="build-worker"
worker timeout-seconds=3600 {
    cgroup "/sys/fs/cgroup/path/to/cps-worker.service" cpu-percent=200 memory-bytes=8589934592 tasks=256
}
```

For a systemd worker service, delegate `cpu memory pids` and place CPS in a
leaf such as the subgroup selected by `DelegateSubgroup=supervisor`. Enable
the three controllers in the delegated parent's `cgroup.subtree_control`
before building. CPS creates a fresh child cgroup per sandbox run, applies
the limits before execution, disables swap for the child, and removes it
after success, deadline expiry, or memory exhaustion. Configured cgroup
limits are required for the build to start.
[systemd cgroup delegation](https://systemd.io/CGROUP_DELEGATION/) ·
[Linux cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html)

## Tests

Run [`tests/build.sh`](../../tests/build.sh) on a worker with Bubblewrap,
Bash, a C compiler with static libc, binutils, Python 3, and OpenSSL. It
exercises isolation, reproducibility, signed provenance, and package
installation. [`tests/build-outputs.sh`](../../tests/build-outputs.sh) needs
the same tools and builds two packages from one recipe with collect rules,
discard patterns, split debug information, and worker job counts, then
publishes, installs, and verifies a rebuild.
[`tests/systemd-build-worker.sh`](../../tests/systemd-build-worker.sh) also
checks resource enforcement and cleanup in a temporary delegated user
scope. [`tests/sources.sh`](../../tests/sources.sh) uses a local TLS server
to exercise source downloads and cache integrity.
