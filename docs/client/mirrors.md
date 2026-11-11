# Sync mirrors and maintain caches

[Documentation](../README.md) · [Clients](README.md)

A mirror distributes a publisher's signed catalogs and package files.
CPS writes the verified objects to a local repository cache, which the
image uses for planning and fetching. Obtain the mirror's initial TUF
root and the CPS publisher trust root through a trusted channel.

On a running system, publishers added with a
[mirror URL](publishers.md#add-a-publisher-from-an-https-mirror) are synced
automatically by `cps upgrade`. Use the commands on this page to manage
caches yourself, for offline images, or for a signed profile that spans
several mirrors.

## On this page

- [Sync a single publisher](#sync-a-single-publisher)
- [Transfer limits](#transfer-limits)
- [Catalog checkpoints and archived signatures](#catalog-checkpoints-and-archived-signatures)
- [Synchronizing a signed profile](#synchronizing-a-signed-profile)
- [Fetch payloads for an index-only cache](#fetch-payloads-for-an-index-only-cache)
- [Recover payloads from an archive](#recover-payloads-from-an-archive)
- [Reclaiming mirror cache space](#reclaiming-mirror-cache-space)

## Sync a single publisher

Use a Linux filesystem that supports atomic directory exchange for the cache.
Pass `--index-only` to fetch metadata now and payloads on demand.

```sh
cps repo sync --repo /var/lib/cps/canopa --tuf-root canopa-root.json \
    --url https://packages.example.org/ --trust-root canopa.trust.json
```

`--tuf-root` is the mirror's provisioned TUF root metadata, and
`--trust-root` is the publisher's pinned trust root, which also names the
publisher. `--url` is the base URL that holds `metadata/` and `targets/`.
When the two directories live elsewhere, pass `--metadata URL` and
`--targets URL` instead.

Point `cps image create` or the KDL image configuration at this cache. Give
write access to the account that runs sync and keep the cache outside all
boot roots on persistent storage, such as the
[`/var` that Btrfs environments share](../setups/btrfs.md#share-var-between-boot-environments). CPS uses the original provisioned root to
identify the repository on each sync and stores authenticated root rotations
and replay state under `.tuf`. Sync accepts HTTPS or a local `file:///`
mirror. URLs passed to `--metadata` and `--targets` end with a slash.

Full sync checks TUF signatures, versions, and expiration, then verifies the
catalog against the publisher's [pinned trust root](publishers.md#pinned-trust-roots)
and downloads the complete release. A failed download leaves the previous
cache available. Sync can repair a damaged cache object when the mirror
release includes that exact digest. The cache retains previous
content-addressed objects for saved plans. Planning and applying can work
offline once the required metadata and payloads are cached.

A signed catalog root commits to content-addressed shards, so a refresh
downloads the new root and only the shards whose digests changed. Between
releases, the publisher re-signs only the TUF timestamp, and a sync
confirms that the cached catalog is still current. CPS limits repository
reads to 16 MiB per signed catalog root, 16 MiB per catalog shard, and
32 MiB per manifest. It bounds payload staging by the signed file length
and checks the digest before recording the transaction. TUF metadata is
limited to 1 MiB for the root and timestamp and 16 MiB for the snapshot and
each targets role, the same limits the CPS mirror signer enforces. When
the mirror offers a smaller patch against a payload you already have, fetch
uses it and verifies the complete result.

To prepare a cache for an existing image, add `--image /path/to/image` to
`repo sync`. CPS verifies the candidate against the installed publisher
trust root, checkpoint, and signed catalog archive before advancing the
cache's CPS checkpoint. This works with an empty cache. TUF metadata still
records authenticated progress when a candidate fails the CPS check. The
image is opened read-only and keeps its existing repository paths until an
explicit [relocation](publishers.md#move-publisher-repositories).

## Transfer limits

A sync or payload fetch holds the cache lock while it downloads, so CPS
bounds how long a slow or hostile mirror can keep it. Each operation has an
overall deadline, and every response body must deliver a minimum rate in
each 60-second window. A read that stalls for 30 seconds fails, and a
connection attempt times out after 10 seconds. A failed transfer leaves the
previous cache in place.

| Variable | Default | Meaning |
| --- | --- | --- |
| `CPS_TRANSFER_DEADLINE_SECONDS` | `14400` (4 hours) | Deadline for one sync or fetch, from 1 second to 7 days |
| `CPS_TRANSFER_MIN_BYTES_PER_SECOND` | `4096` | Throughput floor for each download. `0` disables it. |

Set them in the environment of the command, or of the service that runs
`cps upgrade`, when a large release must sync over a slow link. Exceeding
either limit counts as an unavailable mirror for the
[refresh policy](publishers.md#refresh-policy-for-mirror-publishers). By
default an unavailable mirror stops the upgrade.

## Catalog checkpoints and archived signatures

Each committed repository-backed transaction records the selected catalog
digest, sequence, and trust root history in the image. Subsequent plans,
saved-plan checks, and available-package queries reject a lower sequence, a
different catalog at the same sequence, or a trust root history that drops
or changes a known version, such as a catalog signed under an older version
with a key that a later version revoked. The check works with a replacement
cache that lacks the previous catalog. Each boot environment keeps its own
checkpoints. Use a [signed bootstrap catalog](profiles.md#install-a-particular-release)
to provision a particular release into a new image.

The cache keeps its own replay state. Each new publication or withdrawal
advances a sequence signed by the publisher. Sync remembers the newest
authenticated catalog even if downloading its payloads fails. It rejects
older publisher sequences and different catalogs with a sequence it has
already seen, including when those catalogs arrive with newer TUF metadata.
Sync can retry the same catalog and can skip sequences. A successor must
preserve known package identities, build evidence, withdrawals, and the
publisher's trust root history. Once a repository requires build evidence,
sync also enforces that requirement on successors. A new repository starts
its sequence at one.

When the publisher [rotates or revokes a key](publishers.md#rotation-and-revocation),
sync verifies the new trust root version from the one you pass with
`--trust-root`, so you keep using the same file.

CPS also archives one signed catalog per applied publisher: a compressed copy
of the signed root and all of its shards, so the checks below work offline. A different
catalog must preserve published manifests, withdrawals, obsoletions, and
profile safeguards, even when the cache lacks the earlier catalog. This
applies to both profile companions and independently configured publishers.
Offline verification checks archived signatures, catalog checkpoints, and
the recorded manifest digest for each installed package. A withdrawal in the
archive still authenticates a package that was installed before withdrawal.
An interrupted apply checks the pending catalog archives before resuming,
and an abort restores the archives that the pending transaction replaced.

The publisher signs the association between each package and its build
evidence. Export and sync carry the evidence with the release. To retrieve
it, use
`cps repo attestation --repo /var/lib/cps/canopa --trust-root canopa.trust.json cps://canopa/demo/hello@1.0.0-0`.

## Synchronizing a signed profile

For a profile that spans mirrors, add one `mirror` entry per publisher to
the same KDL file used by `image create`. The `root` path names that mirror's
provisioned TUF root. Relative paths resolve from the KDL file.

```kdl
schema-version 1 kind="image-config"
release-profile "desktop"
publisher "canopa" {
    repository "/var/lib/cps/desktop"
    trust-root "canopa.trust.json"
}
publisher "runtime" {
    repository "/var/lib/cps/runtime"
    trust-root "runtime.trust.json"
}
mirror "canopa" root="canopa-root.json" metadata="https://packages.example.org/canopa/metadata/" targets="https://packages.example.org/canopa/targets/"
mirror "runtime" root="runtime-root.json" metadata="https://packages.example.org/runtime/metadata/" targets="https://packages.example.org/runtime/targets/"
```

```sh
cps repo sync-profile --config image.kdl --index-only
cps image create --root image --config image.kdl
```

CPS verifies the primary release and requires each companion mirror to
advertise the exact signed pin as its current or retained catalog. It
prepares all caches before advancing the primary pointer. CPS checks the
signed composition's exact manifests in publisher order, dependencies,
selected paths, kernel actions, and builder trust from the signed profile
and supplied KDL. These checks also run in index-only mode.

Use `--profile-payloads` to fetch the effective files selected by the signed
composition before the profile becomes current. It includes a configured
bootstrap composition and leaves payloads for unrelated packages and
disabled facets, such as `debug.symbols`, for on-demand fetch.

```sh
cps repo sync-profile --config image.kdl --profile-payloads --image image
```

CPS verifies these downloads against the same TUF snapshots used to stage
the catalogs. If any required download fails, the primary pointer stays on
the previous release. The flag requires a recorded signed composition.
Plans that change packages outside that composition can still need
`cps fetch`.

If a companion mirror is behind or the staged composition fails validation,
clients can still select the previous primary catalog and its cached
objects. Replay checkpoints still remember newer verified metadata.

Use `repo sync-profile --config image.kdl --image /path/to/image` to guard
the whole profile with an existing image. The supplied configuration must
match the installed profile, architecture, facets, variants, publisher order, and
publisher trust roots. CPS checks the current catalog from each mirror and the
exact companion catalogs pinned by the new primary profile. It advances
the CPS checkpoints only after the guarded profile passes validation.

The first successful coordinated sync binds the primary cache to that
profile and its ordered cache directories. Use `repo sync-profile` for
subsequent updates to that primary cache. Give each profile its own primary
cache. Companion current pointers can advance independently, and profile
clients continue to use the exact companion pins recorded by their primary
catalog.

## Fetch payloads for an index-only cache

For an image that uses a small part of a mirror, add `--index-only` to `repo sync`.
This caches the catalog, manifests, and build evidence and verifies the signed
TUF target entries for payloads. A change such as `cps upgrade --root image`
fetches the files its plan needs before it applies the plan. For a saved
plan, fetch the files before applying or staging it.

```sh
cps upgrade --root image -n --save-plan upgrade.json
cps fetch --root image upgrade.json
cps apply --root image upgrade.json
```

Fetch revalidates the plan against the image and its pinned catalogs. If a
required payload is missing or damaged, fetch refreshes TUF metadata using the
mirror endpoints saved by `repo sync` and verifies the exact target. Use the
sync account to fetch into its cache. Fetch leaves the image and the cache's
current catalog unchanged. `cps abort --fetch` uses the same
endpoints to download the payloads that an
[abort](images.md#abort-a-transaction) needs.

## Recover payloads from an archive

A cached copy or a historical archive can supply payloads the live mirror has
dropped. To recover a missing payload for a saved plan or an installed
package, provision a historical cache with `repo sync --archive-catalog` and
pass it to fetch with a saved plan.

```sh
cps repair --root image -n --save-plan repair.json system/kernel
cps fetch --root image --archive-cache canopa=/var/lib/cps/archive/kernel repair.json
cps apply --root image repair.json
```

Repeat `--archive-cache PUBLISHER=PATH` for multiple archives. CPS checks
the archive with the image's pinned trust root and uses the first supplied
archive that lists the exact package manifest. It verifies the required
payloads and copies them into the configured repository's blob cache
without changing catalog pointers or the image. Other payloads use the
configured mirror. An archive that already has verified bytes works offline.

When an archive matches an exact catalog pin in a saved plan, fetch also
restores the signed catalog, manifests, build evidence, and archived rebuild
inputs before replaying the plan. This can recover a plan after the cache is
rebuilt at its configured path. CPS still checks the image revision, publisher configuration, catalog
history, and the complete plan before fetching payloads or applying it.

## Reclaiming mirror cache space

To enable automatic pruning on an installed Btrfs system, add this line to
`/etc/cps/image.kdl`.

```kdl
cache-keep-versions 2
```

After a live transaction commits, a staged update registers its boot entry,
or `cps resume` succeeds, CPS prunes the TUF caches used by registered roots.
For each package and target architecture, it keeps payloads already cached
for the newest two versions with cached files. It also keeps cached payloads
for every package installed in a registered root, including fallback roots,
regardless of version. Set the count to `0` to keep only the registered roots'
payloads. Use this policy for caches dedicated to this system's registered
roots. Offline images and saved plans outside that set can use a separate
cache and the manual retention options below. Omitting the setting leaves
pruning manual.

`repo prune-cache` removes unneeded payload files and lists them with
their lengths. Add `-n` to review the list without removing anything. Run it
on a synchronized TUF mirror cache.

```sh
cps repo prune-cache -n --repo /var/lib/cps/canopa \
    --trust-root canopa.trust.json --keep-image current-root --keep-image fallback-root \
    --keep-plan upgrade.json
```

CPS keeps payloads referenced by the current catalog and by catalogs retained
when that release was synchronized. Coordinated profile sync registers the
visible profile's companion pins, including its configured bootstrap pin.
Pruning keeps those payloads even when a standalone companion sync updates
the mirror's retention list. If promotion stops partway through, pruning
still follows the primary cache's visible catalog. If that pointer is
unavailable or unknown, it conservatively keeps both registered selections.

On an installed Btrfs system, add `--system-config /etc/cps/image.kdl` to
discover registered roots that use the selected cache. The preview reports
each retained boot entry, its package payload count, and any profile
catalog it keeps. The boot entries and image states stay locked while CPS
computes and applies the retention set.

Repeat `--keep-image` for each root that needs offline repair, including
fallback roots. An image following a signed
profile also keeps the profile's current pin for that cache. Repeat
`--keep-plan` for saved plans and `--keep-catalog SHA256` for other historical
catalogs needed by builds or releases. Supply the references for each cache
you prune. Catalogs, manifests, build evidence, and replay checkpoints stay
available.

Use `--keep-lock image.lock.json` to retain the locked packages' payloads
for that cache's publisher without retaining every payload in the pinned
catalog. CPS checks the lock's publisher trust root, catalog, and manifest digests
before pruning.
