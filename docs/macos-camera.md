# Cooperative macOS camera acquisition

The experimental `macos-icc` Cargo feature (off by default, macOS-only native code) implements this
contract in `transport::icc`. It is not production support. A runtime selects it only through
`camera_backend = "icc"` (see [Runtime selection](#runtime-selection-camera_backend--icc)); commissioning
and the remaining hardware validation are tracked in [#77](https://github.com/nredd/tethercam/issues/77)
and [#72](https://github.com/nredd/tethercam/issues/72); this document does not close either.
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
  the owner may still complete it, so `close` (not a second `open`) confirms closure if a lease exists. If none does, nothing was confirmed: `close` reports `IccError::NotOpen` and the session is retired. `close` on a retired session is `NotOpen` too (terminal; a retry cannot help), and only `open` probes for reuse, which stays `Busy` until native cleanup settles. A late open success is closed natively after it completes.
- Failure categories are sanitized statics: permission, busy, removal, admission, deadline, malformed
  completion, event overflow, malformed event, cleanup failure, quarantine
- Failures fault, retire or quarantine the session. A confirming close that fails while `open` is
  discarding an access restriction follows the same mapping as `close`: a native failure of the close
  is `native cleanup failed` and quarantines the session, and a close that submitted nothing (busy,
  admission expired) retires it. A faulted session refuses transactions but `close`
  still confirms closure. A quarantined session (native quarantine, removal, close failure, owner gone)
  accepts nothing and never reopens
- Events are advisory and bounded by the boundary (64), and the boundary has ONE process-wide queue.
  When a session opens, everything queued before it (stale PTP events, a previous native error or access
  restriction, the overflow flag) is discarded so it cannot fault or feed the new session. Removal is not
  lost by this: the native owner keeps a sticky, process-lifetime removal flag that quarantines
  independently of the queue (`driver.rs` `disconnected`): the next transaction, close or open reports it,
  but a consumer that only polls events will not see a removal discarded here until then. Events that
  arrive in the instant between open completion and the discard are lost too, so the guarantee is
  conditional on the caller resynchronizing after open; the Sony handshake re-reads the property table.
  Afterwards `poll_event` fails closed. Overflow, access restriction, a native error observation, an
  oversized event and any malformed, truncated or oversized PTP event fault the session, so the Sony
  layer must close and reconnect to resynchronize. Removal quarantines it. Only benign observations are skipped

Native code runs on the main thread (`run` blocks it in the run loop) and joins one worker, which can
wait forever if the framework never calls back. There is no bounded-shutdown guarantee.

## Runtime selection (`camera_backend = "icc"`)

```toml
# config.toml (synthetic values elsewhere; only this key is ICC-specific)
camera_backend = "icc"   # "usb" (the default) or "icc"; never an automatic fallback
# camera_id = "..."      # optional exact device, from `tethercam cameras` (below)
```

```sh
cargo build --release --features macos-icc
target/release/tethercam run --config ./camera.toml
```

- Needs macOS, a build with `macos-icc` (Rust 1.99 or newer) and the process's one native runtime.
  Anywhere else `probe`, `doctor` and `run` fail at once with `camera_backend `icc` is unavailable`,
  before any camera access, listener or enumeration. If the native runtime fails *after* the
  application started (e.g. its worker panicked), the error is `camera_backend `icc` failed after it
  started` instead, so a platform or build is not blamed for a run that happened. `--simulate` bypasses the backend entirely and
  never starts native code. `setup` and `service` never read the backend
- Selection: see [Listing and selecting cameras](#listing-and-selecting-cameras). With `camera_id` unset,
  discovery must return exactly one camera, and that camera is used. Zero cameras waits like USB does;
  several are refused (a warning with the count only, and an error that names `tethercam cameras`), never
  "the first". `camera_serial` is a raw-USB identity, so it is rejected with `icc` instead of ignored.
  Any other device ICC lists as a camera (e.g. a phone on a charging cable, not verified) counts as a
  candidate. With the Sony body also attached that is two candidates, refused as ambiguous: select the
  camera with `camera_id`, or disconnect the other device. Alone it is the sole candidate and is opened,
  then refused at the Sony handshake (`camera does not advertise the Sony SDIO extension`), which can
  raise a trust or access prompt on that device. A refused handshake is retried every 60 s, not every
  backoff cycle (5 s cap), so such a device is reopened at most once a minute for as long as it stays
  attached. A Sony body outside PC Remote mode is refused the same way and is picked up within a minute
  of switching mode. With `camera_id` set to the camera, a phone is never opened at all, so none of this
  applies to it
- Threading: `ImageCaptureCore` needs the main thread's run loop and one native runtime per process. The
  main thread therefore runs `transport::icc::run` and the whole existing application (camera
  supervisor, RTSP, ONVIF, discovery, signal handling, graceful shutdown) runs as that function's
  joined worker, on its own current-thread tokio runtime. The worker installs the tracing subscriber
  and writes to the process stdout, because the main thread is parked in the run loop
- Readiness follows the ordinary waiting model: the process comes up at once with RTSP and ONVIF bound
  and answering "not ready" (`GET /tethercam/status` reports `waiting`), and the stream becomes ready
  when the ICC session delivers its first live-view frame. No resolution is advertised before it
- Foreground and the installed per-user LaunchAgent run the same command (`run --config`), so the
  service templates are unchanged: select the backend in the service's config file
- Shutdown stays joined: SIGINT or SIGTERM stops the supervisor, the Sony session is closed with
  confirmation, the bridge handle is dropped and the native runtime returns. A timeout is never
  cancellation, and nothing is killed, unloaded or bypassed. If the framework never calls back, that
  join waits forever (below)
- Reconnect: the supervisor's reconnect loop creates new sessions on the same runtime. After a confirmed
  close the next session reopens on it. A caller deadline retires the session, and the next attempt
  reports busy (with backoff) until native cleanup settles. That wait is bounded: a native command
  can outlive its caller deadline forever if the framework never completes it (a camera hung or
  unplugged mid-command), and discovery would then be answered busy indefinitely, with no quarantine
  and no restart. Busy answers in an unbroken run for 300 s are treated like a quarantine: the factory
  logs `icc_busy_limit_exceeded`, refuses further native work and the same orderly exit with status 75
  follows. Nothing is cancelled, killed or bypassed. 300 s is the longest wait this process asks of
  the native side, the Sony driver's 300 s still download (the 60 s default `transaction_timeout` and
  the 30 s default discovery wait are shorter): a command retired at its deadline gets as long again to
  settle, so a slow but progressing cleanup is not restarted, and a completion that never arrives
  costs five minutes of `waiting`. It is a wall clock, not a retry count, because the supervisor's
  backoff (100 ms doubling to 5 s) decides how many attempts fit. Any non-busy outcome ends the run:
  found, not found, a refusal or any other error. Not validated on
  hardware: whether `ImageCaptureCore` really withholds a completion after a removal
- Quarantine is terminal for the process. A camera removal (a pulled cable, power loss), a failed close or
  a lost owner quarantines the runtime for the life of the process: the native layer can never be reused
  and its run loop can then never return. The factory refuses further native work, the application shuts
  down in order (RTSP and ONVIF stop, the session close is attempted) and the process exits with status
  75 after printing `native camera runtime quarantined`. The LaunchAgent's `KeepAlive` restarts it after
  its ten-second throttle. This `exit` is the one deliberate departure from joined shutdown, taken only
  once the native owner has declared its state unresolvable. It is not validated on hardware
- Sony protocol generation: a non-OK response to the 3.00 request falls back to 2.00 on the same session,
  as over USB. A native failure of that request faults the session but leaves it closable, so the driver
  closes with confirmation, reopens, replays the handshake and asks for 2.00. A caller *deadline* is not
  a refusal: native work is still running, the retired session may not be reused, and the connect fails
  until native cleanup settles. A body that needs 2.00 first and stalls the newer request that way
  belongs in the model quirks, like the a7 III, which starts at 2.00
- Access restriction: the open-time event drain keeps the *state* of what it drains. If the last restriction event before
  the session opened says access is restricted, `open` closes the lease with confirmation and fails as
  `camera access restricted` (the permission category) instead of surfacing later as a generic native
  failure. Limit: a restriction event consumed earlier (by an event poll of a previous session or while no
  session was open) is not remembered, so the next open can still succeed and the restriction surface
  as a native failure then

Not validated on hardware, and not claimed: listing and exact selection (including whether the identifier
survives replug, reboot or a camera power-cycle), a live Sony stream over ICC, the LaunchAgent context
(GUI-session domain, camera consent, signing), screen lock and unlock, unplug and replug, deadlines and
late callbacks, and any endurance. A quarantine cannot be recovered in process, and a missing native
callback can block shutdown indefinitely. The experimental label stands.

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
- Running this adapter against a camera is a hardware action reserved to the hardware owner. The
  probe below drives the production adapter, and `tethercam run` with `camera_backend = "icc"` is the
  end-to-end acceptance:

```sh
cargo build --release --features macos-icc --example icc_sample
# default path does nothing native:
target/release/examples/icc_sample
# hardware owner only. Either select the camera only if discovery finds exactly one, so the identifier
# is never handled (zero or several candidates fail with exit 1 and counts only, never "the first"):
target/release/examples/icc_sample --hardware-admitted --sole-candidate
# or name it exactly, the identifier `tethercam cameras` prints and `camera_id` holds, same selection as
# the service; it stays in the environment (mutually exclusive with --sole-candidate):
TETHERCAM_ICC_DEVICE_ID=<exact identifier> target/release/examples/icc_sample --hardware-admitted
```

It performs discovery, then two cycles of open, `GetDeviceInfo` (`0x1001`) and a confirmed close, and
prints one line of counts and response codes, e.g.
`candidates=1 cycles=2 device_info_ok=2 confirmed_closes=2 response_codes=0x2001,0x2001 result=ok`.
It never prints identities or payload bytes, writes no settings, fires no shutter and runs no vendor handshake.
Exit status is 0 for a pass, 1 for a failed sample and 2 for usage errors. CI never passes the flag.

Add `--live-view` to run the live-view probe instead (`transport::icc::live_view_probe`): the production
stack, `IccSession` under the Sony driver, for two cycles of handshake, live-view readiness, thirty
decoded frames and a confirmed close, reopening on the same runtime. It does what the Sony live-view
handshake does (it takes host priority) and fires no shutter. It prints counts and image dimensions only,
e.g. `candidates=1 cycles=2 frames=60 size=640x424 size_changes=0 confirmed_closes=2 result=ok`:

```sh
target/release/examples/icc_sample --hardware-admitted --sole-candidate --live-view
```

## Listing and selecting cameras

```sh
tethercam cameras --config ./camera.toml
tethercam cameras --config ./camera.toml --describe
```

Discovery returns only opaque identifiers: no name, vendor or type, and no way to get one without opening
a session. So the listing prints one line per device, an index and the exact identifier (synthetic here):

```
Cameras found: 2
1  0F6A2C1E-0000-4000-8000-000000000001
2  0F6A2C1E-0000-4000-8000-000000000002
Set camera_id in the configuration to one of these identifiers; --describe opens each device briefly to show its make and model, and a phone can show a trust prompt.
```

- The identifier is printed ONLY by this command, to the terminal of whoever ran it. It is never logged, put
  in tracing, status output, errors or CI output. The configuration file already holds credentials and is
  owner-only, which is where it belongs. The index is for reading the listing only; the configuration takes
  the identifier. It is not a stable position
- `--describe` is an explicit opt-in. Each listed device is opened read-only, asked the standard PTP
  `GetDeviceInfo` (`0x1001`) and closed with confirmation, and its manufacturer and model are printed
  (`2  <identifier>  Synthetic Maker Model X`). The serial number in that dataset is never printed. Opening
  a device can raise a trust prompt on it, which is why a phone is only touched when asked. A device that
  refuses or fails is printed as `not a supported camera (<category>)` with `access restricted`, `timed out`,
  `refused`, `failed` or `no device information`, and the listing goes on. Make and model are cleaned of
  control characters and cut to 64 characters. With `camera_id` set only the selected device is described
  (the others show `not described (not selected by camera_id)` and are never opened), as the service
  ignores them; unset the key to describe them all
- Bounded: at most 64 devices (the native limit, more is an error), and 10 s per step (open,
  `GetDeviceInfo`, close) per device. A deadline or failed close leaves native cleanup unsettled, so the
  remaining devices are listed as `not described (native cleanup pending)` rather than opened into a busy
  refusal. A stop (SIGINT, SIGTERM) is honoured between devices, never mid-command. A quarantine ends the
  command as `native camera runtime quarantined` and exit status 75, like every other command
- Without `--describe` nothing is opened, so listing never touches a phone
- With `camera_id` set, the listing marks that device `(selected by camera_id)`, or says it matches none
- On `camera_backend = "usb"` it prints the same counts and modes as `probe`, and a note that raw USB
  selects by `camera_serial`; serial numbers are not printed and `--describe` is ignored. Listing USB
  serials is out of scope. With `icc` on an unsupported platform or build the command fails like the others,
  with `camera_backend `icc` is unavailable`, before any camera access

Selecting one, in the configuration:

```toml
camera_backend = "icc"
camera_id = "0F6A2C1E-0000-4000-8000-000000000002"   # exact, as printed by `tethercam cameras`
```

| Situation | `camera_id` unset | `camera_id` set |
| --- | --- | --- |
| nothing attached | waits (`waiting`, not ready) | waits |
| only the selected device | used | used |
| several, including the selected one | refused, count only, names `tethercam cameras` | the selected one is used; the others are never opened |
| several, not the selected one | refused, as above | waits; never falls back to another device |
| exactly one other device | that device is used | waits; the other device is never opened |

- `camera_id` is rejected with `camera_backend = "usb"` (as `camera_serial` is with `icc`), and when empty,
  padded with whitespace, over 1024 bytes or containing control characters. These are config validation
  errors raised before any camera access, and they never print the value. An existing configuration
  without the key keeps working. `setup` does not write or require it
- The same selection serves `run`, `probe` and `doctor`, and the gated `icc_sample` example (exact
  identifier in `TETHERCAM_ICC_DEVICE_ID`, or `--sole-candidate`), which share one `Selector` and its choice logic
- Stability of the identifier is NOT established. `image-capture` documents it only as the exact
  framework identifier and takes it from the camera object's `UUIDString`; Apple documents that as "a
  string representation of the Universally Unique ID of the device" and promises nothing about it across
  replug, reboot or camera power-cycle (a separate `persistentIDString` property exists, which this crate
  does not expose). If it changes on replug, `camera_id` keeps matching nothing and the service waits
  forever after the camera comes back, and exact selection by it is only fit for one attachment. This must
  be checked on hardware (list, replug, list; power-cycle the camera, list; reboot, list) before
  relying on it. Names would help distinguish devices, and are requested upstream
  ([image-capture#8](https://github.com/nredd/image-capture/issues/8)); the configuration format does not
  need to change when they arrive

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
