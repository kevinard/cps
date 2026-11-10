# Build a package from pinned sources

[Documentation](../README.md) · [Package maintainers](README.md)

Run `cps build` as an unprivileged user on an x86-64 or AArch64 Linux worker. It needs Linux 5.11 or newer, unprivileged user namespaces, seccomp, and a current Bubblewrap at `/usr/bin/bwrap`. Supply a local CPS repository or synchronized cache containing a signed toolchain catalog. The recipe pins the catalog and its publisher trust root, the package manifest, and each source file.

```kdl
schema-version 1 kind="build-recipe"
build architecture="x86_64" epoch=1700000000 {
    toolchain solver="sat-v2" {
        publisher "canopa" catalog="<catalog SHA-256>" key-sha256="<trust root SHA-256>"
        package "devel/base"
    }
    manifest "hello.kdl" sha256="<manifest SHA-256>"
    source "hello.c" sha256="<source SHA-256>" size=123
    script """
    mkdir -p "$DESTDIR/usr/bin"
    cc -ffile-prefix-map=/build=. "$CPS_SOURCES/hello.c" -o "$DESTDIR/usr/bin/hello"
    """
}
```

Use the digest of the selected repository catalog, such as the digest recorded in its `current` file at recipe preparation. `key-sha256` is the SHA-256 of the publisher trust root file that the toolchain configuration pins, such as `sha256sum canopa.trust.json`. Replace the manifest and source hashes with their SHA-256 digests and set `size` to the source byte count. The manifest path is relative to the recipe. Source paths are relative to `--sources`. The example toolchain package must provide the shell, compiler, and utilities through its dependencies.

Every toolchain block declares `solver="sat-v2"` and at least one publisher pin.

## On this page

