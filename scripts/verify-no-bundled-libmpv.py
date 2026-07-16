#!/usr/bin/env python3
"""Fail when a default MIT build contains or links a native MPV runtime."""

from __future__ import annotations

import argparse
import os
import plistlib
import shutil
import subprocess
import glob
from pathlib import Path


FORBIDDEN_FILE_NAMES = {"mpv", "mpv.exe", "libmpv-2.dll", "mpv-2.dll"}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("artifact", type=Path)
    return parser.parse_args()


def candidate_binaries(artifact: Path) -> list[Path]:
    if artifact.is_file():
        return [artifact]
    if not artifact.is_dir():
        raise SystemExit(f"artifact does not exist: {artifact}")

    candidates: list[Path] = []
    info = artifact / "Contents" / "Info.plist"
    if info.is_file():
        with info.open("rb") as source:
            executable = plistlib.load(source).get("CFBundleExecutable")
        if isinstance(executable, str):
            candidates.append(artifact / "Contents" / "MacOS" / executable)

    candidates.extend(
        path
        for path in artifact.rglob("*")
        if path.is_file() and os.access(path, os.X_OK)
    )
    return sorted(set(candidates))


def find_dumpbin() -> str | None:
    if dumpbin := shutil.which("dumpbin"):
        return dumpbin
    if os.name != "nt":
        return None
    program_files_x86 = os.environ.get("ProgramFiles(x86)")
    if not program_files_x86:
        return None
    pattern = os.path.join(
        program_files_x86,
        "Microsoft Visual Studio",
        "*",
        "*",
        "VC",
        "Tools",
        "MSVC",
        "*",
        "bin",
        "Hostx64",
        "x64",
        "dumpbin.exe",
    )
    matches = sorted(glob.glob(pattern), reverse=True)
    return matches[0] if matches else None


def dependency_output(binary: Path) -> str:
    if shutil.which("otool"):
        kind = subprocess.run(
            ["file", os.fspath(binary)],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        if "Mach-O" in kind:
            return subprocess.run(
                ["otool", "-L", os.fspath(binary)],
                check=True,
                capture_output=True,
                text=True,
            ).stdout
    if shutil.which("ldd"):
        result = subprocess.run(
            ["ldd", os.fspath(binary)],
            capture_output=True,
            text=True,
        )
        if result.returncode == 0:
            return result.stdout + result.stderr
    if (dumpbin := find_dumpbin()) and binary.suffix.lower() in {".exe", ".dll"}:
        return subprocess.run(
            [dumpbin, "/nologo", "/dependents", os.fspath(binary)],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    return ""


def main() -> None:
    artifact = parse_args().artifact.resolve()
    violations: list[str] = []

    paths = [artifact] if artifact.is_file() else list(artifact.rglob("*"))
    for path in paths:
        if not path.is_file():
            continue
        name = path.name.lower()
        if name in FORBIDDEN_FILE_NAMES or name.startswith("libmpv."):
            violations.append(f"bundled MPV file: {path}")

    for binary in candidate_binaries(artifact):
        dependencies = dependency_output(binary)
        if "libmpv" in dependencies.lower() or "mpv-2.dll" in dependencies.lower():
            violations.append(f"load-time MPV dependency: {binary}\n{dependencies}")

    if violations:
        raise SystemExit("default build contains MPV:\n" + "\n".join(violations))
    print(f"default build contains no bundled or load-time MPV runtime: {artifact}")


if __name__ == "__main__":
    main()
