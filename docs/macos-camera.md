# Cooperative macOS camera acquisition

This is a proposed contract, not a shipped backend or production support. The work is tracked
by [#72](https://github.com/nredd/tethercam/issues/72); this specification does not close it.
ImageCaptureCore (ICC) provides a cooperative camera session and PTP command interface while
Apple's camera helpers remain running. Raw USB interface claiming can conflict with those
helpers. Killing or disabling them, installing a daemon, elevating capture privileges or using
whole-device capture is not the proposed solution.

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
sources or local unsafe exception are approved. A separate safe Rust dependency is a proposal,
not an approval or a way to evade unsafe review. [#75](https://github.com/nredd/tethercam/issues/75)
owns the maintainer approval of ownership and maintenance, independent unsafe audit, and actual
safe-library validation. Its [approval-ready design](https://github.com/nredd/tethercam/issues/75#issuecomment-6842174382)
is not implementation or publication authorization. The audit must cover pinned bindings and headers, callback ABI, object,
block and buffer lifetimes, thread transitions, disconnect and teardown. Native linking also needs
the existing written architecture justification and `deny.toml` exception, behind an experimental
feature off by default. Do not unsafely mark native objects `Send`; the native owner needs a real
run loop, while any portable worker must be joined.

## Evidence and validation

A separately executed external Swift feasibility probe, not shipped application code, completed
Sony handshake and decoded 185 unique frames over 20.025 seconds, then reopened in the same
process and decoded 19 frames. Two close callbacks reported success and 216 command callbacks
succeeded with Apple's helpers untouched. This is a short sample, not endurance, production support, or an unsafe audit.
It did not test real deadlines, late callbacks, disconnect, LaunchAgent, or screen lock.

| Case | Required evidence before support |
|---|---|
| Discovery, open, handshake, live view, close and same-process reopen | Actual Rust boundary with helpers untouched, repeated sessions and explicit close results |
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
