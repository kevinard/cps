#!/usr/bin/env python3
"""Seed a fresh offline Cargo home from raw crates.io archives in Cargo.lock."""

import hashlib
import re
import shutil
import sys
import tomllib
from pathlib import Path


def sparse_path(name):
    name = name.lower()
    if len(name) <= 2:
        return Path(str(len(name)), name)
    if len(name) == 3:
        return Path("3", name[0], name)
    return Path(name[:2], name[2:4], name)


def seed(lock, registry, output):
    packages = tomllib.loads(lock.read_text(encoding="utf-8"))["package"]
    crates = {}
    names = set()
    for package in packages:
        source = package.get("source")
        if source is None:
            continue
        name, version, checksum = (package[field] for field in ("name", "version", "checksum"))
        if (
            source != "registry+https://github.com/rust-lang/crates.io-index"
            or not re.fullmatch(r"[A-Za-z0-9_-]+", name)
            or not re.fullmatch(r"[0-9][A-Za-z0-9.+-]*", version)
            or not re.fullmatch(r"[0-9a-f]{64}", checksum)
        ):
            raise ValueError(f"unsupported locked Cargo package {name} {version}")
        archive = f"{name}-{version}.crate"
        if archive in crates:
            raise ValueError(f"duplicate locked crate {archive}")
        crates[archive] = checksum
        names.add(name)

    candidates = [
        cache for cache in (registry / "cache").iterdir()
        if cache.name.startswith("index.crates.io-")
        and (registry / "index" / cache.name / "config.json").is_file()
        and all((cache / archive).is_file() for archive in crates)
    ]
    if len(candidates) != 1:
        raise ValueError("expected one complete sparse crates.io cache, run cargo fetch --locked")
    cache = candidates[0]
    source_index = registry / "index" / cache.name
    dest_index = output / "registry" / "index" / cache.name
    dest_cache = output / "registry" / "cache" / cache.name
    dest_cache.mkdir(parents=True)
    dest_index.mkdir(parents=True)
    shutil.copyfile(source_index / "config.json", dest_index / "config.json")
    for name in sorted(names):
        entry = Path(".cache") / sparse_path(name)
        (dest_index / entry).parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source_index / entry, dest_index / entry)
    limit = 128 * 1024 * 1024
    for archive, expected in crates.items():
        with (cache / archive).open("rb") as source:
            contents = source.read(limit + 1)
        if len(contents) > limit or hashlib.sha256(contents).hexdigest() != expected:
            raise ValueError(f"locked checksum or size mismatch for {archive}")
        (dest_cache / archive).write_bytes(contents)
    print(f"Verified {len(crates)} locked crate archives")


if __name__ == "__main__":
    if len(sys.argv) != 4:
        sys.exit("usage: seed-cargo-cache.py CARGO-LOCK REGISTRY DESTINATION")
    try:
        seed(*(Path(argument) for argument in sys.argv[1:]))
    except (OSError, ValueError, KeyError) as error:
        sys.exit(str(error))
