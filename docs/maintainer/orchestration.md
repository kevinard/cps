# Plan builds across recipes

[Documentation](../README.md) · [Package maintainers](README.md)

A distribution keeps many [build recipes](builds.md) that depend on each
other's packages. Three commands plan work across them: `cps recipe order`
sorts recipes, `cps recipe rebuilds` lists what a change forces you to
rebuild, and `cps recipe repin` refreshes the digests that recipes pin.
None of them builds, schedules, or publishes. Run `cps build` and
`cps publish bundle` yourself, or from your CI, in the order they print.

## On this page

- [Order recipe builds](#order-recipe-builds)
- [Plan rebuilds after a change](#plan-rebuilds-after-a-change)
- [Refresh recipe pins](#refresh-recipe-pins)

## Order recipe builds

`cps recipe order` reads every build recipe in the given files and
directories and prints the recipe paths in an order where each recipe comes
after the recipes it needs. `--explain` adds the reasons under each recipe.

```sh
cps recipe order packaging
cps recipe order --explain packaging/runtime packaging/cps-build.kdl.in
```

```text
packaging/runtime/libseccomp.build.kdl.in
packaging/runtime/sqlite.build.kdl.in
packaging/cps-build.kdl.in
    after packaging/runtime/libseccomp.build.kdl.in: system/cps requires system/libseccomp
    after packaging/runtime/sqlite.build.kdl.in: system/cps requires system/sqlite
```

A directory is searched recursively for `.kdl` and `.kdl.in` files whose
`schema-version` header has `kind="build-recipe"`. Hidden directories are
skipped, and so are build bundles, directories that hold a `build.kdl`
recipe copy next to `manifests/` and `proto/`. Name a bundle's `build.kdl`
directly to include it. A recipe tree can therefore name each recipe
`build.kdl` in its own directory. Each recipe produces the
packages of its `manifest` nodes, so a
[recipe with several packages](builds.md#building-several-packages-from-one-recipe)
counts as the producer of each. Templates ending in `.in` may contain
placeholders: a line that is only a placeholder, such as `@TOOLCHAIN@`, and
a bare value such as `epoch=@EPOCH@` are ignored. When a template's manifest
is missing, CPS reads the same name with `.in` appended.

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
each produced package's own requirements order its recipe. Ties keep sorted
path order, so the output is stable. Recommendations, conditional
requirements, incorporations, and exclusions add no order.

[Generated shared-library capabilities](manifests.md#generated-shared-library-dependencies)
come from build outputs, so they are not known before a build and add no
order here. Declare a `require` on the library package when its recipe must
build first. A recipe whose toolchain is a placeholder, or whose manifest is
generated elsewhere, is still ordered, with a warning that its needs or
packages are unknown.

Two recipes that produce the same package name are an error. A cycle is an
error that lists each step and the reasons for it.

```text
cps: build dependency cycle:
  packaging/gcc.build.kdl follows packaging/glibc.build.kdl: devel/gcc requires system/glibc
  packaging/glibc.build.kdl follows packaging/gcc.build.kdl: toolchain needs devel/gcc
```

Break a toolchain cycle by bootstrapping one side from an existing catalog,
the way the seed toolchain does.

## Plan rebuilds after a change

`cps recipe rebuilds` lists the packages that depend on a changed package,
using the signed dependency summaries of pinned catalogs. It reads no
manifests, so it stays fast for a whole distribution.

```sh
cps recipe rebuilds --config image.kdl --catalog "canopa=$canopa_catalog_sha256" \
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

Repeat `--changed` for each changed package. Each is `NAME`,
`NAME@VERSION`, or a full package ID. CPS compares that version, or the
newest one, with the newest lower version in the catalog, using
[version ordering](manifests.md#versions), and prints the capabilities it
removes and adds. A soname bump removes the old soname and its version
nodes. Then it checks the newest version of every other package:

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
and the final `recipe` lines list those recipes in
[build order](#order-recipe-builds). A package that no given recipe
produces gets a warning.

## Refresh recipe pins

A recipe pins everything it reads by digest: each toolchain publisher's
catalog and trust root, each manifest template, and each source's size and
SHA-256. `cps recipe repin` recomputes the pins you select and rewrites
them in place, so a template edit or a new toolchain catalog does not mean
editing hashes by hand across the tree.

```sh
cps recipe repin --dry-run --toolchain-catalog "seed=$(cat toolchain/current)" packaging
cps recipe repin --toolchain-catalog "seed=$(cat toolchain/current)" \
    --trust-root toolchain.trust.json packaging
cps recipe repin --manifests --local-sources --sources distfiles recipes
```

```text
repinned recipes/system/sqlite/build.kdl toolchain seed catalog 3f2a… -> 9c41…
repinned recipes/system/sqlite/build.kdl manifest sqlite.kdl sha256 5be0… -> 0d17…
recipe recipes/system/sqlite/build.kdl sha256 71d0…
```

Choose at least one kind of pin:

| Option | Rewrites |
| --- | --- |
| `--toolchain-catalog PUBLISHER=SHA256` | `catalog` of that toolchain publisher. Repeat for each publisher to move. |
| `--trust-root PATH` | `key-sha256` of the toolchain publisher that the trust root names, set to the file's SHA-256. CPS verifies the file as a trust root first. Repeat for each publisher. |
| `--manifests` | `sha256` of each `manifest`, from the template at its path next to the recipe. |
| `--local-sources` | `size` and `sha256` of each `source` found at its path in the recipe's own directory, such as a checked-in patch. |
| `--sources DIR` | `size` and `sha256` of each `source` found at its path under `DIR`, a directory laid out like `cps build --sources`. Repeat to search several directories in order, after the recipe's directory with `--local-sources`. |

This is the counterpart of `updpkgsums`: download or update a source, then
repin it from the file. A content-addressed `--source-cache` cannot refresh
pins, because it finds each object by the pin itself; `cps source fetch`
and `cps build` verify cached objects against their pins. A selected
manifest or source that CPS cannot find is left as written with a warning
that its pin is unchecked.

CPS finds recipes the same way as `recipe order` and changes only the
selected properties of matching `publisher`, `manifest`, and `source`
nodes. It edits the text in place, so comments, spacing, and other
properties stay as written, and it confirms the parsed result of every file
before writing any of them. A pin spelled in a form it cannot rewrite
exactly, such as a node split over several lines, is an error, and then no
recipe changes. A placeholder such as `@CATALOG_SHA256@` is left for the
template. Each file is replaced atomically and keeps its permissions.

Each change prints one `repinned` line with the recipe, the pin, and the
old and new values. A concrete recipe then gets a `recipe PATH sha256`
line with its new digest. `--dry-run` writes nothing and prints
`would repin`. `--check` writes nothing, prints each stale pin as a
`stale` line, and exits with status 1 when any pin is stale, which suits
CI:

```sh
cps recipe repin --check --manifests --local-sources \
    --trust-root seed.trust.json --toolchain-catalog "seed=$seed_catalog" recipes
```

A repinned recipe has a new digest. Update the `approve` entries of the
[publish policy](../publisher/publishing.md#publishing-policy) and rebuild
the recipes, in build order, before publishing. A changed `key-sha256`
means the toolchain publisher rotated its trust root; review that change
like any other recipe change.

Run [`tests/recipe-orchestration.sh`](../../tests/recipe-orchestration.sh)
after `cargo build` to exercise build order, cycle reports, pin refreshes and checks, and a
soname rebuild plan.
