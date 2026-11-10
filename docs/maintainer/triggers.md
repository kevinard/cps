# Regenerate shared caches with triggers

[Documentation](../README.md) · [Package maintainers](README.md) ·
[systemd and kernel actions](systemd.md)

Some files are built from what many packages install: GSettings schemas,
icon and MIME caches, the hardware database, module dependency lists, or an
initramfs. The package that ships the tool that builds such a file declares
a **trigger**. CPS has no package scripts, so a trigger is the one way to
rebuild derived files. CPS runs each trigger at most once per transaction,
after the files are written, in a sandbox that can change only the paths the
trigger declares.

## On this page

- [Declare a trigger](#declare-a-trigger)
- [When triggers run](#when-triggers-run)
- [Generated paths](#generated-paths)
- [Write a handler](#write-a-handler)
- [Reruns, resume, and abort](#reruns-resume-and-abort)
- [Common triggers](#common-triggers)
- [Reload services](#reload-services)
- [Regenerate the hardware database](#regenerate-the-hardware-database)
- [Generate an initramfs](#generate-an-initramfs)

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
| `trigger NAME` | A name unique within the package: lowercase ASCII letters, digits, `.`, `_`, and `-`, starting with a letter or digit. `timeout=SECONDS` replaces the 300-second limit, from 1 to 3600. |
| `run HANDLER ARG...` | The handler, an executable `file` action of the same package that is not preserved configuration, and up to 64 fixed string arguments. Paths are image-relative, and the handler starts in `/`. |
| `watch PATTERN` | Paths whose changes run the trigger. One or more. |
| `output PATTERN` | Paths the handler generates. A matched directory includes its contents. |
| `reload UNIT`, `restart UNIT` | Services to reload or restart after a live run, if they are active. |

Patterns use the [build pattern](builds.md#patterns) syntax: `*` and `?`
match within one component, and `**` matches whole components. A pattern
also covers everything below the path it matches.

A trigger follows the facet and variant selection of its handler file:
when the image deselects the handler, the trigger is gone too. Triggers
cannot carry selectors themselves, and they never add dependencies. A
trigger without `run` only [reloads or restarts services](#reload-services)
and declares no outputs.

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

The declaring package owns every path its outputs cover.

- Publication fails if an output covers a path that the package itself
  ships. A plan fails if another package ships a path inside an output, or
  if two triggers generate paths that can overlap. Ship such a file from the
  trigger package or change the pattern.
- An output names paths inside a directory other than the image root,
  `usr`, `usr/lib`, or `usr/lib/sysimage`. It cannot reach the CPS state
  under `usr/lib/sysimage/cps`, the `proc`, `sys`, `dev`, `run`, and `tmp`
  trees that the sandbox replaces, or `var`, which is shared by every boot
  environment and is not part of the image. Neither outputs nor watches can
  name a path in `var`. Ship the nearest parent directory in the
  trigger's package, as
  `dir "etc/udev"` does for the hardware database. A handler cannot create
  directories outside its outputs.
- CPS never follows a symlink to an output. A plan fails if a directory
  named literally in an output, such as `lib` in `lib/foo/cache`, is a
  managed symlink, as after a merged `/usr` turns `lib` into a link to
  `usr/lib`. Declare the output at the link's target instead. A handler
  fails when an unmanaged symlink stands in for the directory that holds
  its output, and removing generated paths skips anything beneath a link.
- `cps verify` does not check generated contents, and `--quick` records no
  fingerprints for them. `--pristine` accepts them as owned paths.
- CPS removes the generated paths when the trigger goes away: its package
  was removed, its handler was deselected, or an upgrade changed its
  outputs.

An output that can cover a boot-sensitive path, such as `usr/lib/modules`,
makes the trigger a boot trigger. A transaction that runs or removes a boot
trigger, including the install that adds it, requires a staged boot
environment, and live changes report that staging is required.

## Write a handler

A handler runs under [Bubblewrap](https://github.com/containers/bubblewrap)
with the image as its root filesystem, so it runs the image's own programs.
CPS runs Bubblewrap from `/usr/bin/bwrap` on the system that applies the
transaction, so a package that ships CPS should depend on Bubblewrap.

- The image is read-only. `/proc`, `/dev`, and an empty `/tmp`, `/run`,
  and `/var` are private to the handler, so a handler sees the same empty
  `/var` on the running system and in an inactive image, and whatever it
  writes there is discarded. `/sys`, `/home`, `/root`, `/boot`, `/efi`,
  `/mnt`, `/media`, `/lost+found`, and the CPS state under
  `/usr/lib/sysimage/cps` are hidden.
- Each directory that holds an output is an overlay. After the handler
  exits, CPS publishes only the changes to declared outputs. If the handler
  wrote, removed, or changed anything else, the trigger fails and none of
  its changes reach the image. A generated file may be at most 4 GiB. Write
  each output in full: an overlay that records only a mode change or a
  rename, as metadata-only copies and redirected directories do, fails the
  trigger, because the contents stay outside the overlay.
- The handler has no network, a private PID namespace, and a seccomp
  filter. It cannot create sockets, so it cannot reach a service through a
  UNIX socket in the image even when it runs as root. `socketpair` works for
  stream and sequenced-packet sockets. Its environment holds only `PATH=/usr/sbin:/usr/bin`, `LC_ALL=C`,
  `LANG=C`, `TZ=UTC`, `HOME=/tmp`, `TMPDIR=/tmp`, and
  `CPS_TRIGGER=PACKAGE:NAME`. Its output goes to the standard error of CPS.
- Offline and in staged boot environments, an unprivileged CPS runs the
  handler as root in a user namespace, so generated files belong to the
  user who runs CPS. On a live system, or when CPS runs as root, the handler
  runs as root with only `CAP_CHOWN`, `CAP_DAC_OVERRIDE`, and `CAP_FOWNER`.

Handlers must be idempotent. Rebuild every output from the image's current
files, as cache tools do, and remove outputs that no longer have inputs. A
handler can run on files that it already processed, or after its own
outputs were removed.

## Reruns, resume, and abort

A trigger that fails leaves the transaction pending. `transaction resume`,
or `resume` on a live system, runs the remaining triggers. CPS journals each
finished trigger, and a trigger interrupted before CPS journaled its success
runs again.

`transaction abort` regenerates outputs too. After it restores the files,
CPS plans the triggers of the change from the pending packages back to the
committed ones and runs them with the same rules:

- a trigger whose watched paths the abort restores runs again over the
  restored files,
- the outputs of a trigger that the transaction added are removed, and
- a trigger that the transaction removed runs again for its restored
  package.

The abort journals each finished trigger as well, so an abort that a
failing or interrupted handler stops runs only the remaining triggers when
`transaction abort` or `transaction resume` finishes it. If one of these
triggers regenerates the initramfs of the committed kernel, the abort also
prepares that kernel again. See
[abort a transaction](../client/images.md#abort-a-transaction).

## Common triggers

| Cache | Handler and arguments | Watch | Output |
| --- | --- | --- | --- |
| GSettings schemas | `usr/bin/glib-compile-schemas` `usr/share/glib-2.0/schemas` | `usr/share/glib-2.0/schemas/*.xml` | `usr/share/glib-2.0/schemas/gschemas.compiled` |
| Icon themes | a script that runs `gtk-update-icon-cache` for each theme | `usr/share/icons/**` | `usr/share/icons/*/icon-theme.cache` |
| MIME database | `usr/bin/update-mime-database` `usr/share/mime` | `usr/share/mime/packages/*.xml` | one `output` for each generated name, such as `usr/share/mime/mime.cache` and `usr/share/mime/application` |
| Desktop entries | `usr/bin/update-desktop-database` `usr/share/applications` | `usr/share/applications/*.desktop` | `usr/share/applications/mimeinfo.cache` |
| Fonts | `usr/bin/fc-cache` `--system-only`, with fontconfig built with `--with-cache-dir=/usr/lib/fontconfig/cache` | `usr/share/fonts/**` | `usr/lib/fontconfig/cache` |
| GIO modules | `usr/bin/gio-querymodules` `usr/lib/gio/modules` | `usr/lib/gio/modules/*.so` | `usr/lib/gio/modules/giomodule.cache` |
| Pixbuf loaders | `usr/bin/gdk-pixbuf-query-loaders` `--update-cache` | `usr/lib/gdk-pixbuf-2.0/2.10.0/loaders/*.so` | `usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache` |
| Certificates | `usr/bin/update-ca-trust` | `usr/share/pki/ca-trust-source/**` | `etc/pki/ca-trust/extracted` |
| Manual pages | `usr/bin/mandb` `--quiet`, with `MANDB_MAP /usr/share/man /usr/lib/man-db/index` in `man_db.conf` | `usr/share/man/**` | `usr/lib/man-db/index` |
| Module dependencies | a script that runs `depmod` for each `usr/lib/modules/*` | `usr/lib/modules/*/kernel/**`, `usr/lib/modules/*/updates/**`, `usr/lib/modules/*/extra/**` | `usr/lib/modules/*/modules.*` |
| Library cache | `usr/bin/ldconfig` | `usr/lib/*.so.*` | `etc/ld.so.cache` |
| Hardware database | `usr/bin/systemd-hwdb` `--strict` `update` | `usr/lib/udev/hwdb.d/*.hwdb` | `etc/udev/hwdb.bin` |

Caches that tools keep in `/var` by default, such as the font and manual
page caches, move below `/usr` as shown, because a trigger cannot write to
the shared `/var`. A cache that stays in `/var` is rebuilt on the running
system by a service or timer instead, as `man-db.timer` does for an index in
`/var/cache/man`.

Images that rely on the runtime linker's default directories need no
library cache. Images that use `ld.so.conf` directories can declare the
`ldconfig` trigger in the glibc package.

## Reload services

A trigger without a handler reloads running services when watched files
change:

```kdl
trigger "rules" {
    watch "usr/lib/udev/rules.d/*.rules"
    reload "systemd-udevd.service"
}
```

After a live transaction, CPS runs `systemctl try-reload-or-restart` for
`reload` units and `systemctl try-restart` for `restart` units, after the
systemd manager reload and service restarts that the file changes require.
Only `.service` units are accepted. These jobs are part of the journaled
live jobs, so `resume` repeats them. Offline and staged images have no
running services, and the next boot uses the new files.

## Regenerate the hardware database

The systemd package ships `systemd-hwdb` and declares the trigger that
compiles every package's hardware database fragments. Packages that ship
files under `usr/lib/udev/hwdb.d` need no declaration of their own.

```kdl
schema-version 1 kind="package-manifest"
package "system/systemd" publisher="canopa" version="262.1-1" {
    dir "etc/udev" mode="0755" uid=0 gid=0
    file "usr/bin/systemd-hwdb" mode="0755" uid=0 gid=0
    trigger "hwdb" {
        run "usr/bin/systemd-hwdb" "--strict" "update"
        watch "usr/lib/udev/hwdb.d/*.hwdb"
        output "etc/udev/hwdb.bin"
    }
}
```

The package ships `etc/udev` so that the handler's overlay has a directory
to write into, and does not ship `etc/udev/hwdb.bin`, which the trigger owns.

## Generate an initramfs

A package that ships an initramfs generator, such as dracut, mkinitcpio, or
a distribution's own tool, declares a trigger whose output is the
per-kernel initramfs that [kernel-install](systemd.md#kernel-packages)
consumes:

```kdl
schema-version 1 kind="package-manifest"
package "system/mkinitrd" publisher="canopa" version="1.0.0-0" {
    file "usr/libexec/canopa/mkinitrd" mode="0755" uid=0 gid=0
    trigger "initramfs" timeout=900 {
        run "usr/libexec/canopa/mkinitrd"
        watch "usr/lib/modules/**"
        watch "usr/lib/firmware/**"
        watch "etc/crypttab"
        watch "usr/share/kbd/keymaps/**"
        output "usr/lib/modules/*/initrd"
    }
}
```

The handler writes `usr/lib/modules/RELEASE/initrd` for each installed
kernel. Kernel packages then omit `initrd=#true` and do not ship the file.
A kernel package that ships its own initrd conflicts with the generator, so
an image uses one or the other. When the trigger runs, CPS prepares the
selected kernel again with the new initramfs and records its digest with the
prepared artifacts, and `boot-env activate-btrfs` checks it like a packaged one.
Because the output is under `usr/lib/modules`, changes that run this trigger
are staged.

Run [`tests/triggers.sh`](../../tests/triggers.sh) after `cargo build` to
exercise ordering, confinement, recovery, aborts, removal, and a generated
initramfs. [`tests/hwdb-trigger.sh`](../../tests/hwdb-trigger.sh) runs the
hardware database trigger with the host's `systemd-hwdb`.
