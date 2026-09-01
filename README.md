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

```bash
cargo build --release
```

The resulting executable will be in `target/release/mapstick`.

## License

MapStick is licensed under GPL-3.0-only. Please see [`LICENSE`](./LICENSE) for
the full license text.
