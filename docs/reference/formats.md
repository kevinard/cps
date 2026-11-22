# Format specifications

This page specifies the bytes of the formats whose exact spelling matters:
documents that are signed or digested, and records CPS keeps across
versions. Each specification has a golden fixture under
[`tests/golden`](../../tests/golden), and a test checks that CPS writes the
fixture byte for byte. A change to one of these fixtures is a format change.

- [Canonical manifest text](#canonical-manifest-text)
- [Signed envelopes](#signed-envelopes)
- [EFI System Partition state](#efi-system-partition-state)

## Canonical manifest text

A package manifest is a KDL 2.0 document of kind `package-manifest`.
Maintainers may write manifests in any valid spelling, but CPS signs,
digests, and compares the canonical text it writes from the parsed
manifest. Two CPS builds write the same bytes for the same manifest, so
independent rebuilds and republished packages compare equal. Golden
fixture: [`package-manifest.kdl`](../../tests/golden/package-manifest.kdl).

The document is UTF-8 and holds two nodes: the
`schema-version 1 kind="package-manifest"` header and one `package` node.

**Layout.** Each node is on its own line, indented four spaces per level of
nesting. Every line ends with a line feed (`\n`), including the last; there
is no carriage return, blank line, comment, or trailing space. A node is its
name followed by its arguments and then its properties, each preceded by one
space. A node with children ends its line with ` {`, its children follow one
level deeper, and a line at the node's own indentation holding `}` closes
the block. A node without children has no block, not even an empty one.

**Names.** A node name or property key is written bare when it is an ASCII
letter followed by ASCII letters, digits, `-`, `_`, or `.`, and as a quoted
string otherwise.

**Values.** Strings are always quoted, never bare or raw. Inside a string,
`\` is written `\\` and `"` is written `\"`; line feed, carriage return,
tab, backspace, and form feed are written `\n`, `\r`, `\t`, `\b`, and `\f`.
Every other control character, the KDL newlines U+0085, U+2028, and U+2029,
the bidirectional formatting characters U+200E, U+200F, U+202A to U+202E,
and U+2066 to U+2069, and U+FEFF are written `\u{X}` with lowercase
hexadecimal digits and no leading zeros. Every other character is written as
itself. Integers are decimal with a `-` sign when negative. Booleans are
`#true` and `#false`. Manifests have no floating-point values, nulls, or
type annotations.

**Order.** The `package` node's argument is the package name, followed by
these properties in this order, each only when set: `publisher`, `version`,
`upstream-version`, `architecture`, `requires-reboot=#true`, `summary`,
`license`, `homepage`, `source-url`. Its children come in this order:

1. Dependencies in the order the manifest declares them: `require`,
   `require-capability`, `recommends`, `incorporate`, `exclude`,
   `require-any` with one `package` child per alternative, and
   `conditional` with `when` and then `when-version`. A dependency target is
   the package argument followed by `version` when the dependency has one.
2. `provides` nodes in name order, each with `version` when set.
3. `optional` nodes, each a target followed by `description`.
4. `mediate` nodes in path order, with `group` and then `priority` when it is
   not zero.
5. Actions in the order the manifest declares them: `file`, `dir`,
   `symlink`, and `hardlink`. After the path come the kind's own properties:
   for `file`, `mode`, `preserve=#true` when set, `original-name`, `sha256`,
   `size`, and `capabilities`; for `dir`, `mode`; for `symlink` and
   `hardlink`, `target`. Then `owner` and `group`, each only when it names
   an account other than `root` and never on `hardlink`, then `facet`, then
   `architecture` and the `variant.NAME` selectors in name order.
6. `sysusers`, `tmpfiles`, and `preset` nodes in the order declared.
7. `trigger` nodes in the order declared, with `timeout` when set and a block
   holding `run` with the handler and its arguments, then `watch`, `output`,
   `reload`, and `restart` children in that order.
8. `kernel-install` with the release and `initrd=#true` when set.

## Signed envelopes

CPS signs documents in two envelopes, both with Ed25519 and both accepted in
one byte form only. A signature names its key by the lowercase hexadecimal
SHA-256 of the raw 32-byte public key, and signatures are sorted by unique
key ID. Readers never skip an entry they cannot decode or reinterpret
another spelling. Every reader checks signatures with the same rule: a
signature by an authorized key must verify, or the document is rejected,
whatever the threshold.

| Envelope | Signs | Specification |
| --- | --- | --- |
| CPS envelope, `{"signed":…,"signatures":[{"keyid":…,"sig":…}]}`, with hexadecimal signatures over a domain prefix and the payload | Catalog roots (`cps signed catalog root v1`) and publisher trust roots (`cps publisher trust root v1`) | [Signed catalog format](../publisher/publishing.md#signed-catalog-format) |
| DSSE, `{"payloadType":…,"payload":…,"signatures":[{"keyid":…,"sig":…}]}`, with base64 signatures over the DSSE pre-authentication encoding | In-toto build statements | [Building and publishing with signed evidence](../maintainer/builds.md#building-and-publishing-with-signed-evidence) |

Build statements keep DSSE so that in-toto tooling can read them. The
envelope's version is part of what is signed: the domain prefix of a CPS
envelope and the `payloadType` and statement `predicateType` of a DSSE
envelope.

## EFI System Partition state

This section specifies version 1 of the boot state that CPS keeps on an EFI
System Partition (ESP). Every boot entry CPS writes carries the line
`# cps-boot-entry-version 1`, and CPS rejects an entry with another version.
CPS writes every file through a temporary file and a rename, syncing the
file, the rename, and the directory, so a crash leaves each path either
before or after its write.

**Identifiers.** A boot environment is named `cps-IMAGE-REVISION`, where
`IMAGE` is the root's image UUID and `REVISION` its boot revision. A root
registered for boot carries that ID in its `user.cps.boot-environment`
extended attribute, and CPS refuses to register a root whose attribute names
another ID.

**Files.** Paths are relative to the ESP root.

| Path | Contents |
| --- | --- |
| `loader/loader.conf` | The systemd-boot configuration. CPS owns the `default ID.conf` line, which names a known-good entry once the first environment is registered, and a `preferred ID.conf` line while a trial is selected. Every other line belongs to the administrator. |
| `loader/entries/ID.conf` | The Type #1 entry of a known-good environment. |
| `loader/entries/ID+LEFT.conf`, `ID+LEFT-DONE.conf` | A trial with systemd-boot boot counting: `LEFT` attempts remain and `DONE` were used. `ID+0-DONE.conf` is an exhausted trial. systemd-bless-boot renames a blessed trial to `ID.conf`. |
| `loader/entries/ID.cps-destroy` | The entry of an environment that `cps boot destroy` withdrew from systemd-boot before it removed the root and boot files. |
| `EFI/cps/ID/vmlinuz.efi`, `EFI/cps/ID/initramfs.img` | The kernel and initramfs of a split entry. |
| `EFI/cps/ID/uki.efi` | The signed unified kernel image of a UKI entry. |
| `EFI/cps-uefi/ID.conf` | The record of a known-good environment that boots directly from the firmware, with the same body as a Type #1 entry. systemd-boot does not read this directory. |
| `EFI/cps-uefi/ID+1.conf`, `ID+0.conf`, `ID+0-1.conf` | A direct UEFI trial: armed with `BootNext` for one attempt, cancelled or spent, or verified by `cps boot bless` before its firmware promotion. |
| `.cps-tmp-*` in any of these directories | A temporary file of a write in progress. |

**Entry body.** An entry holds these lines in this order, each ending with
a line feed: `title`, `sort-key cps`, `version REVISION`,
`# cps-boot-entry-version 1`, then `linux /EFI/cps/ID/vmlinuz.efi` or
`uki /EFI/cps/ID/uki.efi`, `# cps-parent ID` naming the known-good fallback
of a trial, and the artifact digests: `# cps-uki-sha256 HEX`, or
`# cps-linux-sha256 HEX` followed, with an initramfs, by
`initrd /EFI/cps/ID/initramfs.img` and `# cps-initrd-sha256 HEX`. The root's
Btrfs identity follows as `# cps-btrfs-fsid`, `# cps-btrfs-uuid`,
`# cps-btrfs-subvolid`, and `# cps-btrfs-path`, then `options CMDLINE`, or
`# cps-uki-cmdline CMDLINE` for a UKI, whose command line is inside the
image. A direct UEFI trial adds `# cps-uefi-armed BOOT-ID`, the boot that
armed it, and a named environment adds `# cps-name NAME`.

**Selection.** systemd-boot boots the `preferred` entry while its counter
has attempts left, and otherwise the `default` entry. Direct UEFI boot
selects with `BootNext` for a trial and the first `BootOrder` option for the
default.

**Interrupted operations.** Each state that a crash can leave has one owner
that finishes it:

| State | Finished by |
| --- | --- |
| `.cps-tmp-*` files | Any CPS operation that takes the ESP lock exclusively removes them first, since every writer holds that lock. |
| An entry renamed to `ID.cps-destroy` | `cps boot destroy ID`. Every other operation refuses to reuse that ID. |
| A direct UEFI record `ID+0-1.conf` | `cps boot bless`, which promotes the verified trial. |
| A `preferred` line naming an exhausted trial | Nothing: systemd-boot ignores it and boots `default`. The next activation replaces it, and destroying the trial, as `cps boot discard-trial` and pruning do, removes it first. |
| A capture, UKI, or kernel-install scratch directory | The next capture into the same directory, UKI build, or kernel preparation in the same image. Each scratch directory, named `.cps-capture-*`, `.cps-uki-*`, or `.kernel-install-*`, is locked with `flock` by the process using it, and only unlocked ones are removed. |
