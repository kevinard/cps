# Package a project for direct Git installation

[Documentation](../README.md) · [Package maintainers](README.md)

Put one `cps.kdl` at the Git repository root. It combines the usual package
actions and build recipe. CPS assigns the local publisher and supplies the
manifest and `git.tar` pins when it prepares a build.

```kdl
schema-version 1 kind="git-package"
package "demo/hello" version="1.0.0-0" {
    file "usr/bin/hello" mode="0755" uid=0 gid=0
}
build architecture="x86_64" epoch=1700000000 {
    toolchain "default"
    script """
    tar -xf "$CPS_SOURCES/git.tar"
    install -Dm755 hello "$DESTDIR/usr/bin/hello"
    """
}
```

The signed base profile ships `system/git-build`. CPS reads its
`/usr/share/cps/git-build/toolchain.kdl` automatically. The default supplies a
shell, tar, install, and other basic utilities from a local signed catalog.
Use an explicit toolchain pin and a matching
[toolchain configuration](builds.md#building-with-several-publishers) for builds that
need a compiler or other tools. Administrators can still set
`git-toolchain-config "toolchains.kdl"` in `/etc/cps/image.kdl` to use a
different config. To support `toolchain "default"`, place a pinned toolchain
block in `toolchains.default.kdl` with the header
`schema-version 1 kind="git-toolchain"`.

The running Btrfs system needs Git and Bubblewrap. Invoke CPS through `sudo`
from an unprivileged account with read access to the toolchain configuration,
its keys, and the pinned toolchain payloads. Other recipe sources can use their
normal pinned HTTPS downloads. Files committed in Git are available inside
`git.tar`.

## Install and track a branch

Review the `cps.kdl` and Git commit you intend to build. A tracked branch
opts the package into future upgrades from that repository.

A Git install grants root-level trust to whoever can push to that branch.
CPS builds the commit's `cps.kdl` as a package, and packages install files
and run services as root. A tracked branch extends that trust to every
future push. HTTPS authenticates the Git server, not the commit author.
Require signatures when the server or its accounts are outside your control.

```sh
sudo cps install --git https://git.example.org/alice/hello.git --track-branch main
sudo cps upgrade
```

Use `--commit FULL_COMMIT_ID` in place of `--track-branch` to pin a package.
Use an HTTPS URL or a local Git repository path. CPS shows the exact commit
before building, then offers the normal package transaction for approval.
The Git package can depend on packages from configured publishers. Its signed
local catalog is shared across boot environments for offline repair.

An ordinary `cps upgrade` checks opted-in branches for descendants of the
installed commit. It reports a branch rewrite and leaves that package at its
installed commit. `--dry-run` shows available Git commits without building, and
`--no-refresh` uses the installed Git selections. CPS stages tracked updates
in a new boot environment for rollback. When a tracked commit's version
would not advance the package, CPS allocates a higher revision. `cps info NAME` shows the
installed commit as `upstream-version`. That manifest is part of the boot
environment, so rollback restores the commit and the installed files together.

## Require signed commits or tags

Pass `--allowed-signers` with an SSH allowed-signers file or an OpenPGP
public keyring to accept only commits signed by those keys.

```sh
sudo cps install --git https://git.example.org/alice/hello.git --track-branch main \
    --allowed-signers ssh:/etc/cps/git/hello.allowed_signers
```

Use `openpgp:PATH` for an exported OpenPGP keyring, such as the output of
`gpg --export --armor KEY_ID`. Entries in an SSH allowed-signers file need
`namespaces="git"` or no namespace restriction, as described in
[`ssh-keygen(1)`](https://man.openbsd.org/ssh-keygen#ALLOWED_SIGNERS). Add
`--require-signed tag` to require an annotated tag that points at the commit
and carries an allowed signature instead. CPS fetches the repository's tags
for this check. Lightweight tags do not count.

CPS checks the policy with `git verify-commit` or `git verify-tag` before
building, on the initial install and on every tracked-branch upgrade. A
missing or untrusted signature stops the build and leaves the installed
commit in place. OpenPGP checks use a private keyring that contains only
the allowed keys. CPS records the policy with the installed source and
reads the file each time, so editing the file rotates or revokes signers
for later upgrades. The file must be owned by the CPS account, normally
root, and not writable by group or others. The signature covers the commit
being installed. CPS still requires each upgrade to descend from the
installed commit, so a signed commit on a rewritten branch is rejected.
