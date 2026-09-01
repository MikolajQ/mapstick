# MapStick

MapStick is an unofficial desktop app for downloading navigation firmware for
Hyundai, Kia, and Genesis vehicles and getting it onto a USB drive.

It is a graphical frontend for [nudl](https://github.com/chenxiaolong/nudl),
which it uses as a library rather than driving the command line tool. That
matters for two things the CLI cannot provide: the exact download size before
anything is written to disk, and structured progress events.

**Status: early development.** Model browsing and downloading work. Removable
media handling, favourites, and VIN lookup are not implemented yet.

## Scope

MapStick covers the infotainment head unit only — navigation with maps, and
Display Audio units without maps. It does not touch engine, transmission,
driver assistance, or instrument cluster software; those are not publicly
distributed.

Vehicles that support over-the-air updates can still be updated over USB. OTA
is only free during the connected services trial period, so the USB route
remains the one that costs nothing.

## Building

MapStick depends on a fork of nudl through a path dependency, because it needs
a library API and a downloader that can keep archives in a separate working
directory from the output. Both changes are backward compatible and meant to go
upstream. Until they do, check the fork out beside this repository:

```bash
git clone https://github.com/MikolajQ/nudl.git
git -C nudl switch separate-work-dir
git clone https://github.com/MikolajQ/mapstick.git
cd mapstick && cargo build --release
```

The two working copies must be siblings — `crates/mapstick/Cargo.toml` points at
`../../../nudl`.

The resulting executable will be in `target/release/mapstick`.

### Platform support

The removable media backend is implemented for Linux only so far, over udisks2.
Windows and macOS are planned and sit behind the same interface in
`crates/mapstick-media`.

## License

MapStick is licensed under GPL-3.0-only. Please see [`LICENSE`](./LICENSE) for
the full license text.
