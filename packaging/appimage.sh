#!/bin/sh
# SPDX-FileCopyrightText: 2026 Mikołaj
# SPDX-License-Identifier: GPL-3.0-only
#
# Builds MapStick-<version>-x86_64.AppImage and its .zsync from the release
# binary.
#
# The AppDir holds only the binary, the .desktop file and the icons. The
# binary is the one from the tarball (built on the oldest runner, so glibc
# 2.35 is enough); fontconfig and freetype, which it links, are on every
# desktop, and Wayland, X11, EGL and D-Bus it loads from the system at run
# time. Bundling those would break more than it fixes: they have to match
# the host's compositor and graphics driver.
#
# The runtime is the static type 2 runtime: the host needs no libfuse2, only
# fusermount (without FUSE, --appimage-extract-and-run still works).
# The update information lets AppImageUpdate and Gear Lever fetch only the
# changed blocks of the next release.
#
# Usage: appimage.sh <binary> <version> <out dir>
# Needs: appimagetool in PATH, $RUNTIME (the runtime-x86_64 file), zsyncmake.
set -eu

binary=$1
version=$2
out=$3
here=$(cd "$(dirname "$0")" && pwd)
: "${RUNTIME:?path to the AppImage type 2 runtime}"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
appdir=$work/MapStick.AppDir

install -Dm0755 "$binary" "$appdir/usr/bin/mapstick"
install -Dm0644 "$here/mapstick.desktop" "$appdir/usr/share/applications/mapstick.desktop"
install -Dm0644 "$here/icons/mapstick.svg" \
    "$appdir/usr/share/icons/hicolor/scalable/apps/mapstick.svg"
for size in 16 32 48 64 128 256 512; do
    install -Dm0644 "$here/icons/mapstick-$size.png" \
        "$appdir/usr/share/icons/hicolor/${size}x$size/apps/mapstick.png"
done

# What the AppImage format itself looks for in the top directory.
cp "$here/mapstick.desktop" "$appdir/mapstick.desktop"
cp "$here/icons/mapstick-256.png" "$appdir/mapstick.png"
ln -s mapstick.png "$appdir/.DirIcon"
# A symlink, not a script: /proc/self/exe then points into the mount
# ($APPDIR), which is how MapStick knows it runs from an AppImage
# (crates/mapstick/src/schedule.rs, appimage_file).
ln -s usr/bin/mapstick "$appdir/AppRun"

mkdir -p "$out"
name="MapStick-$version-x86_64.AppImage"
(
    cd "$out"
    ARCH=x86_64 appimagetool --no-appstream --runtime-file "$RUNTIME" \
        --updateinformation "gh-releases-zsync|MikolajQ|mapstick|latest|MapStick-*-x86_64.AppImage.zsync" \
        "$appdir" "$name"
    # appimagetool writes the .zsync itself when zsyncmake is in PATH.
    test -f "$name.zsync" || zsyncmake -u "$name" "$name"
)
ls -l "$out/$name" "$out/$name.zsync"
