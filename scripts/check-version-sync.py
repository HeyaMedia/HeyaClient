#!/usr/bin/env python3
"""Keep the application version and an optional release tag in lockstep."""

from __future__ import annotations

import json
import os
import pathlib
import sys
import tomllib


ROOT = pathlib.Path(__file__).resolve().parents[1]


def main() -> int:
    package_version = json.loads((ROOT / "package.json").read_text())["version"]
    tauri_version = json.loads(
        (ROOT / "src-tauri" / "tauri.conf.json").read_text()
    )["version"]
    with (ROOT / "src-tauri" / "Cargo.toml").open("rb") as cargo_file:
        cargo_version = tomllib.load(cargo_file)["package"]["version"]

    versions = {
        "package.json": package_version,
        "src-tauri/tauri.conf.json": tauri_version,
        "src-tauri/Cargo.toml": cargo_version,
    }
    if len(set(versions.values())) != 1:
        for path, version in versions.items():
            print(f"{path}: {version}", file=sys.stderr)
        print("application versions are not synchronized", file=sys.stderr)
        return 1

    tag = os.environ.get("GITHUB_REF_NAME") if os.environ.get("GITHUB_REF_TYPE") == "tag" else None
    if tag:
        expected_tag = f"v{package_version}"
        if tag != expected_tag:
            print(
                f"release tag {tag!r} does not match application version {expected_tag!r}",
                file=sys.stderr,
            )
            return 1

    print(f"HeyaClient version {package_version} is synchronized")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
