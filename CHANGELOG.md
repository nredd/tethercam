# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- Reject partial media decodes when FFmpeg reports an error.
- Prepare and clean up hosted USB/IP prerequisites for the release gate.

Next release: `0.0.2`.

### Fixed

- Make runtime and actor log-capture tests deterministic when the lib tests share one process (tracing callsite interest was cached as disabled by a concurrent thread)
- Make `make coverage` independent of where the toolchain keeps `rust-src`, print the rustc release, and on failure write a filtered text report (`coverage/uncovered.txt`) that locates uncovered regions
- Identify diagnostic property `0xD203` as still-image size, not live-view resolution, in API and verification documentation
- Reject nonregular profiling inputs without blocking, cap aggregate bytes during reads and bound parser records/lines/columns
- Preserve typed diagnostic-example error sources and property-test arbitrary USB/IP management/import inputs

### Added

- Protected-source, OIDC-only crates.io publishing with verified Cargo archives, staged immutable GitHub releases, release-tag integrity policy, and explicit owner bootstrap/recovery instructions
- Off-by-default, importable `test-support` API with a generic USB/IP engine, Sony/SICDD and synthetic non-Sony personalities, fault/resource/deadline/re-import/cancellation verification and hosted native integration
- Rust developer utilities for explicitly scoped VHCI provisioning, protected client inputs, bounded media-client termination with joined camera shutdown, source diagnostics/cadence and aggregate-only profiling; preserve default application install/run behavior
- Typed public verification errors distinguish protocol/identity validation from I/O, model, deadline and task failures

### Changed

- Start RTSP and ONVIF HTTP without a camera: RTSP `DESCRIBE`/`SETUP` and the ONVIF Device and Media services answer `503` until the first live-view frame defines the stream size, then publish profiles and SDP from it; WS-Discovery announces once ready. No resolution is declared or guessed
- Add the Digest-authenticated `GET /tethercam/status` (`waiting`/`ready`, battery percentage and power source, no device identity) and `camera_ready`/`runtime_listening` log events
- Check every live-view frame's JPEG header size against the described streams, even with no subscriber; a changed size now fails the run instead of waiting for the first `PLAY`
- Give live-view snapshots a 10 s reply budget; only opt-in full-resolution stills keep 360 s
- Print `Listening ... waiting for a camera.` first; `Serving authenticated RTSP and ONVIF` now follows the first frame
- `RtspServer::fanout` returns `Option`, and `RtspServer::bind_pending` with `StreamPublisher` and `OnvifService::pending` with `ModelPublisher` allow late stream and model publication
- Serve the newest live-view JPEG as the ONVIF snapshot instead of firing the shutter for every request, fetching one frame when the last is over 2 s old and failing at once with no camera
- Add opt-in `streams.full_resolution_snapshot` (default off) for full-resolution stills; a still that fails without leaving the shutter pressed (no image, unsupported) no longer ends the camera session
- Keep WS-Discovery running when one interface cannot bind or join: skip it with a warning and fail only when none is usable; skip loopback and IPv4 link-local addresses when neither `interface` nor a specific `onvif_address` is set, and keep a skipped interface from claiming its subnet in the responder
- Generalize Sony execution over `PtpSession`; external `CameraFactory` implementations now supply and recover an associated session, and explicit `SonyCamera` type annotations use the session type instead of the transport type
- Delegate SRC coverage enforcement/reporting to native `cargo-llvm-cov` zero-uncovered and fail-under-100 checks, including all source tests and optional verification/tooling code

### Removed

- Shell coverage validator/fixtures and USB/IP setup helper; maintained verification logic is Rust

## [0.0.1] - 2026-11-12

### Fixed

- Bind privileged service config ownership changes to validated no-follow file handles, and retain owned uninstall receipts across unlink, sync and native-reload failures

- Reject unrepresentable USB reset deadlines before port I/O instead of panicking

- Reject unusable Digest credential syntax during config validation, before opening a camera
- Join ONVIF HTTP connection threads on shutdown instead of leaving detached workers behind
- Exercise pacer slot-accounting invariants deterministically even with one property-test case
- Consume a completed Sony capture property refresh even when it finishes after the wait deadline
- Keep RTSP benchmark subscribers alive across bursts so repeated Criterion iterations cannot lose a packet to a stale pump and hang
- Keep Sony sessions open when wait budgets expire, classify protocol fallback errors, and allow longer still-image downloads
- Redact ONVIF response bodies and WS-Security replay caches from debug output
- Redact RTSP response bodies in debug output
- WS-Discovery sockets bound to an ephemeral port no longer set `SO_REUSEPORT`, which let Linux give two sockets the same port (cross-talk between parallel tests)
- Do not advertise IPv6 link-local addresses in WS-Discovery `XAddrs`

### Removed

- Coverage transition list and helpers; the coverage gate now enforces all of `src/`
- `Session::scoped`; callers explicitly open and close sessions
### Changed

- Document the WS-Discovery `XAddrs` limitation on overlapping subnets and test a synthetic multicast Probe round-trip on loopback
- Disable implicit coverage filename exclusions and enforce exact per-file coverage for all executable source modules, including separately named tests

- Bound ONVIF SOAP conversion, redact request debug output, and return typed RTSP errors

