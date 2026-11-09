# Publish and retire packages

[Documentation](../README.md) · [Publishers](README.md)

A publisher controls the catalog and its signing key. Use
`cps publish` for a reviewed manifest and prototype in a simple local
repository. Use `cps publish-bundle` or `cps repo publish-release`
for repositories that require signed build evidence.

## On this page

- [Initialize a local publishing repository](#initialize-a-local-publishing-repository)
- [Publish many manifests at once](#publish-many-manifests-at-once)
- [Signed catalog format](#signed-catalog-format)
- [Publishing policy](#publishing-policy)
- [Publish a coordinated release](#publish-a-coordinated-release)
- [Publisher signing key rotation](#publisher-signing-key-rotation)
- [Withdrawing and obsoleting packages](#withdrawing-and-obsoleting-packages)
- [Pruning withdrawal history](#pruning-withdrawal-history)
- [Renaming packages within a publisher](#renaming-packages-within-a-publisher)

## Initialize a local publishing repository

On the publishing host, generate an Ed25519 key pair in DER form with
OpenSSL. Run these commands in a private key directory.

```sh
umask 077
openssl genpkey -algorithm ED25519 -outform DER -out private.der
openssl pkey -inform DER -in private.der -pubout -outform DER -out public.der
```

Keep `private.der` with the signing account. Distribute `public.der`
through the channel your clients use to provision trust. For a simple
repository, publish the first reviewed manifest with its prototype tree.

```sh
cps publish --repo repo --proto proto --signing-key private.der hello.kdl
```

The signed catalog digest appears in `repo/current`. Use the
[quickstart](../quickstart.md) for a complete local example. For a
protected repository, set the policy below before the first publication.

## Publish many manifests at once

`cps publish` accepts any number of manifests that share one prototype tree.
CPS publishes them in a single signed catalog update, so a distribution build
signs one catalog per batch instead of one per package. The batch is atomic:
if any manifest fails, the current catalog stays unchanged.

```sh
cps publish --repo repo --proto proto --signing-key private.der manifests/*.kdl
```

A plain publication reads and rewrites only the catalog shards that hold the
published names, so its cost follows the size of the change rather than the
size of the catalog. CPS writes manifests, payloads, and shards without a
per-object fsync and syncs the repository filesystem once, before it writes
the signed root that references them. Repositories with a publish policy, and bundle
publications, still check the complete catalog.

## Signed catalog format

A catalog is a small signed **root** that commits by SHA-256 to
content-addressed **shards**. The **catalog digest**, used as the catalog pin,
is the SHA-256 of the exact bytes of the stored root.

The root is stored at `catalogs/<digest>` as a signed envelope in compact
JSON:

```json
{"signed":"<base64 of the root payload>","signatures":[{"keyid":"<SHA-256 of the Ed25519 public key>","sig":"<128 lowercase hex digits>"}]}
```

Each signature covers the bytes `cps signed catalog root v1`, a newline, and
the exact decoded payload. CPS accepts an envelope only in this byte form:
padded standard base64, lowercase hex, compact JSON without escapes, and
signatures sorted by unique key ID. Every listed signature must verify with an
authorized key. Today the only authorized key is the publisher's current
catalog key, so an envelope carries exactly one signature. The list leaves
room for threshold signing.

The root payload is a JSON object with these fields:

| Field | Meaning |
|-------|---------|
| `format` | `1`. |
| `publisher` | Publisher name. |
| `sequence` | Catalog sequence. The first catalog has sequence 1; each successor increases it. |
| `shards` | Map from a two-hex-digit prefix to the SHA-256 of that shard. Empty shards are omitted. |
| `file_index` | `true` when every shard carries a signed file index. |
| `attestations_required`, `toolchains_verified`, `source_archives_required`, `profiles_required` | Protection flags. Once set, successors keep them. |
| `pruned_through` | Withdrawal retention cutoff, see [Pruning withdrawal history](#pruning-withdrawal-history). |
| `renames`, `key_rotations`, `release_profiles` | Signed renames, key handoffs, and composition profiles. |

A shard holds the records of every package name whose SHA-256 begins with its
prefix, so a catalog has at most 256 shards. It is stored at
`catalog-shards/<digest>` as compact JSON with `format` 1, `publisher`,
`prefix`, and these maps:

| Field | Keyed by | Value |
|-------|----------|-------|
| `entries` | package ID | Active version: `manifest` digest, optional `architecture` and `summary`, and the dependency summary `depends` and `provides`. |
| `attestations` | package ID | Build attestation digest. |
| `build_inputs` | package ID | Archived recipe and reviewed manifest digests. |
| `withdrawals` | package ID | Withdrawn entry, its attestation, and the `sequence` that withdrew it. |
| `retired` | package ID | Pruned withdrawal: `manifest` digest and `withdrawn` sequence. |
| `obsoletions` | package name | Obsoletion cutoff. |
| `file_index` | | Digest of this shard's basename index in `file-indexes/`. |

The dependency summary repeats the manifest's dependencies and provided
capabilities. The planner resolves with these signed summaries and reads full
manifests only for the packages it selects. CPS rejects a manifest whose
dependencies, capabilities, architecture, or summary differ from its catalog
entry.

## Publishing policy

To require attested builds for a release repository, install `publish-policy.kdl` in the repository directory before its first publication. Builder key paths resolve relative to that directory. Approve the reviewed recipe separately for each package version.

```kdl
schema-version 1 kind="publish-policy"
publisher "canopa" {
    builder-key "builder-public.der"
    approve "demo/hello" version="1.0.0-0" sha256="<reviewed recipe SHA-256>"
}
```

Protected repositories accept packages through `publish-bundle` and `repo publish-release`. CPS checks the verified worker key and recipe digest against the policy, independently of the command-line arguments. The publisher signs the protection flag into the catalog, and later publication fails if the policy file is missing. Keep bootstrap packages in a separate toolchain repository. Export carries package attestations and leaves the publisher's local policy and key files on the publishing machine.

For distribution repositories, enable toolchain verification before the
first publication.

```kdl
schema-version 1 kind="publish-policy"
publisher "canopa" verify-toolchains=#true {
    builder-key "builder-public.der"
    approve "demo/hello" version="1.0.0-0" sha256="<reviewed recipe SHA-256>" toolchain-config="../review-toolchain.kdl"
}
```

The signer uses its own KDL toolchain configuration and authenticated local
catalogs to resolve every requested tool and transitive dependency. It uses
the recipe's pinned catalog digests and solver version, then compares the
complete selection with the signed build statement. This works offline
with signed catalogs and manifests and uses any builder trust configured for
those toolchain publishers. The signer can perform the check without
installing the tools or fetching their payloads.

The `toolchain-config` path is relative to the publishing repository. It
accepts the publisher blocks for the recipe’s pinned publishers. A repository can also set it on
individual approvals. With `verify-toolchains=#true`, every approval needs a
configuration and the signed catalog preserves the requirement across
releases.

For independent rebuilds, set `minimum-builders=2` on the policy's `publisher` node and provision a `builder-key` for each worker. Run the same pinned recipe on each worker and pass the additional signed statements with `--witness`. The publishing policy defaults to one worker key.

```sh
cps publish-bundle --repo repo --signing-key private.der --builder-key worker-a-public.der \
    --recipe-sha256 "$reviewed_recipe_sha256" \
    --witness worker-b-public.der=worker-b-build/attestation.json worker-a-build
```

CPS counts distinct authorized keys whose signatures cover the same build statement, including the exact payload hashes and pinned inputs. The release stores those signatures together in the DSSE envelope. Administrators establish worker independence by assigning keys to separately operated workers.

## Publish a coordinated release

Group related package changes and withdrawals in a KDL release file. Use the
reviewed catalog digest from `repo/current` as `base`, or `base="empty"` when
initializing a repository. Bundle, builder key, and witness paths are relative
to the release file's directory.

```kdl
schema-version 1 kind="publish-release"
release "canopa" base="<reviewed catalog SHA-256>" {
    bundle "cps://canopa/demo/hello@2.0.0-0" path="hello-build" builder-key="worker-a-public.der" recipe-sha256="<reviewed recipe SHA-256>" {
        witness "worker-b-build/attestation.json" key="worker-b-public.der"
    }
    withdraw "cps://canopa/demo/hello@1.0.0-0"
}
```

```sh
cps repo publish-release --repo repo --signing-key private.der release.kdl
```

CPS checks the base under the repository lock and uses one policy snapshot for
all bundles. Readers see a single new signed catalog after every action succeeds.
If verification fails, the current catalog stays unchanged. Content-addressed
objects may remain on disk. Historical catalogs remain available for pinned plans.

When only the release profiles change, update the publish policy and publish a
release with `refresh-profiles=#true` and the current `base` digest. The signer
reruns the profile checks and publishes a new catalog if the signed profile
records changed. This lets a primary publisher advance a companion pin without
rebuilding its own packages.

## Publisher signing key rotation

Rotate a publisher's catalog signing key on the publishing repository with both
private keys and the reviewed current catalog digest.

```sh
cps repo rotate-key --repo repo --publisher canopa --base "$reviewed_catalog_sha256" \
    --signing-key old-private.der --new-signing-key new-private.der
```

Both keys sign the handoff, including the base catalog digest and the sequence
where the new key takes effect. The new key signs the successor catalog and all
subsequent publications and withdrawals. The catalog carries the handoff
history, so clients can skip releases and still verify the current signing key.
TUF sync remembers authenticated handoffs even if a payload download fails. It
rejects releases that drop or rewrite known handoffs or backdate a new handoff
into an already authenticated catalog sequence.

Keep the original publisher public key as the trust anchor for export, sync,
images, and pinned build toolchains. It verifies historical catalogs and anchors
newer ones. Export and sign the rotated catalog with the repository's TUF roles
as usual. The publisher and TUF roles have separate keys and rotation procedures.
If the current private key is lost, recovery requires a new trust bootstrap.

## Withdrawing and obsoleting packages

To withdraw an exact version, use the publisher signing key on the publishing
repository, then export and sign a new TUF release.

```sh
cps repo withdraw --repo repo --signing-key private.der cps://canopa/demo/hello@1.0.0-0
cps repo withdrawals --repo repo --publisher canopa --trust-key public.der
```

The signed withdrawal records the original manifest digest and prevents reuse of
that package ID. A cache that has accepted the withdrawal rejects later releases
that omit or change the record. New installs use the remaining available versions.
An installed withdrawn version remains eligible for ordinary retention until it
can be upgraded or is explicitly removed. Saved plans still use their pinned
catalogs. The publishing repository and existing caches retain previously fetched
payloads and build evidence for recovery and auditing. A fresh cache only fetches
objects for active versions.

Withdrawal records stay in every successor catalog until a publisher prunes
them, as described in the next section.

To retire a package name as part of a rolling release, add an `obsolete` action
to the KDL file for `repo publish-release`.

```kdl
schema-version 1 kind="publish-release"
release "canopa" base="<reviewed catalog SHA-256>" {
    obsolete "cps://canopa/library/retired@2.0.0-0"
}
```

The cutoff must be newer than every previously published version of that
package. CPS withdraws its active versions and signs the cutoff into the same
catalog. New install and update plans cannot select versions through the cutoff.
If a selected publisher has obsoleted an installed package and offers no newer
version, resolving a transaction removes it when remaining dependencies allow
that removal. Coordinate changes to dependents in the same release. Removal
preserves locally edited configuration under the ordinary image rules.

An obsoletion reserves the name at its publisher's position in the search order.
A higher version can reintroduce the package, while the cutoff and earlier
withdrawals remain in the signed history. Saved plans still use their pinned
catalogs. Inspect the cutoffs with `cps repo obsoletions --repo repo --publisher
canopa --trust-key public.der`.

## Pruning withdrawal history

A long-lived distribution accumulates withdrawals. To cap that history, prune
the records withdrawn at or before a catalog sequence from the reviewed
current catalog.

```sh
cps repo prune-withdrawals --repo repo --publisher canopa --signing-key private.der \
  --base "$reviewed_catalog_sha256" --through 120
```

CPS replaces each pruned withdrawal with a compact `retired` record. The record
keeps the manifest digest and withdrawal sequence and drops the build evidence
and rebuild inputs. The cutoff is signed into the root as `pruned_through`, can
only move forward, and cannot pass the current sequence. Clients accept a
successor that turns a withdrawal into a retired record only through that
cutoff, with the same manifest digest. Other changes to the history are still
rejected as a fork.

Pruning keeps these guarantees:

- A retired package ID can never be republished.
- Installed copies of a retired version still verify against later catalogs.
- Withdrawals whose manifest a signed release profile composition names are
  kept.
- Historical catalogs keep their full records, so saved plans, image locks, and
  profiles pinned to them still resolve while their objects are available. The
  publishing repository never deletes catalog roots or shards.

Build audits of a retired version need a catalog from before the pruning.

## Renaming packages within a publisher

Publish the successor before the rename or include its build bundle in the
same release. Add a `rename` action to the reviewed release document.

```kdl
schema-version 1 kind="publish-release"
release "canopa" base="<reviewed catalog SHA-256>" {
    rename "cps://canopa/apps/old-editor@2.0.0-0" to="apps/editor"
}
```

The source version is an obsoletion cutoff and must be newer than every
published version of the old name. The publisher signs the rename and
permanently retires the old name. Later releases can rename the successor
again, and clients that skip releases follow the chain to its final name.

`cps upgrade` removes an installed source and selects its successor from
the same publisher in one transaction. It carries forward explicit
installation intent and keeps automatic dependencies automatic unless
another explicit request or the signed profile marks the successor as
explicit. Update packages that depend on the old name as part of the release.
If the installed source is frozen, the successor cannot satisfy a freeze
or other constraints, or the successor is installed from another publisher,
CPS blocks the upgrade. If the source belongs to the protected signed
composition, the new signed profile must include the successor.

Use `cps upgrade --dry-run` or `cps check-updates --json` to preview the
transaction. Other catalog-based package changes wait until the rename
has been applied.

To distribute the new catalog to clients, [export and sign a mirror](mirrors.md).
For an automated production process, see [Rehearse and promote releases](release-process.md).
