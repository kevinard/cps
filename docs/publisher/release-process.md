# Rehearse and promote releases

[Documentation](../README.md) · [Publishers](README.md)

Run the `Release rehearsal` workflow on the protected `main` branch. It
runs the package and boot preflight and builds the rehearsal tool on a
x86-64 Linux runner labeled `cps-rehearsal`. Provision Docker on the
keyless runner. CI builds CPS and runs the tests in [the pinned build
image](../../ci/Dockerfile). The image pins the Arch base by digest, verifies the
dated package indexes before installation, and checks each Rust archive
against the committed SHA-256. CI fetches the locked crates before building
with `--frozen` and runs builds and tests in a container without network access.

The **Rust dependency audit** check scans `Cargo.lock` on pull requests and
daily against the current RustSec advisory database. It blocks builds on
vulnerabilities, unsound crates, and yanked versions. Protect `main` with
required code-owner review and require the audit check in GitHub. Review
changes to dependencies, the CI image and its hashes, and packaging recipes
before updating those pins. The hosted runner supplies the kernel and Docker
engine used to run the pinned image.

Keep the signing host outside GitHub Actions. Install the reviewed CPS
binary and release scripts on that host through a separate administrator
process. Keep the publisher catalog keys, online TUF keys, and release notes key there,
outside the publishing repository and the public mirror. Each key can
instead stay in a hardware token or KMS behind an
[external signer](mirrors.md#release-keys-and-external-signers). Only
releases need the catalog and TUF targets keys. The daily mirror refresh
needs only the timestamp key, and occasionally the snapshot key. The rehearsal
runner needs a read-only copy of release objects and publisher trust roots.
Give it no signing or deployment credentials, access to the signing host,
or write access to its repository, release requests, scripts, or mirror.
Retire the old `cps-release` Actions runner and remove the old signing
secrets from GitHub. Rotate keys that were available to that runner if
untrusted code may have run there.

Set these variables in the signing host's private service or operator
configuration. Use absolute paths for files and directories.

| Variable | Value |
| --- | --- |
| `CPS_BIN` | Installed and reviewed CPS binary |
| `CPS_RELEASE_REPO` | Writable publishing repository |
| `CPS_RELEASE_PUBLISHER` | Publisher name |
| `CPS_RELEASE_SIGNING_KEY` | Catalog keys outside the publishing repository, one per line, enough to meet the trust root's catalog threshold. Each line is a private key path or `command:DESCRIPTOR` for an external signer |
| `CPS_RELEASE_INPUTS` | Trusted parent directory of release requests |
| `CPS_RELEASE_SOURCE_CACHE` | Optional source cache for release build bundles |
| `CPS_RELEASE_ROLLOUT_GATE` | Administrator-installed cohort health adapter |
| `CPS_RELEASE_ROLLOUT_PLAN` | Approved policy for rollout advancement |
| `CPS_RELEASE_DEPLOY` | Administrator-installed deployment adapter, such as `ci/deploy-mirror.sh` |

1. On the signing host, run `bash ci/release.sh prepare REQUEST /path/to/new-snapshot`.
   CPS reads the reviewed `REQUEST/release.kdl` and its bundle references.
   Preparation signs the candidate for rehearsal and exports a private
   snapshot of content-addressed repository objects. The production catalog
   pointer and mirror are unchanged. Record the candidate and base digests.
2. Transfer the snapshot from the signing host to the rehearsal host using
   a one-way copy. On the rehearsal host, make it available read-only under
   the directory configured as `CPS_REHEARSAL_REPO`. Configure
   `CPS_REHEARSAL_CASES` as the parent of the named case directories,
   `CPS_REHEARSAL_VM_GATE` as the absolute path of the boot and service test
   adapter, and `CPS_RELEASE_PUBLISHER` as the publisher name. Put the
   snapshot, cases, adapter, and any referenced companion repositories and
   public keys under the absolute `CPS_REHEARSAL_DATA` directory. The workflow
   mounts that directory read-only at the same path inside the pinned image.
   Dispatch the workflow with the request name and both recorded digests.
   Leave the base empty for a first release.
3. After reviewing the successful preflight and rehearsal for the intended
   request, candidate, and base, authorize promotion on the signing host
   with `bash ci/release.sh promote REQUEST REHEARSED_SHA256`. Use the digest
   recorded on the signing host. Promotion rebuilds the catalog from the
   signing host's reviewed inputs and requires an exact digest match and
   a current base before it advances the production pointer and deploys.
   The rehearsal job's result informs this operator decision.

Each named case contains an `image.kdl` using pinned publisher trust roots and the
intended profile and architecture. List any additional historical catalog
digests in `baselines`, one per line. An optional executable
`before-upgrade` receives the CPS binary, baseline root, baseline digest,
and candidate digest. Use it to install representative third-party software
or prepare service state. The runner rehearses fresh installation and upgrade
from the current signed profile and listed historical baselines. It checks
the applied catalog pin and calls the VM gate with the mode (`fresh` or
`upgrade`), profile, candidate digest, baseline digest, resulting root, and
previous root. The baseline digest and previous root are empty for a fresh install.

Run `bash ci/release.sh widen CURRENT_SHA256 PROFILE PERCENTAGE` on the
signing host to widen rollout after the installed health adapter approves.
Widening changes the signed rollout percentage and catalog sequence while
preserving cohort assignment. To advance automatically, schedule
`bash ci/release.sh advance` on the signing host and configure the protected
policy file. Set `catalog` to the initial promoted digest, `profile` to the
profile name, and `percentages` to approved increasing steps beginning at
the signed initial percentage. Each run can widen by one step. The health
adapter checks that the mirror serves the requested digest and checks the
cohort's health. Exit 75 to wait for further observations.

The deployment adapter receives the repository, publisher, and exact catalog
digest. The bundled adapter exports and verifies that catalog, publishes
`publisher.json` with its TUF root and the publisher trust root that clients pin, and publishes
the new timestamp last. The release script records successful deployment in
`.ci-deployed-catalog`. If deployment fails after local promotion, retry on
the signing host with `bash ci/release.sh deploy-current CURRENT_SHA256`.
`refresh-mirror` calls the adapter with a fourth argument, `refresh`, for
the catalog recorded as deployed. The adapter then re-signs only the mirror
metadata that is about to expire and must not need the targets or publisher
keys. The release script removes `CPS_RELEASE_SIGNING_KEY` from the
adapter's environment in both cases.
Keep the publisher's offline [root keys](publishing.md#publisher-trust-root)
off the signing host. They are needed only to rotate, revoke, or renew the trust
root, which you do with `cps repo trust rotate` or the detached `draft`, `sign`,
and `commit` flow, followed by `cps repo resign` and a deployment. Renew the
trust root well before its `expires` time: clients reject new current catalogs
signed under an expired trust root, whatever their refresh policy.
Run `bash tests/release-ci.sh` to exercise the split using disposable keys
and test adapters.
