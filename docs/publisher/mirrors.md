# Export signed catalogs and serve a mirror

[Documentation](../README.md) · [Publishers](README.md)

## On this page

- [Export and sign a release](#export-and-sign-a-release)
- [Sharded targets and delta payloads](#sharded-targets-and-delta-payloads)
- [Serving a basic HTTPS mirror](#serving-a-basic-https-mirror)
  - [Provisioning a 2-of-3 offline root](#provisioning-a-2-of-3-offline-root)
- [Rotating a mirror's TUF root](#rotating-a-mirrors-tuf-root)
- [Publishing and verifying signed mirror release notes](#publishing-and-verifying-signed-mirror-release-notes)

## Export and sign a release

`publish` assembles the local CPS repository using a publisher signing key. Export a snapshot to a new directory before signing it with [The Update Framework](https://theupdateframework.github.io/specification/latest/).

```sh
cps repo export --repo repo --publisher canopa --trust-key public.der --output release-targets
```

The live export includes the catalog pointer, signed catalog, and the manifests and payloads for active packages. It also includes the `publisher` and `publisher.der` targets for publisher discovery. Sign the exported files with a TUF repository tool such as [`tuftool`](https://github.com/awslabs/tough/tree/develop/tuftool), using the export directory as `--add-targets`. Keep root signing keys offline, assign separate keys to the release roles, enable consistent snapshots, and advance the relevant metadata versions whenever their contents change. Publish the versioned root files, targets, and versioned metadata before publishing the new `timestamp.json`.

Export compresses each distinct payload with Zstandard when the result is
smaller. The mirror carries either `blobs-SHA256.zst` or `blobs-SHA256`,
where `SHA256` identifies the original file. Sync and on-demand fetch verify
the TUF target, then check the original length and digest against the signed
manifest before caching the file.

When a signed release profile still references an older companion catalog,
include that digest in the companion mirror's export with
`--retain-catalog SHA256`. Repeat the option for up to 32 historical catalogs.
Sign the `retained` file and the exported objects together as TUF targets. Sync
verifies the retained catalogs and their relation to the current catalog, then
caches their objects without advancing the pointer or replay checkpoint to a
historical digest. Index-only sync leaves their payloads for plan-specific fetch.
Move primary profiles to an updated companion catalog when rolling out a fix.
The old catalog remains selectable by profiles and saved plans that pin it while
its objects are available.

## Sharded targets and delta payloads

Sync downloads missing manifests, attestations, and rebuild metadata in
bounded parallel batches. The bundled deployment adapter signs mirror
objects in 16 delegated targets roles. CPS applies the 16 MiB download limit
to each role and can refresh an existing cache from a sharded mirror.

For smaller upgrade downloads, include byte-level patches when exporting a
mirror release. Pass `--delta-from` with a previously signed catalog digest
that your users are likely to have installed. Repeat the option for other
baselines.

```sh
cps repo export --repo repo --publisher canopa --trust-key public.der \
    --delta-from "$previous_catalog" --output release-targets
```

The export compares new versions against the newest earlier version of the
same package and architecture in each supplied catalog. It includes patches
that save space and keeps the full payloads. Sign the entire export with TUF.
During `cps upgrade` or `cps fetch`, CPS can use the old installed or cached
file as a base when the patch is smaller than the full mirror target.
It checks the patch's TUF metadata and verifies the complete result against
the signed package manifest. If the patch fails, CPS fetches the full
target. CPS streams files when generating and applying patches. Version 1
patches use the `CPSP1` header and 64-bit offsets and lengths. The mirror
target name is `payload-delta-v1-<base SHA-256>-<result SHA-256>`.

## Serving a basic HTTPS mirror

Install `jq` and `flock` on the signing host. Use
[`tuftool`](https://github.com/awslabs/tough/tree/develop/tuftool) on the
key provisioning machine to create and rotate the TUF root. Provision an existing, writable
directory for this publisher and serve it with a static HTTPS server. The
adapter stages files in the parent directory, which must also be writable
and on the same filesystem. Point the server at the mirror directory.

Configure these absolute paths as workflow variables or shell environment
variables when invoking the adapter directly.

| Variable | Value |
| --- | --- |
| `CPS_MIRROR_DIR` | The directory served over HTTPS |
| `CPS_MIRROR_PUBLISHER_KEY` | CPS publisher's public DER key |
| `CPS_MIRROR_TUF_ROOT` | Original signed TUF root JSON used to bootstrap and verify the rotation chain |
| `CPS_MIRROR_TUF_ACTIVE_ROOT` | Optional current or next signed TUF root JSON, with consistent snapshots enabled |
| `CPS_MIRROR_TUF_KEYS` | Private TUF release keys directory containing `targets.pem`, `snapshot.pem`, and `timestamp.pem` |

Keep the CPS publisher signing key and all TUF private keys outside the
HTTPS directory. To provision the TUF roles, run this on a trusted key
provisioning machine with `root_json`, `offline_root_key`, and `release_keys`
set to paths outside the web root. Use a new root JSON path and an existing
private release keys directory.

```sh
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

### Provisioning a 2-of-3 offline root

For a new 2-of-3 mirror, choose a fresh `root_json` path and an empty private
`release_keys` directory for the online roles. Set `root_key_a`, `root_key_b`,
and `root_key_c` to three unused private key paths on the offline provisioning
machine.

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

Copy the signed root and the three release keys to the signing host. Keep
the root signing keys offline. For 2-of-3, store the root keys separately so
two remain available if one is lost. The adapter pins the published bootstrap
descriptor to that root and CPS public key.

To deploy the current catalog directly, export the required variables above
and run this from a checkout after building CPS. Set `repo` to the local
publishing repository.

```sh
export CPS_BIN="$PWD/target/release/cps"
CPS_RELEASE_REPO="$repo" CPS_RELEASE_PUBLISHER=canopa \
    CPS_RELEASE_DEPLOY="$PWD/ci/deploy-mirror.sh" \
    bash ci/release.sh deploy-current "$(cat "$repo/current")"
```

Install the signed TUF root at `/etc/cps/tuf-root.json` on clients for URL-only
registration. The mirror exporter includes the publisher discovery targets.
To refresh automatically, schedule
`bash ci/release.sh refresh-mirror` daily on the signing host with
`CPS_MIRROR_DIR` and the deployment adapter configured. It signs fresh TUF
metadata for the deployed catalog when the last signed timestamp has 48 hours
or less left or its bytes are missing or differ at the origin. The bundled
adapter reserves versions and records published timestamp fingerprints in
`.ci-tuf-state.json` in the private publishing repository with `"format": 1`.
Back up that file with the repository and keep it private to the signing account.
A custom adapter that keeps no such record refreshes on each scheduled run.

If signer state is lost, restore a trusted backup or set
`CPS_MIRROR_INITIAL_VERSION` to the highest version previously
published or reserved, from signing-host records. Run `deploy-current`
with that setting once to create the protected state, then remove the
setting. A new mirror starts at version 1.
Serve `metadata/timestamp.json` with revalidation so clients see the latest
version promptly.
Run `bash tests/deploy-mirror.sh` to exercise publishing and syncing a
disposable mirror with the same signer.

## Rotating a mirror's TUF root

Keep `CPS_MIRROR_TUF_ROOT` pointing to the original bootstrap root. On the
offline signing machine, prepare a root one version higher than the current
root. Sign it with enough keys to satisfy both the current and new root
thresholds. With `tuftool`, first sign `next-root.json` with the new root keys.
Then run `tuftool root sign next-root.json --cross-sign current-root.json`
with the previous root keys. This appends the previous root signatures.
For a 2-of-3 root, the two remaining keys can authorize a successor that
revokes the third. A signature from a key authorized by both roots counts
toward both thresholds.

Copy the signed successor to the signing host and set
`CPS_MIRROR_TUF_ACTIVE_ROOT` to its path. If you also rotated the online
roles, point `CPS_MIRROR_TUF_KEYS` at the matching release keys. Run
`deploy-current` for the deployed catalog digest.

The hook verifies the chain from the bootstrap root before publishing,
retains every versioned root file, and publishes the new root just before
the new timestamp. Existing clients pick up the successor on sync. Keep
`CPS_MIRROR_TUF_ACTIVE_ROOT` set for later deployments and timestamp refreshes.

## Publishing and verifying signed mirror release notes

To publish notes for the bundled mirror, install GitHub CLI and OpenSSH on
the signing host. Configure `CPS_MIRROR_URL`, the mirror paths, and
`CPS_RELEASE_NOTES_SIGNING_KEY` in its private configuration. Use an
Ed25519 OpenSSH key kept outside the HTTPS directory with the trusted
root and publisher public key. Provide the signing host with GitHub
credentials scoped to publishing releases, `GH_REPO` for the repository,
and `GITHUB_SHA` for the reviewed release commit.

After deployment, run
`bash ci/publish-release-notes.sh PUBLISHER DEPLOYED_CATALOG_SHA256` on the
signing host. The `publisher.json` descriptor declares `"schema_version": 1`
and `"kind": "publisher-bootstrap"`. The script checks it against the trusted
TUF root and CPS public key and publishes an optional GitHub Release with
`release-notes.md` and `release-notes.md.sig`. The signed notes identify the
publisher, mirror URL, catalog digest, and SHA-256 fingerprints of the root
JSON and DER key bytes. Rerun the command with the same catalog to retry.

For independent verification of the release notes, obtain the notes signer's
public key through a trusted channel. With that key in
`cps-release-notes.pub`, download both GitHub release assets and verify them.

```sh
printf 'cps-release namespaces="cps-release-notes" %s\n' \
    "$(cat cps-release-notes.pub)" > cps-release-allowed-signers
ssh-keygen -Y verify -f cps-release-allowed-signers -I cps-release \
    -n cps-release-notes -s release-notes.md.sig < release-notes.md
```

Run `bash tests/release-notes.sh` to test signing and publication retries
with a disposable signing key and a local GitHub CLI fixture.

Clients can [configure and sync a mirror](../client/mirrors.md) after
you provide the trust anchors.
