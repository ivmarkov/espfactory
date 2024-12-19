# espfactory (WIP!)

A utility for flashing/provisioning ESP32 PCBs at the factory premises

[![CI](https://github.com/ivmarkov/espfactory/actions/workflows/ci.yml/badge.svg)](https://github.com/ivmarkov/espfactory/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/espfactory.svg)](https://crates.io/crates/espfactory)
[![Matrix](https://img.shields.io/matrix/esp-rs:matrix.org?label=join%20matrix&color=BEC5C9&logo=matrix)](https://matrix.to/#/#esp-rs:matrix.org)

## What is this really?

A Rust alternative to a custom-made Python or shell script driving the ESP provisioning tools (`esptool`, `espefuse`, `espflash`)

## Highlights

- Pure Rust
- Interactive terminal UI with [ratatui](https://github.com/ratatui/ratatui)
- Library (API) or command-line
- Only needs the C lib pre-installed on the flashing PC. Everything else is statically linked in the executable
- Cross-platform:
  -  Windows X86_64
  -  MacOSX
  -  Linux + gLibc X86_64
  -  Linux + gLibc ARM64 (rPI Ubuntu or RaspOS)
  -  Linux + gLibc ARM32
- OOTB bundle loaders for a [directory](src/loader/dir.rs) (could also be NFS or something mounted with FUSE), [HTTP(S)](src.loader.http.rs) or [S3](src/loader/s3.rs)
- [Pluggable bundle loaders](src/loader.rs)

## How to use?

TBD

## Cross-building for other targets than the host one

As long as the `libudev` feature is disabled (by default it is), you can easily cross-build the `espfactory` CLI executable.

The rest of `espfactory` is pure-Rust so you only need a [linker for your cross-target](https://capnfabs.net/posts/cross-compiling-rust-apps-raspberry-pi/) and a C cross toolchain for the few dependencies that still need to compile custom C files (`ring`).

Sample ways to cross-compile:

(NOTE: If `cargo` greets you with a "note: the `XXX` target may not be installed" error, install the target first with `rustup target add XXX`.)

### With [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) 

> Note: does not support cross-compiling to Windows. For Windows, use some of the other options.

```sh
cargo install cargo-zigbuild
pip3 install zig
cargo zigbuild --target aarch64-unknown-linux-gnu # rPI 4+
```

### With [`cargo-xwin`](https://github.com/rust-cross/cargo-xwin) 

> Note: cross-compiles for Windows-MSVC only. You'll need `wine` pre-installed.

```sh
cargo install cargo-xwin
cargo xwin build --target x86_64-pc-windows-msvc
```

### With [`cross`](https://hackernoon.com/building-a-wireless-thermostat-in-rust-for-raspberry-pi-part-2) 

> Note: needs Docker or Podman pre-installed.

```sh
cargo install cross
cross build --target=x86_64-pc-windows-gnu # For e.g. Windows; Windows MSVC is not supported, only the GNU target
```
