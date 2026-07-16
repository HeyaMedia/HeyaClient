#!/usr/bin/env python3
"""Create Tauri's signed static updater manifest from release artifacts."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import pathlib
import re
import sys
import urllib.parse


def exactly_one(root: pathlib.Path, pattern: str) -> pathlib.Path:
    matches = sorted(path for path in root.rglob(pattern) if path.is_file())
    if len(matches) != 1:
        rendered = ", ".join(str(path) for path in matches) or "none"
        raise ValueError(f"expected one {pattern} below {root}, found: {rendered}")
    return matches[0]


def signature_for(artifact: pathlib.Path) -> str:
    signature_path = pathlib.Path(f"{artifact}.sig")
    if not signature_path.is_file():
        raise ValueError(f"missing updater signature {signature_path}")
    signature = signature_path.read_text(encoding="utf-8").strip()
    if not signature:
        raise ValueError(f"updater signature {signature_path} is empty")
    return signature


def release_url(repository: str, tag: str, artifact: pathlib.Path) -> str:
    filename = urllib.parse.quote(artifact.name)
    return f"https://github.com/{repository}/releases/download/{tag}/{filename}"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repository", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--macos", required=True, type=pathlib.Path)
    parser.add_argument("--windows", required=True, type=pathlib.Path)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    args = parser.parse_args()

    if not re.fullmatch(r"v\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?", args.tag):
        parser.error(f"release tag {args.tag!r} is not a supported semantic version tag")

    try:
        macos_artifact = exactly_one(args.macos, "*.app.tar.gz")
        windows_artifact = exactly_one(args.windows, "*.exe")
        macos_release = {
            "url": release_url(args.repository, args.tag, macos_artifact),
            "signature": signature_for(macos_artifact),
        }
        windows_release = {
            "url": release_url(args.repository, args.tag, windows_artifact),
            "signature": signature_for(windows_artifact),
        }
    except ValueError as error:
        print(error, file=sys.stderr)
        return 1

    manifest = {
        "version": args.tag.removeprefix("v"),
        "notes": f"HeyaClient {args.tag}",
        "pub_date": dt.datetime.now(dt.timezone.utc)
        .replace(microsecond=0)
        .isoformat()
        .replace("+00:00", "Z"),
        "platforms": {
            "darwin-aarch64": macos_release,
            "windows-x86_64": windows_release,
        },
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"wrote signed updater manifest to {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
