# Architecture

`tethercam` makes a USB-tethered Sony Alpha camera look like an ordinary ONVIF network camera.
One process, one binary, Rust only.

```text
 Sony Alpha body                      tethercam                         consumers
┌───────────────┐  USB-C   ┌──────────────────────────────┐  LAN   ┌──────────────────┐
│ PC Remote mode│◄────────►│ transport ─ ptp ─ sony       │        │ UniFi Protect    │
│ live view JPEG│  power,  │              │               │◄──────►│  (ONVIF adopt)   │
│ stills        │  data,   │            media             │  ONVIF │ Home Assistant   │
│ properties    │  control │         ┌────┴────┐          │  RTSP  │  (ONVIF / RTSP)  │
└───────────────┘          │        rtsp      onvif       │        │ any RTSP client  │
                           │ config · supervisor · service│        └──────────────────┘
                           └──────────────────────────────┘
```

## Requirements

- **Single cable.** One USB-C cable carries power, data and control. No HDMI capture, no
  second cable, no Wi-Fi on the camera
- **Rust only.** No C in the repo. Native dependencies are avoided; any exception is
  feature-gated, off by default and justified here
- **General.** Any Sony body that speaks the SDIO PTP extension in PC Remote mode, detected by
  capability, not by a hardcoded model list. Model quirks live in one registry
- **Embedded-ready.** Runs on a small Linux SoC board (`aarch64`, `armv7`) as well as macOS.
  Bounded memory, no per-frame allocation on hot paths where avoidable, no GUI
- **Secure by default.** Nothing on the network is unauthenticated. Credentials are generated
  on first run, never shipped as defaults
- **For regular people.** `cargo install tethercam`, one setup command, adopt in Protect

## Layers

Bottom to top. Each is a module under `src/` and depends only on layers above it in this list.