- [Setting build options](#setting-build-options)
- [Debugging a recipe in its build environment](#debugging-a-recipe-in-its-build-environment)
- [Building with several publishers](#building-with-several-publishers)
- [Preparing a source cache](#preparing-a-source-cache)
- [Preparing a snapshot from Git](#preparing-a-snapshot-from-git)
- [Building and publishing with signed evidence](#building-and-publishing-with-signed-evidence)
- [Worker limits](#worker-limits)

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
sandbox. Option names start with an uppercase ASCII letter and can also
contain digits and underscores. The recipe decides how to use each value.
Pass `--option NAME=VALUE` for each override.

```sh
cps build --toolchain-config toolchain.kdl --source-cache source-cache \
    --output custom --check-reproducible --option 'CFLAGS=-O3 -g' hello.build.kdl
```

Both reproducibility runs use the same effective values. CPS saves them
in `custom/build.kdl`, and any builder attestation signs that recipe's
digest. Review the effective recipe before approving its digest for
publication. A changed option produces a different recipe digest from
the publisher's original, so an exact rebuild uses the publisher's
archived values.

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

Use a toolchain block with publisher pins when a compiler and its dependencies
come from separate signed catalogs. Each pin gives the catalog digest and
the SHA-256 of the publisher trust root file the configuration pins for that
publisher, such as `sha256sum tools.trust.json`.

```kdl
toolchain solver="sat-v2" {
    publisher "canopa" catalog="<catalog SHA-256>" key-sha256="<trust root SHA-256>"
    publisher "tools" catalog="<catalog SHA-256>" key-sha256="<trust root SHA-256>"
    package "devel/base"
    package "cps://tools/devel/compiler@1.0.0-0"
}
```

Unqualified packages use the first pinned publisher that offers the name.
A qualified request can select another pinned publisher. Give the build
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

CPS verifies each key and reads the exact signed catalog from its local
repository. Build evidence for this recipe form records the ordered pins
and each selected package's publisher, version, and manifest digest.

## Preparing a source cache

Add a `url="https://upstream.example.org/releases/hello.c"` property to a pinned `source` entry to fetch it. Use the final HTTPS URL. The fetcher checks the exact byte count and SHA-256 before publishing an object to the cache. It rejects redirects and rechecks existing objects before reuse. Recipes can share a cache by digest.

```sh
cps source fetch --cache source-cache hello.build.kdl
cps build --repo toolchain-cache --publisher canopa --trust-root canopa.trust.json \
    --source-cache source-cache --output hello-build --check-reproducible hello.build.kdl
```

Use `cps source fetch --from sources --cache source-cache hello.build.kdl`
when a recipe combines checked-in patches with upstream downloads. CPS checks
the local directory for uncached inputs and uses the recipe URLs for missing
files. A local file that fails verification stops the fetch.

`cps source import --from sources --cache source-cache hello.build.kdl` fills
the same cache offline. Builds read the cache offline and verify each input
again before staging it under its recipe path. The `--sources` build option
accepts a directory laid out by recipe path.

## Preparing a snapshot from Git

Use `cps source git-snapshot` to build from a branch, tag, or commit.
Supply a build recipe with its normal manifest and toolchain pins. The
template manifest can use `version="0.0.0-0"` as a placeholder. Write the script to unpack
`$CPS_SOURCES/git.tar` before building.

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
or the full commit ID in `snapshot/manifest.kdl` and pins the new manifest
and archive by SHA-256 in `snapshot/build.kdl`. Git submodule contents need
separate source pins. Use the generated recipe and cache with the usual
build and publication commands.

When the tag name is a valid CPS version after removing an optional
lowercase `v`, CPS uses that version directly. For example, `v1.2.0`
becomes `1.2.0`, and `1.3.0-rc.1` keeps its spelling. The manifest still
records the original tag and resolved commit.

Branches, commit SHAs, and other tags reserve increasing versions in the
`0.0.0-N` series. CPS accounts for published and withdrawn versions in that
series independently of numbered releases. A release such as `1.2.0` sorts
above these snapshots in automatic version selection. Concurrent preparations
for the same package fetch and reserve in order. Repeating the latest snapshot
preparation with the same inputs reuses its version while it remains eligible.

## Building and publishing with signed evidence

```sh
cps build --repo toolchain-cache --publisher canopa --trust-root canopa.trust.json \
    --sources sources --output hello-build --check-reproducible \
    --builder-signing-key builder-private.der hello.build.kdl
cps publish-bundle --repo repo --signing-key private.der --builder-key builder-public.der \
    --recipe-sha256 "$reviewed_recipe_sha256" hello-build
```

CPS resolves build dependencies against the pinned catalog and verifies their payloads. The toolchain and source mounts are read-only. The recipe gets a fixed user, hostname, locale, timezone, umask, paths, and `SOURCE_DATE_EPOCH` with fresh writable scratch space. Bubblewrap isolates namespaces and seccomp limits sockets to local Unix sockets. Package signing is a separate step with its own inputs. [Bubblewrap sandboxing](https://github.com/containers/bubblewrap/blob/main/README.md) · [SOURCE_DATE_EPOCH](https://reproducible-builds.org/docs/source-date-epoch/)

`--builder-signing-key` accepts `PATH`, `file:PATH`, or `command:DESCRIPTOR`, so the builder key can stay in a hardware token or KMS behind an [external signer](../publisher/mirrors.md#external-signer-protocol). The helper receives `CPS_SIGNER_CONTEXT=attestation`.

The bundle contains the recipe, package manifest, and the declared regular file outputs. The manifest supplies the packaged modes, owners, directory entries, and symlinks. `--check-reproducible` repeats the build in a fresh sandbox and requires identical file hashes before making the bundle available. Both runs use the same worker kernel and CPU.

## Worker limits

Each sandbox run has a wall-clock deadline, bounded scratch mounts, a per-file size limit, and a log limit. CPS also caps the total size of declared outputs it collects. Pass `--worker-config worker.kdl` to set limits on the build worker. These settings are separate from the package recipe.

```kdl
schema-version 1 kind="build-worker"
worker timeout-seconds=3600 scratch-bytes=8589934592 file-bytes=8589934592 output-bytes=17179869184 log-bytes=16777216
```

The example shows the defaults. `scratch-bytes` applies to each of `/build` and `/tmp`, with a separate cap of 64 MiB for `/run`. On deadline expiry CPS terminates the sandbox, including its descendants, and the bundle stays unpublished. The writable output filesystem uses a worker-provided disk quota when aggregate disk use during a build must be bounded.

For CPU, memory, and task limits, add a delegated cgroup v2 directory to the worker configuration.

```kdl
schema-version 1 kind="build-worker"
worker timeout-seconds=3600 {
    cgroup "/sys/fs/cgroup/path/to/cps-worker.service" cpu-percent=200 memory-bytes=8589934592 tasks=256
}
```

For a systemd worker service, delegate `cpu memory pids` and place CPS in a leaf such as the subgroup selected by `DelegateSubgroup=supervisor`. Enable the three controllers in the delegated parent's `cgroup.subtree_control` before building. CPS creates a fresh child cgroup per sandbox run, applies the limits before execution, disables swap for the child, and removes it after success, deadline expiry, or memory exhaustion. Configured cgroup limits are required for the build to start. [systemd cgroup delegation](https://systemd.io/CGROUP_DELEGATION/) · [Linux cgroup v2](https://docs.kernel.org/admin-guide/cgroup-v2.html)

The optional worker key signs an in-toto statement in a DSSE envelope after both runs match. The predicate type is `urn:canopa:cps:published-build-provenance:v1`. The statement binds the recipe and source digests, pinned publisher catalogs and keys, resolved package manifests, output hashes and lengths, and exact published KDL manifest. Provision the worker public key and the expected recipe digest on the publishing machine from independently reviewed inputs. Keep the publishing key separate from the worker key. `publish-bundle` verifies the statement and streams the exact attested payloads into the repository. [in-toto statement specification](https://github.com/in-toto/attestation/blob/main/spec/v1/statement.md) · [DSSE signature protocol](https://github.com/secure-systems-lab/dsse/blob/master/protocol.md)

Run [`tests/build.sh`](../../tests/build.sh) on a worker with Bubblewrap, Bash, a C compiler with static libc, binutils, Python 3, and OpenSSL. It exercises isolation, reproducibility, signed provenance, and package installation. [`tests/systemd-build-worker.sh`](../../tests/systemd-build-worker.sh) also checks resource enforcement and cleanup in a temporary delegated user scope. [`tests/sources.sh`](../../tests/sources.sh) uses a local TLS server to exercise source downloads and cache integrity.

For a package intended for a protected repository, give the publisher
the bundle and the information needed to
[review its recipe and worker evidence](../publisher/publishing.md#publishing-policy).
