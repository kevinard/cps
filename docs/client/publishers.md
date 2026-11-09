# Configure trusted publishers

[Documentation](../README.md) · [Clients](README.md)

## Publisher order

An image configuration can declare multiple publisher blocks in search order.
Give each publisher its own repository or synchronized cache and trust key.

```kdl
schema-version 1 kind="image-config"
publisher "canopa" {
    repository "/var/lib/cps/canopa"
    trust-key "/etc/cps/keys/canopa.der"
}
publisher "extras" {
    repository "/var/lib/cps/extras"
    trust-key "/etc/cps/keys/extras.der"
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

`plan install` and `plan exact-install` accept names, `NAME@VERSION`,
`cps://PUBLISHER/NAME`, and full package IDs. A publisher-qualified request can
select a lower-priority publisher for a new package. To change the publisher of
an installed package, remove the package first. An exact version can select an
older version from the package's own publisher when the resulting image
satisfies all dependencies. A version on its own keeps normal publisher order.

On an existing image, use `publisher list` to inspect search order.
`publisher add --root image --repo /var/lib/cps/extras --trust-key extras.der extras`
verifies the signed catalog before appending it. Use
`publisher prefer --root image extras` to move it to the front of its group.
`publisher remove --root image extras` requires that no installed package
comes from that publisher and at least one publisher remains.
These changes invalidate outstanding plans for the image and preserve its
boot environment identity. They are local to the named image root.

Use `publisher set-repository` to move configured publishers to replacement
local repositories or caches. Supply multiple `--repo` arguments to move
the paths together in one transaction.

```sh
cps publisher set-repository --root image \
    --repo canopa=/srv/cps/canopa --repo extras=/srv/cps/extras
```

CPS verifies the replacement catalogs with the installed trust keys and
checks them against the image's catalog checkpoints. Signed profiles validate
their primary catalog and every pinned companion together. An index-only cache works once
it contains the required signed catalogs. Package payloads are fetched during
the normal plan and apply workflow. Relocation invalidates outstanding plans
and preserves the installed packages, boot identity, and applied checkpoints.
Use the new paths in the KDL configuration used for subsequent mirror syncs.

## Refresh policy for mirror publishers

`upgrade` and `check-updates` refresh publishers added with
`publisher add --url` before planning. By default, any refresh failure
stops the update, so an image never keeps using an outdated catalog
without notice. To let updates continue through a mirror outage, declare
a policy for that publisher in `/etc/cps/image.kdl`.

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

`cps status` and `cps check-updates` show each mirror publisher's last
successful refresh, the expiry of its verified TUF timestamp, and its
policy. The JSON report lists them in `publisher_freshness` with
`last_refreshed` as Unix seconds, `age_seconds`, `metadata_expires`, the
policy's `defer_unavailable_hours`, and `beyond_policy`, which is true
when the cache is too old to defer or its age is unknown.
