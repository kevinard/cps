#!/usr/bin/env python3
"""Measure CPS transactions on an image with many managed files.

The image holds --packages packages of --files files each (50,000 managed
files by default). For each --cps binary, the script times installing the
base image, a one-package install, a one-package upgrade, and full and quick
verification. The repository is published once and copied for each binary,
so several builds can be compared on identical input. Use release builds:

    cargo build --release --bin cps
    python3 tests/benchmark-apply.py --cps old/cps --cps target/release/cps
"""

import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
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
    parser.add_argument("--cps", type=Path, action="append",
                        help="CPS binary to measure; repeat to compare builds "
                             "(default: target/release/cps)")
    options = parser.parse_args()
    if options.packages < 1 or options.files < 1 or options.size < 1:
        parser.error("packages, files, and size must be positive")
    binaries = [path.resolve(strict=True)
                for path in options.cps or [project / "target/release/cps"]]
    uid, gid = os.getuid(), os.getgid()

    with tempfile.TemporaryDirectory(prefix="cps-apply-") as work:
        work = Path(work)

        def run(cps, *args, cwd, timings=None, label=None):
            started = time.perf_counter()
            subprocess.run([cps, *args], check=True, cwd=cwd, stdout=subprocess.DEVNULL)
            elapsed = round(time.perf_counter() - started, 4)
            if label is not None:
                timings[label] = min(timings.get(label, elapsed), elapsed)

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
        for number in range(options.packages):
            publish(binaries[0], "repo", f"bench/base-{number:04}", 1, options.files, "a")
        publish(binaries[0], "repo", "bench/extra", 1, 10, "a")
        publish_seconds = round(time.perf_counter() - started, 4)

        results = []
        bases = [f"bench/base-{number:04}" for number in range(options.packages)]
        for index, cps in enumerate(binaries):
            run_dir = work / f"run-{index}"
            run_dir.mkdir()
            shutil.copytree(work / "repo", run_dir / "repo", symlinks=True)
            for key in ("private.der", "public.der"):
                shutil.copy(work / key, run_dir / key)
            timings = {}

            def step(*args, label=None):
                run(cps, *args, cwd=run_dir, timings=timings, label=label)

            step("image-create", "--root", "image", "--repo", "repo",
                 "--publisher", "bench", "--trust-key", "public.der")
            step("plan", "--root", "image", "--output", "base.json", "install", *bases,
                 label="plan_base")
            step("apply", "--root", "image", "base.json", label="apply_base")
            step("plan", "--root", "image", "--output", "extra.json", "install",
                 "bench/extra", label="plan_install_one")
            step("apply", "--root", "image", "extra.json", label="apply_install_one")
            publish(cps, run_dir / "repo", "bench/base-0000", 2, options.files, "b")
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
            shutil.rmtree(run_dir)

        print(json.dumps({
            "managed_files": options.packages * options.files + 10,
            "file_size": options.size,
            "filesystem": "local disposable directory (warm page cache)",
            "publish_seconds": publish_seconds,
            "runs": results,
        }, indent=2))


if __name__ == "__main__":
    main()
