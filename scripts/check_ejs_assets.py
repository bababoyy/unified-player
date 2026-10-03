#!/usr/bin/env python3
"""Verify bundled EJS bytes before spending time on Rust compilation."""

from __future__ import annotations

import hashlib
from pathlib import Path

try:
    import tomllib
except ModuleNotFoundError:  # Python 3.10 local maintenance environments.
    import tomli as tomllib


ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / "unified-player/src/client/youtube/ejs"


def check_assets(directory: Path = ASSETS) -> bool:
    manifest = tomllib.loads((directory / "manifest.toml").read_text())
    ok = True
    for name, filename in (("lib", "yt.solver.lib.js"), ("core", "yt.solver.core.js")):
        asset = manifest["assets"][name]
        actual = hashlib.sha256((directory / filename).read_bytes()).hexdigest()
        if asset["path"] != filename or actual.upper() != asset["sha256"].upper():
            print(f"EJS {name}: SHA-256 mismatch (check asset bytes and Git line endings)")
            ok = False
    if ok:
        print("EJS assets: both SHA-256 hashes match the manifest")
    return ok


if __name__ == "__main__":
    raise SystemExit(0 if check_assets() else 1)
