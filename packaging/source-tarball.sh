#!/bin/bash
# SPDX-FileCopyrightText: 2026 Mikołaj
# SPDX-License-Identifier: GPL-3.0-only
#
# Builds mapstick-<version>-vendored.tar.xz: the source of a tag together with
# every Rust dependency, so that it builds without network access — Launchpad
# (Ubuntu PPA) and Copr (Fedora) both build offline. Crates that Linux never
# compiles are left as empty stubs (prune-vendor.py).
#
# The archive is reproducible: the same tag gives the same bytes.
#
# Usage: packaging/source-tarball.sh <tag> <output dir>
set -euo pipefail

tag=$1
out=$(realpath -m "$2")
version=${tag#v}
name=mapstick-$version
here=$(cd "$(dirname "$0")" && pwd)
repo=$(git -C "$here" rev-parse --show-toplevel)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

git -C "$repo" archive --format=tar --prefix="$name/" "$tag" | tar -C "$work" -x
cd "$work/$name"

cargo vendor --locked --versioned-dirs vendor > vendor-config.toml
python3 "$here/prune-vendor.py" . vendor x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu

# Ścieżka względna, nie bezwzględna z wyjścia `cargo vendor`.
mkdir -p .cargo
sed 's|^directory = .*|directory = "vendor"|' vendor-config.toml > .cargo/config.toml
rm vendor-config.toml

mtime=$(git -C "$repo" log -1 --format=%ct "$tag^{commit}")
mkdir -p "$out"
tar -C "$work" --sort=name --mtime="@$mtime" --owner=0 --group=0 --numeric-owner \
    --pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime \
    -cf - "$name" | xz -9 -T1 > "$out/$name-vendored.tar.xz"
echo "$out/$name-vendored.tar.xz"
