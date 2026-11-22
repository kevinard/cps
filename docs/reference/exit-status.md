# Exit status

Every `cps` command exits with one of these statuses. Scripts can rely on
them: a status keeps its meaning across releases.

| Status | Meaning |
| --- | --- |
| 0 | Success. A check found nothing to report. |
| 1 | An error. CPS prints it on standard error, prefixed with `cps:`. |
| 2 | Invalid usage, such as an unknown option or a missing argument. `--help` and `--version` exit with 0. |
| 3 | A check found something. See below. |
| 4 | A confirmation prompt was declined. CPS prints `Canceled.` and changes nothing. |

Status 3 separates a result a script acts on from a failure:

| Command | Exits with 3 when |
| --- | --- |
| `cps upgrade --check` | Updates are available. Updates held back by a phased rollout exit with 0; blocked updates are an error. |
| `cps verify` | Verification reports an error finding. |
| `cps publisher audit-builds` | An installed package fails the build audit. |
| `cps recipe repin --check` | A recipe has a stale pin. |
| `cps config diff` | The local and vendor configuration differ. |
| `cps image diff` | The images have different packages or paths. |
| `cps manifest dependencies` | A detected dependency has no matching declaration. |
| `cps search` | No package matches. |
| `cps owner` | No installed package owns the path. |

`cps develop` exits with the status of the command it runs in the
development environment.
