# Changelog

## Unreleased

- Archives are unpacked part by part while they download, so a download no
  longer ends with a long unpacking phase. The progress line now shows the
  unpacked amount growing right behind the downloaded one, where it used to
  stand still until all parts of the large map file had arrived.

## 0.1.1

- AppImage for any 64-bit Linux distribution with glibc 2.35 or newer
  (Ubuntu 22.04, Debian 12 and newer). Background checks and notifications
  start the `.AppImage` file itself, and AppImageUpdate or Gear Lever can
  update it from GitHub releases.
- Packages for Ubuntu 24.04 and newer (`ppa:mikolaj-q/mapstick`), Fedora
  (Copr `mikolaj-q/mapstick`) and a `.deb` for Debian 13 and newer, built from
  every release.
- Building from source needs Rust 1.92 or newer.

## 0.1.0

First public release.

- Browse the official navigation update catalogue of Hyundai, Kia and Genesis
  for 13 regions, with the exact download size before anything is written.
- Find a car by its VIN through the maker's update site.
- Open the "Download" links from update.hyundai.com, update.kia.com and
  update.genesis.com (Linux).
- Prepare a USB drive: safe drive detection that never offers system disks,
  mounting when the desktop doesn't, free space check, exFAT formatting after
  confirmation, removal of the previous release's files.
- Download with resume, pause and clean-up; archives stay on the hard disk and
  are unpacked straight onto the drive while the next ones download.
- Copy or move a finished download from the hard disk onto a drive later.
- Favourites with the version in the car, and a background check that reminds
  you daily until the car has the new release.
- Weekly status report from the background check, and an alarm when it stops
  working.
- Interface in English, Polish, German, French, Spanish, Brazilian Portuguese
  and Korean.
- Linux and Windows. macOS builds from source, without drive support.
