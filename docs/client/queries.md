# Find and inspect packages

[Documentation](../README.md) · [Clients](README.md)

The examples query an image in the `image` directory. On a running
CPS system these commands default to `/` when you omit `--root`.

```sh
cps list --root image
cps list --root image --intent
cps list --root image --available --all-versions
cps search --root image hello
cps search --root image --file hello
cps search --root image --file /usr/share/cps-demo/hello
cps info --root image demo/hello
cps info --root image --available demo/hello@1.0.0-0
cps policy --root image --probe demo/hello@1.0.0-0
cps owner --root image /usr/share/cps-demo/hello
cps contents --root image demo/hello
cps deps --root image demo/hello
cps rdeps --root image demo/hello
cps why --root image demo/hello
```

`list` shows installed packages and their explicit or dependency status. With `--available` it shows the newest version of each package in the signed catalog. `search` matches name and summary fragments case-insensitively and shows the signed summary when present. Use `--installed` to search installed packages. `info` prints the verified manifest in KDL, including optional dependency descriptions, requirements, and file actions. It accepts a name, a name and version, or a full CPS package ID. Installed queries work offline. Available queries use the configured local repository or cache.

`policy` lists candidate versions in publisher order with their verified
catalog pins, the preferred source, active freeze, signed profile manifests,
and whether each source requires build trust. Use `--json` for scripting.
`--probe` runs the normal install planner for the exact selector and
reports its result. Use `check-updates --explain` for a probe in the
system upgrade context.

`search --file` finds which available packages ship a file. Use an exact
basename or a path within the image, with or without a leading slash. It
prints the package ID and root-relative path for each match and exits with
status 1 if none match. It checks the newest version of each package for the
image's architecture in each publisher's verified catalog and honors the
image's facets and signed profile catalog pins. The search uses cached or
local manifests and works with an index-only mirror cache. Publishers can
enable indexed lookups with `cps repo index-files --repo REPO --publisher NAME
--signing-key KEY`. This publishes a successor catalog whose signature binds
a basename index. Subsequent releases regenerate it. Searches open only
candidate manifests and still check their file actions and digests. Mirror
export and sync carry the index alongside the signed catalog.

`owner` prints one installed package ID per line for an exact path. Use
`/usr/…` or `usr/…` to address a path within the selected image. Shared
directories can have multiple owners. The command exits with an error
for paths with no owner, including implicit parent directories.

`contents` accepts the same installed package selectors as `info` and
lists owned paths in sorted order, relative to the image root. Both
commands use committed ownership records offline, so a locally deleted
file still shows up and actions excluded by the image's facets or
architecture are omitted.

`deps` lists installed packages that satisfy the selected package's direct
requirements. `rdeps` lists installed packages with a direct requirement the
selected package satisfies. Both accept the same installed package selectors
as `info` and work offline from the committed image. They include all
matching installed providers for `require-any` and `require-capability`, and
conditional requirements whose predicate matches the installed image. Each
match appears once as a package ID, sorted by package name. An empty result
produces no output.

`why` uses the same selectors and works offline. It shows the package's
effective explicit local request, its role in the applied signed profile,
and its matching direct dependents together. Profile roles are `root` and `dependency`.
A listed dependent can also have another installed provider for an alternative
or capability. Installed packages with a matching `recommends` declaration
appear separately under `Recommended by`. For a dependency package, `why` also
shows a shortest path of active requirements from a local request or signed
profile root when one exists. It labels alternative requirements and counts
matching installed providers.

`list --intent` shows effective local requests separately from the roles in
the applied signed profile.

```sh
cps plan --root image --output mark.json mark explicit demo/hello
cps apply --root image mark.json
```

Use `mark automatic` to clear a local request. Both operations accept
installed package IDs or names and work with the repository offline.
They change the marks in the image database and record them in transaction
history. Package files and versions stay in place. Automatic packages can
be removed later by `autoremove` or signed profile reconciliation if the
resulting image has no use for them. A signed profile root stays protected
even after its local request is cleared. Apply marks to an inactive image
or stage the saved plan in a boot environment.