### Added

- Explicit `service install|uninstall|status` for protected per-user launchd agents and Linux system/user systemd units, with persisted identity, dedicated system account, Sony USB udev access, native state diagnostics and unlimited graceful stop time

- Loopback-only, pure-Rust USB/IP Sony simulator for real `nusb` Linux backend tests on hosted x64 and arm64 runners, with targeted synthetic-device permissions, stateful cancellation and an export-lifetime memory admission budget

- Foreground `setup`, `probe`, `doctor` and `run` commands with generated config/identity reuse, sanitized tracing, explicit operator output, and an opt-in JPEG simulator
- End-to-end on-demand Sony actor, bounded paced H.264/JPEG pipelines, shared-port RTSP mounts, matching ONVIF profiles/full-resolution snapshots, reconnect/backoff and joined signal/error cleanup, with real authenticated simulator network tests and a persistent-client pipeline benchmark
- Deterministic coverage of RTSP connection failures, completing RTSP's coverage transition
- Deterministic scripted RTSP media source for tests and examples
- Reproducible default-branch ruleset payload and workflow inventory tests, with native all-targets/all-features CI checks and staged administrator activation docs
- Deterministic ONVIF HTTP socket, listener and thread setup failure tests; enforce full coverage of `src/onvif/http.rs`
- Deterministic WS-Discovery socket setup and receive failure tests, completing discovery coverage
- Reject fully covered stale entries in `coverage-pending.txt` during `make coverage`
- Deterministic coverage of the media pipeline, removing its files from `coverage-pending.txt`

- Deterministic configuration failure tests and full coverage for `src/config.rs`
- Configurable outgoing and simulated PTP container payload limits, checked before encoding
- Sony SDIO extension driver (`tethercam::sony`): connect handshake with protocol-generation negotiation (2.00 and 3.00), property table with change tracking, property writes and shutter controls, live-view frames with the Sony header stripped, still capture and download, battery and power source, protocol fallback from 3.00 to 2.00 (including after a stalled request), deadline-based waits with a short event-poll timeout, capability detection and a per-model quirks registry. `transport::sim::sony` teaches the simulated camera the extension for both generations, `transport::sim::Responder` lets any extension keep state. Adds the `sony_live_view` benchmark and an `#[ignore]`d hardware soak test
- USB transport on `nusb` (`tethercam::transport::usb`): Sony still-image interface selection from descriptor bytes, connection-mode detection (PC Remote / MTP / Mass Storage) with a "set USB Connection to PC Remote" error, bulk and interrupt transfers with timeouts and cancellation, clear-halt, class Device Reset and Get Device Status, hot-plug watching, udev rule and setup notes in `docs/usb.md`, and an `#[ignore]`d hardware test
- Coverage gate: `make coverage` (part of `make all`) and a `Coverage` workflow require 100% line, function and region coverage of `src/` via `cargo-llvm-cov`; `coverage-pending.txt` lists the files exempt during the transition and may only shrink
- `tethercam::cli::run`, the testable core of the binary, with an integration test that runs the built binary
- PTP data types, `DeviceInfo` / `ObjectInfo` / `DevicePropDesc` datasets and operation, response, event and property code tables (`tethercam::ptp`)
- RTP layer (`tethercam::rtp`): RFC 3550 header codec, H.264 (RFC 6184) and JPEG (RFC 2435) packetizers writing into a reusable `PacketPool`, `sprop-parameter-sets` extraction, 90 kHz timestamp helpers, property tests and benchmarks
- TOML configuration with generated credentials and persisted ONVIF identity

- ONVIF Device and Media services (`tethercam::onvif`): SOAP 1.2 parser, WS-Security `UsernameToken` digest with replay cache, HTTP Digest, bounded HTTP/1.1 server, snapshot endpoint, golden tests and benchmark
- WS-Discovery responder for ONVIF (`tethercam::onvif::discovery`): `Hello`/`Bye`, `Probe`/`Resolve` matching in both the ONVIF-era (2005/04) and WS-Discovery 1.1 dialects, with per-source and global rate limits (reflected traffic bounded) and de-duplication
- RTSP 1.0 server (`tethercam::rtsp`): `OPTIONS`/`DESCRIBE`/`SETUP`/`PLAY`/`TEARDOWN`/`GET_PARAMETER`, TCP-interleaved and UDP unicast RTP, RTCP sender reports, Digest authentication (`SHA-256`, `MD5`) on every request, `MediaSource` trait with drop-on-full fan-out and on-demand start/stop, RFC 2069 `qop`-less `MD5` accepted for live555-style clients
- Media pipeline (`tethercam::media`): constant-frame-rate `Pacer` with an injectable clock, `zune-jpeg` based `JpegDecoder` to I420, `Encoder` trait with a pure-Rust software H.264 backend, Annex B helpers, `media` benchmark; evaluation in `docs/encoder-evaluation.md`
- PTP generic container codec (`tethercam::ptp`) with property tests and benchmarks
- `Transport` trait, scriptable simulated camera (`transport::sim`) and PTP session engine (`ptp::session`) with stall and wedge recovery
- Project scaffold: gate (`make all`), CI workflows, crates.io release workflow
