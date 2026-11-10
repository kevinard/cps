# Regenerate shared caches with triggers

[Documentation](../README.md) · [Package maintainers](README.md) ·
[systemd and kernel actions](systemd.md)

Some files are built from what many packages install: GSettings schemas,
icon and MIME caches, the hardware database, module dependency lists, or an
initramfs. The package that ships the tool that builds such a file declares a
**trigger**. CPS has no package scripts. A trigger is the one way to rebuild
derived files.

## Declare a trigger

```kdl
schema-version 1 kind="package-manifest"
package "desktop/glib" publisher="canopa" version="2.86.0-0" {
    dir "usr/share/glib-2.0/schemas" mode="0755" uid=0 gid=0
    file "usr/bin/glib-compile-schemas" mode="0755" uid=0 gid=0
    trigger "schemas" {
        run "usr/bin/glib-compile-schemas" "usr/share/glib-2.0/schemas"
        watch "usr/share/glib-2.0/schemas/*.xml"
        watch "usr/share/glib-2.0/schemas/*.override"
        output "usr/share/glib-2.0/schemas/gschemas.compiled"
    }
}
```

| Node | Meaning |
| --- | --- |
| `trigger NAME` | A name unique within the package. `timeout=SECONDS` replaces the 300-second limit, up to 3600. |
| `run HANDLER ARG...` | An executable file action of the same package, and fixed arguments. Paths are image-relative, and the handler starts in `/`. |
| `watch PATTERN` | Paths whose changes run the trigger. Use one or more. |
| `output PATTERN` | Paths the handler generates. A matched directory includes its contents. |
| `reload UNIT`, `restart UNIT` | Services to reload or restart after a live run, if they are active. |

Patterns use the [collected file list](builds.md) syntax: `*` and `?` match
within one component, and `**` matches whole components. A pattern also
covers everything below the path it matches.

A trigger follows the facet and variant selection of its handler file.
Triggers cannot carry selectors, and they never add dependencies. A trigger
without `run` only reloads or restarts services, and it declares no outputs.

## When triggers run

After a transaction writes every file, CPS runs each trigger once if:

- a package added, changed, or removed a watched path, including through a
  facet or variant change,
- the trigger's package was installed or upgraded, or
- a trigger that runs generates a path this trigger watches.

The last rule also orders the runs. A trigger runs after every trigger whose
outputs it watches, and the package and trigger names break ties. A plan
fails if triggers watch each other's outputs in a cycle. The plan lists each
run as `Run trigger PACKAGE:NAME`.

Triggers run after sysusers, tmpfiles, and presets, so a handler can use the
accounts that the transaction creates. They run before kernel-install, so a
generated initramfs reaches the boot artifacts.

Triggers react only to paths that packages manage. A local administrator
file under `/etc` does not run one.

## Generated paths

The declaring package owns every path its outputs cover:

- Publication fails if an output covers a path that the package itself ships.
  A plan fails if another package ships a path inside an output, or if two
  triggers generate paths that can overlap. Ship such a file from the trigger
  package or change the pattern.
- `cps verify` does not check generated contents, and `--quick` records no
  fingerprints for them. `--pristine` accepts them as owned paths.
- CPS removes the generated paths when the trigger goes away, because its
  package was removed, its handler was deselected, or an upgrade changed its
  outputs.

An output must be inside a directory below the image root, outside `var` and
`var/lib`. Ship the nearest parent directory in the trigger's package, as
`dir "etc/udev"` does for the hardware database. A handler cannot create
directories outside its outputs.

## Write a handler

