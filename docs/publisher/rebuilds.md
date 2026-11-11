# Archive and verify build evidence

[Documentation](../README.md) · [Publishers](README.md)

A publisher can archive the reviewed inputs of every build so that anyone
can rebuild a published package and compare the result with the signed
release. Images and workers can independently require builder signatures.

## On this page

- [Archive build inputs](#archive-build-inputs)
- [Archive build sources](#archive-build-sources)
- [Rebuilding a published package](#rebuilding-a-published-package)
- [Archive mirrors for older pins](#archive-mirrors-for-older-pins)
- [Verifying a rebuild](#verifying-a-rebuild)
- [Verifying builds on images and workers](#verifying-builds-on-images-and-workers)

## Archive build inputs

Set `archive-build-inputs=#true` on the publishing policy's `publisher`
node to make reviewed recipes and their pinned manifest templates available
to rebuilders. CPS requires version 1 build provenance and signs the
digests of the exact documents into the catalog. Every package of a recipe
that builds several packages records the same recipe and templates. Once
the archive is in use, later releases keep existing records and include
inputs for new packages. For a previously published version, republish the
original bundle with its original attestation to add its inputs.

Both full and index-only mirror sync carry the archived inputs, including
those for withdrawn versions.

## Archive build sources

When the source files are available for redistribution, the publisher can
include them in the publication using its build source cache. Set
`archive-build-inputs=#true require-source-archives=#true` on the publish
policy's `publisher` node to require the complete archive for every
published version.

```sh
cps publish release --repo repo --key release-private.der \
    --source-cache publisher-source-cache release.kdl
```

`publish bundle` accepts the same `--source-cache` option. CPS verifies the
source bytes against the reviewed recipe and advances the catalog after the
entire release passes validation. It reuses verified sources already stored
in the repository. A recipe without sources needs no cache. With
`archive-build-inputs=#true` alone, supplying a cache archives sources for
that publication.

Once a signed catalog requires archives, later releases preserve that
requirement. To enable it in an existing repository, first backfill every
published version, including withdrawn versions. For packages with archived
recipes and manifests, use `repo archive-sources` to attach the sources.

```sh
cps repo archive-sources --repo repo --key release-private.der \
    --source-cache publisher-source-cache cps://canopa/demo/hello@1.0.0-0
cps repo export --repo repo --trust-root canopa.trust.json \
    --include-sources --output source-mirror-targets
```

`repo archive-sources` attaches the shared sources to every package of the
build. It works offline with the current catalog and its version 1
attestation, including for packages published before recipe archiving. A
repository signs the archive record only after verifying
every pinned source, and later catalogs preserve the record. The source
archive is available after withdrawal.

Sign the exported targets through the same [TUF release process](mirrors.md).
When the catalog requires source archives, `repo export` includes the
source targets automatically. Sync checks the signed target digests and
sizes before promoting the catalog. Both full and index-only sync leave
source targets for an explicit `source fetch-archived`.

## Rebuilding a published package

Extract the archived inputs, fetch the sources and toolchain, rebuild, and
compare the result with the signed release.

```sh
recipe=$(cps repo rebuild-inputs --repo /var/lib/cps/canopa \
    --trust-root canopa.trust.json --output reviewed cps://canopa/demo/hello@1.0.0-0)
cps source fetch --cache source-cache "$recipe"
cps recipe fetch-toolchain --toolchain-config toolchain.kdl "$recipe"
cps build --toolchain-config toolchain.kdl --source-cache source-cache \
    --check-reproducible --output rebuilt "$recipe"
cps repo verify-rebuild --repo /var/lib/cps/canopa \
    --trust-root canopa.trust.json --bundle rebuilt cps://canopa/demo/hello@1.0.0-0
```

`repo rebuild-inputs` verifies the signed catalog, archived bytes, and
their binding to the admitted attestation before writing a new directory.
The command prints the recipe path, preserves each template's relative
path, and writes the original attestation alongside the recipe directory.
Any package of a multi-package build selects the same inputs. Use
`--catalog SHA256` to select a cached historical catalog.

When the publisher archives sources, prepare the cache from a local
repository or a synchronized source-inclusive mirror instead of contacting
the recipe's upstream URLs. Use this command in place of `cps source fetch`.

```sh
cps source fetch-archived --repo /var/lib/cps/canopa \
    --trust-root canopa.trust.json --cache source-cache cps://canopa/demo/hello@1.0.0-0
```

CPS checks the TUF target digest and size against the verified recipe
before writing to the worker's source cache. Valid cached sources can be
reused offline. The same `--catalog` pin selects a historical archived
release. Supply source archives without URLs through `cps source import` or
`cps source fetch --from`.

The recipe pins the toolchain catalogs. Configure local toolchain
repositories with the pinned catalogs and provisioned trust roots. For a
new mirror cache, sync each toolchain publisher with its catalog digest
from the recipe.

```sh
cps repo sync --repo /var/lib/cps/build/canopa --tuf-root tuf-root.json \
    --url "$mirror_url" --trust-root canopa.trust.json \
    --index-only --require-catalog "$catalog_sha256"
```

Repeat `--require-catalog` when provisioning one cache for several recipes.
The signed mirror release must advertise each digest as current or include
it through `repo export --retain-catalog`. Point the toolchain configuration
at the synchronized caches. `recipe fetch-toolchain` resolves the pinned
selection with the same solver and builder trust policy as the build. It
downloads and verifies the selected payloads across publishers. The build
verifies those payloads again and runs offline.

To find which packages a library change requires you to rebuild, and in
which order, see [Plan builds across recipes](../maintainer/orchestration.md).

## Archive mirrors for older pins

For pins outside the live mirror's retention window, publish a separate TUF
archive for the exact catalog. Export from a repository that still has the
catalog and its objects. CPS checks an older catalog against the
publisher's current catalog before exporting it.

```sh
cps repo export --repo toolchain-repo --trust-root tools.trust.json \
    --archive-catalog "$catalog_sha256" --output archive-targets
```

Sign the targets as a separate TUF repository and provision its trusted
root to rebuilders. Use one archive cache per catalog pin.

```sh
cps repo sync --repo "cache/tools/$catalog_sha256" --tuf-root archive-root.json \
    --url "$archive_url" --trust-root tools.trust.json --index-only \
    --archive-catalog "$catalog_sha256"
```

Point the toolchain configuration at that cache. The archive has no live
catalog pointer and uses independent TUF replay state. CPS checks the exact
pin in the signed archive metadata on sync and on later downloads.
`repo rebuild-inputs` and `source fetch-archived` can also use an archive
cache with an explicit `--catalog` pin. The archive includes required
source targets, and `--include-sources` adds optional sources recorded by
the archived catalog.

## Verifying a rebuild

`repo verify-rebuild` compares the exact recipe and templates in the
bundle, each package's expanded manifest and reconstructed published
manifest, and every payload's length and digest against the signed release.
The published manifest includes the
[generated shared-library dependencies](../maintainer/manifests.md#generated-shared-library-dependencies),
so a rebuild must reproduce those declarations as well. The command rejects
undeclared entries in the payload tree.

The admitted attestation covers every package that the recipe builds, so
verifying any one of them checks all of them: the rebuild must reproduce
every package, and each package's catalog entry must name the same
attestation, which proves the packages came from one build.

To verify a signed rebuild, build with `--key` and pass
that builder's provisioned public key to `repo verify-rebuild --witness-key`.
CPS verifies the witness signature over the same statement, including the
resolved toolchain and source pins. It checks that the key is distinct from
the publisher and the builders who signed the admitted statement.

## Verifying builds on images and workers

Add `build-trust` inside a `publisher` block in the image or toolchain
configuration to require builder signatures independently of publication
policy. Provision the worker keys separately from the repository.

```kdl
schema-version 1 kind="image-config"
publisher "canopa" {
    repository "/var/lib/cps/canopa"
    trust-root "/etc/cps/keys/canopa.trust.json"
    build-trust minimum-builders=2 {
        builder-key "/etc/cps/keys/worker-a.der"
        builder-key "/etc/cps/keys/worker-b.der"
    }
}
```

The threshold defaults to one. CPS verifies evidence from the pinned
catalog for each package entering or changing in an image. It repeats the
check when applying a saved plan. With `--toolchain-config`, CPS checks
selected inputs before materializing the toolchain. The signatures must be
from distinct configured builders, separate from every key in the
publisher's trust root history. The version 1 attestation covers the exact
published manifest and payload digests. For a recipe that builds several
packages, one attestation names all of them, and CPS checks the statement
entries for the package being installed. Cached evidence works offline.

Use `image create --config` to provision this policy on a new image. CPS
stores the public keys in the image state.

To enable or replace build trust on an existing image, put the policy in
its own KDL file. Relative key paths resolve from the file's directory.

```kdl
schema-version 1 kind="build-trust"
build-trust minimum-builders=2 {
    builder-key "worker-a.der"
    builder-key "worker-b.der"
}
```

```sh
cps publisher set-build-trust --root image --policy builders.kdl canopa
cps publisher audit-builds --root image canopa
```

Applying a guarded plan archives each admitted package's signed
attestation, catalog digest, and trust root history in the image database.
The archive follows the installed versions in each boot environment.
`audit-builds` rechecks those signatures against the installed manifest,
the recorded trust root history, and the image's current builder policy,
even without the repository. Where the image has a signed catalog archive,
the envelope must match the attestation digest admitted by the publisher
for that exact manifest. The same check applies when reusing archived
evidence during a signed profile upgrade.

By default, CPS checks the current or profile-pinned catalog's trust root
history and audits every installed package before saving the policy. For a
package without an archived receipt, it checks the installed manifest
against the signed catalog and reads the referenced evidence from the
repository cache. This covers withdrawn packages whose evidence is still
cached. Run `set-build-trust` with the same policy to archive that evidence
for future offline audits. When this records evidence from a later catalog,
CPS keeps one additional signed catalog per publisher to authenticate the
receipt offline. The installed image's catalog checkpoint stays at its
applied revision.

For immediate key revocation, use `set-build-trust --future-only` to change
admission policy even with the repository offline. Use `audit-builds` to
identify installed packages whose evidence fails the new policy. CPS
requires a clean transaction state before changing the policy. A change
invalidates unapplied saved plans and preserves the image's boot identity.
It applies to the named root.

Use `publisher add --build-trust builders.kdl` to set build trust as part
of adding a publisher.
