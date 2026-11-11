# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- PTP data types, `DeviceInfo` / `ObjectInfo` / `DevicePropDesc` datasets and operation, response, event and property code tables (`tethercam::ptp`)
- RTP layer (`tethercam::rtp`): RFC 3550 header codec, H.264 (RFC 6184) and JPEG (RFC 2435) packetizers writing into a reusable `PacketPool`, `sprop-parameter-sets` extraction, 90 kHz timestamp helpers, property tests and benchmarks
- TOML configuration with generated credentials and persisted ONVIF identity

- ONVIF Device and Media services (`tethercam::onvif`): SOAP 1.2 parser, WS-Security `UsernameToken` digest with replay cache, HTTP Digest, bounded HTTP/1.1 server, snapshot endpoint, golden tests and benchmark
- PTP generic container codec (`tethercam::ptp`) with property tests and benchmarks
- `Transport` trait, scriptable simulated camera (`transport::sim`) and PTP session engine (`ptp::session`) with stall and wedge recovery
- Project scaffold: gate (`make all`), CI workflows, crates.io release workflow
