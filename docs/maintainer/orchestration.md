# Plan builds across recipes

[Documentation](../README.md) · [Package maintainers](README.md)

A distribution keeps many [build recipes](builds.md) that depend on each
other's packages. Three read-only commands plan work across them. They
never build, schedule, or publish. Run `cps build` and `cps publish-bundle`
yourself, or from your CI, in the order they print.

## On this page

- [Order recipe builds](#order-recipe-builds)
- [Plan rebuilds after a change](#plan-rebuilds-after-a-change)
- [Repin toolchain catalogs](#repin-toolchain-catalogs)

## Order recipe builds

`cps build-order` reads every build recipe in the given files and
directories and prints the recipe paths in an order where each recipe comes
after the recipes it needs.

```sh
cps build-order packaging
cps build-order --explain packaging/runtime packaging/cps-build.kdl.in
```

```text
packaging/runtime/libseccomp.build.kdl.in
packaging/runtime/sqlite.build.kdl.in
packaging/cps-build.kdl.in
    after packaging/runtime/libseccomp.build.kdl.in: system/cps requires system/libseccomp
    after packaging/runtime/sqlite.build.kdl.in: system/cps requires system/sqlite
```

A directory is searched recursively for `.kdl` and `.kdl.in` files whose
`schema-version` header has `kind="build-recipe"`. Hidden directories and
files named `build.kdl`, the recipe copy inside a build bundle, are skipped.
Name such a file directly to include it. Each recipe produces the packages
of its `manifest` file. Templates ending in `.in` may contain placeholders.
A line that is only a placeholder, such as `@TOOLCHAIN@`, and a bare value
such as `epoch=@EPOCH@` are ignored. When the template's manifest is
missing, CPS reads the same name with `.in` appended.

A recipe follows another recipe when:

- its toolchain block requests a package that the other recipe produces.
  A selector with a publisher, such as `cps://seed/build/cc@1.0.0-0`,
  matches only a manifest from that publisher;
- one of its packages has a `require` or `require-any` entry naming a
  package that the other recipe produces; or
- one of its packages has a `require-capability` for a capability that a
  package of the other recipe declares with `provides`.

The order is conservative: a recipe follows every recipe that could satisfy
one of its needs, so every alternative of a `require-any` and every
provider of a capability come first. Toolchain closures are covered because
each produced package's own requirements order its recipe. Ties keep
sorted path order, so the output is stable. Recommendations, conditional
requirements, and exclusions add no order.

[Generated shared-library capabilities](manifests.md#generated-shared-library-dependencies)
come from build outputs, so they are not known before a build and add no
order here. Declare a `require` on a library package when its recipe must
build first. A recipe whose toolchain is a placeholder or whose manifest is
generated elsewhere is still ordered, with a warning that its needs or
packages are unknown.

Two recipes that produce the same package name are an error. A cycle is
an error that lists each step and the reasons for it.

```text
cps: build dependency cycle:
  packaging/compiler.build.kdl follows packaging/libc.build.kdl: compiler requires libc
  packaging/libc.build.kdl follows packaging/compiler.build.kdl: toolchain needs compiler
```

Break a toolchain cycle by bootstrapping one side from an existing catalog,
the way the seed toolchain does.

## Plan rebuilds after a change

`cps rebuild-plan` lists the packages that depend on a changed package,
using the signed dependency summaries of pinned catalogs. It reads no
manifests, so it stays fast for a whole distribution.

```sh
cps rebuild-plan --config image.kdl --catalog "canopa=$canopa_catalog_sha256" \
    --changed system/sqlite@3.55.0-1 --recipes packaging
```

```text
changed cps://canopa/system/sqlite@3.55.0-1 from cps://canopa/system/sqlite@3.54.1-1
    removes cap/elf/x86_64/libsqlite3.so.0
    adds cap/elf/x86_64/libsqlite3.so.1
breaks cps://canopa/system/cps@1.0.0-0
    requires cap/elf/x86_64/libsqlite3.so.0, which system/sqlite no longer provides
    recipe packaging/cps-build.kdl.in
recipe packaging/cps-build.kdl.in
```

The image configuration supplies the architecture, the publisher order, and
the local repositories or caches. Supply one exact catalog digest per
publisher, as for [`cps manifest dependencies`](manifests.md#reviewing-runtime-dependencies).
Like the solver, CPS takes each package name from the first publisher that
offers an active version for the architecture.

Each `--changed` package is `NAME`, `NAME@VERSION`, or a full package ID.
CPS compares that version, or the newest one, with the newest lower version
in the catalog and prints the capabilities it removes and adds. A soname
bump removes the old soname and its version nodes. Then it checks the
newest version of every other package:

- `breaks` marks a package that requires a capability the change removed.
  It cannot be installed with the new version, so rebuild it before
  publishing.
- `rebuild` marks a package that requires, requires one of, conditionally
  requires, or incorporates the changed package, or requires a capability
  that the changed version provides. Rebuild it to pick up the change.

Only direct dependents are listed. Rebuilding a package for a compatible
change does not force its own dependents to rebuild. Run the plan again
after you publish the rebuilt packages to check for remaining breaks.

With `--recipes`, each listed package names the recipe that produces it,
and the final `recipe` lines list those recipes in [build order](#order-recipe-builds).
A package that no given recipe produces gets a warning.

## Repin toolchain catalogs

A recipe pins an exact toolchain catalog digest for each toolchain
publisher. After a new toolchain catalog is published, move recipes to it
with `cps recipe repin`.

```sh
cps recipe repin --dry-run --toolchain-catalog "seed=$(cat toolchain/current)" packaging
cps recipe repin --toolchain-catalog "seed=$(cat toolchain/current)" packaging
```

```text
repinned packaging/runtime/sqlite.build.kdl seed 3f2a… -> 9c41… (recipe sha256 71d0…)
```

Repeat `--toolchain-catalog PUBLISHER=SHA256` for each publisher to move.
CPS finds recipes the same way as `build-order` and changes only the
`catalog` property of matching `publisher` entries in toolchain blocks. It
edits the text in place, so comments, spacing, and other properties stay as
written, and it confirms the parsed result before writing each file. A pin
spelled in a form it cannot rewrite exactly is an error. A placeholder such
as `@CATALOG_SHA256@` is left for the template. Each file is replaced
atomically and keeps its permissions. `--dry-run` prints the same lines
without writing.

A repinned recipe has a new digest, which the output prints for concrete
recipes. Update the `approve` entries of the
[publish policy](../publisher/publishing.md#publishing-policy) and rebuild
the recipes, in build order, before publishing. Repinning does not change
`key-sha256`. A key rotation needs a reviewed recipe change.
