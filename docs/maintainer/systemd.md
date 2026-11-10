# Declare systemd and kernel actions

[Documentation](../README.md) · [Package maintainers](README.md)

## Offline systemd actions

A manifest can request an operation for a vendor file it delivers. The operation is selected with that file's architecture and facet settings.

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

CPS shows the actions in the plan and invokes the host's systemd tools with the inactive image as their alternate root. It installs the package files first, then provisions accounts, creates state, and applies presets. New or changed sysusers and tmpfiles declarations are processed by name, with target-local replacements and masks taking precedence. For example, a tmpfiles rule of `d /var/lib/example 0750 example example -` can use the account created earlier in the same transaction. Offline tmpfiles supports `d`, `f`, and `L` creation rules. Boot-only rules and paths under `/dev`, `/proc`, `/run`, and `/sys` are deferred to boot. [systemd-tmpfiles](https://www.freedesktop.org/software/systemd/man/latest/systemd-tmpfiles.html)

Presets apply the image's enablement policy to newly installed units.
CPS preserves enablement during upgrades and honors local unit overrides
and masks. When a vendor unit is removed, CPS cleans persistent enablement
links that point directly to it. It also clears stale links to newly installed
vendor units before applying their presets. [systemd presets](https://www.freedesktop.org/software/systemd/man/latest/systemd.preset.html)

These actions are declarations that systemd applies to the files they name.
To rebuild a file derived from what many packages install, such as the
hardware database `etc/udev/hwdb.bin`, the package that ships the tool
declares a trigger. The systemd package declares the `hwdb` trigger, which
runs `systemd-hwdb --strict update` once per transaction when
`usr/lib/udev/hwdb.d` changes. [Regenerate shared caches with triggers](triggers.md)

Run `cargo build` followed by [`tests/systemd-actions.sh`](../../tests/systemd-actions.sh) to exercise the real helpers against a staged root. The test also checks recovery from a failed tmpfiles step after sysusers has run.

## Kernel packages

An image can select one kernel from its package manifests. The kernel release identifies the `usr/lib/modules` directory and the packaged kernel image. An optional prebuilt initrd goes in the same directory. Without one, an
installed [initramfs trigger](triggers.md#generate-an-initramfs) can generate
`usr/lib/modules/RELEASE/initrd` instead.

```kdl
schema-version 1 kind="package-manifest"
package "system/kernel" publisher="canopa" version="7.2.11-1" {
    file "usr/lib/modules/7.2.11-arch1-1/vmlinuz" mode="0644" uid=0 gid=0
    file "usr/lib/modules/7.2.11-arch1-1/initrd" mode="0644" uid=0 gid=0
    kernel-install "7.2.11-arch1-1" initrd=#true
}
```

Installing or changing the selected kernel runs the host's `kernel-install add` with systemd's Type #1 BLS plugin and a private boot tree inside the image. CPS verifies the resulting kernel and initrd against the signed manifest. A staged upgrade prepares its files before publishing the new root. Later, `boot-env activate` takes the prepared files from that root and publishes them alongside its boot entry. Omit `--kernel` and `--initrd` for manifest-managed kernels. Images with manually supplied boot files can supply explicit boot paths during [activation](../setups/README.md). [systemd kernel-install](https://www.freedesktop.org/software/systemd/man/260/kernel-install.html)
