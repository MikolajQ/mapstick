#!/bin/sh
# SPDX-FileCopyrightText: 2026 Mikołaj
# SPDX-License-Identifier: GPL-3.0-only
#
# Copr "Custom" source for MapStick: paste this into the package settings in
# Copr (Source Type: Custom; chroot fedora-latest-x86_64; build dependencies:
# curl sed). It runs with network access and leaves the spec and the source
# archive in the current directory; Copr then builds them offline in every
# chroot of the project.
#
# It takes the newest published GitHub release and the spec from master —
# the release workflow pokes Copr's webhook once an Ubuntu/Fedora build of
# that release has passed in GitHub Actions (.github/workflows/distro.yml).
set -eu

repo=MikolajQ/mapstick

# /releases/latest redirects to /releases/tag/<tag>; no API rate limit.
latest=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest")
tag=${latest##*/}
version=${tag#v}
case $version in
    [0-9]*) ;;
    *) echo "No release found ($latest)" >&2; exit 1 ;;
esac

curl -fsSLO "https://raw.githubusercontent.com/$repo/master/packaging/fedora/mapstick.spec"
sed -i "s/^Version:.*/Version:        $version/" mapstick.spec
curl -fsSLO "https://github.com/$repo/releases/download/$tag/mapstick-$version-vendored.tar.xz"
