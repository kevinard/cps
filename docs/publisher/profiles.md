# Define and validate signed release profiles

[Documentation](../README.md) · [Publishers](README.md)

## Checking release composition

Add a `profile` inside the policy's `publisher` block to require a representative
image composition before changing the catalog.

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

CPS resolves each profile against the candidate catalog using the image solver
and the specified architecture, facet rules, and variants. Facet rules accept
exact names and trailing wildcards, with the same precedence and built-in
`debug.*` and `isa.*` defaults as images. `allow-facet-override PATTERN` lets
images add per-package facet rules for matching facets, such as `devel` or
`doc.*`, on profile-protected packages. Without it, images may only override
facets of packages they install locally. See
[facet rules on profile images](../client/profiles.md#facet-rules-on-profile-images). An unversioned profile package
selects its newest active version. Dependencies can select any compatible active
version. The check covers dependencies, file ownership, and the consistency of
selected kernel actions. Publish coordinated changes in one batch when the
individual changes would break a profile. The first catalog that passes these
checks records the requirement in its signature. Subsequent publications and
withdrawals require a policy with at least one profile. Profile roots come from
the release's publisher.

Once a profile has a signed composition, CPS checks an upgrade from the
current release before publishing its successor. Set
`upgrade-from="<signed catalog SHA-256>"` to check an additional historical
release, or to provide a baseline before the first composition is signed.
For a fleet with several supported deployed releases, add one
`upgrade-from "<catalog SHA-256>"` child per historical composition inside
the profile block. Each child requires a signed composition for that profile
and uses its recorded companion pins. The current release is still checked.

Use `repo publish-release --prepare` to stage the signed candidate and its
objects while keeping the current catalog pointer on the previous release.
Test the candidate digest in a private repository used by your upgrade and
boot checks. Then rerun publication with `--expect-catalog SHA256` to
advance the pointer only if the release base is still current and the
recomputed catalog equals the tested digest.

Signed profiles record the exact versions of the requested roots and
the manifest digest of every package in the checked composition. When
the baseline has the same named profile, CPS
replays its recorded composition using its companion pins. The architecture,
facets, variants, and publisher order must match the baseline image configuration.
For a baseline catalog without the named profile, CPS
selects the newest baseline versions of the current profile's root names.

CPS runs the update planner with the resulting installed set. For a signed
baseline profile, the candidate's signed package set supplies exact
constraints. A baseline catalog without a signed profile uses the ordinary
solver with the candidate's root versions as the acceptance check. The
result must retain only packages available in the candidate and pass the
planner's directory and kernel transition checks. This models the managed
package state of a clean image. Live changes and boot health are checked during
staging and boot. A profile that records its roots retains its image
configuration across releases. Use a separate profile name for a new
configuration.

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
symlinks, hard links, and pinned companion packages. Each object can use its
own `DT_RUNPATH` or `DT_RPATH`, with absolute paths and `$ORIGIN` supported.
The check covers file availability, ELF class and machine compatibility,
and the GNU version nodes required by each direct dependency. It follows
version-table pointers in the dynamic segment, so it works on objects with
section headers removed. The selected library must define each non-weak
required version with its recorded name and hash. Symbol-by-symbol
relocation, libraries loaded with `dlopen`, inherited search paths, and
loader state from the running system are outside its scope.

Set `require-declarations=#true` on the `elf-check` block to also require
a matching direct `require` for each external package supplying a managed
ELF interpreter or shared library. The publisher checks the actual provider
in the signed composition. This requirement carries forward to later
catalogs for the profile.

The gate applies to the fresh composition and the result of a checked
upgrade. The signed profile records its library directories and version
checking requirement. Later catalogs must retain those checks for that
profile. Run
[`tests/elf-profile.sh`](../../tests/elf-profile.sh) to exercise the publication
gate with a compiler-generated executable and shared library and GNU
`objcopy` with `--strip-section-headers` support.

List companion publishers inside a profile when dependencies are supplied by
another repository.

```kdl
profile "cps-runtime" architecture="x86_64" {
    package "system/cps"
    package "system/canopa-trust"
    package "system/git-build"
    publisher "canopa-runtime-seed" catalog="<signed seed catalog SHA-256>" {
        repository "../runtime-seed"
        trust-key "../runtime-seed-public.der"
    }
}
```

Paths resolve relative to the directory containing `publish-policy.kdl`. The
signer checks each pinned catalog against its configured trust key. It uses the
release publisher first, then companions in declaration order. During an
upgrade check, installed packages keep their original publishers. Set
`upgrade-from` on a companion to pin a different baseline catalog for that
publisher when the baseline lacks a signed record for the profile. If the
baseline has that record, any companion `upgrade-from` must match its signed
pin. A baseline without a profile record or a companion override uses the
candidate's `catalog` pin. The primary
catalog signs the checked architecture, facets, variants, allowed facet
overrides, and ordered companion catalog digests. For each companion it also records the signed catalog sequence and
the fingerprint of the provisioned trust anchor. Images verify both against
the pinned companion catalog and their configured key.

When advancing a profile's companion pin, keep the previous catalog object in
the publishing machine's companion repository or cache. CPS checks that the
new companion catalog advances the sequence and preserves known package
identities, withdrawals, attestations, and key handoffs. Primary mirror sync
rejects successors that drop recorded companion lineage, replace its trust
anchor, or reuse a sequence with another digest. A signing-key rotation through
an authenticated handoff still uses the original anchor. Saved plans continue
to use their original catalog set.

Add a `build-trust` block inside a companion publisher to require version 1
build provenance for the companion packages in the fresh composition and in
the result of a checked upgrade. This includes packages the update solver
retains from the baseline. Builder keys must be separate from the primary
and companion publishers' current and retired signing keys.

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
against that signed policy and any local policy for the companion, even when
the repositories are offline. Later catalogs can rotate the signed policy and
must retain a builder policy for the guarded companion in that profile.

## Phased profile updates

Set `rollout-percentage` on a profile in the publisher's release policy.
The value is signed with the checked composition.

```kdl
profile "desktop" architecture="x86_64" rollout-percentage=10 {
    package "desktop/base@2.0.0-0"
}
```

An image outside the rollout defers the entire composition-changing upgrade,
including the packages from pinned companion catalogs. A percentage of
`0` pauses these upgrades and `100` makes the profile available to every
cohort. Omitting the property also makes the profile available to everyone.
New images can install a profile at any percentage. Mirror refreshes can
continue while the package upgrade is deferred.

CPS assigns a cohort from 0 to 99 using the image's original identity and the
target profile's roots, manifest digests, and companion catalog pins.
Boot environments inherit their source image's cohort for the same target.
Images cloned from the same provisioned image share a cohort. Independently
created images get separate identities by default.

To widen the rollout, change the percentage and publish with
`refresh-profiles=#true` using the current catalog as the release base.
Keeping the same composition, roots, and companion pins keeps the same
cohorts. If an earlier composition is still deployed, the publish policy's
`upgrade-from` can also check the transition from that catalog.

A deferred `plan update` reports the reason without saving a plan. Operators
can override the gate explicitly.

```sh
sudo cps upgrade --force-phased-update
cps plan --root image --output upgrade.json update --force-phased-update
```

The override is recorded in the saved plan and transaction history. The
normal signature, dependency, and image checks still apply.

Attach an advisory to the profile that contains the fix. The publisher
checks that the package is in the selected composition and that its
selected version falls outside the affected range.

```kdl
advisory "CPS-2026-01" package="library/runtime" affected="<2.0.0-0" summary="Runtime security fix" action="Restart dependent applications"
```

`check-updates` reports matching advisories for installed packages,
including when the target profile is deferred. The signed advisory reports
the fixed version and any operator action. Later releases carry the advisory
forward while the profile still contains the affected package from the
same publisher. Publication rejects a new profile version that matches the
affected range. Publishers can revise a notice by declaring it again for
the same ID and package.
