#!/usr/bin/env python3
"""Relocate a development libmpv graph into an existing Heya.app bundle."""

from __future__ import annotations

import argparse
import importlib.util
import os
import plistlib
import subprocess
from pathlib import Path
from types import ModuleType


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("app_bundle", type=Path)
    parser.add_argument("--mpv-source", required=True, type=Path)
    parser.add_argument(
        "--adhoc-sign",
        action="store_true",
        help="ad-hoc sign nested dylibs and the app after relocation",
    )
    return parser.parse_args()


def load_relocator(mpv_source: Path) -> ModuleType:
    relocator_path = mpv_source / "TOOLS" / "dylib_unhell.py"
    if not relocator_path.is_file():
        raise SystemExit(f"missing MPV relocator: {relocator_path}")

    spec = importlib.util.spec_from_file_location("heya_mpv_dylib_unhell", relocator_path)
    if spec is None or spec.loader is None:
        raise SystemExit(f"could not load MPV relocator: {relocator_path}")

    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def bundle_executable(app_bundle: Path) -> Path:
    info_path = app_bundle / "Contents" / "Info.plist"
    if not info_path.is_file():
        raise SystemExit(f"not a macOS application bundle: {app_bundle}")

    with info_path.open("rb") as info_file:
        executable_name = plistlib.load(info_file).get("CFBundleExecutable")
    if not isinstance(executable_name, str) or not executable_name:
        raise SystemExit(f"missing CFBundleExecutable in {info_path}")

    executable = app_bundle / "Contents" / "MacOS" / executable_name
    if not executable.is_file():
        raise SystemExit(f"missing application executable: {executable}")
    return executable


def tolerate_missing_developer_rpaths(relocator: ModuleType) -> None:
    upstream = relocator.get_rpaths_dev_tools

    def safe_get_rpaths(binary: str) -> list[str]:
        try:
            return upstream(binary)
        except subprocess.CalledProcessError:
            return []

    relocator.get_rpaths_dev_tools = safe_get_rpaths


def fix_moltenvk_install_name(app_bundle: Path) -> None:
    moltenvk = app_bundle / "Contents" / "Frameworks" / "libMoltenVK.dylib"
    if moltenvk.is_file():
        subprocess.run(
            ["install_name_tool", "-id", "@rpath/libMoltenVK.dylib", moltenvk],
            check=True,
        )


def is_macho(path: Path) -> bool:
    result = subprocess.run(
        ["file", path],
        check=True,
        capture_output=True,
        text=True,
    )
    return "Mach-O" in result.stdout


def adhoc_sign(app_bundle: Path) -> None:
    contents = app_bundle / "Contents"
    native_roots = [contents / "MacOS" / "lib", contents / "Frameworks"]
    for root in native_roots:
        if not root.is_dir():
            continue
        for path in sorted(root.rglob("*")):
            if path.is_file() and is_macho(path):
                subprocess.run(
                    ["codesign", "--force", "--sign", "-", path],
                    check=True,
                    stdout=subprocess.DEVNULL,
                )

    subprocess.run(
        ["codesign", "--force", "--deep", "--sign", "-", app_bundle],
        check=True,
        stdout=subprocess.DEVNULL,
    )


def main() -> None:
    args = parse_args()
    app_bundle = args.app_bundle.resolve()
    executable = bundle_executable(app_bundle)
    relocator = load_relocator(args.mpv_source.resolve())
    tolerate_missing_developer_rpaths(relocator)

    relocator.process(os.fspath(executable))
    fix_moltenvk_install_name(app_bundle)
    if args.adhoc_sign:
        adhoc_sign(app_bundle)


if __name__ == "__main__":
    main()
