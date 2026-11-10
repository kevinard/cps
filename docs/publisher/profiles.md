# Define and validate signed release profiles

[Documentation](../README.md) · [Publishers](README.md)

A [release profile](../concepts.md#release-profiles-and-locks) is a named
image composition that the publisher resolves, checks, and signs into the
catalog before publishing. Images that follow the profile install exactly
that composition and can still add compatible local packages.

## On this page

- [Checking release composition](#checking-release-composition)
- [Checking upgrades from deployed releases](#checking-upgrades-from-deployed-releases)
- [Checking shared libraries](#checking-shared-libraries)
- [Companion publishers](#companion-publishers)
- [Phased profile updates](#phased-profile-updates)
- [Security advisories](#security-advisories)

## Checking release composition

Add a `profile` inside the policy's `publisher` block in
[`publish-policy.kdl`](publishing.md#publishing-policy) to require a
representative image composition before changing the catalog.

```kdl
profile "desktop" architecture="x86_64" {
    package "desktop/base"
    package "system/kernel@7.2.11-0"
    facet "doc.man" #false
    facet "locale.*" #false
    variant "container" #false
    allow-facet-override "devel"
}
```

CPS resolves each profile against the candidate catalog using the image
solver and the specified architecture, facet rules, and variants. An
unversioned profile package selects its newest active version.
Dependencies can select any compatible active version. The check covers
dependencies, file ownership, and the consistency of selected kernel
actions. Profile roots come from the release's publisher.

Facet rules accept exact names and trailing wildcards, with the same
precedence and built-in `debug.*` and `isa.*` defaults as images. Facets
and variants select content within the resolved packages and never change
the package set. `allow-facet-override PATTERN` lets images add per-package
facet rules for matching facets, such as `devel` or `doc.*`, on
profile-protected packages. Without it, images may only override facets of
packages they install locally. See
[facet rules on profile images](../client/profiles.md#facet-rules-on-profile-images).

Publish coordinated changes in one batch when the individual changes would
break a profile. The first catalog that passes these checks records the
requirement in its signature. Subsequent publications and withdrawals
require a policy with at least one profile.

Signed profiles record the exact versions of the requested roots and the
manifest digest of every package in the checked composition. The primary
catalog signs the checked architecture, facets, variants, and allowed
facet overrides. A profile that records its roots retains its image
configuration across releases. Use a separate profile name for a new
configuration.

## Checking upgrades from deployed releases

Once a profile has a signed composition, CPS checks an upgrade from the
current release before publishing its successor. Set
`upgrade-from="<signed catalog SHA-256>"` on the profile to check an
additional historical release, or to provide a baseline before the first
composition is signed. For a fleet with several supported deployed
releases, add one `upgrade-from "<catalog SHA-256>"` child per historical
composition inside the profile block. Each child requires a signed
composition for that profile and uses its recorded companion pins. The
current release is still checked.

When the baseline has the same named profile, CPS replays its recorded
composition using its companion pins. The architecture, facets, variants,
and publisher order must match the baseline image configuration. For a
baseline catalog without the named profile, CPS selects the newest baseline
versions of the current profile's root names.

CPS runs the update planner with the resulting installed set. For a signed
baseline profile, the candidate's signed package set supplies exact
constraints. A baseline catalog without a signed profile uses the ordinary
solver with the candidate's root versions as the acceptance check. The
result must retain only packages available in the candidate and pass the
planner's directory and kernel transition checks. This models the managed
package state of a clean image. Live changes and boot health are checked
during staging and boot.

Use `repo publish-release --prepare` to stage the signed candidate and its
objects while keeping the current catalog pointer on the previous release.
Test the candidate digest in a private repository used by your upgrade and
boot checks. Then rerun publication with `--expect-catalog SHA256` to
advance the pointer only if the release base is still current and the
recomputed catalog equals the tested digest. The
[release process](release-process.md) automates these steps.

## Checking shared libraries

For a native x86-64 or AArch64 profile, add `elf-check` with the image's
ordered default library directories.

```kdl
profile "desktop" architecture="x86_64" {
    package "desktop/base"
    elf-check {
        library-dir "/usr/lib"
        library-dir "/lib64"
    }
}
```

CPS hashes the selected payloads and reads ELF program headers as data. It
checks `PT_INTERP` and `DT_NEEDED` against the signed file map, including
symlinks, hard links, and pinned companion packages. Each object can use
its own `DT_RUNPATH` or `DT_RPATH`, with absolute paths and `$ORIGIN`
supported. The check covers file availability, ELF class and machine
compatibility, and the GNU version nodes required by each direct
dependency. It follows version-table pointers in the dynamic segment, so it
works on objects with section headers removed. The selected library must
define each non-weak required version with its recorded name and hash.
Symbol-by-symbol relocation, libraries loaded with `dlopen`, inherited
search paths, and loader state from the running system are outside its
scope.

Set `require-declarations=#true` on the `elf-check` block to also require a
declaration for each external package supplying a managed ELF interpreter
or shared library. An interpreter needs a matching direct `require`. A
library is declared by a matching `require` or by the
[generated capability requirement](../maintainer/manifests.md#generated-shared-library-dependencies)
for that library, which the provider satisfies. The publisher checks the
actual provider in the signed composition. This requirement carries forward
to later catalogs for the profile.

Generated capabilities let the profile solver skip a provider version that
lost a required soname or version node, so the gate inspects compositions
that already resolve. The gate still checks the profile's own ordered
library directories, interpreters, and content selected by facets.

The gate applies to the fresh composition and the result of a checked
upgrade. The signed profile records its library directories and version
checking requirement. Later catalogs must retain those checks for that
profile. Run [`tests/elf-profile.sh`](../../tests/elf-profile.sh) to
exercise the publication gate with a compiler-generated executable and
shared library and GNU `objcopy` with `--strip-section-headers` support.

## Companion publishers

List companion publishers inside a profile when dependencies are supplied
by another repository.

```kdl
profile "cps-runtime" architecture="x86_64" {
    package "system/cps"
    package "system/canopa-trust"
    package "system/git-build"
    publisher "canopa-runtime-seed" catalog="<signed seed catalog SHA-256>" {
        repository "../runtime-seed"
        trust-root "../runtime-seed.trust.json"
    }
}
```

Paths resolve relative to the directory containing `publish-policy.kdl`.
The signer checks each pinned catalog against the configured trust root. It
uses the release publisher first, then companions in declaration order.
During an upgrade check, installed packages keep their original publishers.
Set `upgrade-from` on a companion to pin a different baseline catalog for
that publisher when the baseline lacks a signed record for the profile. If
the baseline has that record, any companion `upgrade-from` must match its
signed pin. A baseline without a profile record or a companion override
uses the candidate's `catalog` pin.

The primary catalog signs the ordered companion catalog digests. For each
companion it also records the signed catalog sequence and the SHA-256 of
the companion's version 1 trust root, which identifies the publisher across
trust root rotations. Images verify both against the pinned companion
catalog and their configured trust root.

When advancing a profile's companion pin, keep the previous catalog object
in the publishing machine's companion repository or cache. CPS checks that
the new companion catalog advances the sequence and preserves known package
identities, withdrawals, attestations, and trust root history. Primary
mirror sync rejects successors that drop recorded companion lineage,
replace its version 1 trust root, or reuse a sequence with another digest.
A companion's key rotation through a new trust root version keeps its
lineage. Saved plans continue to use their original catalog set.

Add a `build-trust` block inside a companion publisher to require version 1
build provenance for the companion packages in the fresh composition and in
the result of a checked upgrade. This includes packages the update solver
retains from the baseline. Builder keys must be separate from every key in
the primary and companion publishers' trust root histories.

```kdl
build-trust minimum-builders=2 {
    builder-key "../keys/worker-a.der"
    builder-key "../keys/worker-b.der"
}
```

The primary signs the companion's builder keys and threshold with its
catalog pin. Images following the profile verify that policy for all
companion packages selected by an install or update, including unchanged
packages and packages outside the release's tested composition. For
unchanged packages, CPS reuses archived evidence that satisfies the signed
policy or checks the attestation in the pinned catalog. New and changed
packages must also satisfy any independently configured image policy for
their publisher. Applying a guarded plan also archives the primary catalog
that signed the companion policy. Use
`cps publisher audit-builds --root image seed` to verify installed packages
against that signed policy and any local policy for the companion, even
when the repositories are offline. Later catalogs can rotate the signed
policy and must retain a builder policy for the guarded companion in that
profile.

## Phased profile updates

Set `rollout-percentage` on a profile in the publisher's release policy.
The value is signed with the checked composition.

```kdl
profile "desktop" architecture="x86_64" rollout-percentage=10 {
    package "desktop/base@2.0.0-0"
}
```

An image outside the rollout defers the entire composition-changing
upgrade, including the packages from pinned companion catalogs. A
percentage of `0` pauses these upgrades and `100` makes the profile
available to every cohort. Omitting the property also makes the profile
available to everyone. New images can install a profile at any percentage.
Mirror refreshes continue while the package upgrade is deferred.

CPS assigns a cohort from 0 to 99 using the image's original identity and
the target profile's roots, manifest digests, and companion catalog pins.
Boot environments inherit their source image's cohort for the same target.
Images cloned from the same provisioned image share a cohort. Independently
created images get separate identities by default.

To widen the rollout, change the percentage and publish with
`refresh-profiles=#true` using the current catalog as the release base, or
run `cps repo widen-rollout` on the exact current catalog. Keeping the same
composition, roots, and companion pins keeps the same cohorts. If an
earlier composition is still deployed, the publish policy's `upgrade-from`
can also check the transition from that catalog. The
[release process](release-process.md#widen-a-phased-rollout) can widen in
approved steps after a health check.

A deferred `plan update` reports the reason without saving a plan.
Operators can override the gate explicitly.

```sh
sudo cps upgrade --force-phased-update
cps plan --root image --output upgrade.json update --force-phased-update
```

The override is recorded in the saved plan and transaction history. The
normal signature, dependency, and image checks still apply.

## Security advisories

Attach an advisory to the profile that contains the fix. The publisher
checks that the package is in the selected composition and that its
selected version falls outside the affected range.

```kdl
advisory "CPS-2026-01" package="library/runtime" affected="<2.0.0-0" summary="Runtime security fix" action="Restart dependent applications"
```

`check-updates` reports matching advisories for installed packages,
including when the target profile is deferred. The signed advisory reports
the fixed version and any operator action. Later releases carry the
advisory forward while the profile still contains the affected package from
the same publisher. Publication rejects a new profile version that matches
the affected range. Publishers can revise a notice by declaring it again
for the same ID and package.
