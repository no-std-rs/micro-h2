#!/usr/bin/env python3
"""Align the packaged crate with Runnerless's recorded release version."""

from pathlib import Path
import re
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def prepare(tag):
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag):
        raise ValueError("expected a stable vMAJOR.MINOR.PATCH release tag")
    version = tag[1:]
    if ROOT.joinpath("version.txt").read_text().strip() != version:
        raise ValueError("tag differs from version.txt")
    manifest = ROOT / "Cargo.toml"
    lock = ROOT / "Cargo.lock"
    source = manifest.read_text()
    if tomllib.loads(source)["package"]["name"] != "micro-h2":
        raise ValueError("unexpected package name")
    updated, count = re.subn(r'(?m)^version\s*=\s*"[^"\n]+"', f'version     = "{version}"', source)
    if count != 1:
        raise ValueError("expected one package version in Cargo.toml")
    locked, count = re.subn(r'(\[\[package\]\]\nname = "micro-h2"\n)version = "[^"\n]+"',
                            lambda match: match[1] + f'version = "{version}"', lock.read_text())
    if count != 1:
        raise ValueError("expected one micro-h2 entry in Cargo.lock")
    # Validate both files before writing either.
    manifest.write_text(updated)
    lock.write_text(locked)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: prepare-release.py vMAJOR.MINOR.PATCH")
    try:
        prepare(sys.argv[1])
    except ValueError as error:
        raise SystemExit(str(error)) from None
