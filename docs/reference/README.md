# CLI reference

[Documentation](../README.md) · [Quickstart](../quickstart.md) ·
[Concepts](../concepts.md)

Use `cps --help` to list commands and `cps COMMAND --help` for arguments
and options. For a nested command, use the full path, for example
`cps repo sync --help` or `cps repo trust rotate --help`. Options such as
`plan --root` and `plan --output` belong to the parent command and appear
before its subcommand. Follow the synopsis for the command you are using.

## Man pages

Read the checked-in section 1 pages from the checkout.

```sh
man -l docs/reference/man/cps.1
man -l docs/reference/man/cps-install.1
man -l docs/reference/man/cps-repo.1
```

`cps(1)` lists every command. Each top-level command has its own page. A
command with subcommands documents all of them, at every depth, under
**Subcommand reference** on its page. For example, search for
`cps repo trust rotate` in `cps-repo(1)`.

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
| Recover a live change | [repair](man/cps-repair.1), [revert](man/cps-revert.1), [status](man/cps-status.1), [resume](man/cps-resume.1) | [Recover an interrupted change](../client/system.md#recover-an-interrupted-change) |
| Find packages and inspect their dependencies | [list](man/cps-list.1), [search](man/cps-search.1), [info](man/cps-info.1), [policy](man/cps-policy.1), [owner](man/cps-owner.1), [contents](man/cps-contents.1), [deps](man/cps-deps.1), [rdeps](man/cps-rdeps.1), [why](man/cps-why.1) | [Package queries](../client/queries.md) |
| Inspect package preferences and configuration proposals | [facet](man/cps-facet.1), [variant](man/cps-variant.1), [mediator](man/cps-mediator.1), [freeze](man/cps-freeze.1), [config](man/cps-config.1) | [Image configuration](../client/configuration.md) |
| Initialize, compare, or replicate images | [image-create](man/cps-image-create.1), [image-diff](man/cps-image-diff.1), [image-lock](man/cps-image-lock.1) | [Profiles and image locks](../client/profiles.md) |
| Apply, verify, or abort an image change | [plan](man/cps-plan.1), [fetch](man/cps-fetch.1), [validate](man/cps-validate.1), [apply](man/cps-apply.1), [transaction](man/cps-transaction.1), [verify](man/cps-verify.1) | [Inactive images](../client/images.md) · [Abort a transaction](../client/images.md#abort-a-transaction) |
| Configure trust | [publisher](man/cps-publisher.1) | [Trusted publishers](../client/publishers.md) |
| Prepare and build packages | [manifest](man/cps-manifest.1), [source](man/cps-source.1), [build](man/cps-build.1), [develop](man/cps-develop.1) | [Package maintainers](../maintainer/README.md) |
| Order recipe builds, plan rebuilds, and repin toolchains | [build-order](man/cps-build-order.1), [rebuild-plan](man/cps-rebuild-plan.1), [recipe](man/cps-recipe.1) | [Plan builds across recipes](../maintainer/orchestration.md) |
| Publish packages and manage the publisher trust root | [publish](man/cps-publish.1), [publish-bundle](man/cps-publish-bundle.1), [repo](man/cps-repo.1) (`repo trust`, `repo publish-release`, `repo resign`, `repo withdraw`) | [Publish and retire packages](../publisher/publishing.md) |
| Export, sign, sync, or refresh a mirror | [repo](man/cps-repo.1) (`repo export`, `repo sign-export`, `repo refresh-metadata`, `repo sync`) | [Serve a mirror](../publisher/mirrors.md) · [Mirror clients](../client/mirrors.md) |
| Stage, activate, prune, or roll back a root | [boot-env](man/cps-boot-env.1) | [Boot setup guides](../setups/README.md) |

## Configuration and data formats

KDL documents declare `schema-version 1` and a document `kind`.

| File | Where to start |
| --- | --- |
| KDL `package-manifest` | [Actions, dependency declarations, and versions](../maintainer/manifests.md) |
| Signed publisher catalog root and shards in JSON, version 1 | [Signed catalog format](../publisher/publishing.md#signed-catalog-format) |
| Publisher trust root in JSON, version 1 | [Publisher trust root](../publisher/publishing.md#publisher-trust-root) |
| KDL `build-recipe`, `toolchain-config`, and `build-worker` | [Pinned toolchains, sources, and limits](../maintainer/builds.md) |
| KDL `git-package` and `git-toolchain` | [Package definitions and default toolchains for Git installs](../maintainer/git.md) |
| KDL `image-config` | [Publisher blocks](../client/publishers.md), [mirror definitions](../client/mirrors.md#synchronizing-a-signed-profile), and [Btrfs system settings](../setups/btrfs.md#configure-live-system-commands) |
| KDL `build-trust` | [Verifying builds on images and workers](../publisher/rebuilds.md#verifying-builds-on-images-and-workers) |
| KDL `publish-policy` and `publish-release` | [Publishing policy](../publisher/publishing.md#publishing-policy) and [coordinated releases](../publisher/publishing.md#publish-a-coordinated-release) |
| KDL `signer` | [External signer protocol](../publisher/mirrors.md#external-signer-protocol) |
| Image state in SQLite, version 1 | [Verify an image and locate its state](../client/images.md#verify-an-image-and-locate-its-state) |
| Saved plan in JSON, version 1 | [Plan, fetch, validate, and apply](../client/images.md#plan-fetch-validate-and-apply) |
| Transaction history in JSON | [Inspect transaction history](../client/images.md#inspect-transaction-history) |
| Build provenance predicate, version 1 | [Building and publishing with signed evidence](../maintainer/builds.md#building-and-publishing-with-signed-evidence) |
| Payload delta, version 1 | [Sharded targets and delta payloads](../publisher/mirrors.md#sharded-targets-and-delta-payloads) |
| Image lock in JSON, version 1 | [Reproduce a package composition](../client/profiles.md#provisioning-the-same-composition-on-another-machine) |
| KDL `boot-export-receipt` | [Export and deploy ext4 roots](../setups/ext4.md) |
| CPS boot entry metadata, version 1 | [Inspect and roll back boot environments](../setups/boot-operations.md) |
| Publisher bootstrap descriptor in JSON, version 1 | [Signed mirror release notes](../publisher/mirrors.md#publishing-and-verifying-signed-mirror-release-notes) |

## Regenerate the man pages

[`generate-man.py`](generate-man.py) builds every page from the help of a
built CPS binary. It runs `cps --help` and `cps COMMAND --help` for each
command and subcommand with `LC_ALL=C` and `NO_COLOR=1`, and turns the
description, usage line, arguments, commands, and options into roff. The
first sentence of a command's description becomes its page summary.
Command names, syntax, and option descriptions come from the clap
definitions and doc comments in `src/main.rs`, so edit help there, never
the generated pages. The `cps(1)` description, files, and examples are
fixed text in the script.

Build CPS and regenerate from the repository root using Python 3.

```sh
cargo build --locked --bin cps
python3 docs/reference/generate-man.py --cps target/debug/cps
python3 docs/reference/generate-man.py --cps target/debug/cps --check
```

Regeneration writes `cps.1` and one `cps-COMMAND.1` per top-level command
to `docs/reference/man/`, or to `--output DIR`, and removes pages for
commands that the CLI does not have. With `--check`, the script writes nothing
and exits with status 1, naming the pages to regenerate, if a page is
missing, stale, or differs from the CLI. CI runs the check after building
CPS. Commit the regenerated pages with the help change.