A handler runs under [Bubblewrap](https://github.com/containers/bubblewrap)
with the image as its root filesystem, so it runs the image's own programs.
CPS requires `/usr/bin/bwrap`, and the CPS package should depend on it.

- The image is read-only. `/proc`, `/dev`, and an empty `/tmp` and `/run`
  are private to the handler. `/home`, `/root`, `/boot`, `/efi`, `/mnt`,
  `/media`, and the CPS state under `/var/lib/cps` are hidden.
- Each directory that holds an output is an overlay. After the handler
  exits, CPS publishes only the changes to declared outputs. If the handler
  wrote, removed, or changed anything else, the trigger fails and none of its
  changes reach the image.
- The handler has no network, a private PID namespace, and a seccomp
  filter. Its environment has only `PATH`, `LC_ALL=C`, `TZ=UTC`,
  `HOME=/tmp`, `TMPDIR=/tmp`, and `CPS_TRIGGER`. Its output goes to the
  standard error of CPS.
- Offline and in staged boot environments, an unprivileged CPS runs the
  handler as root in a user namespace, so generated files belong to the
  user who runs CPS. On a live system or as root, the handler runs as root
  with only the file ownership capabilities.

Handlers must be idempotent. A trigger that fails leaves the transaction
pending. `transaction resume`, or `resume` on a live system, runs the
remaining triggers, and a trigger interrupted before CPS journaled its
success runs again. Rebuild every output from the image's current files, as
cache tools do, and remove outputs that no longer have inputs.

Aborting a transaction regenerates outputs too. After `transaction abort`
restores the files, CPS plans the triggers of the change from the pending
packages back to the committed ones and runs them with the same rules:

- a trigger whose watched paths the abort restores runs again over the
  restored files,
- the outputs of a trigger that the transaction added are removed, and
- a trigger that the transaction removed runs again for its restored
  package.

A handler can therefore run on files that it already processed, or after
its own outputs were removed. The abort journals each finished trigger, so
an abort that a failing or interrupted handler stops runs only the
remaining triggers when `transaction abort` or `transaction resume`
finishes it. If one of these triggers regenerates the initramfs of the
committed kernel, the abort also prepares that kernel again.

## Common triggers

| Cache | Handler and arguments | Watch | Output |
| --- | --- | --- | --- |
| GSettings schemas | `usr/bin/glib-compile-schemas` `usr/share/glib-2.0/schemas` | `usr/share/glib-2.0/schemas/*.xml` | `usr/share/glib-2.0/schemas/gschemas.compiled` |
| Icon themes | a script that runs `gtk-update-icon-cache` for each theme | `usr/share/icons/**` | `usr/share/icons/*/icon-theme.cache` |
| MIME database | `usr/bin/update-mime-database` `usr/share/mime` | `usr/share/mime/packages/*.xml` | one `output` for each generated name, such as `usr/share/mime/mime.cache` and `usr/share/mime/application` |
| Desktop entries | `usr/bin/update-desktop-database` `usr/share/applications` | `usr/share/applications/*.desktop` | `usr/share/applications/mimeinfo.cache` |
| Fonts | `usr/bin/fc-cache` `--system-only` | `usr/share/fonts/**` | `var/cache/fontconfig` |
| GIO modules | `usr/bin/gio-querymodules` `usr/lib/gio/modules` | `usr/lib/gio/modules/*.so` | `usr/lib/gio/modules/giomodule.cache` |
| Pixbuf loaders | `usr/bin/gdk-pixbuf-query-loaders` `--update-cache` | `usr/lib/gdk-pixbuf-2.0/2.10.0/loaders/*.so` | `usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache` |
| Certificates | `usr/bin/update-ca-trust` | `usr/share/pki/ca-trust-source/**` | `etc/pki/ca-trust/extracted` |
| Manual pages | `usr/bin/mandb` `--quiet` | `usr/share/man/**` | `var/cache/man` |
| Module dependencies | a script that runs `depmod` for each `usr/lib/modules/*` | `usr/lib/modules/*/kernel/**`, `usr/lib/modules/*/updates/**`, `usr/lib/modules/*/extra/**` | `usr/lib/modules/*/modules.*` |
| Library cache | `usr/bin/ldconfig` | `usr/lib/*.so.*` | `etc/ld.so.cache` |
| Hardware database | `usr/bin/systemd-hwdb` `--strict` `update` | `usr/lib/udev/hwdb.d/*.hwdb` | `etc/udev/hwdb.bin` |

Images that rely on the runtime linker's default directories need no
library cache. Images that use `ld.so.conf` directories can declare the
`ldconfig` trigger in the glibc package.

Reload running services without a handler:

```kdl
trigger "rules" {
    watch "usr/lib/udev/rules.d/*.rules"
    reload "systemd-udevd.service"
}
```

After a live transaction, CPS runs `systemctl try-reload-or-restart` for
`reload` units and `systemctl try-restart` for `restart` units, after the
systemd manager reload and service restarts that the file changes require.
These jobs are part of the journaled live jobs, so `resume` repeats them.
Offline and staged images have no running services, and the next boot uses
the new files.

## Generate an initramfs

A package that ships an initramfs generator, such as dracut, mkinitcpio, or a
distribution's own tool, declares a trigger whose output is the per-kernel
initramfs that [kernel-install](systemd.md#kernel-packages) consumes:

```kdl
trigger "initramfs" timeout=900 {
    run "usr/libexec/canopa/mkinitrd"
    watch "usr/lib/modules/**"
    watch "usr/lib/firmware/**"
    watch "etc/crypttab"
    watch "usr/share/kbd/keymaps/**"
    output "usr/lib/modules/*/initrd"
}
```

The handler writes `usr/lib/modules/RELEASE/initrd` for each installed
kernel. Kernel packages then omit `initrd=#true` and do not ship the file.
A kernel package that ships its own initrd conflicts with the generator, so
an image uses one or the other. When the trigger runs, CPS prepares the
selected kernel again with the new initramfs and records its digest with the
prepared artifacts. `boot-env activate` checks it like a packaged one.

A trigger whose outputs can cover a boot-sensitive path, such as
`usr/lib/modules`, requires a staged boot environment, as does removing it.
Live changes report that staging is required.

Run [`tests/triggers.sh`](../../tests/triggers.sh) after `cargo build` to
exercise ordering, confinement, recovery, aborts, removal, and a generated
initramfs. [`tests/hwdb-trigger.sh`](../../tests/hwdb-trigger.sh) runs the
hardware database trigger with the host's `systemd-hwdb`.
