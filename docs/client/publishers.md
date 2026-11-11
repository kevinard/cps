# Configure trusted publishers

[Documentation](../README.md) · [Clients](README.md)

A publisher signs the catalog of packages that an image installs. The image
trusts each publisher through a pinned [trust root](#pinned-trust-roots) and
searches its publishers in a fixed order.

## On this page

- [Publisher order](#publisher-order)
- [Add, reorder, and remove publishers](#add-reorder-and-remove-publishers)
- [Add a publisher from an HTTPS mirror](#add-a-publisher-from-an-https-mirror)
- [Move publisher repositories](#move-publisher-repositories)
- [Pinned trust roots](#pinned-trust-roots)
- [Refresh policy for mirror publishers](#refresh-policy-for-mirror-publishers)
- [Require build evidence](#require-build-evidence)

## Publisher order

An image configuration can declare multiple publisher blocks in search order.
Give each publisher its own repository or synchronized cache and pinned trust
root.

```kdl
schema-version 1 kind="image-config"
publisher "canopa" {
    repository "/var/lib/cps/canopa"
    trust-root "/etc/cps/keys/canopa.trust.json"
}
publisher "extras" {
    repository "/var/lib/cps/extras"
    trust-root "/etc/cps/keys/extras.trust.json"
}
```

By default, for a new package, the solver considers versions from the first
publisher that offers the package name. An installed package stays with its original
publisher. Each plan that resolves packages pins a signed catalog per publisher,
and apply checks the same catalog set and stages the selected payloads before
changing the image. [IPS publisher search order and stickiness](https://docs.oracle.com/cd/E53394_01/html/E54739/gijmo.html)

`list --available` shows the newest version from the selected publisher for each
name. Use `list --available --all-versions` to see every publisher's catalog
entries, or a full package ID with `info --available` to inspect a particular one.

`install` and `install --exact` accept the
[package selectors](queries.md#package-selectors-and-versions): names,
`NAME@VERSION`, `cps://PUBLISHER/NAME`, and full package IDs. A
publisher-qualified request can select a lower-priority publisher for a new
package. To change the publisher of an installed package, remove the package
first. An exact version can select an older version from the package's own
publisher when the resulting image satisfies all dependencies. A version on
its own keeps normal publisher order.

## Add, reorder, and remove publishers

```sh
cps publisher list --root image
cps publisher add --root image --repo /var/lib/cps/extras --trust-root extras.trust.json extras
cps publisher prefer --root image extras
cps publisher remove --root image extras
```

`publisher add` verifies the signed catalog with the trust root before
appending the publisher. `publisher prefer` moves it to the front of its
group. `publisher remove` requires that no installed package comes from that
publisher and at least one publisher remains. These changes invalidate
outstanding plans for the image and preserve its boot environment identity.
They are local to the named image root. Without `--root`, every `publisher`
command acts on the running system at `/`.

On an image that follows a signed release profile, the profile fixes its
publishers and their order. Local publishers come after the signed ones, and
`prefer` and `remove` act only within that group; see
[profile images](profiles.md#add-local-publishers-to-a-profile-image).

## Add a publisher from an HTTPS mirror

On a running system, add a publisher from its mirror URL. CPS verifies the
mirror's TUF metadata with the preinstalled TUF root at
`/etc/cps/tuf-root.json`, then fetches the publisher name and its newest
trust root as authenticated targets.

```sh
sudo cps publisher add https://packages.example.org/extras/
sudo cps install cps://extras/apps/editor
```

CPS displays the mirror, the TUF root's SHA-256, and the trust root's
version, expiry, and the SHA-256 fingerprints of its root and catalog keys,
and asks `Trust this publisher? [y/N]`. After confirmation, CPS checks the
catalog signatures and pins that trust root for later refreshes. Provision
the TUF root through a trusted channel, such as the signed installer. Use
`--tuf-root PATH` to select another locally trusted root.

To provision without trusting the first response, pass the root key
fingerprints that the publisher announced through another channel. CPS then
requires the fetched trust root to name exactly those root keys and does not
ask. This also works without a terminal.

```sh
sudo cps publisher add https://packages.example.org/extras/ \
    --expect-root-key 3f2a…e91c
```

Repeat `--expect-root-key` for each root key. For independently provisioned
trust, supply the TUF root metadata and the publisher's trust root file
explicitly.

```sh
sudo cps publisher add extras --url https://packages.example.org/extras/ \
    --tuf-root /etc/cps/keys/extras-root.json \
    --trust-root /etc/cps/keys/extras.trust.json
```

The URL must serve `metadata/` and `targets/` beneath it. CPS verifies
and registers the mirror in one command, and `upgrade` refreshes it
automatically under its [refresh policy](#refresh-policy-for-mirror-publishers).
The default cache lives in `repositories/NAME` under the configured Btrfs
environments directory, so it is available across boots. Use `--cache PATH`
for a different location, which is required when adding to an offline image
with `--root`. Add `--build-trust POLICY` to
[require build evidence](#require-build-evidence) from the start.

## Move publisher repositories

Use `publisher set-repository` to move configured publishers to replacement
local repositories or caches. Supply multiple `--repo` arguments to move
the paths together in one transaction.

```sh
cps publisher set-repository --root image \
    --repo canopa=/srv/cps/canopa --repo extras=/srv/cps/extras
```

CPS verifies the replacement catalogs with the pinned trust roots and
checks them against the image's catalog checkpoints. Signed profiles validate
their primary catalog and every pinned companion together. An index-only cache works once
it contains the required signed catalogs. Package payloads are fetched during
the next change that needs them. Relocation invalidates outstanding plans
and preserves the installed packages, boot identity, and applied checkpoints.
Use the new paths in the KDL configuration used for subsequent mirror syncs.

## Pinned trust roots

A publisher's [trust root](../publisher/publishing.md#publisher-trust-root)
names its offline root keys and online catalog keys, each with a signature
threshold, and an expiry. The publisher signs a new version of it for every
key change. Each catalog carries the publisher's whole trust root history,
from version 1 to the version it is signed under.

`trust-root` in the image configuration, or `--trust-root`, names a file
holding one version. The image stores that version when you create the image
or add the publisher. Any version works, so provision the newest one you
have, and inspect it first:

```sh
cps repo trust show extras.trust.json
```

CPS accepts a catalog only if its history contains the pinned version
unchanged, each later version is signed by the root keys of the previous
version and of the new one, and exactly the catalog threshold of the newest
version's catalog keys sign the catalog.

### Rotation and revocation

Key rotation needs nothing from you. When the publisher rotates or revokes a
key, the next catalog carries the new trust root version, and CPS verifies it
from your pinned version. The image's catalog checkpoint for the publisher
records the history of the catalog it last applied, which is the highest
version it has verified. A later catalog must extend that history, so CPS
rejects as a rollback a catalog signed under an older version, such as one
signed with a key that a later version revoked. The
[catalog checkpoints](mirrors.md#catalog-checkpoints-and-archived-signatures)
also reject older catalog sequences.

Historical catalogs that a saved plan, lock, or signed profile pins exactly
still verify against their own history, even if a key in it was later
revoked: they were valid when signed, and they cannot replace a newer
current catalog. A pinned version that is later than a catalog's history
cannot verify that catalog, and CPS reports that the catalog is signed under
an older trust root version than the pinned one. Pin the oldest version
whose catalogs you still need, such as the catalog of an image lock.

### Recovery after a key compromise

When a catalog key is compromised, the publisher signs a trust root version
without it and re-signs its current catalog with a new catalog key. Once
your image applies that catalog, CPS rejects every catalog signed under the
compromised version. Until it refreshes, an image can still accept a catalog
that the attacker signed. Mirror freshness, which the attacker would also
need to defeat, and the trust root expiry bound that window, so refresh
promptly after the publisher announces a compromise. Provision new images
with the newest trust root, or with `--expect-root-key` and its root key
fingerprints, so they never trust the compromised version.

If the publisher loses its root keys, or an attacker holds a threshold of
them, the publisher starts a new trust root. Its catalogs do not contain the
history that your image has verified, so CPS rejects them. An attacker's
alternative history from an older version is rejected the same way. To
follow the publisher's new trust root, provision images with it as you
would for a new publisher.

### Expiry

CPS rejects a new current catalog whose trust root version has expired.
Pinned historical catalogs are not subject to expiry. The publisher renews
the trust root by signing a new version before it expires. An expired trust
root is a signed statement from the publisher, not a mirror outage, so it
fails an update whatever the [refresh policy](#refresh-policy-for-mirror-publishers).

## Refresh policy for mirror publishers

`upgrade` and `upgrade --check` refresh publishers added with a mirror URL
before planning. By default, any refresh failure stops the update, so an
image never keeps using an outdated catalog without notice. To let updates
continue through a mirror outage, declare a policy for that publisher in
`/etc/cps/image.kdl`.

```kdl
refresh-policy "extras" on-unavailable="defer" max-staleness-hours=72
```

With `on-unavailable="defer"`, CPS defers updates from the publisher when
the mirror is unreachable, too slow, or serves expired metadata, as long as
the last successful refresh is no older than `max-staleness-hours` (1 to
8784). Packages from that publisher stay at their installed versions.
Beyond that age, or when CPS has no record of a refresh, the update fails.
Failures that indicate tampering or a broken cache always fail: invalid
signatures, rollback to older metadata, digest or size mismatches, and CPS
catalog check failures. `on-unavailable="fail"` states the default
explicitly. Policies do not apply to the publishers in the image
configuration or the signed profile, which must always refresh.

A mirror stays fresh between releases because its publisher re-signs the
short-lived TUF timestamp, typically every day. A deferred publisher stays
usable until the earlier of `max-staleness-hours` after its last successful
refresh and the expiry of the trust root its current catalog is signed under.

`cps status` and `cps upgrade --check` show each mirror publisher's last
successful refresh, the expiry of its verified TUF timestamp, the expiry of
the trust root its cached current catalog is signed under, and its policy,
for example
`Publisher extras: refreshed 5 hours ago, metadata expires 2026-11-11T00:00:00Z, trust root expires 2027-06-01T00:00:00Z, fail when unavailable`.
The JSON report of `upgrade --check` lists them in `publisher_freshness` with
`last_refreshed` as Unix seconds, `age_seconds`, `metadata_expires`,
`trust_root_expires`, the policy's `defer_unavailable_hours`, and
`beyond_policy`, which is true when the cache is too old to defer, its
trust root has expired, or its age or trust root is unknown. For a publisher
with a defer policy, the text report then ends with `(stale beyond policy)`.

## Require build evidence

The trust root does not name builder keys. To require that installed
packages carry signatures from build workers you choose, add `build-trust`
to a publisher block, or replace it on an existing image:

```sh
cps publisher set-build-trust --root image --policy builders.kdl canopa
cps publisher audit-builds --root image canopa
```

See [Verifying builds on images and workers](../publisher/rebuilds.md#verifying-builds-on-images-and-workers).
