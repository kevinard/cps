# Publish and retire packages

[Documentation](../README.md) · [Publishers](README.md)

A publisher controls the catalog, its trust root, and its signing keys. Use
`cps publish` for a reviewed manifest and prototype in a simple local
repository. Use `cps publish-bundle` or `cps repo publish-release`
for repositories that require signed build evidence.

## On this page

- [Initialize a local publishing repository](#initialize-a-local-publishing-repository)
- [Publish many manifests at once](#publish-many-manifests-at-once)
- [Signed catalog format](#signed-catalog-format)
- [Publishing policy](#publishing-policy)
- [Publish a coordinated release](#publish-a-coordinated-release)
- [External catalog signing keys](#external-catalog-signing-keys)
- [Publisher trust root](#publisher-trust-root)
- [Rotate, revoke, and recover keys](#rotate-revoke-and-recover-keys)
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

Create the repository's [trust root](#publisher-trust-root). For a simple
repository, one key can be both the only root key and the only catalog key.

```sh
cps repo trust init --repo repo --publisher canopa --key private.der
```

This writes `repo/trust/1.json`, valid for one year, and prints the SHA-256
fingerprint of its root key. Keep `private.der` with the signing account.
Distribute `repo/trust/1.json` through the channel your clients use to provision
trust, and publish the root key fingerprint separately so clients can check it.
Then publish the first reviewed manifest with its prototype tree.

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
signatures sorted by unique key ID. The authorized keys are the catalog keys of
the newest trust root version in the root's `trust` field. The envelope must
carry exactly the catalog threshold of signatures, each by a distinct
authorized key, and every signature must verify. Requiring exactly the
threshold keeps one signed root from having several valid byte forms.

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
| `renames`, `release_profiles` | Signed renames and composition profiles. |
| `trust` | Every [trust root](#publisher-trust-root) envelope from version 1 through the version the catalog is signed under. |

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

To require attested builds for a release repository, install `publish-policy.kdl` in the repository directory before its first publication. Builder key paths resolve relative to that directory. Approve the reviewed recipe separately for each package version. A recipe that [builds several packages](../maintainer/builds.md#building-several-packages-from-one-recipe) needs an approval with the same digest for each of them.

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
all bundles. A `bundle` action names one package of the bundle; CPS publishes
every package that the bundle's build produced, because they share one
attestation. Readers see a single new signed catalog after every action succeeds.
If verification fails, the current catalog stays unchanged. Content-addressed
objects may remain on disk. Historical catalogs remain available for pinned plans.

When only the release profiles change, update the publish policy and publish a
release with `refresh-profiles=#true` and the current `base` digest. The signer
reruns the profile checks and publishes a new catalog if the signed profile
records changed. This lets a primary publisher advance a companion pin without
rebuilding its own packages.

## External catalog signing keys

Every `--signing-key`, every trust root `--key`, and `CPS_RELEASE_SIGNING_KEY`
accept `PATH`, `file:PATH`, or `command:DESCRIPTOR`. A descriptor names an
external signer that holds the Ed25519 key, such as a PKCS#11 token or a cloud
KMS. Its `public-key` is the key the trust root lists. CPS checks each returned
signature against that key before it uses it. The helper receives
`CPS_SIGNER_CONTEXT` set to `catalog`, `trust-root`, or `attestation`. The
descriptor format and helper protocol are described in
[Release keys and external signers](mirrors.md#external-signer-protocol).

```sh
cps repo publish-release --repo repo \
    --signing-key command:/etc/cps/signers/catalog.signer.kdl release.kdl
```

When the catalog threshold is more than one, repeat `--signing-key` once for
each required catalog key. Every publishing command takes the same set.

```sh
cps publish --repo repo --proto proto \
    --signing-key command:/etc/cps/signers/catalog-a.signer.kdl \
    --signing-key command:/etc/cps/signers/catalog-b.signer.kdl hello.kdl
```

## Publisher trust root

The trust root states which keys may sign the publisher's catalogs. It follows
the TUF root and targets split inside CPS, so it works the same for a local
`--repo` repository and for a TUF mirror.

- **Root keys** are offline keys. A threshold of them signs every trust root
  version. They never sign catalogs.
- **Catalog keys** are online keys. Exactly the catalog threshold of them signs
  every catalog root.

Each version is stored at `trust/<version>.json` in the publishing repository,
in the same canonical envelope as a catalog root. Its signatures cover the bytes
`cps publisher trust root v1`, a newline, and the decoded payload:

```json
{"publisher":"canopa","version":2,"expires":"2027-11-10T00:00:00Z","root_keys":["<64 hex digits>"],"root_threshold":1,"catalog_keys":["<64 hex digits>","<64 hex digits>"],"catalog_threshold":2}
```

Keys are raw Ed25519 public keys in lowercase hex, sorted. A role lists 1 to 16
keys and a threshold no larger than its key count. A key ID or fingerprint is the
SHA-256 of the raw key, as printed by `cps repo trust show`.

- Version 1 is valid when a threshold of its own root keys signs it.
- Each later version is valid only when it is signed by the root threshold of
  the previous version **and** the root threshold of the new version.
- Every catalog root carries the whole chain from version 1 in its `trust`
  field, at most 64 versions, and is signed under the newest one. A successor
  catalog must extend its predecessor's chain unchanged.

Builder keys are not part of the trust root. Build attestations are evidence
that is independent of the publisher. Clients choose builder keys in their own
[build trust](rebuilds.md#verifying-builds-on-images-and-workers) policy and the
publisher in its publish policy, so a compromised root key cannot also
authorize its own builders. CPS rejects any builder key that appears anywhere
in the trust root history.

### Expiry

Every version has an `expires` time, one year after creation unless you pass
`--expires`. A client accepts a new current catalog only while the trust root
version it is signed under is unexpired, and CPS refuses to publish under an
expired version. Renew before then by signing a new version with the root keys
and [re-signing the current catalog](#re-sign-the-current-catalog). A pinned
historical catalog stays valid after its trust root expires.

## Rotate, revoke, and recover keys

Every change is a new trust root version that the root keys sign. Clients learn
of it from the next catalog, so follow each rotation with a publication or a
re-signed catalog.

### Rotate or revoke keys

`cps repo trust rotate` writes the next version. Omitted options keep the
previous version's values. `--root-key` and `--catalog-key` take public keys in
DER, PEM, or hexadecimal form, or `command:` descriptors, and replace that role's
whole key list. Each `--key` is a root key that signs: list enough keys to meet
both the old and the new root thresholds.

```sh
# Renew the expiry with the same keys.
cps repo trust rotate --repo repo --key root-a.der

# Move to two of three catalog keys, which revokes every catalog key not listed.
cps repo trust rotate --repo repo --key root-a.der \
    --catalog-key catalog-a.pub --catalog-key catalog-b.pub --catalog-key catalog-c.pub \
    --catalog-threshold 2

# Replace a root key: both the old and the new root keys sign.
cps repo trust rotate --repo repo --key root-a.der --key root-b.der --root-key root-b.pub
```

Revoking a key means publishing a version that omits it. A catalog signed by the
revoked key under an older version is no longer accepted as a client's new
current catalog once the client has seen the newer version.

### Sign with offline root keys

When root keys are held by different people or on air-gapped machines, use the
detached flow. Draft the next version, sign it on each signing machine, then
combine the signatures on the publishing host.

```sh
cps repo trust draft --repo repo --catalog-key catalog-new.pub --output trust-2.draft
cps repo trust sign --draft trust-2.draft --key root-a.der --output root-a.sig
cps repo trust sign --draft trust-2.draft --key command:/etc/cps/signers/root-b.signer.kdl --output root-b.sig
cps repo trust commit --repo repo --draft trust-2.draft --signature root-a.sig --signature root-b.sig
cps repo trust show repo
```

`commit` checks both thresholds and that the version follows the repository's
newest version before it writes `trust/<version>.json`. A draft for
version 1 needs `--publisher`, `--root-key`, and `--catalog-key`. Use it instead
of `cps repo trust init` to start with several keys.

### Re-sign the current catalog

Publish the exact current catalog again under the newest trust root version
with its catalog keys:

```sh
cps repo resign --repo repo --publisher canopa --signing-key catalog-new.der \
    --base "$(cat repo/current)"
```

The successor has the next sequence and reuses every shard. Then export and sign
a new mirror release as usual.

### Recover from a compromised catalog key

1. Use the offline root keys to rotate to a version without the compromised key.
   Add a new catalog key in the same version.
2. Re-sign the current catalog with the new catalog key and release it.

Clients that accept the recovery catalog record the new version in their catalog
checkpoint and reject every later catalog signed under the old version. A client
that has not refreshed since the compromise can still accept a catalog the
attacker signed, as with any revocation. TUF mirror freshness, which the attacker
would also need to defeat, and the trust root expiry bound that window.
Provision new clients with the newest trust root, or its root key fingerprints,
so they never trust the compromised version.

### Recover from a compromised or lost root key

While a threshold of the current root keys remains available, rotate to a new
root key set. If a threshold of root keys is lost, the publisher can no longer
sign a new version. If an attacker holds a threshold of root keys, they can sign
a different history from that version. Clients that already hold a newer
version reject it, because a catalog's chain must contain their checkpoint
unchanged. In both cases, create a new trust root and provision it to clients
again.

## Withdrawing and obsoleting packages

To withdraw an exact version, use the publisher's catalog keys on the publishing
repository, then export and sign a new TUF release.

```sh
cps repo withdraw --repo repo --signing-key private.der cps://canopa/demo/hello@1.0.0-0
cps repo withdrawals --repo repo --publisher canopa --trust-root repo/trust/1.json
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
canopa --trust-root repo/trust/1.json`.

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
