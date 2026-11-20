#!/usr/bin/env python3
"""Check the resolved Linux dependency graph against the project's crate policy.

CPS links one crypto implementation, aws-lc-rs, so the graph must not contain
the crates listed in BANNED. A crate resolved at several versions builds
twice; ci/duplicate-crates.txt lists the duplicates that are accepted for now,
so that a dependency update that adds one fails until it is reviewed.

Run from the checkout root: python3 ci/check-dependencies.py [--update]
"""

import collections
import json
import pathlib
import subprocess
import sys

BANNED = {"ring"}
TARGET = "x86_64-unknown-linux-gnu"
ALLOWLIST = pathlib.Path(__file__).with_name("duplicate-crates.txt")


def resolved():
    # cargo tree applies features as a build does; cargo metadata also lists
    # optional dependencies that no build enables.
    tree = subprocess.check_output(
        ["cargo", "tree", "--locked", "--offline", "--target", TARGET,
         "--edges", "normal,build,dev", "--prefix", "none", "--format", "{p}"],
        text=True,
    )
    versions = collections.defaultdict(set)
    for line in tree.splitlines():
        fields = line.split()
        if len(fields) >= 2 and fields[1].startswith("v"):
            versions[fields[0]].add(fields[1][1:])
    return versions


def main():
    versions = resolved()
    errors = [f"banned crate {name} is in the dependency graph" for name in sorted(BANNED & versions.keys())]
    duplicates = sorted(name for name, found in versions.items() if len(found) > 1)
    if "--update" in sys.argv:
        ALLOWLIST.write_text("".join(f"{name}\n" for name in duplicates))
    else:
        accepted = {line.strip() for line in ALLOWLIST.read_text().splitlines() if line.strip()}
        errors += [
            f"{name} resolves to several versions: {', '.join(sorted(versions[name]))}"
            for name in duplicates
            if name not in accepted
        ]
        errors += [f"{name} is no longer duplicated; remove it from {ALLOWLIST.name}"
                   for name in sorted(accepted - set(duplicates))]
    for error in errors:
        print(error, file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
