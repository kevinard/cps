#!/usr/bin/env python3
"""Measure the CPS CLI on a reproducible repository with version history."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time


def main():
    project = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--packages", type=int, default=1000)
    parser.add_argument("--versions", type=int, default=2)
    parser.add_argument("--closure", type=int, default=128)
    parser.add_argument("--cps", type=Path, default=project / "target/debug/cps")
    parser.add_argument("--tuf-fixture", type=Path,
                        default=project / "target/debug/examples/tuf-fixture")
    options = parser.parse_args()
    if not 1 <= options.closure <= options.packages or options.versions < 2:
        parser.error("require 1 <= closure <= packages and at least two versions")
    cps = options.cps.resolve(strict=True)
    tuf = options.tuf_fixture.resolve(strict=True)

    with tempfile.TemporaryDirectory(prefix="cps-catalog-") as work:
        os.chdir(work)
        timings = {}

        def run(*args, label=None):
            started = time.perf_counter()
            result = subprocess.run([cps, *args], check=True, stdout=subprocess.PIPE)
            if label is not None:
                timings[label] = round(time.perf_counter() - started, 4)
            return result.stdout

        subprocess.run(["openssl", "genpkey", "-algorithm", "ED25519",
                        "-outform", "DER", "-out", "private.der"], check=True)
        subprocess.run(["openssl", "pkey", "-inform", "DER", "-in", "private.der",
                        "-pubout", "-outform", "DER", "-out", "public.der"], check=True)
        proto = Path("proto")
        (proto / "usr/share/bench").mkdir(parents=True)
        manifest = Path("package.kdl")

        def publish(name, version, path, dependencies=""):
            (proto / path).write_text(f"{name} {version}\n")
            manifest.write_text(
                f'schema-version 1\npackage "{name}" publisher="bench" version="{version}.0.0-0" {{\n'
                f'{dependencies}    file "{path}" mode="0644" uid={os.getuid()} gid={os.getgid()}\n'
                '}\n'
            )
            run("publish", "--repo", "repo", "--proto", "proto",
                "--signing-key", "private.der", manifest)

        def publish_packages(version):
            for number in range(options.packages):
                name = f"pkg-{number:06}"
                publish(f"bench/{name}", version, f"usr/share/bench/{name}")

        started = time.perf_counter()
        publish_packages(1)
        shared = "usr/share/bench/collision"
        publish("bench/guard", 1, shared)
        publish("bench/unsafe-provider", 1, shared)
        publish("bench/safe-provider", 1, "usr/share/bench/safe")
        dependencies = ''.join(
            f'    require "bench/pkg-{number:06}"\n' for number in range(options.closure)
        )
        dependencies += (
            '    require "bench/guard"\n'
            '    require-any { package "bench/unsafe-provider"; package "bench/safe-provider"; }\n'
        )
        publish("bench/workload", 1, "usr/share/bench/workload", dependencies)
        timings["publish_baseline"] = round(time.perf_counter() - started, 4)
        run("image-create", "--root", "image", "--repo", "repo",
            "--publisher", "bench", "--trust-key", "public.der")
        run("plan", "--root", "image", "--output", "install.json",
            "install", "bench/workload", label="plan_install")
        run("apply", "--root", "image", "install.json", label="apply_install")
        started = time.perf_counter()
        for version in range(2, options.versions + 1):
            publish_packages(version)
        timings["publish_version_history"] = round(time.perf_counter() - started, 4)
        run("plan", "--root", "image", "--output", "upgrade.json",
            "update", label="plan_upgrade")
        run("apply", "--root", "image", "upgrade.json", label="apply_upgrade")
        run("verify", "--root", "image", "--pristine", label="verify_offline")
        lookup = ("search", "--root", "image", "--file",
                  f"pkg-{options.packages - 1:06}")
        for attempt in range(3):
            run(*lookup, label=f"file_search_manifests_{attempt + 1}")
        run("repo", "index-files", "--repo", "repo", "--publisher", "bench",
            "--signing-key", "private.der", label="publish_file_index")
        for attempt in range(3):
            run(*lookup, label=f"file_search_index_{attempt + 1}")
        run("repo", "export", "--repo", "repo", "--publisher", "bench",
            "--trust-key", "public.der", "--output", "targets", label="export")
        mirror = subprocess.check_output([tuf, "targets", "mirror", "1"], text=True).strip()
        sync = ("repo", "sync", "--repo", "cache", "--root", "mirror/root.json",
                "--metadata", mirror + "metadata/", "--targets", mirror + "targets/",
                "--publisher", "bench", "--trust-key", "public.der", "--index-only")
        run(*sync, label="refresh_empty_cache")
        run(*sync, label="refresh_existing_cache")
        print(json.dumps({
            "packages": options.packages + 4,
            "catalog_entries": options.packages * options.versions + 4,
            "installed_packages": options.closure + 3,
            "filesystem": "local disposable directory",
            "timings_seconds": timings,
        }, indent=2))


if __name__ == "__main__":
    main()
