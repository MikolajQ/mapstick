#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Mikołaj
# SPDX-License-Identifier: GPL-3.0-only

"""Turn vendored crates that Linux never compiles into empty stubs.

`cargo vendor` copies every crate in Cargo.lock, including the Windows and
macOS ones (windows-sys and friends alone are hundreds of megabytes). Cargo
still reads their manifests when it resolves the lock file, but it never
builds them for a Linux target, so all they need is:

  * the original Cargo.toml,
  * an empty file where the library would be,
  * a .cargo-checksum.json with the package checksum from Cargo.lock and no
    per-file checksums (cargo checks only the files that are listed).

Usage: prune-vendor.py <workspace dir> <vendor dir> <target triple>...
"""

import json
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path


def needed_packages(workspace: Path, targets: list[str]) -> set[tuple[str, str]]:
    needed = set()
    for target in targets:
        metadata = json.loads(
            subprocess.check_output(
                [
                    "cargo", "metadata", "--format-version", "1", "--locked",
                    "--offline", "--filter-platform", target,
                ],
                cwd=workspace,
            )
        )
        by_id = {p["id"]: p for p in metadata["packages"]}
        for node in metadata["resolve"]["nodes"]:
            package = by_id[node["id"]]
            needed.add((package["name"], package["version"]))
    return needed


def stub(crate: Path) -> None:
    manifest = (crate / "Cargo.toml").read_bytes()
    checksum = json.loads((crate / ".cargo-checksum.json").read_text())
    lib = tomllib.loads(manifest.decode()).get("lib", {}).get("path", "src/lib.rs")

    shutil.rmtree(crate)
    crate.mkdir()
    (crate / "Cargo.toml").write_bytes(manifest)
    (crate / lib).parent.mkdir(parents=True, exist_ok=True)
    (crate / lib).write_text("")
    (crate / ".cargo-checksum.json").write_text(
        json.dumps({"files": {}, "package": checksum.get("package")})
    )


def main() -> None:
    if len(sys.argv) < 4:
        sys.exit(__doc__.strip().splitlines()[-1])
    workspace, vendor = Path(sys.argv[1]), Path(sys.argv[2])
    needed = needed_packages(workspace, sys.argv[3:])

    stubbed = 0
    for crate in sorted(p for p in vendor.iterdir() if p.is_dir()):
        package = tomllib.loads((crate / "Cargo.toml").read_text())["package"]
        if (package["name"], package["version"]) not in needed:
            stub(crate)
            stubbed += 1
    print(f"{stubbed} of {sum(1 for p in vendor.iterdir() if p.is_dir())} crates stubbed",
          file=sys.stderr)


if __name__ == "__main__":
    main()
