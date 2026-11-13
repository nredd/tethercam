# Architecture

`tethercam` makes a USB-tethered Sony Alpha camera look like an ordinary ONVIF network camera.
One normal application process, Rust only. Optional verification APIs/tools do not change default application startup.

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
| `ptp` | PTP wire format: containers, data types, datasets, operation / response / event codes, and the `PtpSession` transaction contract. The raw `Session<T>` implements it over `Transport`; callers explicitly close sessions, with clear-halt then reset recovery |
| `transport` | `Transport` trait (bulk out, bulk in, interrupt in, stall recovery; generic, not object-safe, so no boxed future per transfer). `usb` implementation on [`nusb`](https://docs.rs/nusb) (pure Rust, no libusb; `UsbTransport` is generic over a small `Port` trait so timeouts, packet alignment, stall and reset recovery are tested against a fake, and only `usb::nusb_backend` touches `nusb`; setup in [`usb.md`](usb.md)). `sim`: an in-memory simulated camera used by every test, extensible with a stateful `Responder` (`sim::sony` is a simulated Sony body of either protocol generation) |
| `sony` | Sony SDIO extension: connect handshake, extended device info, property table, set-property and control operations, live-view fetch, still capture and download, capability detection, model quirks |
| `media` | Live-view frame source, constant-frame-rate pacing, JPEG decode, `Encoder` trait with a pure-Rust software H.264 backend. Hardware encoders are optional features. Contract: the `Pacer` is a pull-style state machine over an injected `Clock` (no sleeping, no threads); `JpegDecoder` yields limited-range BT.601 I420; `Encoder::encode` writes exactly one Annex B access unit per frame, each IDR prefixed with SPS and PPS; `sps()` / `pps()` are bare NAL units for SDP `sprop-parameter-sets`. Dependencies (`zune-jpeg`, `rusty_h264-encoder`) are pure Rust; the encoder's SIMD kernels use `unsafe` inside the dependency only, and its global-allocator feature is off |
| `rtp` | RTP packetizers: H.264 (RFC 6184) and JPEG (RFC 2435). Frames in, packets out into a caller-owned reusable `PacketPool` (no per-packet allocation once warm). No I/O: `rtsp` sends what the pool holds |
| `rtsp` | RTSP 1.0 server: `OPTIONS` / `DESCRIBE` / `SETUP` / `PLAY` / `TEARDOWN`, TCP-interleaved and UDP transport, Digest authentication |
| `onvif` | WS-Discovery responder, Device and Media services, WS-Security `UsernameToken` digest, snapshot endpoint |
| `config` | TOML configuration, credential generation and storage |
| `supervisor` | Camera hot-plug, reconnect with backoff, session hygiene, battery and power telemetry, on-demand start and stop of the capture pipeline |
| `service` | Explicit, idempotent native lifecycle for per-user `launchd` agents and Linux system/user `systemd` units, secure generated files and persisted configuration; see [`service.md`](service.md) |

The `ptp` payload layer (`src/ptp/`) sits above the container codec:

- `Reader` / `Writer`: bounds-checked little-endian primitives, PTP strings (count-prefixed UTF-16LE, NUL-terminated, at most 254 characters) and `u32`-counted arrays
- `DataType` / `Value`: every standard datatype code (scalars to 128 bits, arrays, strings), decoded and encoded by code
- `DeviceInfo`, `ObjectInfo`, `DevicePropDesc`: dataset decode and encode. Decoders are total (typed `CodecError`, never a panic) and ignore trailing padding
- `OperationCode`, `ResponseCode`, `EventCode`, `DevicePropCode`: `u16` newtypes with named constants. Vendor codes pass through; `Display` names standard codes and prints the rest as hex

### Session API

`PtpSession` exposes typed transactions, borrowed outgoing data, event polling, open/close state
and the session's monotonic clock, with `Send` futures. `Session<T>` retains its concrete API.
`SonyCamera` is generic over a session rather than its transport, and `CameraFactory` constructs
its associated session with the supplied clock and recovers the acquired handle after cleanup.
Raw USB factories retain the endpoint's actual packet size for framing. This changes external
factory implementations and explicit `SonyCamera` type annotations; replace the former transport
parameter with `Session<TransportType>` for the raw implementation. No backend selection or
fallback is introduced.

### Proposed cooperative macOS acquisition

[Cooperative macOS camera acquisition](macos-camera.md) specifies a future shared
ImageCaptureCore backend using the shared session/transaction seam, not a shipped backend. ICC cannot
emulate USB pipes or recovery. Its caller deadlines do not cancel native work: one retained
native operation must complete before close, and confirmed closure is required before reuse.
Quarantine bounds admission but joined shutdown may remain unresolved indefinitely. The native
dependency, independent unsafe audit and off-by-default feature require approval before implementation.
Linux USB, simulator behavior and the existing security and CI contracts remain unchanged.

### Sony driver contract

- `SonyCamera::run(session, config, body)` is the only way to get a connected camera: it opens the session, runs the handshake, calls `body` and closes the session on every exit path (the body's error wins over a close error). A camera therefore cannot outlive its session, and a failure anywhere leaves the body un-wedged
- Handshake: `GetDeviceInfo` (must list `SDIOGetExtDeviceInfo`, else `SdioUnsupported`, usually a body outside PC Remote mode), `SDIOConnect` phases 1 and 2, `SDIOGetExtDeviceInfo` (retried while empty or busy), `SDIOConnect` phase 3, the first property table, then priority mode when the camera lists it as writable
- The driver requests the newest protocol generation (a model quirk may name another start) and uses whatever the camera answers. A refused 3.00 request (non-OK response, stall or timeout) falls back to 2.00 once: after a response the same session carries on, after a stall the session layer has reset the device, so the driver reopens the session and replays phases 1 and 2 first, leaving the camera un-wedged. Refusing both is `SonyError::ProtocolRefused`, naming both attempts. The negotiated generation is: `Protocol::V200` (pre-2020 bodies, the measured a7 III) or `Protocol::V300`. The generations differ in the property table (3.00 enumerations carry a second list; 2.00 mixes controls into the stream) and in the `SDIOGetAllExtDevicePropInfo` parameters
- Capabilities (`Capabilities`) come from the advertised operations, the extended property and control lists and the property table. The per-model exceptions live in `Quirks`; nothing else keys on a model name
- Properties: `PropertyTable` from a full `SDIOGetAllExtDevicePropInfo` read, refreshed when a property-changed event arrives (`pump_events`). Writes go through `set_property` (checked against the table: known, writable, right datatype) and `control`
- Waits (extended-info retries, live-view readiness, per-frame retries, capture completion) are elapsed-time deadlines on the session's injectable clock, covering time spent in transactions and event reads. The remaining budget gates the start of each transaction and caps event polls and retry pauses. An in-flight transaction uses the normal session timeout and may finish after the wait budget, avoiding a reset of a healthy camera. A completed event-triggered property refresh is consumed before the budget gates another operation. Event polls use their own short timeout (`SonyConfig::event_poll_timeout`, via `Session::poll_event_within`), because an idle interrupt pipe holds a poll for the whole timeout on real hardware
- Live view: `wait_live_view_ready` polls the status property; `next_live_view_frame` retries the `0xFFFFC002` fetch while the camera answers `InvalidObjectHandle` or `AccessDenied`, then strips the Sony header (offset and size of the JPEG, then the JPEG) in place
- Stills: `capture_still` discards a stale in-memory image, half-presses, full-presses, looks once for the image, releases both buttons on every path, then waits if needed for the object-added event or the object-in-memory property (either can be missed) and downloads handle `0xFFFFC001`. Captured and discarded stale images use `SonyConfig::download_timeout` (default five minutes), independent of the capture wait budget
- Right after connecting, property values read as zero for a few seconds and live view is not ready. Battery readings and stale-image detection are only meaningful after `wait_live_view_ready`
- The simulated body (`transport::sim::sony`) models the handshake order, both table layouts, the warm-up, denied live-view fetches, shutter and memory behavior, and injectable defects. It is an independent implementation of the protocol, not a mirror of the driver

### Native service contract

- `service::Manager` resolves platform/identity without installing anything. Its explicit
  `manage` operation owns only fixed standard paths; exact matching, ownership, permission,
  symlink and hardlink checks prevent replacing unrelated files. Privileged config and directory
  ownership changes use validated no-follow descriptor-relative opens and same-handle `fchown`,
  so an unprivileged service account cannot redirect root's chown through a replacement entry.
  A protected exact-template uninstall receipt survives unlink/sync/reload failures and authorizes
  only retry cleanup of the owned installation, never arbitrary cached manager jobs. Configuration uses the same
  protected `Config::load_or_create` identity/credential store as foreground setup and is retained
  on uninstall. Native argv contains only executable/config paths and fixed manager arguments,
  never secrets; external command output is not echoed and inherited environment is filtered
- Generated templates escape XML and systemd independently, including `%`, `$`, quotes and
  backslashes. Linux system scope requires root, a durable binary under `/usr` or `/opt`,
  `systemd-sysusers` and Sony-vendor-scoped udev access. User scope never escalates or emits
  privileged namespace/device hardening that would guarantee startup failure
- Stop uses unlimited native timeouts (`TimeoutStopSec=infinity`, launchd `ExitTimeOut=0`),
  preserving the actor's joined five-minute still-download/session hygiene. A truly wedged
  process can delay uninstall/host shutdown; force-killing remains an explicit operator choice
- The process runner and concrete syscall table are narrow test seams. Tests execute real
  temp-root file lifecycles and generated-template native validators, never host daemon,
  account, udev or physical-camera lifecycle operations. Details and limitations: [`service.md`](service.md)

The Unix service filesystem uses `rustix` with only `std` and `fs` features for safe `openat`
with `NOFOLLOW`/`DIRECTORY` and `fchown`. Standard Rust exposes path-based chown but not these
safe descriptor-relative operations. `rustix` is a permissively licensed Rust syscall wrapper,
already present transitively via `tempfile`; it vendors no C/C++ and requires no native build.
No local unsafe code, platform-number constants, or new license exception is needed.

### RTSP server contract

- `rtsp::MediaSource` hands the server ready RTP packets as bytes plus a `TrackInfo` (codec, clock
  rate, `fmtp`) for SDP. The server pulls only while a client is playing. `rtsp::source::ScriptedSource`
  supplies a finite or repeating packet list, optionally gated, or a channel-fed stream for tests and examples
- `on_first_subscriber` / `on_last_unsubscribe` fire on the first `PLAY` and the last leaver, so
  the supervisor can start and stop the capture pipeline
- `RtspServer::bind_mounts` puts distinct, non-nested stream paths on one configured listener.
  Authentication, connection/session caps, deadlines and UDP policy are shared across mounts;
  a session stays bound to the mount selected by its `SETUP`. The primary mount's fan-out
  remains available through `fanout()`
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

### CLI and supervisor contract

- `cli::Options::parse` is total over OS strings, rejects unknown/duplicate options, preserves
  non-UTF-8 foreground paths, and consumes at most seven arguments. The bounded `service`
  syntax accepts only `install|uninstall|status [--user]` and dispatches before runtime config,
  signals or USB access; service template paths reject unsupported bytes/control characters.
  `setup`, `probe`, `doctor` and `run`
  use explicit `Write` outputs. Setup loads or atomically creates the existing config-layer
  credentials and UUID; it does not create a second identity or credential store
- The executable bridges the existing `log` facade into `tracing`. A scoped subscriber takes
  its maximum level from validated configuration. Lifecycle events are static and USB-open
  categories are the payload-free `OpenError` enum; no untrusted transport error, config Debug,
  URL, request body, credential or device serial is logged
- `Supervisor<F>` owns the only runtime `Session<F::Camera>`. `CameraFactory` opens a transport,
  never a PTP session. Native selection reuses `usb::camera::pick` or matches the configured
  serial exactly. Factory-built unopened sessions preserve caller timeouts/limits while using
  the endpoint's actual 512/1024-byte geometry, including for `probe`. One initial JPEG
  determines actual dimensions before listeners start; ongoing capture is demanded by the first `PLAY` across either stream and stops after both streams'
  last subscriber leaves. Missing/unplugged cameras reconnect with exponential 100 ms to 5 s
  backoff, and battery/power telemetry is refreshed while idle too
- Every handshake, frame fetch, capture, download and close is awaited to completion. Only
  idle waits and backoff timers enter cancellation `select!` branches. `SonyCamera::run`
  always owns session cleanup. Shutdown never aborts a camera task, shortens a live transaction
  to a caller wait budget, or drops a shutter operation; the driver's normal transaction and
  five-minute still-download timeouts remain intact
- Camera-to-media delivery is one latest-frame `watch` slot. A `Pacer` driven by a monotonic,
  injectable clock supplies constant-rate output. Decode, encode and RTP packetization run in
  at most one joined blocking-pool job, so CPU work cannot stall PTP I/O or create an unbounded
  work queue. Each stream has a 512-packet queue and existing per-client queues still drop on
  full. New subscription generations discard stale packets. Unplug clears the retained image,
  forces the next H.264 IDR, and preserves the RTP timeline across the new pacer epoch.
  Per-run SSRCs, initial sequence numbers and timestamp origin use system entropy, with no
  fallback to predictable identifiers; reconnect keeps that identity and timeline
- ONVIF profiles advertise actual dimensions, configured frame rate/bitrate, H.264 Baseline
  `/live/main` and/or JPEG `/live/mjpeg` on that one RTSP port. A changed camera resolution is
  rejected, not silently sent under stale SDP. Enabled JPEG is validated against RFC 2435
  before opening listeners; unsupported layouts are not silently omitted. Bridge identity
  comes from the persisted UUID, including a stable locally administered logical MAC, not a fabricated physical-host MAC
- Authenticated snapshot requests use a four-entry bounded actor channel. They execute between
  complete live-view transactions, capture/download a full-resolution still and validate the
  PTP JPEG object format/SOI before serving it. RAW objects are refused without discarding an
  otherwise usable session. A snapshot caller's 360 s reply budget never cancels the actor's
  in-flight work. Shutdown drains queued replies as unavailable
- `supervisor::serve` joins actor, media and network cleanup even after startup or media errors.
  A shutdown future requests a stop; callers must keep polling `serve` until completion rather
  than aborting/dropping it. SIGINT/SIGTERM registrations are installed before any CLI PTP
  session is opened. Enabled discovery sends `Bye`; RTSP closes all sessions; HTTP joins connection
  threads off the executor so a pending snapshot cannot deadlock the actor's shutdown

New glue dependencies are pure Rust: `tracing` with `std`, `tracing-subscriber` with formatting,
registry and facade bridging (no environment-filter language), `tracing-log` for the existing
`log` producers, and `if-addrs` without default features for local interface addresses/prefixes.
The Tokio `signal` feature supplies portable OS signal handling. These wrap platform APIs but
vendor no native codec, compile no C/C++, and add no external runtime executable.

`--simulate JPEG` uses the same Sony actor, pacer, encoders, packetizers and network servers over
`SimCamera`, with no USB enumeration. `--no-discovery` omits only multicast discovery. The fixture
example writes a synthetic image, while the steady-state `pipeline` benchmark keeps both real
RTSP clients subscribed outside its measured loop. Network integration tests authenticate ONVIF
profile/URI calls and snapshots, reassemble H.264 FU-A and JPEG scan data from actual RTP, unplug
and reconnect the simulator, and reopen the cleanly closed camera without a reset. Fault tests
cover malformed media after a valid startup frame, bounded queues, native worker failures,
clock/permission/port diagnostics and shutdown during Sony waits and shutter release.

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

- RTSP: Digest authentication on every request, including from loopback. Response debug output reports body length without body bytes
- ONVIF: An allocation-free XML preflight limits nesting, elements, 64 attributes per element,
  32 namespace declarations, 256-byte names and namespace URIs, and 4096-byte attribute values
  before namespace resolution. DTDs, entity declarations, CDATA and processing
  instructions other than the XML declaration are rejected. SOAP conversion also limits copied
  strings to four times the input size before allocating the owned tree. Request debug output redacts
  credentials and bodies; response debug output reports only body length, and WS-Security replay
  cache contents are redacted. WS-Security `UsernameToken` uses password digest (plaintext refused),
  nonce replay cache and a bounded clock-skew window. HTTP Digest is the alternative. Only
  `GetSystemDateAndTime`, `GetCapabilities` and `GetServices` are pre-auth
- Discovery reveals only what ONVIF requires: at most one unicast reply per accepted probe (each at most 1400 bytes), on-link sources only, `MessageID` de-duplicated, a per-source token bucket and one global bucket (burst 20, 10 per second) charged before parsing, so reflected traffic is bounded by `refill * reply cap` (14000 bytes per second by default) regardless of the number of sources. IPv6 link-local subnets are served only when the same interface has a non-link-local IPv6 address to advertise; otherwise they are omitted and a warning is emitted through the `log` facade (applications must configure a logger to display it). A per-interface bind or multicast-join failure is likewise a warning; the interface keeps an empty socket slot so interface numbers stay aligned, and discovery fails only when no interface is usable
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

- Every layer is tested without physical hardware against the simulated camera. Hosted Linux
  test and coverage jobs also exercise the real `nusb` backend through VHCI and a test-only
  pure-Rust USB/IP export (`src/test_support/`, with scenarios in `tests/usbip/`, bound only to loopback). Sony/SICDD and independent vendor-class echo personalities share one generic protocol engine. No libusb or native
  dependency is used. USB/IP network headers are big endian; USB setup/descriptors are little
  endian. URBs, pending memory and timeouts are bounded; interrupted transfers support unlink.
  An export-wide, non-refundable admission budget also bounds simulator histories and queued
  replies. Class cancellation uses `SimCamera::cancel_transaction` to discard the matching
  bulk transaction, preserve its session and resume at the next transaction id.
  Explicit Rust utility setup/cleanup changes permissions only on exact synthetic VHCI identities;
  Cargo and tests remain unprivileged. Local native integration requires preconfigured delegated
  controls/node access and never provisions the host. The sole local coverage constraint is the
  thin `nusb_backend.rs` adapter when `CI` is absent and VHCI is absent; CI never exempts it.
  `make coverage` cleans profiles, disables implicit filename exclusions and delegates zero-
  uncovered line/function/region enforcement and empty-aggregate guards to cargo-llvm-cov alone.
  Named and nested actual source tests count; actual external source aliases do not. Native
  fixture-crate acceptance exercises uncovered source/support/nested tests, empty/stale inputs
  and malformed profile merging. No bespoke report validator or `grcov` remains.
  Unfiltered JSON, LCOV and HTML remain available through `make coverage-report`.
  The public off-by-default `test-support` feature includes reusable engine/personality/diagnostic
  APIs; `repo-tools` adds a thin developer binary for native setup, client inputs, subprocess
  verification and aggregate profiling. The protocol state machine is shared through a private
  boxed model-I/O boundary, with typed model access retained for explicit fixture mutation.
  Persistent frame state survives select cancellation, and shutdown joins every owned
  connection without a detached reader. Runtime application layers do not depend on this
  verification layer. See [`verification.md`](verification.md) for bounds, feature/package roles,
  privilege boundaries, helper interfaces and resource-accounting scope.
- ONVIF HTTP tests inject timeout-setting, listener-inspection, accept and thread-spawn
  failures through the same non-generic helpers used by the real sockets and threads. Loopback
  tests exercise request deadlines, pipelining, connection caps and wildcard shutdown; IPv4
  must succeed, while IPv6 may skip only an explicitly unavailable address family
- WS-Discovery UDP tests check fixed-port sharing and ephemeral-port exclusivity, exercise socket setup failures without privileged host changes, and round-trip a synthetic multicast Probe over loopback. Captured client probes remain blocked on #13 and tracked in #33. A concrete receive syscall seam covers fatal errors without invalidating descriptors
- RTSP connection tests use real loopback sockets for reset, EOF and stalled-write failures;
  narrow entropy and UDP allocation seams cover OS failures without relying on ephemeral-port order
- RTSP status conversion folds exactly three validated ASCII digits into `u16`; all values
  `000` through `999` remain accepted. An exhaustive regression covers this finite domain,
  with and without a reason phrase, without retaining an impossible integer-parse error path
- Supervisor lifecycle regressions attach scoped tracing subscribers to synchronous snapshot
  checks and each poll of actor/runtime futures. They assert rejected-snapshot, sanitized
  open-failure, disconnect, orderly-stop and runtime-failure events without exposing payloads,
  underlying error text or credentials. The flush-failure regression also verifies joined camera
  session cleanup and listener release on the error path.
  Testing with logging enabled exercises event-field construction as well as the disabled
  logging paths, whose coverage regions differ between LLVM platforms
- Discovery scope matching deterministically rejects non-ASCII requested and offered URIs;
  this invalid-input branch must not depend on arbitrary-byte property-test generation
- Property tests for every parser; golden-byte tests for every encoder against captures
- `criterion` benchmarks for per-frame paths, compiled and smoke-run in CI
- Hardware-in-the-loop tests are `#[ignore]`d and run by hand with a camera attached

## References

- [PTP, ISO 15740](https://www.iso.org/standard/63602.html)
- [Linux USB/IP wire protocol](https://docs.kernel.org/usb/usbip_protocol.html)
- [USB 2.0 specification](https://www.usb.org/document-library/usb-20-specification)
- [USB Still Image Capture Device class](https://www.usb.org/document-library/still-image-capture-device-definition-10-and-errata-16-mar-2007)
- [RTSP 1.0, RFC 2326](https://www.rfc-editor.org/rfc/rfc2326)
- [RTP payload for H.264, RFC 6184](https://www.rfc-editor.org/rfc/rfc6184)
- [RTP payload for JPEG, RFC 2435](https://www.rfc-editor.org/rfc/rfc2435)
- [ONVIF Core Specification](https://www.onvif.org/profiles/specifications/)
- [WS-Discovery](https://docs.oasis-open.org/ws-dd/discovery/1.1/os/wsdd-discovery-1.1-spec-os.html)
- [Third-Party Cameras in UniFi Protect](https://help.ui.com/hc/en-us/articles/26301104828439-Third-Party-Cameras-in-UniFi-Protect)