| Module | Responsibility |
|---|---|
| `ptp` | PTP wire format: containers, data types, datasets, operation / response / event codes, the session and transaction engine (`ptp::session`, generic over `Transport`: it does no I/O itself, `CloseSession` on every exit path, clear-halt then reset recovery) |
| `transport` | `Transport` trait (bulk out, bulk in, interrupt in, stall recovery; generic, not object-safe, so no boxed future per transfer). `usb` implementation on [`nusb`](https://docs.rs/nusb) (pure Rust, no libusb). `sim`: an in-memory simulated camera used by every test |
| `sony` | Sony SDIO extension: connect handshake, extended device info, property table, set-property and control operations, live-view fetch, still capture and download, capability detection, model quirks |
| `media` | Live-view frame source, constant-frame-rate pacing, JPEG decode, `Encoder` trait with a pure-Rust software H.264 backend. Hardware encoders are optional features. Contract: the `Pacer` is a pull-style state machine over an injected `Clock` (no sleeping, no threads); `JpegDecoder` yields limited-range BT.601 I420; `Encoder::encode` writes exactly one Annex B access unit per frame, each IDR prefixed with SPS and PPS; `sps()` / `pps()` are bare NAL units for SDP `sprop-parameter-sets`. Dependencies (`zune-jpeg`, `rusty_h264-encoder`) are pure Rust; the encoder's SIMD kernels use `unsafe` inside the dependency only, and its global-allocator feature is off |
| `rtp` | RTP packetizers: H.264 (RFC 6184) and JPEG (RFC 2435). Frames in, packets out into a caller-owned reusable `PacketPool` (no per-packet allocation once warm). No I/O: `rtsp` sends what the pool holds |
| `rtsp` | RTSP 1.0 server: `OPTIONS` / `DESCRIBE` / `SETUP` / `PLAY` / `TEARDOWN`, TCP-interleaved and UDP transport, Digest authentication |
| `onvif` | WS-Discovery responder, Device and Media services, WS-Security `UsernameToken` digest, snapshot endpoint |
| `config` | TOML configuration, credential generation and storage |
| `supervisor` | Camera hot-plug, reconnect with backoff, session hygiene, battery and power telemetry, on-demand start and stop of the capture pipeline |
| `service` | Install as a `launchd` agent or `systemd` unit |

The `ptp` payload layer (`src/ptp/`) sits above the container codec:

- `Reader` / `Writer`: bounds-checked little-endian primitives, PTP strings (count-prefixed UTF-16LE, NUL-terminated, at most 254 characters) and `u32`-counted arrays
- `DataType` / `Value`: every standard datatype code (scalars to 128 bits, arrays, strings), decoded and encoded by code
- `DeviceInfo`, `ObjectInfo`, `DevicePropDesc`: dataset decode and encode. Decoders are total (typed `CodecError`, never a panic) and ignore trailing padding
- `OperationCode`, `ResponseCode`, `EventCode`, `DevicePropCode`: `u16` newtypes with named constants. Vendor codes pass through; `Display` names standard codes and prints the rest as hex

### RTSP server contract

- `rtsp::MediaSource` hands the server ready RTP packets as bytes plus a `TrackInfo` (codec, clock
  rate, `fmtp`) for SDP. The server pulls only while a client is playing
- `on_first_subscriber` / `on_last_unsubscribe` fire on the first `PLAY` and the last leaver, so
  the supervisor can start and stop the capture pipeline
- Each client has a bounded queue; a full queue drops packets for that client only
- Sessions are bound to the control connection that created them and die with it. UDP media goes
  only to that connection's peer address
- Digest nonces are server-issued, expire, and carry an `nc` high-water mark; a repeated `nc` is a
  replay
- RFC 2069 clients (`qop`-less `MD5`, e.g. live555-based) are accepted. Their nonce is valid only on
  the control connection that received it and until it expires; a correct digest on another
  connection or an expired nonce gets `401` with `stale=true`. Replay is therefore bounded to
  someone who already owns that TCP connection. `SHA-256` always requires `qop`
- Source hooks and polls run on one task per fan-out: a stop finishes cancelling the pump before
  the stop hook runs or a new start begins, so hooks lag the subscription slightly and never overlap

## Streams

The camera produces two things over USB:

- **Live view**: JPEG frames, roughly 1024x680 at about 15 fps on an a7 III. This is the
  only motion video available over USB on bodies without a UVC streaming mode
- **Stills**: full sensor resolution, on demand

They map to:

- **Main RTSP stream**: live view re-encoded as H.264, constant frame rate. This is what an
  NVR records
- **MJPEG RTSP stream**: live-view JPEGs passed through untouched (RFC 2435). Zero transcode
  cost, for clients that accept it and for low-power boards
- **ONVIF snapshot**: a full-resolution still. The camera allows one PTP session, so a still
  briefly pauses live view; the supervisor serializes the two

## Security model

- RTSP: Digest authentication on every request, including from loopback
- ONVIF: WS-Security `UsernameToken` with password digest (plaintext refused), nonce replay
  cache and a bounded clock-skew window. HTTP Digest is the alternative. Only
  `GetSystemDateAndTime`, `GetCapabilities` and `GetServices` are pre-auth
- Discovery reveals only what ONVIF requires: at most one unicast reply per accepted probe (each at most 1400 bytes), on-link sources only, `MessageID` de-duplicated, a per-source token bucket and one global bucket (burst 20, 10 per second) charged before parsing, so reflected traffic is bounded by `refill * reply cap` (14000 bytes per second by default) regardless of the number of sources
- Credentials: random, generated at first run, stored in a config file created with owner-only
  permissions. Compared in constant time. Never logged
- The `config` layer loads an explicit path or resolves the platform default. On first run it
  generates credentials and an endpoint UUID, then atomically publishes a new owner-only file;
  competing first runs load the winning file instead of replacing its identity. Publication syncs
  the file and containing directory, and syncs parents of newly created directories. Listener
  validation rejects overlapping addresses and ports within the same IP family. On Unix, loading
  rejects files accessible to group or others. Unsupported permission models fail closed
- No remote configuration surface. The config file and the CLI are the only way to change
  behavior
- Parsers of untrusted bytes (USB and network) are total and fuzz-tested with `proptest`

## Power

The camera charges and runs from USB when its "USB Power Supply" setting is on. Whether the
host port supplies enough to hold the battery level during continuous live view depends on the
host. The supervisor reads the battery level property and reports it; `docs/` records measured
endurance per host as it is gathered.

## What the prototype established

Measured on an a7 III (ILCE-7M3, firmware 4.04) with off-the-shelf tools before this crate
existed:

- PC Remote mode is required. In MTP or Mass Storage mode the camera rejects remote operations
- Live view over PTP: 1024x680 JPEG, about 46 KB per frame, 14.5 fps
- The camera stalls its bulk pipe and stays wedged until power-cycled when a session is
  abandoned without `CloseSession`. Session hygiene and stall recovery are therefore part of
  the driver contract, and the simulated camera models the wedge
- The camera may enumerate at USB 2 speed depending on port and cable. Live view fits easily;
  full-resolution stills benefit from USB 3
- The body uses the older Sony protocol generation (pre-2020 bodies). The driver negotiates the
  generation instead of assuming the newest

Not yet established, tracked as issues:

- Which codecs UniFi Protect accepts from third-party cameras (H.264 assumed)
- Whether a pure-Rust H.264 encoder reaches 15 fps at 1024x680 on an SoC board. Measured on a desktop core: yes, 137 fps Baseline (41 fps without the SIMD kernels), see `docs/encoder-evaluation.md`. Still unmeasured on `aarch64` / `armv7` hardware
- Battery endurance on bus power during continuous live view

## Testing

- Every layer is tested without hardware against the simulated camera
- Property tests for every parser; golden-byte tests for every encoder against captures
- `criterion` benchmarks for per-frame paths, compiled and smoke-run in CI
- Hardware-in-the-loop tests are `#[ignore]`d and run by hand with a camera attached

## References

- [PTP, ISO 15740](https://www.iso.org/standard/63602.html)
- [USB Still Image Capture Device class](https://www.usb.org/document-library/still-image-capture-device-definition-10-and-errata-16-mar-2007)
- [RTSP 1.0, RFC 2326](https://www.rfc-editor.org/rfc/rfc2326)
- [RTP payload for H.264, RFC 6184](https://www.rfc-editor.org/rfc/rfc6184)
- [RTP payload for JPEG, RFC 2435](https://www.rfc-editor.org/rfc/rfc2435)
- [ONVIF Core Specification](https://www.onvif.org/profiles/specifications/)
- [WS-Discovery](https://docs.oasis-open.org/ws-dd/discovery/1.1/os/wsdd-discovery-1.1-spec-os.html)
- [Third-Party Cameras in UniFi Protect](https://help.ui.com/hc/en-us/articles/26301104828439-Third-Party-Cameras-in-UniFi-Protect)
