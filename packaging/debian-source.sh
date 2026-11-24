#!/bin/bash
# SPDX-FileCopyrightText: 2026 Mikołaj
# SPDX-License-Identifier: GPL-3.0-only
#
# Builds an unsigned Debian source package of MapStick for the Ubuntu release
# this script runs on (inside an ubuntu:<series> container), from the
# vendored release archive (source-tarball.sh) and packaging/debian.
#
# Ubuntu needs a separate upload per release, each with its own version:
#   0.1.0-1~ppa1~ubuntu24.04.1   (Launchpad's recommended scheme)
# <ppa revision> goes up when only the packaging changes for the same
# MapStick release.
#
# Rust: MapStick needs 1.88 or newer. When the release's default rustc is
# older, the newest versioned toolchain (rustc-1.NN, cargo-1.NN) is used.
#
# Needs: dpkg-dev, debhelper (for `debian/rules clean`), apt lists present.
#
# Every upload carries the .orig tarball (-sa): Launchpad accepts a file that
# is already in the PPA as long as the contents are identical, so the uploads
# for different releases can go in any order.
#
# Usage: debian-source.sh <vendored tarball> <ppa revision> <out dir>
set -euo pipefail

tarball=$(realpath "$1")
revision=$2
out=$(realpath -m "$3")
here=$(cd "$(dirname "$0")" && pwd)
min_rust=1.88

# shellcheck source=/dev/null
. /etc/os-release
series=$VERSION_CODENAME
base=$(basename "$tarball")
version=${base#mapstick-}
version=${version%-vendored.tar.xz}
debversion="$version-1~ppa$revision~ubuntu$VERSION_ID.1"

candidate() {
    apt-cache policy "$1" 2>/dev/null | awk '/Candidate:/ { print $2 }'
}

newer_or_equal() {
    dpkg --compare-versions "$1" ge "$2"
}

rust_default=$(candidate rustc)
if [ -n "$rust_default" ] && [ "$rust_default" != "(none)" ] &&
    newer_or_equal "${rust_default#*:}" "$min_rust"; then
    rust_suffix=""
else
    rust_suffix=$(apt-cache pkgnames rustc-1. | { grep -E '^rustc-1\.[0-9]+$' || true; } |
        sed 's/^rustc-//' | sort -V | tail -n1)
    if [ -z "$rust_suffix" ] || ! newer_or_equal "$rust_suffix" "$min_rust"; then
        echo "No Rust >= $min_rust on Ubuntu $VERSION_ID ($series)" >&2
        exit 1
    fi
fi
echo "Ubuntu $VERSION_ID ($series): Rust ${rust_suffix:-default $rust_default}" >&2

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cp "$tarball" "$work/mapstick_$version.orig.tar.xz"
tar -C "$work" -xJf "$tarball"
src="$work/mapstick-$version"
rm -rf "$src/debian"
cp -r "$here/debian" "$src/debian"

if [ -n "$rust_suffix" ]; then
    sed -i -e "s/^\( *\)cargo (>= $min_rust)/\1cargo-$rust_suffix/" \
           -e "s/^\( *\)rustc (>= $min_rust)/\1rustc-$rust_suffix/" "$src/debian/control"
fi

maintainer=$(sed -n 's/^Maintainer: //p' "$src/debian/control")
cat > "$src/debian/changelog" <<EOF
mapstick ($debversion) $series; urgency=medium

  * MapStick $version for Ubuntu $VERSION_ID.
    https://github.com/MikolajQ/mapstick/releases/tag/v$version

 -- $maintainer  $(date -R)
EOF

(cd "$src" && dpkg-buildpackage -S -sa -d -us -uc)

mkdir -p "$out"
for f in "$work"/mapstick_*; do
    [ -f "$f" ] && cp "$f" "$out/"
done
echo "$out/mapstick_${debversion}_source.changes"
