# Declare systemd and kernel actions

[Documentation](../README.md) · [Package maintainers](README.md)

CPS runs no package scripts. A manifest declares what systemd should do
with the files it ships, and CPS applies those declarations with systemd's
own tools. Derived files that many packages contribute to are rebuilt by
[triggers](triggers.md).

## Offline systemd actions

A manifest can request an operation for a vendor file it delivers.

```kdl
schema-version 1 kind="package-manifest"
package "service/example" publisher="canopa" version="1.0.0-0" {
    file "usr/lib/sysusers.d/example.conf" mode="0644" uid=0 gid=0
    sysusers "example.conf"
    file "usr/lib/tmpfiles.d/example.conf" mode="0644" uid=0 gid=0
    tmpfiles "example.conf"
    file "usr/lib/systemd/system/example.service" mode="0644" uid=0 gid=0
    preset "example.service"
}
```

| Node | Names a file in | File name |
| --- | --- | --- |
| `sysusers NAME` | `usr/lib/sysusers.d` | ends in `.conf` |
| `tmpfiles NAME` | `usr/lib/tmpfiles.d` | ends in `.conf` |
| `preset UNIT` | `usr/lib/systemd/system` | a `.service`, `.socket`, `.timer`, `.path`, `.target`, `.mount`, `.automount`, `.swap`, or `.slice` unit |

The package must ship the named file as an immutable `file` action, and the
operation follows that file's architecture, facet, and variant selection.

CPS shows the actions in the plan and invokes the host's systemd tools with
the inactive image as their alternate root. It installs the package files
first, then provisions accounts, creates state, and applies presets. New or
changed sysusers and tmpfiles declarations are processed by name, with
target-local replacements and masks taking precedence. For example, a
tmpfiles rule of `d /srv/example 0750 example example -` can use the
account created earlier in the same transaction. A declared tmpfiles file
may contain only the `d`, `f`, `L`, and `C` rules, which create missing
paths, and publication rejects other rule types. Boot-only rules, such as
`d!`, and paths under `/dev`, `/proc`, `/run`, and `/sys` are deferred to
boot.
[systemd-tmpfiles](https://www.freedesktop.org/software/systemd/man/latest/systemd-tmpfiles.html)

Packages never ship `/var`. They declare what they need there with these
rules, as described in
[Packages never ship /var](manifests.md#packages-never-ship-var-declare-it-with-tmpfilesd-and-sysusersd).
On the running system, CPS runs the tmpfiles rules against the mounted
`/var`, so a newly installed service finds its directories without a
reboot. In an inactive image, such as a staged boot environment or a root
being assembled, `var` is the empty mount point of a `/var` that the image
does not contain, so CPS defers rules for `/var` to boot, where
`systemd-tmpfiles-setup.service` applies them to whichever `/var` the system
mounts. A rule such as `d /var/lib/example 0750 example example -` then
resolves `example` from the accounts that sysusers already wrote into the
image's `/etc/passwd`.

Presets apply the image's enablement policy to newly installed units. CPS
preserves enablement during upgrades and honors local unit overrides and
masks. When a vendor unit is removed, CPS cleans persistent enablement links
that point directly to it. It also clears stale links to newly installed
vendor units before applying their presets.
[systemd presets](https://www.freedesktop.org/software/systemd/man/latest/systemd.preset.html)

These actions apply to the files they name. To rebuild a file derived from
what many packages install, such as the hardware database
`etc/udev/hwdb.bin`, the package that ships the tool declares a trigger.
The systemd package declares the
[`hwdb` trigger](triggers.md#regenerate-the-hardware-database), which runs
`systemd-hwdb --strict update` once per transaction when
`usr/lib/udev/hwdb.d` changes.

Run `cargo build` followed by
[`tests/systemd-actions.sh`](../../tests/systemd-actions.sh) to exercise the
real helpers against a staged root. The test also checks recovery from a
failed tmpfiles step after sysusers has run.

## Kernel packages

An image can select one kernel from its package manifests. The
`kernel-install` node names the kernel release, which identifies the
`usr/lib/modules/RELEASE` directory. The package ships the kernel image as
`usr/lib/modules/RELEASE/vmlinuz`, and with `initrd=#true` a prebuilt
initramfs as `usr/lib/modules/RELEASE/initrd`. Without a prebuilt one, an
installed [initramfs trigger](triggers.md#generate-an-initramfs) generates
`usr/lib/modules/RELEASE/initrd` instead.

```kdl
schema-version 1 kind="package-manifest"
package "system/kernel" publisher="canopa" version="7.2.11-1" {
    file "usr/lib/modules/7.2.11-arch1-1/vmlinuz" mode="0644" uid=0 gid=0
    file "usr/lib/modules/7.2.11-arch1-1/initrd" mode="0644" uid=0 gid=0
    kernel-install "7.2.11-arch1-1" initrd=#true
}
```

The release starts with a letter or digit and contains letters, digits,
`.`, `_`, `+`, and `-`, up to 128 bytes. A manifest has at most one
`kernel-install` node.

Installing or changing the selected kernel runs the host's
`kernel-install add` with systemd's Type #1 BLS plugin and a private boot
tree inside the image. CPS verifies the resulting kernel and initrd against
the signed manifest, or against the digest it recorded for a generated
initramfs. A staged upgrade prepares its files before publishing the new
root. Later, `boot-env activate-btrfs` takes the prepared files from that root and
publishes them alongside its boot entry. Omit `--kernel` and `--initrd` for
manifest-managed kernels. Images with manually supplied boot files can
supply explicit boot paths during [activation](../setups/README.md).
[systemd kernel-install](https://www.freedesktop.org/software/systemd/man/260/kernel-install.html)
