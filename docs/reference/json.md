# JSON output

Commands that list or inspect state accept `--json` and then print one
JSON document on standard output instead of text. Warnings and errors still
go to standard error, and the [exit status](exit-status.md) is the same as
in text mode, so `cps search --json NAME` prints `[]` and exits with 3 when
nothing matches.

| Command | Document |
| --- | --- |
| `list`, `search` | An array of packages: `id`, `name`, `status`, `summary` |
| `info`, `policy` | One package, or its selection policy across publishers |
| `owner`, `contents`, `deps`, `rdeps` | An array of package IDs or paths |
| `why` | The package's local request, signed profile role, dependents, recommenders, and the dependency path that keeps it installed |
| `verify` | An array of findings, each with `is_error` and `message` |
| `history`, `status` | Transactions, each with its `operation` and its `details`, which are `null` when another CPS version recorded them, or the image revision and its pending transaction. On the running system, `status` adds `running`, `next_boot`, `trial`, `publishers`, and `publisher_error` when publisher freshness cannot be read |
| `config list` | `pending` vendor copies and `saved` local versions |
| `publisher list` | An array of publishers: `name`, `repository` |
| `boot list` | An array of boot environments. `invalid` holds the reason an entry file cannot be read, and is `null` otherwise |
| `image diff` | Both images' metadata and their package and path changes |
| `upgrade --check` | The update report, with `status` and the package changes |
| `recipe order` | An array of recipes, each with its packages and the recipes it follows |
| `recipe rebuilds` | The changed packages, the packages to rebuild, and their recipes |
| `repo withdrawals`, `repo obsoletions` | The current catalog's withdrawn package IDs with their manifest digests, or its obsoleted package IDs |

Field names are lowercase with underscores. A field without a value is
either omitted or `null`, as each command does consistently; read both as
absent.

The JSON documents follow CPS releases rather than carrying their own
format version. A release may add fields. Removing or renaming a field, or
changing its type, is a breaking change and is listed in the release notes.
[`tests/json-output.sh`](../../tests/json-output.sh) pins the shape of each
document to a key list under [`tests/golden/json`](../../tests/golden/json),
so such a change cannot happen unnoticed. Files that CPS reads back, such as
saved plans and image locks, carry their own `format` and are described in
[Configuration and data formats](README.md#configuration-and-data-formats).
