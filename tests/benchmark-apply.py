#!/usr/bin/env python3
"""Measure CPS transactions on an image with many managed files.

The image holds --packages packages of --files files each (50,000 managed
files by default). The script publishes the repository and provisions the
base image once, with the last --cps binary. For each --cps binary, it then
times, on its own copy of that image, installing, removing, and reinstalling
a 10-file package, a 1000-file package upgrade, and full and quick
verification. Older builds can be compared as long as they can open the
provisioned image state. Use release builds:

    cargo build --release --bin cps
    python3 tests/benchmark-apply.py --cps old/cps --cps target/release/cps

Publishing and provisioning dominate the setup. Pass --work DIR to keep the
published repository and provisioned image in DIR and reuse them later.
"""

import argparse
import contextlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time


def main():
    project = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--packages", type=int, default=50)
    parser.add_argument("--files", type=int, default=1000, help="files per package")
    parser.add_argument("--size", type=int, default=8192, help="bytes per file")
    parser.add_argument("--repeat", type=int, default=3,
                        help="runs of each verification mode; the minimum is reported")
    parser.add_argument("--work", type=Path,
                        help="keep and reuse the published repository in this directory")
    parser.add_argument("--cps", type=Path, action="append",
                        help="CPS binary to measure; repeat to compare builds "
                             "(default: target/release/cps)")
    options = parser.parse_args()
    if options.packages < 1 or options.files < 1 or options.size < 1:
        parser.error("packages, files, and size must be positive")
    binaries = [path.resolve(strict=True)
                for path in options.cps or [project / "target/release/cps"]]
    uid, gid = os.getuid(), os.getgid()

    if options.work:
        options.work.mkdir(parents=True, exist_ok=True)
        workspace = contextlib.nullcontext(options.work.resolve())
    else:
        workspace = tempfile.TemporaryDirectory(prefix="cps-apply-")
    with workspace as work:
        work = Path(work)

        def run(cps, *args, cwd, timings=None, label=None):
            started = time.perf_counter()
            subprocess.run([cps, *args], check=True, cwd=cwd, stdout=subprocess.DEVNULL)
            elapsed = round(time.perf_counter() - started, 4)
            if label is not None:
                timings[label] = min(timings.get(label, elapsed), elapsed)

        published = (work / "repo").exists()
        if not published:
            subprocess.run(["openssl", "genpkey", "-algorithm", "ED25519",
                            "-outform", "DER", "-out", "private.der"], check=True, cwd=work)
            subprocess.run(["openssl", "pkey", "-inform", "DER", "-in", "private.der",
                            "-pubout", "-outform", "DER", "-out", "public.der"],
                           check=True, cwd=work)

        def publish(cps, repo, name, version, files, salt):
            proto = work / f"proto-{name.replace('/', '-')}-{version}"
            lines = []
            if not proto.exists():
                for number in range(files):
                    path = (f"usr/share/bench/{name.split('/')[1]}/"
                            f"{number // 100:03}/f{number:05}")
                    target = proto / path
                    target.parent.mkdir(parents=True, exist_ok=True)
                    header = f"{name} {version} {salt} {number}\n".encode()
                    target.write_bytes(header + b"\0" * max(0, options.size - len(header)))
            for number in range(files):
                path = (f"usr/share/bench/{name.split('/')[1]}/"
                        f"{number // 100:03}/f{number:05}")
                lines.append(f'    file "{path}" mode="0644" uid={uid} gid={gid}\n')
            manifest = work / "package.kdl"
            manifest.write_text(
                'schema-version 1 kind="package-manifest"\n'
                f'package "{name}" publisher="bench" version="{version}.0.0-0" {{\n'
                + "".join(lines) + "}\n"
            )
            run(cps, "publish", "--repo", repo, "--proto", proto,
                "--signing-key", "private.der", manifest, cwd=work)

        started = time.perf_counter()
        if not published:
            for number in range(options.packages):
                publish(binaries[0], "repo", f"bench/base-{number:04}", 1, options.files, "a")
            publish(binaries[0], "repo", "bench/extra", 1, 10, "a")
        publish_seconds = None if published else round(time.perf_counter() - started, 4)

        # Provision the base image once, with the last binary, and give each
        # binary its own copy. Copies get new inodes and change times, so no
        # binary starts with trusted file fingerprints.
        provision = work / "provision"
        timings_file = provision / "timings.json"
        if not timings_file.exists():
            shutil.rmtree(provision, ignore_errors=True)
            provision.mkdir()
            timings = {}
            bases = [f"bench/base-{number:04}" for number in range(options.packages)]
            run(binaries[-1], "image-create", "--root", provision / "image", "--repo", "repo",
                "--publisher", "bench", "--trust-key", "public.der", cwd=work)
            run(binaries[-1], "plan", "--root", provision / "image", "--output",
                provision / "base.json", "install", *bases, cwd=work,
                timings=timings, label="plan_base")
            run(binaries[-1], "apply", "--root", provision / "image", provision / "base.json",
                cwd=work, timings=timings, label="apply_base")
            publish(binaries[-1], "repo", "bench/base-0000", 2, options.files, "b")
            timings_file.write_text(json.dumps({"cps": str(binaries[-1]), **timings}))
        provisioned = json.loads(timings_file.read_text())

        results = []
        for index, cps in enumerate(binaries):
            run_dir = work / f"run-{index}"
            shutil.rmtree(run_dir, ignore_errors=True)
            run_dir.mkdir()
            subprocess.run(["cp", "-a", provision / "image", run_dir / "image"], check=True)
            timings = {}

            def step(*args, label=None):
                run(cps, *args, cwd=run_dir, timings=timings, label=label)

            step("plan", "--root", "image", "--output", "extra.json", "install",
                 "bench/extra", label="plan_install_one")
            # The first transaction after provisioning hashes every file once.
            step("apply", "--root", "image", "extra.json", label="apply_install_one")
            step("plan", "--root", "image", "--output", "remove.json", "remove",
                 "bench/extra", label="plan_remove_one")
            step("apply", "--root", "image", "remove.json", label="apply_remove_one")
            step("plan", "--root", "image", "--output", "again.json", "install",
                 "bench/extra", label="plan_reinstall_one")
            step("apply", "--root", "image", "again.json", label="apply_reinstall_one")
            step("plan", "--root", "image", "--output", "upgrade.json", "update",
                 label="plan_upgrade_one")
            step("apply", "--root", "image", "upgrade.json", label="apply_upgrade_one")
            help_text = subprocess.run([cps, "verify", "--help"], check=True,
                                       stdout=subprocess.PIPE, text=True).stdout
            for _ in range(options.repeat):
                step("verify", "--root", "image", label="verify_full")
                if "--quick" in help_text:
                    step("verify", "--quick", "--root", "image", label="verify_quick")
            results.append({"cps": str(cps), "timings_seconds": timings})
            print(json.dumps(results[-1]), file=sys.stderr, flush=True)
            shutil.rmtree(run_dir)

        print(json.dumps({
            "managed_files": options.packages * options.files + 10,
            "file_size": options.size,
            "filesystem": "local directory under TMPDIR",
            "publish_seconds": publish_seconds,
            "provision": provisioned,
            "runs": results,
        }, indent=2))


if __name__ == "__main__":
    main()
