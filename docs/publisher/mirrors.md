# Export signed catalogs and serve a mirror

[Documentation](../README.md) · [Publishers](README.md)

A mirror serves an exported publishing repository as
[TUF](https://theupdateframework.github.io/specification/latest/) targets.
The publisher's catalog keys have already signed the catalog. The mirror's
TUF keys add transport authentication and freshness, so clients detect a
frozen or replayed mirror. See [Keys and where they live](publishing.md#keys-and-where-they-live)
for how the two layers fit together.

## On this page

- [Export a release](#export-a-release)
- [Sign the export](#sign-the-export)
- [Sharded targets and delta payloads](#sharded-targets-and-delta-payloads)
- [Serving a basic HTTPS mirror](#serving-a-basic-https-mirror)
  - [Provisioning the TUF root](#provisioning-the-tuf-root)
  - [Deploying and refreshing](#deploying-and-refreshing)
- [Release keys and external signers](#release-keys-and-external-signers)
  - [Refreshing metadata without the targets key](#refreshing-metadata-without-the-targets-key)
  - [External signer protocol](#external-signer-protocol)
- [Rotating publisher keys on a mirror](#rotating-publisher-keys-on-a-mirror)
- [Rotating a mirror's TUF root](#rotating-a-mirrors-tuf-root)
- [Publishing and verifying signed mirror release notes](#publishing-and-verifying-signed-mirror-release-notes)

## Export a release

Export a snapshot of the publishing repository to a new directory.

```sh
cps repo export --repo repo --publisher canopa --trust-root repo/trust/1.json --output release-targets
```

`--trust-root` is any version of the publisher's trust root that the
current catalog's history contains. The export holds flat target files:

- `current`, the catalog pointer.
- The signed catalog root, its shards, and any per-shard file indexes.
- The manifests and payloads of active packages, and their attestations and
  archived build inputs.
- `publisher`, the publisher name, and `trust-root.json`, the newest trust
  root version that the current catalog carries. Clients that add the
  publisher by URL pin this file.

Export compresses each distinct payload with Zstandard when the result is
smaller. The mirror carries either `blobs-SHA256.zst` or `blobs-SHA256`,
where `SHA256` identifies the original file. Sync and on-demand fetch verify
the TUF target, then check the original length and digest against the
signed manifest before caching the file.

When a signed release profile still references an older companion catalog,
include that digest in the companion mirror's export with
`--retain-catalog SHA256`. Repeat the option for up to 32 historical
catalogs. CPS writes their digests to a `retained` target. Sync verifies the
retained catalogs and their relation to the current catalog, then caches
their objects without advancing the pointer or replay checkpoint to a
historical digest. Index-only sync leaves their payloads for plan-specific
fetch. Move primary profiles to an updated companion catalog when rolling
out a fix. The old catalog remains selectable by profiles and saved plans
that pin it while its objects are available.

`--include-sources` and `--archive-catalog` export build sources and
separate archive mirrors for rebuilders. See
[Archive and verify build evidence](rebuilds.md).

## Sign the export

`cps repo sign-export` signs an export as a new TUF release. It needs the
mirror's TUF root and the online role keys. CPS checks that each key
belongs to its role and that the keys meet the root's thresholds before it
signs.

```sh
cps repo sign-export --root root.json \
    --targets-key keys/targets.pem --snapshot-key keys/snapshot.pem \
    --timestamp-key command:/etc/cps/signers/timestamp.signer.kdl \
    --version 8 --release-expires 2026-12-10T00:00:00Z \
    --timestamp-expires 2026-11-17T00:00:00Z --output signed release-targets
```

The output directory holds `targets/`, with consistent-snapshot names, and
`metadata/`, with the root, the versioned targets, `objects-*`, and
snapshot roles, and `timestamp.json`. `--release-expires` sets the targets
and snapshot expiry and `--timestamp-expires` the timestamp expiry. Use a
version higher than every version published or reserved before. Publish the
targets and versioned metadata before `timestamp.json`. The
[bundled deployment adapter](#serving-a-basic-https-mirror) does all of
this, keeps the version state, and verifies the result before publishing.

Any TUF repository tool that writes consistent snapshots can also sign an
export, for example
[`tuftool`](https://github.com/awslabs/tough/tree/develop/tuftool) with the
export directory as `--add-targets`. Large repositories need
`sign-export`, which splits the content targets across delegated roles to
stay within the metadata limits below.

## Sharded targets and delta payloads

Catalog shards are content-addressed targets named `catalog-shards-SHA256`,
so after a publication a cache downloads the new root and only the shards
that changed. Sync downloads missing shards, manifests, attestations, and
rebuild metadata in bounded parallel batches.

`sign-export` keeps `current`, `publisher`, `trust-root.json`, and
`retained` in the top-level targets role and places every content target
in one of 16 delegated roles, `objects-0` to `objects-f`, by the first
hexadecimal digit of the SHA-256 of its name. Each release signs all 16
roles, including empty ones, because a TUF snapshot cannot drop a role
that the previous snapshot listed. `sign-export` and clients share one set of metadata limits: 1 MiB
for `root.json` and `timestamp.json`, and 16 MiB for the snapshot, the
top-level targets, and each delegated role. `sign-export` refuses to produce
metadata that clients would reject.

For smaller upgrade downloads, include byte-level patches when exporting a
mirror release. Pass `--delta-from` with a previously signed catalog digest
that your users are likely to have installed. Repeat the option for other
baselines.

```sh
cps repo export --repo repo --publisher canopa --trust-root repo/trust/1.json \
    --delta-from "$previous_catalog" --output release-targets
```

The export compares new versions against the newest earlier version of the
same package and architecture in each supplied catalog. It includes patches
that save space and keeps the full payloads. Sign the entire export. During
`cps upgrade` or `cps fetch`, CPS can use the old installed or cached file
as a base when the patch is smaller than the full mirror target. It checks
the patch's TUF metadata and verifies the complete result against the
signed package manifest. If the patch fails, CPS fetches the full target.
CPS streams files when generating and applying patches. Version 1 patches
use the `CPSP1` header and 64-bit offsets and lengths. The mirror target
name is `payload-delta-v1-<base SHA-256>-<result SHA-256>`.

## Serving a basic HTTPS mirror

The bundled adapter [`ci/deploy-mirror.sh`](../../ci/deploy-mirror.sh)
exports, signs, verifies, and publishes a catalog to a directory served by
a static HTTPS server. Install `jq` and `flock` on the signing host.
Provision an existing, writable directory for this publisher. The adapter
stages files in the parent directory, which must also be writable and on
the same filesystem. Point the server at the mirror directory.

Configure these absolute paths as workflow variables or shell environment
variables when invoking the adapter directly.

| Variable | Value |
| --- | --- |
| `CPS_BIN` | The CPS executable |
| `CPS_MIRROR_DIR` | The directory served over HTTPS |
| `CPS_MIRROR_TRUST_ROOT` | The publisher [trust root](publishing.md#publisher-trust-root) file that clients pin, used to verify the catalog and published in `publisher.json` |
| `CPS_MIRROR_TUF_ROOT` | Original signed TUF root JSON used to bootstrap and verify the rotation chain |
| `CPS_MIRROR_TUF_ACTIVE_ROOT` | Optional current or next signed TUF root JSON, with consistent snapshots enabled |
| `CPS_MIRROR_TUF_KEYS` | Directory of TUF release key sources, described in [Release keys and external signers](#release-keys-and-external-signers) |
| `CPS_MIRROR_TARGETS_LIFETIME` | Optional targets and snapshot lifetime for a deployment, such as `90 days` (default `30 days`) |
| `CPS_MIRROR_SNAPSHOT_LIFETIME` | Optional lifetime of a snapshot re-signed by a refresh (default `30 days`) |
| `CPS_MIRROR_TIMESTAMP_LIFETIME` | Optional timestamp lifetime (default `7 days`) |
| `CPS_MIRROR_OBJECTS_THRESHOLD` | Optional signatures required on each `objects-*` delegation (default 1) |
| `CPS_MIRROR_INITIAL_VERSION` | Only for recovering lost signer state, see below |

Lifetimes are a number of `days` or `hours`. Keep the publisher's keys and
all TUF private keys outside the HTTPS directory. The adapter refuses a key
directory inside the mirror and a mirror inside the publishing repository.

### Provisioning the TUF root

Use [`tuftool`](https://github.com/awslabs/tough/tree/develop/tuftool) on a
trusted key provisioning machine to create the TUF root. Set `root_json`,
`offline_root_key`, and `release_keys` to paths outside the web root: a new
root JSON path, an unused private key path, and an existing private
directory for the online role keys.

```sh
umask 077
tuftool root init "$root_json"
tuftool root expire "$root_json" 'in 52 weeks'
for role in root targets snapshot timestamp; do
    tuftool root set-threshold "$root_json" "$role" 1
done
tuftool root gen-rsa-key "$root_json" "$offline_root_key" --role root
for role in targets snapshot timestamp; do
    tuftool root gen-rsa-key "$root_json" "$release_keys/$role.pem" --role "$role"
done
tuftool root sign "$root_json" --key "$offline_root_key"
```

For a 2-of-3 offline root, set `root_key_a`, `root_key_b`, and `root_key_c`
to three unused private key paths on the offline provisioning machine
instead.

```sh
umask 077
tuftool root init "$root_json"
tuftool root expire "$root_json" 'in 52 weeks'
tuftool root set-threshold "$root_json" root 2
for root_key in "$root_key_a" "$root_key_b" "$root_key_c"; do
    tuftool root gen-rsa-key "$root_json" "$root_key" --role root
done
for role in targets snapshot timestamp; do
    tuftool root set-threshold "$root_json" "$role" 1
    tuftool root gen-rsa-key "$root_json" "$release_keys/$role.pem" --role "$role"
done
tuftool root sign "$root_json" --key "$root_key_a" --key "$root_key_b"
```

Copy the signed root and the release keys to the signing host, or move the
release keys into tokens behind [external signers](#release-keys-and-external-signers).
Keep the root signing keys offline. For 2-of-3, store the root keys
separately so two remain available if one is lost.

### Deploying and refreshing

To deploy the current catalog, export the variables above and run this from
a checkout after building CPS. Set `repo` to the local publishing
repository.

```sh
export CPS_BIN="$PWD/target/release/cps"
CPS_RELEASE_REPO="$repo" CPS_RELEASE_PUBLISHER=canopa \
    CPS_RELEASE_DEPLOY="$PWD/ci/deploy-mirror.sh" \
    bash ci/release.sh deploy-current "$(cat "$repo/current")"
```

The adapter verifies the staged release from the bootstrap root as a new
client would, publishes `publisher.json` with the bootstrap TUF root and
the publisher trust root, and publishes the new timestamp last. Install the
signed TUF root at `/etc/cps/tuf-root.json` on clients for URL-only
registration. Serve `metadata/timestamp.json` with revalidation so clients
see the latest version promptly.

Clients fail an update when they cannot refresh a URL-added publisher,
unless their image allows a bounded deferral. See
[Refresh policy for mirror publishers](../client/publishers.md#refresh-policy-for-mirror-publishers).
Keep the timestamp fresh: schedule `bash ci/release.sh refresh-mirror`
daily with `CPS_MIRROR_DIR` and the deployment adapter configured. It
re-signs the deployed catalog's metadata when the last signed timestamp has
48 hours or less left, or when its bytes are missing or differ at the
origin. The refresh needs only the timestamp key, and the snapshot key when
the snapshot is about to expire. See
[Refreshing metadata without the targets key](#refreshing-metadata-without-the-targets-key).

The bundled adapter reserves versions and records the published timestamp
and snapshot fingerprints in `.ci-tuf-state.json` in the private publishing
repository with `"format": 1`. Back up that file with the repository and
keep it private to the signing account. A custom adapter that keeps no such
record refreshes on each scheduled run.

If signer state is lost, restore a trusted backup or set
`CPS_MIRROR_INITIAL_VERSION` to the highest version previously published or
reserved, from signing-host records. Run `deploy-current` with that setting
once to create the protected state, then remove the setting. A new mirror
starts at version 1.

Run `bash tests/deploy-mirror.sh` to exercise publishing and syncing a
disposable mirror with the same adapter. It needs `tuftool`.

## Release keys and external signers

The root role stays offline. The online roles have different exposure:

| Role | Needed for | Compromise allows |
| --- | --- | --- |
| `targets` and `objects-*` | Each release that changes content | Signing arbitrary mirror targets. CPS still requires the publisher's catalog signatures on packages. |
| `snapshot` | Each release, and a refresh when the snapshot is about to expire | Mixing role versions, which clients' rollback checks limit |
| `timestamp` | Each release and the routine refresh | Freezing clients on old metadata until the snapshot and targets expire |

Keep the targets key off the host that runs the daily refresh, or in a
hardware token, and supply it only when you deploy a new catalog. Give the
targets and snapshot roles lifetimes that cover the longest expected gap
between releases. Clients reject metadata once the targets role expires,
whatever the timestamp says.

`cps repo sign-export` and `cps repo refresh-metadata` accept each role key
as one of these key sources:

- `PATH` or `file:PATH` names a private key file that CPS reads. Use PKCS#8
  Ed25519 or ECDSA P-256 keys in DER or PEM form, or RSA keys in PEM form.
- `command:DESCRIPTOR` names an external signer. CPS passes it the bytes to
  sign and verifies the returned signature against the declared public key.

Repeat `--targets-key`, `--snapshot-key`, or `--timestamp-key` to supply
several keys for a role. The TUF root sets which keys belong to each role
and how many signatures it needs. Use `tuftool root set-threshold` and
repeat `tuftool root gen-rsa-key` on the provisioning machine. CPS checks
that every supplied key belongs to its role and that the keys meet the
root's threshold before it signs, and rejects a key listed twice.

The 16 `objects-*` delegations carry the content targets. By default they
use the first targets key with a threshold of 1. Pass `--objects-key` one
or more times with `--objects-threshold N` to delegate them to their own
keys. The targets role records these keys, so changing them takes effect
with the next release.

The bundled adapter reads key sources from the `CPS_MIRROR_TUF_KEYS`
directory. For each role (`targets`, `snapshot`, `timestamp`, and
optionally `objects`), it uses `ROLE.pem` and `ROLE-NAME.pem` as key files
and `ROLE.signer.kdl` and `ROLE-NAME.signer.kdl` as signer descriptors. List
several files for a role to meet its threshold. It reads only the roles
that the operation needs, so a refresh host can hold just the timestamp
key.

```kdl
schema-version 1 kind="signer"
command "/usr/local/libexec/cps-kms-sign" "--key" "release/timestamp"
public-key "timestamp.pub"
timeout-seconds 30
```

`command` gives an absolute program path and up to 64 fixed arguments. CPS
runs it directly, without a shell, and adds no arguments. `public-key`
names the declared public key, relative to the descriptor: a PEM
SubjectPublicKeyInfo for RSA, ECDSA P-256, or Ed25519, an Ed25519 DER key,
or the hexadecimal Ed25519 form used in TUF metadata. Convert an RSA private
key's public half with `openssl pkey -in timestamp.pem -pubout -out timestamp.pub`.
`timeout-seconds` is optional, from 1 to 600, and defaults to 30.

### Refreshing metadata without the targets key

`cps repo refresh-metadata` signs a new `timestamp.json` over the published
snapshot. With `--snapshot-key` and `--snapshot-expires`, it also signs a
new snapshot with the same role versions and digests. It reads only the
root and the published versioned snapshot, so it needs neither the export
nor the targets key.

```sh
cps repo refresh-metadata --root root.json --snapshot mirror/metadata/7.snapshot.json \
    --timestamp-key command:/etc/cps/signers/timestamp.signer.kdl \
    --version 8 --timestamp-expires 2026-11-17T00:00:00Z --output refreshed
```

CPS verifies the snapshot's signatures against the root and requires the
new version to be higher than the snapshot's. The output directory holds
`timestamp.json` and, when re-signed, `VERSION.snapshot.json`. Publish the
snapshot before the timestamp.

Run as `deploy-mirror.sh REPO PUBLISHER CATALOG refresh`, the bundled
adapter uses this command for the catalog recorded as deployed. It locates
the snapshot from its signer state and checks the published snapshot and
roles against the recorded digests before signing. It re-signs the snapshot
when it would expire within a day of the new timestamp, and fails if no
snapshot key is present then. It warns when the targets role expires within
14 days and fails once it has expired. A refresh requires the active root
to be published already. Publish a new root with `deploy-current`.

### External signer protocol

An external signer lets a small helper reach a PKCS#11 token, an HSM, or a
cloud KMS without CPS linking those libraries. The same descriptors serve
TUF role keys, publisher root and catalog keys, and builder keys. Protocol
version 1 works as follows:

- CPS executes the program with the descriptor's arguments and its own
  environment, plus `CPS_SIGNER_PROTOCOL=1`, `CPS_SIGNER_CONTEXT` (`tuf`,
  `catalog`, `trust-root`, or `attestation`), `CPS_SIGNER_KEY_ID` (the TUF
  key ID, or the SHA-256 of the Ed25519 public key otherwise), and
  `CPS_SIGNER_ALGORITHM` (`ed25519`, `rsassa-pss-sha256`, or
  `ecdsa-sha2-nistp256`).
- Stdin carries the exact message bytes followed by end of file. For TUF
  this is the canonical JSON of the role. For a catalog or trust root it is
  the domain prefix, such as `cps signed catalog root v1` and a newline,
  followed by the payload. The helper signs the message itself. For RSA, use
  PSS with SHA-256 and a 32-byte salt. ECDSA signatures are DER.
- The helper writes only the raw signature bytes to stdout, at most 2048
  bytes, and exits with status 0. Stderr reaches the operator.
- Any other exit status, a signal, an empty or oversized output, or a
  signature that does not verify fails the operation. CPS writes nothing.
- When the timeout expires, CPS kills the helper's process group and fails.

Publisher root, catalog, and builder keys are Ed25519. TUF role keys can
also be RSA or ECDSA P-256.
[`tests/support/openssl-signer.sh`](../../tests/support/openssl-signer.sh)
is a minimal helper that signs with a local key file. Use it as a template.
See also [Sign with several catalog keys or an external signer](publishing.md#sign-with-several-catalog-keys-or-an-external-signer).

## Rotating publisher keys on a mirror

Publisher key changes travel inside the catalog, so the mirror needs no
change of its own. After a [trust root rotation](publishing.md#rotate-revoke-and-recover-keys)
and a new or re-signed catalog, deploy that catalog as usual. The export's
`trust-root.json` target then carries the newest version for clients that
add the publisher by URL.

Keep `CPS_MIRROR_TRUST_ROOT` pointing to the trust root file that the
mirror first published. The adapter publishes `publisher.json` once and
refuses to replace it with different bootstrap material. Any version in the
catalog's history verifies the catalog, so an older pin keeps working after
rotation.

## Rotating a mirror's TUF root

Keep `CPS_MIRROR_TUF_ROOT` pointing to the original bootstrap root. On the
offline signing machine, prepare a root one version higher than the current
root. Sign it with enough keys to satisfy both the current and new root
thresholds. With `tuftool`, first sign `next-root.json` with the new root
keys. Then run `tuftool root sign next-root.json --cross-sign current-root.json`
with the previous root keys. This appends the previous root signatures. For
a 2-of-3 root, the two remaining keys can authorize a successor that
revokes the third. A signature from a key authorized by both roots counts
toward both thresholds.

Copy the signed successor to the signing host and set
`CPS_MIRROR_TUF_ACTIVE_ROOT` to its path. If you also rotated the online
roles, point `CPS_MIRROR_TUF_KEYS` at the matching release keys. Run
`deploy-current` for the deployed catalog digest. The active root must be
the latest published root or its immediate successor.

The adapter verifies the chain from the bootstrap root before publishing,
retains every versioned root file, and publishes the new root just before
the new timestamp, because the new root may revoke the old timestamp key.
Existing clients pick up the successor on sync. Keep
`CPS_MIRROR_TUF_ACTIVE_ROOT` set for later deployments and timestamp
refreshes.

## Publishing and verifying signed mirror release notes

To publish notes for the bundled mirror, install GitHub CLI and OpenSSH on
the signing host. Configure `CPS_MIRROR_URL`, the mirror paths, and
`CPS_RELEASE_NOTES_SIGNING_KEY` in its private configuration. Use an
Ed25519 OpenSSH key kept outside the HTTPS directory with the trusted root
and publisher trust root. Provide the signing host with GitHub credentials
scoped to publishing releases, `GH_REPO` for the repository, and
`GITHUB_SHA` for the reviewed release commit.

After deployment, run
`bash ci/publish-release-notes.sh PUBLISHER DEPLOYED_CATALOG_SHA256` on the
signing host. The `publisher.json` descriptor declares `"schema_version": 1`
and `"kind": "publisher-bootstrap"`. The script checks it against the
trusted TUF root and publisher trust root and publishes an optional GitHub
Release with `release-notes.md` and `release-notes.md.sig`. The signed notes
identify the publisher, mirror URL, catalog digest, the SHA-256 fingerprints
of the TUF root and trust root files, and the fingerprint of each root key,
which clients can pass to `cps publisher add --expect-root-key`. The script
needs `python3` to read the trust root. Rerun the command with the same
catalog to retry.

For independent verification of the release notes, obtain the notes
signer's public key through a trusted channel. With that key in
`cps-release-notes.pub`, download both GitHub release assets and verify
them.

```sh
printf 'cps-release namespaces="cps-release-notes" %s\n' \
    "$(cat cps-release-notes.pub)" > cps-release-allowed-signers
ssh-keygen -Y verify -f cps-release-allowed-signers -I cps-release \
    -n cps-release-notes -s release-notes.md.sig < release-notes.md
```

Run `bash tests/release-notes.sh` to test signing and publication retries
with a disposable signing key and a local GitHub CLI fixture.

Clients can [configure and sync a mirror](../client/mirrors.md) after you
provide the trust anchors.
