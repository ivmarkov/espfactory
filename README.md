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
