# CLI reference

[Documentation](../README.md) · [Quickstart](../quickstart.md) ·
[Concepts](../concepts.md)

Use `cps --help` to list commands and `cps COMMAND --help` for arguments
and options. For a nested command, use the full path, for example
`cps repo sync --help`. Options such as `plan --root` and `plan --output`
belong to the parent command and appear before its subcommand. Follow
the synopsis for the command you are using.

## Man pages

Read the checked-in section 1 pages from the checkout.

```sh
man -l docs/reference/man/cps.1
man -l docs/reference/man/cps-install.1
man -l docs/reference/man/cps-repo.1
```

Each top-level command has its own page. For nested commands, open the
parent page and look under **Subcommand reference**. For example, search
for `cps repo sync` in `cps-repo(1)`.

These commands install the pages under your user account.

```sh
man_dir="${XDG_DATA_HOME:-$HOME/.local/share}/man"
install -d "$man_dir/man1"
install -m 0644 docs/reference/man/*.1 "$man_dir/man1/"
man -M "$man_dir" cps
```

Omit `-M` if the directory is already on your system's man search path.
For a shared installation, put the pages in `man1/` under your system's
local manual directory.

## Commands by task

The links open the man page sources. Use the `man` commands above for
formatted output.

| Task | Commands | Guide |
| --- | --- | --- |
| Install and upgrade a running system | [install](man/cps-install.1), [upgrade](man/cps-upgrade.1), [check-updates](man/cps-check-updates.1), [remove](man/cps-remove.1), [autoremove](man/cps-autoremove.1) | [Running system](../client/system.md) |
| Recover a live change | [repair](man/cps-repair.1), [revert](man/cps-revert.1), [status](man/cps-status.1), [resume](man/cps-resume.1) | [Preview and resume changes](../client/system.md#preview-and-resume-changes) |
| Find packages and inspect their dependencies | [list](man/cps-list.1), [search](man/cps-search.1), [info](man/cps-info.1), [policy](man/cps-policy.1), [owner](man/cps-owner.1), [contents](man/cps-contents.1), [deps](man/cps-deps.1), [rdeps](man/cps-rdeps.1), [why](man/cps-why.1) | [Package queries](../client/queries.md) |
| Inspect package preferences and configuration proposals | [facet](man/cps-facet.1), [mediator](man/cps-mediator.1), [freeze](man/cps-freeze.1), [config](man/cps-config.1) | [Image configuration](../client/configuration.md) |
| Initialize, compare, or replicate images | [image-create](man/cps-image-create.1), [image-diff](man/cps-image-diff.1), [image-lock](man/cps-image-lock.1) | [Profiles and image locks](../client/profiles.md) |
| Apply and verify an image change | [plan](man/cps-plan.1), [fetch](man/cps-fetch.1), [validate](man/cps-validate.1), [apply](man/cps-apply.1), [transaction](man/cps-transaction.1), [verify](man/cps-verify.1) | [Inactive images](../client/images.md) |
| Configure trust | [publisher](man/cps-publisher.1) | [Trusted publishers](../client/publishers.md) |
| Prepare and build packages | [manifest](man/cps-manifest.1), [source](man/cps-source.1), [build](man/cps-build.1), [develop](man/cps-develop.1) | [Package maintainers](../maintainer/README.md) |
| Publish, export, or sync a catalog | [publish](man/cps-publish.1), [publish-bundle](man/cps-publish-bundle.1), [repo](man/cps-repo.1) | [Publishers](../publisher/README.md) · [Mirror clients](../client/mirrors.md) |
| Stage, activate, or roll back a root | [boot-env](man/cps-boot-env.1) | [Boot setup guides](../setups/README.md) |

## Configuration and data formats

KDL documents declare `schema-version 1` and a document `kind`.

| File | Where to start |
| --- | --- |
| KDL `package-manifest` | [Actions, dependency declarations, and versions](../maintainer/manifests.md) |
| Signed publisher catalog in JSON, version 1 | [Signed catalog format](../publisher/publishing.md#signed-catalog-format) |
| KDL `build-recipe`, `toolchain-config`, and `build-worker` | [Pinned toolchains, sources, and limits](../maintainer/builds.md) |
| KDL `git-package` and `git-toolchain` | [Package definitions and default toolchains for Git installs](../maintainer/git.md) |
| KDL `image-config` | [Publisher blocks](../client/publishers.md), [mirror definitions](../client/mirrors.md#synchronizing-a-signed-profile), and [Btrfs system settings](../setups/btrfs.md#configure-live-system-commands) |
| KDL `build-trust` | [Verifying build evidence on an image](../publisher/rebuilds.md) |
| KDL `publish-policy` and `publish-release` | [Policy and coordinated publication](../publisher/publishing.md) |
| Image state in SQLite, version 1 | [Verify an image and locate its state](../client/images.md#verify-an-image-and-locate-its-state) |
| Saved plan in JSON, version 1 | [Plan, fetch, validate, and apply](../client/images.md#plan-fetch-validate-and-apply) |
| Transaction history in JSON | [Resume a transaction and inspect its history](../client/images.md#resume-a-transaction-and-inspect-its-history) |
| Build provenance predicate, version 1 | [Building and publishing with signed evidence](../maintainer/builds.md#building-and-publishing-with-signed-evidence) |
| Payload delta, version 1 | [Sharded targets and delta payloads](../publisher/mirrors.md#sharded-targets-and-delta-payloads) |
| Image lock in JSON, version 1 | [Reproduce a package composition](../client/profiles.md#provisioning-the-same-composition-on-another-machine) |
| KDL `boot-export-receipt` | [Export and deploy ext4 roots](../setups/ext4.md) |
| CPS boot entry metadata, version 1 | [Inspect and roll back boot environments](../setups/boot-operations.md) |
| Publisher bootstrap descriptor in JSON, version 1 | [Signed mirror release notes](../publisher/mirrors.md#publishing-and-verifying-signed-mirror-release-notes) |

## Regenerate the man pages

The pages use the built CLI's help for command names, syntax, and option
descriptions. Update help in `src/main.rs`, build CPS, and regenerate from
the repository root using Python 3.

```sh
cargo build --locked --bin cps
python3 docs/reference/generate-man.py --cps target/debug/cps
python3 docs/reference/generate-man.py --cps target/debug/cps --check
```

The check fails if a generated page is missing, stale, or differs from
the CLI. CI runs the same check after building CPS.
