# tethercam

Turn a Sony Alpha camera into a network camera for UniFi Protect and Home Assistant, over a
single USB-C cable.

> **Status: development.** The CLI, camera actor, codecs, authenticated RTSP and ONVIF run
> together against simulated cameras. The native USB factory is wired through `nusb`; physical
> camera end-to-end operation and real Protect/Home Assistant adoption are not yet validated. See [`docs/architecture.md`](docs/architecture.md).

## Pipeline

- Drive the camera over USB (PTP with Sony's remote-control extension). One cable for power,
  data and control
- Serve the camera's live view as an authenticated RTSP stream
- Present an ONVIF endpoint so UniFi Protect can discover and adopt it like any third-party
  camera, and Home Assistant can add it through its ONVIF integration
- Serve full-resolution stills as the ONVIF snapshot
- Run in the foreground on macOS and Linux, with graceful interrupt/terminate cleanup

Install a per-user macOS launchd agent or a Linux system/user systemd service with
`tethercam service install` (`--user` for Linux's user manager). Installation starts the service.
See [`docs/service.md`](docs/service.md) for privileges, standard paths, USB permissions, status,
uninstall and graceful stop policy. Real NVR adoption remains unvalidated.

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

## Commands

```sh
tethercam setup --config ./camera.toml
tethercam probe --config ./camera.toml
tethercam doctor --config ./camera.toml
tethercam run --config ./camera.toml
```

`setup` creates an owner-only configuration with random credentials and a persistent endpoint
identity, or loads the existing file without replacing it. It explicitly prints credentials to
the operator, so keep its output private. Without `--config`, the platform configuration directory
is used. `probe` lists candidate USB modes and reports the selected body's remote-control capabilities
and battery/USB power. `camera_serial` in the configuration selects an exact device; otherwise
the first usable camera is chosen. USB serials and OS bus paths are not printed. `doctor` checks file
permissions and values, listener addresses/ports, a plausible UTC clock and camera readiness; it
does not prove that NTP is synchronized. These foreground commands never install a daemon or
background themselves; only the explicit `service` command performs native service operations.

Both streams share the configured RTSP port. Enabled MJPEG is checked for RFC 2435 compatibility
before any listener starts; disable `streams.mjpeg` if a body's JPEG layout is unsupported.

Streams:

- `rtsp://192.0.2.10:8554/live/main`: constant-rate H.264 Baseline
- `rtsp://192.0.2.10:8554/live/mjpeg`: original JPEG, RFC 2435

Use the generated credentials in the client's authentication settings, not in a logged URL.
ONVIF advertises only enabled profiles and the corresponding stream and snapshot URIs. Stills
are captured at full sensor resolution through the same camera actor; use JPEG capture mode,
not RAW. A still briefly pauses live-view transactions. A changed live-view resolution fails
closed rather than advertising mismatched SDP/ONVIF metadata; restart to describe the new format.

The actor waits for a camera and reconnects with capped backoff after unplug. One initial JPEG
establishes actual stream dimensions before listeners start. Continuous capture and encoding begin
only after the first authenticated `PLAY`, and stop after the last subscriber leaves. `SIGINT` or
`SIGTERM` waits for in-flight camera work, releases shutter buttons, closes the PTP session, sends
WS-Discovery `Bye` when enabled, and joins network/CPU workers. A long still download can delay
shutdown; force-killing the process skips this hygiene. A snapshot reply can time out independently
without interrupting the camera operation.

For an isolated hardware-free run:

```sh
cargo run --example simulator_fixture -- synthetic.jpg
cargo run -- setup --config ./camera.toml
# Set rtsp_address and onvif_address in camera.toml to "127.0.0.1".
cargo run -- doctor --config ./camera.toml --simulate ./synthetic.jpg
cargo run -- run --config ./camera.toml --simulate ./synthetic.jpg --no-discovery
```

WS-Discovery infers the arrival interface from the sender's subnet, rather than receiving the
interface from the socket. With overlapping subnets it chooses the most specific match; equal
prefixes select the first configured interface. The advertised `XAddrs` can therefore point to
the wrong interface on such networks.

`--simulate JPEG` never enumerates USB. The example generates a synthetic 1024x680 JPEG, refuses
to overwrite an existing file, and starts no listener. In this CLI simulator the same input JPEG
is used for stills; integration tests inject a larger independent still. `--no-discovery` suppresses
multicast announcements without disabling authentication. Logs use `log_level` from the config
and contain static lifecycle events and sanitized error categories, never credentials, URLs,
request/response bodies or device serials.

## Development

```sh
make help       # list targets
make all        # the full gate: format check, clippy, tests, docs, benchmarks, dependency audit, coverage
make coverage   # 100% line, function and region coverage of src/ (cargo-llvm-cov)
```

Tools: `cargo-nextest`, `cargo-deny` and `cargo-llvm-cov` 0.9.1 or newer (with the
`llvm-tools-preview` rustup component). Put the rustup toolchain's `cargo` first on `PATH`.
`make coverage` cleans profiles and uses cargo-llvm-cov's native zero-uncovered line/function/
region checks plus fail-under-100 guards, with implicit filename exclusions disabled. Actual
`src/` includes named/nested source tests, developer tools and optional test-support implementation.
There is no bespoke JSON/schema/inventory validator. Native empty aggregates and malformed
profiles fail; external tests/examples/benchmarks/dependencies are outside SRC.
`make coverage-report` writes unfiltered JSON, LCOV and HTML under `coverage/`.

Hosted Linux test/coverage jobs explicitly provision VHCI through the Rust utility, import
loopback-only Sony and independent non-Sony USB/IP fixtures, and drive actual `nusb` handles.
Cargo/tests never run as root; only exact synthetic VHCI identities authorize node permissions
or detach. Local native testing requires explicitly preconfigured delegated access, never silent
`sudo` or host mutation. The sole local coverage constraint remains exactly
`src/transport/usb/nusb_backend.rs`, only when `CI` is absent and VHCI is absent. It is announced
loudly; any `CI` value disables it, and no other source file is eligible.

The next `0.0.2` release adds the off-by-default `test-support` feature with reusable `DeviceModel`/`Server` APIs and
diagnostics, separate from normal camera runtime. The optional `repo-tools` feature builds
`tethercam-tools` for native provisioning, protected client inputs, subprocess verification,
offline profiling summaries and release-tag validation. Default install/run behavior is unchanged.
See [`docs/verification.md`](docs/verification.md) for API, privilege, coverage, profiling and
extracted-package verification contracts, and [`docs/usb.md`](docs/usb.md) for normal camera setup.

`make bench-check` smoke-runs each case once; `make bench` records full Criterion measurements.
The RTSP fan-out benchmark keeps subscribers alive across bursts, measuring steady-state packet
delivery rather than subscription churn. Its shared workload also runs for 100 consecutive bursts
at each client count in the test suite, catching stalls that a one-iteration smoke run misses.
The `pipeline` benchmark keeps both authenticated RTSP clients playing while measuring the real
1024x680 simulator -> codec -> packetizer -> bounded queue -> TCP path; setup and subscription
churn are outside its measured loop. It is paced at 120 fps per stream, not an uncapped codec
throughput measurement.

Repository rules are in [`AGENTS.md`](AGENTS.md); releases are described in
[`docs/releasing.md`](docs/releasing.md). The checked-in branch ruleset, required CI inventory and
staged `gh` activation/read-back commands are in [`docs/branch-ruleset.md`](docs/branch-ruleset.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.

This project is not affiliated with Sony or Ubiquiti.
