# tethercam

Turn a Sony Alpha camera into a network camera for UniFi Protect and Home Assistant, over a
single USB-C cable.

> **Status: early development.** The wire-format foundation exists; the camera driver, RTSP and
> ONVIF layers are being built. Nothing here streams video yet. See
> [`docs/architecture.md`](docs/architecture.md) for the plan.

## What it will do

- Drive the camera over USB (PTP with Sony's remote-control extension). One cable for power,
  data and control
- Serve the camera's live view as an authenticated RTSP stream
- Present an ONVIF endpoint so UniFi Protect can discover and adopt it like any third-party
  camera, and Home Assistant can add it through its ONVIF integration
- Serve full-resolution stills as the ONVIF snapshot
- Run as a background service on macOS and on small Linux boards

## Supported hardware

Developed against the a7 III (ILCE-7M3). The goal is every Sony body that supports
"PC Remote" over USB. A compatibility table will live here as bodies are verified.

Live-view resolution is set by the camera, not by this software. On the a7 III it is
1024x680 at about 15 fps. Full resolution is available as stills.

## Install

Requires a Rust toolchain, 1.90 or newer ([rustup.rs](https://rustup.rs)).

```sh
cargo install tethercam
```

From source:

```sh
git clone https://github.com/nredd/tethercam
cd tethercam
cargo install --path .
```

## Camera setup

On the camera:

1. Menu → Setup → USB Connection → **PC Remote**
2. Menu → Setup → USB Power Supply → **On**
3. Connect it with a USB-C cable rated for data (USB 3 recommended), directly to the host

## Development

```sh
make help   # list targets
make all    # the full gate: format check, clippy, tests, docs, benchmarks, dependency audit
```

Tools: `cargo-nextest` and `cargo-deny`. Everything runs without a camera attached.
Repository rules are in [`AGENTS.md`](AGENTS.md); releases are described in
[`docs/releasing.md`](docs/releasing.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.

This project is not affiliated with Sony or Ubiquiti.
