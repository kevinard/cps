#!/usr/bin/env python3
"""Measure the CPS CLI on a large reproducible repository with version history.

The generated distribution has realistic dependency fan-out: every package
requires a handful of lower-numbered packages (so low numbers act as core
libraries), some packages use require-any, capabilities, and recommendations,
and every package version carries many file actions. Versions are published
with batched `cps publish` calls, so one signed catalog update admits many
manifests.

Typical scale run with a release build:

    cargo build --release --bin cps --example tuf-fixture
    python3 tests/benchmark-catalog.py --release --packages 30000 --versions 3 --files 40

Each measured command also reports its wall time and peak resident set size.
"""

import argparse
import json
import os
from pathlib import Path
import random
import re
import shutil
import subprocess
import tempfile
import time

PUBLISHER = "bench"


def package_name(number):
    return f"bench/pkg-{number:06}"


def capability_name(number):
    return f"cap/bench-{number:04}"


def package_model(packages, seed):
    """Return deterministic dependency lines and provided capabilities per package.

    Dependencies point at lower-numbered packages with a bias towards the
    lowest numbers, which gives a deep, wide graph whose closures look like a
    distribution's: a few hundred packages for a typical application.
    """
    rng = random.Random(seed)
    capabilities = max(1, packages // 200)
    providers = {}
    model = []
    for number in range(packages):
        lines = []
        provides = []
        if number >= 8 and rng.random() < 0.03:
            capability = rng.randrange(capabilities)
            provides.append(capability_name(capability))
            providers.setdefault(capability, []).append(number)
        if number > 0:
            fan_out = rng.randint(2, 7)
            targets = sorted({int(number * rng.random() ** 3) for _ in range(fan_out)})
            for target in targets:
                lines.append(f'    require "{package_name(target)}"\n')
            if number > 16 and rng.random() < 0.05:
                first, second = sorted(rng.sample(range(number), 2))
                lines.append(
                    f'    require-any {{ package "{package_name(first)}"; '
                    f'package "{package_name(second)}"; }}\n'
                )
            if number > 16 and rng.random() < 0.10:
                lines.append(
                    f'    recommends "{package_name(rng.randrange(number))}"\n'
                )
            offered = [cap for cap, owners in providers.items() if owners[0] < number]
            if offered and rng.random() < 0.03:
                lines.append(
                    f'    require-capability "{capability_name(rng.choice(offered))}"\n'
                )
        model.append(("".join(lines), provides))
    return model


def closure(model, root):
    pattern = re.compile(r'"(bench/pkg-(\d+))"')
    seen = set()
    pending = [root]
    while pending:
        number = pending.pop()
        if number in seen:
            continue
        seen.add(number)
        for match in pattern.finditer(model[number][0]):
            pending.append(int(match.group(2)))
    return seen


def manifest_text(number, version, model, files, uid, gid, payloads=None):
    """Render one package manifest, optionally with published payload digests."""
    dependencies, provides = model[number]
    name = package_name(number)
    lines = [
        'schema-version 1 kind="package-manifest"\n',
        f'package "{name}" publisher="{PUBLISHER}" version="{version}.0.0-0" '
        f'summary="Benchmark package {number}" {{\n',
        dependencies,
    ]
    for capability in provides:
        lines.append(f'    provides "{capability}"\n')
    for index in range(files):
        path = f"usr/share/bench/pkg-{number:06}/file-{index:03}"
        extra = ""
        if payloads is not None:
            sha256, size = payloads
            extra = f' sha256="{sha256}" size={size}'
        lines.append(f'    file "{path}" mode="0644" uid={uid} gid={gid}{extra}\n')
    lines.append("}\n")
    return "".join(lines)


def measured(command):
    """Run a command to completion and return its resource usage."""
    process = subprocess.Popen(command, stdout=subprocess.DEVNULL)
    _, status, rusage = os.wait4(process.pid, 0)
    process.returncode = os.waitstatus_to_exitcode(status)
    if process.returncode:
        raise subprocess.CalledProcessError(process.returncode, command)
    return rusage


def payload(number, version):
    return f"bench package {number} version {version}\n".encode()


def main():
    project = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--packages", type=int, default=1000)
    parser.add_argument("--versions", type=int, default=2)
    parser.add_argument("--files", type=int, default=4,
                        help="file actions per package version")
    parser.add_argument("--batch", type=int, default=5000,
                        help="manifests per signed catalog update")
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--closure", type=int, default=64,
                        help="approximate closure size of the installed workload")
    parser.add_argument("--release", action="store_true",
                        help="use target/release binaries")
    parser.add_argument("--cps", type=Path)
    parser.add_argument("--tuf-fixture", type=Path)
    parser.add_argument("--work", type=Path,
                        help="keep the generated repository in this new directory")
    parser.add_argument("--skip-mirror", action="store_true",
                        help="skip the TUF export and sync measurements")
    options = parser.parse_args()
    if options.packages < 32 or options.versions < 2 or options.files < 1 or options.batch < 1:
        parser.error("require at least 32 packages, two versions, one file, and a batch size")
    profile = "release" if options.release else "debug"
    cps = (options.cps or project / f"target/{profile}/cps").resolve(strict=True)
    tuf = (options.tuf_fixture or project / f"target/{profile}/examples/tuf-fixture")
    tuf = tuf.resolve(strict=True)
    model = package_model(options.packages, options.seed)
    # Install an application from the top of the graph whose closure is
    # closest to the requested size.
    workload = min(
        range(options.packages - 1, options.packages // 2, -1),
        key=lambda number: abs(len(closure(model, number)) - options.closure),
    )

    if options.work:
        options.work.mkdir(parents=True)
        context = None
        work = options.work.resolve()
    else:
        context = tempfile.TemporaryDirectory(prefix="cps-catalog-")
        work = Path(context.name)
    os.chdir(work)
    timings = {}
    peak_rss_kib = {}

    def progress():
        # Keep partial results of long runs.
        (work / "results.json").write_text(json.dumps(
            {"timings_seconds": timings, "peak_rss_kib": peak_rss_kib}, indent=2))

    def run(*args, label=None):
        started = time.perf_counter()
        rusage = measured([str(cps), *map(str, args)])
        if label is not None:
            timings[label] = round(time.perf_counter() - started, 4)
            # Linux reports ru_maxrss in KiB.
            peak_rss_kib[label] = rusage.ru_maxrss
            progress()

    subprocess.run(["openssl", "genpkey", "-algorithm", "ED25519",
                    "-outform", "DER", "-out", "private.der"], check=True)
    subprocess.run(["openssl", "pkey", "-inform", "DER", "-in", "private.der",
                    "-pubout", "-outform", "DER", "-out", "public.der"], check=True)
    uid, gid = os.getuid(), os.getgid()
    manifests = Path("manifests")
    manifests.mkdir()

    def stage(version, numbers):
        """Write a proto tree whose file actions share one payload per package."""
        proto = Path(f"proto-{version}")
        paths = []
        for number in numbers:
            directory = proto / f"usr/share/bench/pkg-{number:06}"
            directory.mkdir(parents=True)
            first = directory / "file-000"
            first.write_bytes(payload(number, version))
            for index in range(1, options.files):
                os.link(first, directory / f"file-{index:03}")
            path = manifests / f"{number:06}-{version}.kdl"
            path.write_text(manifest_text(number, version, model, options.files, uid, gid))
            paths.append(path)
        return proto, paths

    def publish_version(version, numbers, label):
        proto, paths = stage(version, numbers)
        started = time.perf_counter()
        for start in range(0, len(paths), options.batch):
            run("publish", "--repo", "repo", "--proto", proto,
                "--signing-key", "private.der", *paths[start:start + options.batch])
        timings[label] = round(time.perf_counter() - started, 4)
        progress()
        shutil.rmtree(proto)

    everything = range(options.packages)
    for version in range(1, options.versions):
        publish_version(version, everything, f"publish_version_{version}")
    run("image-create", "--root", "image", "--repo", "repo",
        "--publisher", PUBLISHER, "--trust-key", "public.der")
    run("plan", "--root", "image", "--output", "install.json",
        "install", package_name(workload), label="plan_install")
    run("apply", "--root", "image", "install.json", label="apply_install")
    publish_version(options.versions, everything, f"publish_version_{options.versions}")
    run("plan", "--root", "image", "--output", "upgrade.json",
        "update", label="plan_upgrade")
    run("apply", "--root", "image", "upgrade.json", label="apply_upgrade")

    # Publishing one more package into the large catalog should cost what the
    # change costs, not what the catalog costs.
    model.append(("    require \"bench/pkg-000000\"\n", []))
    proto, paths = stage(1, [options.packages])
    run("publish", "--repo", "repo", "--proto", proto, "--signing-key", "private.der",
        *paths, label="publish_one_package")
    shutil.rmtree(proto)
    run("plan", "--root", "image", "--output", "single.json",
        "install", package_name(options.packages), label="plan_install_after_publish")

    run("verify", "--root", "image", "--pristine", label="verify_offline")
    lookup = ("search", "--root", "image", "--file",
              f"/usr/share/bench/pkg-{workload:06}/file-000")
    run(*lookup, label="file_search_manifests")
    run("repo", "index-files", "--repo", "repo", "--publisher", PUBLISHER,
        "--signing-key", "private.der", label="publish_file_index")
    run(*lookup, label="file_search_index")
    if not options.skip_mirror:
        run("repo", "export", "--repo", "repo", "--publisher", PUBLISHER,
            "--trust-key", "public.der", "--output", "targets", label="export")
        mirror = subprocess.check_output([tuf, "targets", "mirror", "1"], text=True).strip()
        sync = ("repo", "sync", "--repo", "cache", "--root", "mirror/root.json",
                "--metadata", mirror + "metadata/", "--targets", mirror + "targets/",
                "--publisher", PUBLISHER, "--trust-key", "public.der", "--index-only")
        run(*sync, label="refresh_empty_cache")
        run(*sync, label="refresh_existing_cache")
    print(json.dumps({
        "binary": str(cps),
        "packages": options.packages + 1,
        "catalog_entries": options.packages * options.versions + 1,
        "files_per_package": options.files,
        "installed_packages": len(closure(model, workload)),
        "filesystem": "local disposable directory",
        "timings_seconds": timings,
        "peak_rss_kib": peak_rss_kib,
    }, indent=2))
    if context is not None:
        os.chdir("/")
        context.cleanup()


if __name__ == "__main__":
    main()
