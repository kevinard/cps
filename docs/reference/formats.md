# Format specifications

This page specifies the bytes of the formats whose exact spelling matters:
documents that are signed or digested, and records CPS keeps across
versions. Each specification has a golden fixture under
[`tests/golden`](../../tests/golden), and a test checks that CPS writes the
fixture byte for byte. A change to one of these fixtures is a format change.

- [Canonical manifest text](#canonical-manifest-text)
- [Signed envelopes](#signed-envelopes)

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
   `hardlink`, `target`. Then `uid` and `gid`, except on `hardlink`, then
   `facet`, then `arch` and the `variant.NAME` selectors in name order.
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
