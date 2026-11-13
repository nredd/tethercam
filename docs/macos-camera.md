# Cooperative macOS camera acquisition

The experimental `macos-icc` Cargo feature (off by default, macOS-only native code) implements this
contract in `transport::icc`. It is not production support and no runtime selects it: runtime
selection and commissioning are separate work in [#77](https://github.com/nredd/tethercam/issues/77).
The work is tracked by [#72](https://github.com/nredd/tethercam/issues/72); this document does not
close it.
ImageCaptureCore (ICC) provides a cooperative camera session and PTP command interface while
Apple's camera helpers remain running. Raw USB interface claiming can conflict with those
helpers. Killing or disabling them, installing a daemon, elevating capture privileges or using
whole-device capture is not the solution.

## Implementation (`macos-icc`)

```toml
tethercam = { version = "0.0.2", features = ["macos-icc"] }  # macOS, Rust 1.99 or newer
```

The feature depends on the registry crate [`image-capture`](https://crates.io/crates/image-capture)
`=0.1.0` ([source](https://github.com/nredd/image-capture)), target-gated to macOS. All native
interop and every `unsafe` block live in that crate. This repository stays `unsafe`-free and keeps
`unsafe_code = "forbid"`. Default, Linux, simulator and cross builds are unchanged: the dependency
is not compiled off macOS, `Cargo.toml` pins `=0.1.0` and `deny.toml` bans every newer version. The feature needs Rust
1.99 on macOS because that crate does; the declared MSRV is unchanged for everything else.

Layers, all in `src/transport/icc/`:

- `Boundary` / `Lease`: the crate's blocking owned-session API as traits that name none of its types.
  Everything below runs on any platform against deterministic implementations
- `run_with` / `IccHandle`: one owner thread holds the boundary and its lease. Async callers hold the
  handle. A shared busy gate admits one request at a time and is released when the native call has
  *finished*, so a cancelled caller future neither backlogs nor overlaps native work
- `IccSession`: a `PtpSession`, so `SonyCamera` runs unchanged and no Sony logic is duplicated
- `classify`: total classification of the boundary's tagged events
- `native` (macOS only): one-line delegations onto the crate, plus `run`, the main-thread entry point
- `read_only_sample`: the read-only acceptance consumer described below

Session semantics, all following from "ICC owns the pipes":

- Raw `OpenSession`/`CloseSession` are refused (`IccError::RawSessionOperation`). Response transaction
  ids are opaque and not exposed. More than five parameters, outgoing data over
  `min(max_data_out, 16 MiB)` and incoming data over `max_data_len` are typed errors. A non-`OK` camera
  response is `SessionError::Response`, never an ICC failure
- A caller deadline ends only the wait. `IccError::AdmissionExpired` means nothing was submitted;
  `IccError::DeadlineExceeded` means native work continues. After a deadline the session is *retired*:
  it refuses work, never issues a second close, and a replacement `open` returns `IccError::Busy`
  until the native owner has settled cleanup. A cancelled `open` future leaves the session `Opening`:
  the owner may still complete it, so `close` (not a second `open`) confirms closure if a lease exists. If none does, nothing was confirmed: `close` reports `IccError::NotOpen` and the session is retired. `close` on a retired session is `NotOpen` too (terminal; a retry cannot help), and only `open` probes for reuse, which stays `Busy` until native cleanup settles. A late open success is closed natively after it completes
- Failure categories are sanitized statics: permission, busy, removal, admission, deadline, malformed
  completion, event overflow, malformed event, cleanup failure, quarantine
- Failures fault, retire or quarantine the session. A faulted session refuses transactions but `close`
  still confirms closure. A quarantined session (native quarantine, removal, close failure, owner gone)
  accepts nothing and never reopens
- Events are advisory and bounded by the boundary (64), and the boundary has ONE process-wide queue.
  When a session opens, everything queued before it (stale PTP events, a previous native error or access
  restriction, the overflow flag) is discarded so it cannot fault or feed the new session. Removal is not
  lost by this: the native owner keeps a sticky, process-lifetime removal flag that quarantines
  independently of the queue (`driver.rs` `disconnected`). The Sony handshake re-reads the property table
  after open, so events lost in the instant between open completion and the discard are resynchronized.
  Afterwards `poll_event` fails closed. Overflow, access restriction, a native error observation, an
  oversized event and any malformed, truncated or oversized PTP event fault the session, so the Sony
  layer must close and reconnect to resynchronize. Removal quarantines it. Only benign observations are skipped

Native code runs on the main thread (`run` blocks it in the run loop) and joins one worker, which can
wait forever if the framework never calls back. There is no bounded-shutdown guarantee.

### Validation status

- AI-assisted independent source review of the `image-capture` crate (not a certification or formal audit)
- ONE short real-camera sample of that crate: discovery, two open / `GetDeviceInfo` / confirmed-close
  cycles with a same-process reopen, helpers untouched
- Hosted tests prove the state machine, bridge, framing and failure paths with deterministic fakes and
  the simulated Sony body behind the seam. They do not prove native memory safety, callback delivery,
  framework behavior, disconnect ordering or deployment permissions
- The thin `native` delegation is compiled only on macOS and needs a real device, so hosted coverage
  cannot execute it (see `docs/verification.md`). Its decisions (error mapping, event draining,
  conversions) are covered by macOS unit tests
- Running this adapter against a camera is a hardware action reserved to the hardware owner:

```sh
cargo build --release --features macos-icc --example icc_sample
# default path does nothing native:
target/release/examples/icc_sample
# hardware owner only; the exact device identifier stays in the environment:
TETHERCAM_ICC_DEVICE_ID=<exact identifier> target/release/examples/icc_sample --hardware-admitted
```

It performs discovery, then two cycles of open, `GetDeviceInfo` (`0x1001`) and a confirmed close, and
prints one line of counts and response codes, e.g.
`candidates=1 cycles=2 device_info_ok=2 confirmed_closes=2 response_codes=0x2001,0x2001 result=ok`.
It never prints identities or payload bytes, writes no settings, fires no shutter and runs no vendor handshake.
Exit status is 0 for a pass, 1 for a failed sample and 2 for usage errors. CI never passes the flag.

## Transaction boundary

The existing `Transport` is a USB bulk/interrupt pipe interface, not an ICC abstraction. Introduce
a session/transaction seam above it: open one selected camera, execute a serialized operation
with owned command parameters and optional outbound bytes, return owned response and inbound
data bytes, receive bounded events, and close the session. Define typed errors and preserve
response codes and transaction ordering. Bounds must cover command and response payloads,
event queues, and admission; overflow must be explicit rather than silently losing state.
Event loss or incomplete delivery must trigger an explicit resynchronization or failure policy,
not an assumption that all property changes arrived.

The raw USB implementation retains its existing PTP framing, event polling and clear-halt/reset
recovery; ICC owns its native session and sends PTP commands at transaction level. Do not invent
bulk endpoints, clear-halt, USB reset, or raw `OpenSession`/`CloseSession` commands for ICC.
Precisely map ICC command/response framing, transaction IDs, data phases and event semantics
against framework behavior before implementation; the short probe does not establish a universal
mapping. Reuse the existing Sony handshake, property parsing and operations above the seam.
Preserve Linux USB, simulator behavior, authentication, credential redaction, and the protected
CI and coverage inventory. No new capture surface bypasses existing authentication.

## Ownership and deadlines

A native owner must keep the camera, delegate, callback blocks, request and response buffers,
and its actual run loop alive on their required thread until framework completion permits release.
Native activity and caller interest are separate state: a caller deadline ends the caller's wait,
not the native operation. At most one selected session and one native operation are outstanding.
Bound admission and quarantine unresolved work; reject new work and replacement generations
while ownership is unresolved, instead of accumulating retained sessions. Distinguish admission
expiry from expiry after submission; establish retained request state before submitting, including
synchronous callback reentrancy. Match request identity and device generation, but do not skip
cleanup for stale or duplicate callbacks. Reserve completion capacity independently of the bounded
event queue. Copy callback-scoped bytes before returning; wrapper copy limits do not bound
framework-internal allocations.

- Opening: after a deadline retain the open and await its callback. If it succeeds late, schedule
  orderly close only after open completion; a failed open still settles ownership.
- Command: after a deadline retain the command until its callback completes. Only then may close
  begin. A stale callback must settle native ownership and cleanup even when its result no longer
  belongs to an interested caller.
- Closing: a close deadline retains the outstanding close. Never issue a second close or reopen
  while it is unresolved. Reuse requires confirmed completion of native work and confirmed closure.
- Disconnect and error callbacks must settle the same ownership state; do not presume a disconnect
  is completion of an unrelated outstanding operation. If completion cannot be confirmed, retain
  and quarantine it.

Shutdown joins the owner and cleanup. It can wait indefinitely if the framework never calls back;
there is no bounded-shutdown guarantee. Process termination is not a cleanup protocol. Simulation
can test state transitions and bounded admission, but cannot prove native memory safety or callback
delivery.

## Dependency and security gate

Shipped repository code remains Rust-only with `unsafe_code` forbidden. No Swift or Objective-C
sources or local unsafe exception are approved. The safe Rust dependency is not a way to evade unsafe
review: [#75](https://github.com/nredd/tethercam/issues/75) recorded the maintainer approval of
ownership and maintenance, and the published `image-capture` crate carries its own unsafe-site inventory
and native contract (AI-assisted independent source review, not a certification). That review covered
the pinned bindings and headers, callback ABI, object, block and buffer lifetimes, thread transitions,
disconnect and teardown, and any change to them (SDK, toolchain, dependency versions, unsafe sites) needs
a renewed review before hardware use. `Cargo.toml` therefore pins the reviewed `0.1.0` with `=0.1.0`, and
`deny.toml` bans every newer version, so a bump fails the gate until the review is renewed. It links only Apple system frameworks, vendors
no C, and sits behind the off-by-default `macos-icc` feature (justification in `architecture.md`). Native
objects are never marked `Send` here: the native owner has a real run loop, and the portable worker is joined.

## Evidence and validation

A separately executed external Swift feasibility probe, not shipped application code, completed
Sony handshake and decoded 185 unique frames over 20.025 seconds, then reopened in the same
process and decoded 19 frames. Two close callbacks reported success and 216 command callbacks
succeeded with Apple's helpers untouched. This is a short sample, not endurance, production support, or an unsafe audit.
It did not test real deadlines, late callbacks, disconnect, LaunchAgent, or screen lock.

| Case | Required evidence before support |
|---|---|
| Discovery, open, handshake, live view, close and same-process reopen | Actual Rust boundary with helpers untouched, repeated sessions and explicit close results. The `icc_sample` run above would cover discovery, open, `GetDeviceInfo`, confirmed close and reopen only, once the hardware owner has run it: it has not been run. Handshake and live view are not yet run on hardware |
| Deadline during open, command and close; late success/error | Deterministic callback scheduler plus real deadline/late-callback tests, ownership and quarantine verified |
| Disconnect and reconnect at each lifecycle phase | Real device tests plus scheduler tests; no overlapping replacement or assumed cleanup |
| Events, overflow and property refresh | Verify framing and event completeness; test bounded overflow and explicit resynchronization |
| Long-running live view and power | Sustained measurements, resource bounds and battery/power behavior; endurance is [#14](https://github.com/nredd/tethercam/issues/14) |
| Interactive, signed/unsigned, permissions, lock/unlock, logout and LaunchAgent | Test each applicable deployment context; do not infer service behavior from an interactive probe |
| Linux and simulator regressions | Full protected CI inventory, source coverage and both embedded cross-builds without hardware |
| Protect and Home Assistant | Separate commissioning in [#13](https://github.com/nredd/tethercam/issues/13) |

Record OS and framework versions, signing/permission context and sanitized results without camera
identifiers, private network details, credentials or images. Unknowns include callback liveness,
framing, event completeness, disconnect ordering, payload limits and deployment permissions.

## References

- [Apple ImageCaptureCore](https://developer.apple.com/documentation/imagecapturecore)
- [Apple PTP command request](https://developer.apple.com/documentation/imagecapturecore/iccameradevice/requestsendptpcommand(_:outdata:completion:))
- [Apple PTP event delegate](https://developer.apple.com/documentation/imagecapturecore/iccameradevicedelegate/cameradevice(_:didreceiveptpevent:))
- [Apple USB device capture](https://developer.apple.com/documentation/iousbhost/iousbhostobjectinitoptions/devicecapture)
- [Apple USB entitlement](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.device.usb)
- [Apple launchd jobs](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)
- [nusb device API](https://docs.rs/nusb/0.2.1/src/nusb/device.rs.html)
