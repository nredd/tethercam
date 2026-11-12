# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- WS-Discovery sockets bound to an ephemeral port no longer set `SO_REUSEPORT`, which let Linux give two sockets the same port (cross-talk between parallel tests)
- Do not advertise IPv6 link-local addresses in WS-Discovery `XAddrs`

### Removed

- `Session::scoped`; callers explicitly open and close sessions

### Added

- Reject fully covered stale entries in `coverage-pending.txt` during `make coverage`
- Deterministic coverage of the media pipeline, removing its files from `coverage-pending.txt`

- Deterministic configuration failure tests and full coverage for `src/config.rs`
- Configurable outgoing and simulated PTP container payload limits, checked before encoding
- Sony SDIO extension driver (`tethercam::sony`): connect handshake with protocol-generation negotiation (2.00 and 3.00), property table with change tracking, property writes and shutter controls, live-view frames with the Sony header stripped, still capture and download, battery and power source, protocol fallback from 3.00 to 2.00 (including after a stalled request), deadline-based waits with a short event-poll timeout, capability detection and a per-model quirks registry. `transport::sim::sony` teaches the simulated camera the extension for both generations, `transport::sim::Responder` lets any extension keep state. Adds the `sony_live_view` benchmark and an `#[ignore]`d hardware soak test
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
