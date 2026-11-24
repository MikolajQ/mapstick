# SPDX-FileCopyrightText: 2026 Mikołaj
# SPDX-License-Identifier: GPL-3.0-only
#
# Built in Copr (copr.fedorainfracloud.org/coprs/<owner>/mapstick) from the
# vendored release archive: every crate is inside and .cargo/config.toml
# points cargo at it, so the build needs no network. Copr takes this file from
# master and sets Version from the newest GitHub release
# (packaging/copr/custom-script.sh) — raise Release here to ship a packaging
# fix for the same MapStick version.

Name:           mapstick
Version:        0.1.0
Release:        1%{?dist}
Summary:        Navigation map updater for Hyundai, Kia and Genesis cars

# MapStick itself; the bundled Rust crates are mostly MIT, Apache-2.0 or
# either — each crate's licence is in vendor/<crate>/Cargo.toml.
License:        GPL-3.0-only
URL:            https://github.com/MikolajQ/mapstick
Source0:        %{url}/releases/download/v%{version}/%{name}-%{version}-vendored.tar.xz

ExclusiveArch:  x86_64 aarch64

BuildRequires:  cargo >= 1.92
BuildRequires:  rust >= 1.92
BuildRequires:  gcc
BuildRequires:  pkgconfig(fontconfig)
BuildRequires:  desktop-file-utils

Requires:       hicolor-icon-theme
Requires:       libxkbcommon
Requires:       udisks2
# Loaded at run time (dlopen), so rpm cannot see them: OpenGL rendering
# (there is a software fallback), Wayland and X11 windows.
Recommends:     exfatprogs
Recommends:     libglvnd-egl
Recommends:     libwayland-client
Recommends:     libwayland-cursor
Recommends:     libX11
Recommends:     libX11-xcb
Recommends:     libXcursor
Recommends:     libXi
Recommends:     libxkbcommon-x11
Recommends:     libXrandr

# The release profile strips the binary (Cargo.toml): nothing for a
# -debuginfo package.
%global debug_package %{nil}

%description
MapStick downloads the official navigation and infotainment software for a
car straight from the maker's update servers and puts it on a USB drive,
ready to plug into the car — the job of the maker's Navigation Updater, which
is not available for Linux.

It shows how much space an update needs before downloading anything, writes
half as much to the USB drive as the official tool, finds a car by its VIN
and can check in the background for new map releases.

MapStick is an unofficial project, not affiliated with Hyundai Motor Company,
Kia Corporation or Genesis.

%prep
%autosetup -n %{name}-%{version}

%build
# Same binary as the GitHub release: the profile in Cargo.toml, not Fedora's
# RUSTFLAGS (codegen-units=1 makes Slint build several times longer).
export RUSTFLAGS=""
export CARGO_HOME="$PWD/.cargo-home"
cargo build --release --frozen %{?_smp_mflags} --package mapstick

%install
install -Dpm0755 target/release/mapstick %{buildroot}%{_bindir}/mapstick
desktop-file-install --dir=%{buildroot}%{_datadir}/applications packaging/mapstick.desktop
install -Dpm0644 packaging/icons/mapstick.svg \
    %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/mapstick.svg
for size in 16 32 48 64 128 256 512; do
    install -Dpm0644 packaging/icons/mapstick-$size.png \
        %{buildroot}%{_datadir}/icons/hicolor/${size}x$size/apps/mapstick.png
done

%check
export RUSTFLAGS=""
export CARGO_HOME="$PWD/.cargo-home"
cargo test --release --frozen %{?_smp_mflags} --workspace

%files
%license LICENSE
%doc README.md README.pl.md CHANGELOG.md
%{_bindir}/mapstick
%{_datadir}/applications/mapstick.desktop
%{_datadir}/icons/hicolor/*/apps/mapstick.*
