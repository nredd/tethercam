# Verification infrastructure

Normal `cargo install tethercam` and `cargo run -- ...` still build/run the camera application.
One Cargo package contains an additional, off-by-default `test-support` library feature and a
`repo-tools` developer binary feature. Neither starts an export, enumerates USB or provisions a
host merely by being enabled. Cargo metadata labels their verification role; it is not a
privilege sandbox. `default-run = "tethercam"` preserves ordinary `cargo run` selection.

## Reusable USB/IP API

```toml
[dependencies]
tethercam = { version = "0.0.2", features = ["test-support"] }
tokio = { version = "1", features = ["macros", "rt", "net"] }
```

```rust,no_run
use tethercam::test_support::{Server, Synthetic};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), tethercam::test_support::Error> {
    let export = Server::start(0, Synthetic::default()).await?;
    assert!(export.address.ip().is_loopback());
    export.shutdown().await
}
```

`Error` distinguishes malformed protocol data, invalid identities, model transport failures,
I/O, deadlines and owned-task failures; `kind()` provides a payload-free retry/observation category.
`DeviceModel` supplies identity/descriptors, endpoint transfers and class/vendor controls.
`Identity` describes a single interface in configuration one, including its alternate settings
and bulk endpoint numbers. Standard USB configuration/descriptor handling, network framing,
import ownership, deadlines and admission live in the common engine, not in a Sony personality.
The private boxed I/O boundary shares that state machine across model types; it is not on the
normal camera runtime path.

`Sony` wraps the existing `SimCamera` Sony responder, including SICDD cancellation/reset/status.
`Synthetic` is a separate vendor-class echo device, vendor `ffff`, product `0001`, with OUT endpoint
4 and IN endpoint 5. It has no PTP/Sony protocol. Hosted integration imports both personalities
through Linux VHCI and uses actual `nusb` handles. The Sony path also executes the thin production
adapter; the generic echo path bypasses the Sony camera classifier.

`export.model` is the typed `Arc<Mutex<M>>` for deliberate model changes. `export.state` contains
common observations and explicit fault injection (`hold_out`, `disconnect`, pending/submitted/
unlinked counters, bounded control/error histories). Notify `export.changed` after changing data
needed by a pending request. Do not hold an observation/model lock across unrelated operations.
Await `shutdown()` for a joined cleanup barrier; dropping an export aborts its owned tasks.

Bounds:

- IPv4 loopback listener only; at most four connections and one active import
- 1 MiB transfers, 32 pending URBs and 4 MiB pending transfer memory
- 30 s header/negotiation deadline and 5 s payload, model-call, pending-URB and reply deadlines
- Persistent frame buffers/deadlines survive `select!` cancellation; no detached reader task
- 128 MiB non-refundable OUT admission credits, including zero-length submissions; read, unlink,
  reset and re-import do not refund credits
- 4096 control setup records and 64 payload-free error categories

Models are trusted Rust implementations, not sandboxed plugins. They must return promptly,
respect transfer sizes and bound their own retained state/reply generation. The shipped fixtures
have elaborately tested bounds; arbitrary downstream code cannot inherit a memory-safety or
resource-isolation claim simply from Cargo metadata.

Portable tests exercise real loopback framing, independent echo transfers, fragmented reads,
malformed requests, duplicate imports/sequences, stalls/failures, resource exhaustion, deadlines,
class cancellation, re-import and joined cleanup. Arbitrary-byte property tests cover management
headers and fixed-width import identifiers as well as URB and model-control parsing. Unit tests also inject native socket failures
without invalidating descriptors. Integration scenarios remain in `tests/`; public engine and
fixture implementation lives under `src/test_support/` and ships in the package.

## Native Linux verification

Portable protocol tests need no privilege or kernel module. VHCI is needed only to present a
virtual imported USB device to the real Linux `nusb` backend. Its `attach` and `detach` sysfs
controls are normally root-owned mode `0200`; loading `vhci_hcd` alone does not delegate them.
The imported `/dev/bus/usb/...` node also needs read/write access.

Hosted disposable CI explicitly runs:

```sh
cargo run --features repo-tools --bin tethercam-tools -- usbip prepare
# The job sets TETHERCAM_USBIP=1, then runs make test / make coverage / make all.
cargo run --features repo-tools --bin tethercam-tools -- usbip detach
```

`prepare` requires exact GitHub-hosted Linux environment markers and a non-root caller. It invokes
argv-form package installation and `modprobe` through narrowly scoped `sudo` commands. Ordinary
tests never provision the host, and Cargo/test processes never run as root. Hosted attach/detach
uses native `usbip`; node ownership changes require canonical VHCI ancestry plus the exact
synthetic serial/vendor/product tuple. Wrong-mode Sony and independent echo identities are
explicitly enumerated. Physical USB ancestry and unrelated devices are rejected, never globally
permissioned or detached. Test, Coverage and Release gates provision the same hosted prerequisites;
cleanup runs even when a CI gate fails.

Local Linux requires an *explicitly preconfigured* host: installed native `usbip` tools, a loaded
VHCI controller, delegated writable attach **and** detach controls, and access to the exact
synthetic nodes. The local command verifies that delegation, never runs `sudo`, installs packages,
loads modules, changes permissions or configures services. It is rejected when `CI` is set or
`TETHERCAM_USBIP` is not `1`. The operator/admin, not an ordinary test run, owns that setup.
The native tool is resolved below `/usr/lib/linux-tools/`; attach/detach execution is bounded by
`/usr/bin/timeout`.

```sh
TETHERCAM_USBIP=1 cargo nextest run --all-features -E 'binary(usb_emulated)'
```

Linux CI fails rather than skipping if native emulation is disabled. Physical-camera tests are
separate and remain ignored; enabling either feature does not opt into them.

## Native coverage gate

`make coverage` cleans workspace profiles, collects all-feature nextest coverage, then delegates
enforcement to **cargo-llvm-cov alone** (0.9.1 or newer):

- `--no-default-ignore-filename-regex`
- `--failure-mode any`
- `--fail-uncovered-lines 0 --fail-uncovered-functions 0 --fail-uncovered-regions 0`
- `--fail-under-lines 100 --fail-under-functions 100 --fail-under-regions 100`

Integer zero-uncovered checks avoid rounded-percent admission. Native fail-under checks also
reject empty aggregate counts. No coverage JSON/schema/inventory validator or `grcov` is used.
The utility only wires native arguments and exit status. Native malformed-profile merge failure
remains fatal; cleaning prevents stale profiles from admitting a new run.

Filters are rooted at the canonical workspace/Cargo homes and exclude only actual external
`tests/`, `examples/`, `benches/`, toolchain/dependency sources, and the existing
`src/supervisor/../../tests/` external-source alias. They do not exclude test filenames or nested
source tests. The sole local constraint remains exactly `src/transport/usb/nusb_backend.rs`,
and only when `CI` is absent and VHCI is absent. It is announced loudly; any `CI` value disables
that exception. Hosted Linux requires the adapter's full coverage too.

Toolchain: `rust-toolchain.toml` pins one exact stable release (plus the `clippy`, `rustfmt` and
`llvm-tools-preview` components and the two cross targets) for every gate, and LLVM region
accounting changes between rustc releases (e.g. rustc 1.99.1 and 1.100.0 disagree on the inline
arguments of a `log::warn!`), so an unpinned local run can pass where CI fails, or the reverse.
Use rustup, which honors the file and installs the pin on first use; there is nothing to
`rustup update`. The gate prints a `TOOLCHAIN:` line; it must match the CI log. A non-rustup
install (distro, Homebrew) ignores the file, has no `llvm-tools-preview` (cargo-llvm-cov fails
early and says so) and is unsupported for the gates, though its sysroot `rust-src` sources are
excluded like `/rustc/` ones. The native
summary lists uncovered *lines* only, so a region-only failure names no file. On any failure the
gate also writes the same filtered text report to `coverage/uncovered.txt` and prints
`grep -n -B1 '\^0' coverage/uncovered.txt`; `^0` marks an unexecuted region.

Acceptance tests run native instrumented fixture crates: uncovered ordinary source, test support
and nested source tests fail; empty/cleaned/stale data cannot silently reuse success; malformed
profiles fail merging. All-feature `make test`, including extracted-package tests, therefore needs
`cargo-llvm-cov` and the toolchain's `llvm-tools-preview` as well as `cargo-nextest`; both Test and
Coverage CI jobs provision these prerequisites explicitly. Workflow tests bind Linux jobs to explicit native provisioning and the
unskippable native adapter scenario. `make coverage-report` writes unfiltered JSON, LCOV and HTML
under `coverage/`; it does not replace the enforcement result with a custom policy.

### `macos-icc` coverage

The portable ICC layers (`src/transport/icc/`, except `native.rs`) and the backend wiring
(`src/cli/icc.rs`: factory, selection, reconnect and quarantine policy, worker, launch and its process exit;
`src/cli/cameras.rs`: the `cameras` listing and its opt-in `--describe`, whose output path runs against
scripted boundaries and never starts `ImageCaptureCore`) are fully
covered on hosted Linux through deterministic fakes, the simulated Sony body and a stand-in for the
main-thread runtime. Off macOS the real `run` entry point is a stub that reports the runtime unavailable,
so even the real launch path up to that point runs there. `native.rs` is `cfg(target_os = "macos")`, so
it is not compiled, and not in the report, on the required Linux coverage job. On macOS its error
mapping, conversions and event draining are covered by unit tests, but its delegations onto
`image-capture` (the `Boundary`/`Lease` impls and `run`) need a live `Client`, which exists only inside
`native::run` on the main thread with a camera. A local macOS `make coverage` therefore reports those
few lines as uncovered. This is a real gap, not an exclusion: no ignore regex, `coverage(off)` or other
waiver covers it. It is closed only by the hardware-owner `icc_sample` and `tethercam run` runs, see
[`macos-camera.md`](macos-camera.md). No new macOS-only source was added for runtime selection. A
deliberate no-camera `ImageCaptureCore` initialize and shutdown on a hosted macOS runner is not part of
the CI inventory yet: adding it changes the macOS job set and is coordinated with the macOS coverage job
(PR [#100](https://github.com/nredd/tethercam/pull/100)).

## Diagnostics and profiling

All maintained analysis, orchestration and diagnostic logic is Rust. There are no maintained
Python or shell implementations. `Makefile` and workflow YAML only wire commands.

The public `test_support::diagnostics` API accepts a caller-owned `Session<T: Transport>` and
returns artifacts only after `SonyCamera::run` cleanup. It supports read-only advertised-property
inspection, bounded live-view acquisition CSV with SHA-256 fingerprints, explicit still-image
size/drive/JPEG-quality changes, and one/two-still focus/capture cadence. Sony property `0xD203`
selects still-image size, not live-view resolution; no live-view-resolution control is established. Capture failures still release
both shutter stages and close the session. Still artifacts use `.bin`, not an assumed JPEG
format; numerical output includes the actual PTP object format. Nothing enumerates USB inside
this reusable API. `Operation::validate` allows bounds checking before transport acquisition.

The thin example defaults to no implicit hardware choice: pass `--simulate` or explicitly
`--native`. It preserves typed argument, diagnostic, I/O and native USB error sources.
Output directories must be new. Synthetic examples:

```sh
cargo run --features test-support --example diagnostic -- --simulate source 4 /tmp/new-source-measurement
cargo run --features test-support --example diagnostic -- --simulate capture 2 /tmp/new-capture-measurement
```

The developer binary provides:

```text
tethercam-tools prepare-clients CONFIG NEW_CLIENT_DIRECTORY
tethercam-tools verify MODE BIN CONFIG NEW_OUTPUT_DIRECTORY FFMPEG FFPROBE CURL JPEG_OR_DASH
tethercam-tools profile-summary FRAMEHASH PACKETS METRICS_OR_DASH SOURCE_CSV_OR_DASH H264
tethercam-tools soak CONFIG NEW_OUTPUT_DIRECTORY FFPROBE CURL DURATION_SECS SEGMENT_SECS GAP_MS
tethercam-tools release-tag TAG
```

Build it with `cargo build --features repo-tools --bin tethercam-tools`. `repo-tools` enables
`test-support` and optional measurement JSON support; both remain off by default.

`prepare-clients` loads the protected normal configuration, requires loopback listeners and
nonempty generated lowercase hexadecimal credentials, and writes new mode-`0600` client inputs
inside a new mode-`0700` directory. It never overwrites existing inputs or prints credentials.
FFmpeg concat/URL inputs and curl config carry credentials in files, not argv/logs. A failed
publication may leave a protected partial directory; use a fresh directory for a new attempt.

`verify` modes are `streams` (two 3 s streams), `snapshot`, `native` (both streams plus snapshot),
and `profile` (30 s framehash/H.264/packet measurements). All paths must be absolute. A JPEG path
selects the application's explicit simulator; `-` explicitly requests the configured native
camera. **Do not use `-` for hardware-free testing.** It requires independent FFmpeg/ffprobe/curl
executables, checks unauthenticated `401` and authenticated snapshot `200`, independently inspects
and fully decodes recordings with FFmpeg `-xerror`, failing on decode errors or client deadlines. It never manipulates photo
import daemons or host services.

The verifier owns its exact children, never searches for process names/PIDs, never uses shell
strings, and joins termination on failures. Media clients receive TERM, then a one-second grace
period before targeted kill/reap if they resist; client cleanup cannot indefinitely block server
shutdown. Camera/server processes never use that escalation. The native `time` process waits for a Rust controller
which owns the server. Closing the controller's stdin requests graceful server shutdown, even on
an early verifier failure. No force-kill deadline shortens camera session cleanup. Resource
accounting includes the small controller and its waited-for server, **not** the separate media
clients; compare measurements only with matching accounting scope. BSD byte RSS and GNU KiB RSS
are normalized explicitly.

`soak` samples an already running service for a long run and is documented in [`soak.md`](soak.md).
It reuses `prepare-clients`' protected inputs, but accepts a wildcard listener (reached through loopback)
or a configured host address, which `verify` refuses.

`profile-summary` consumes aggregate artifacts only and emits numerical JSON. Image SHA is CSV
field 5 (zero-based), never trailing PRFT side-data hashes. It reports output and distinct-
consecutive FPS, repeated-frame percentage, payload rates, nearest-rank-style percentiles,
population jitter and optional CPU/RSS/source-fetch measurements. Complete acquisition CSV also
retains source-only duration, FPS, distinct-consecutive FPS and JPEG byte/payload rates. Missing optional inputs use
`-` and produce `null`, not invented measurements. It validates finite scalars, monotonic times,
nonempty records and typed CSV/JSON structure. Each aggregate input must be a regular file,
not a FIFO, device, directory or final-component symlink. On Linux/macOS it opens with native
nonblocking/no-follow flags and validates the opened handle, not a path preflight; unsupported
platforms fail closed rather than risk a blocking special-file open. A 32 MiB cap per aggregate
input is enforced during reading, including file growth after metadata inspection. Text inputs
also allow at most 100,000 lines (including headers/comments), 16 KiB per line and 64 CSV columns;
packet JSON and statistics allow at most 100,000 records. These generous bounds cover roughly
55 minutes at 30 FPS; ordinary 30-second profiles are far smaller. Tests include real FIFOs on
every aggregate path, opened-handle/path replacement, growing inputs and exact/over-limit cases.
The H.264 input is inspected for **file metadata length only**, never opened/read as media; the
aggregate read limits do not cause media payload access. Absolute glass-to-glass latency is explicitly not
measured. Keep all generated configuration, client inputs, measurements and media private.

## Package verification

Changes to packaging/features must validate a real extracted `cargo package` archive, not just
repository metadata: normal default application selection/build, all-feature sources/tests, and
a separate downstream crate importing `test_support` with that feature enabled. The package
includes the reusable engine, personalities, scenarios and synthetic fixture sources. Only the
repository-policy test and the development `rust-toolchain.toml` pin are excluded; their
workflow/ruleset inputs are intentionally unpublished.

## Toolchain pin

`rust-toolchain.toml` is the single source of truth for the compiler. Every workflow job that
runs cargo, including `deny` and the release workflow, installs that exact `channel` through its one
`dtolnay/rust-toolchain` step (never `stable`). With the file present, rustup would fetch any
listed component or cross target missing from the job on its first `cargo` call (seen in the `fmt`
job log as a second "downloading 4 components"), so each step lists exactly the file's
`components` and `targets`. That costs a few extra downloads in jobs that need less, and makes
every job fetch the same set up front. `tests/repository_policy.rs` fails when a step's
`toolchain`, `components` or `targets` differ from the file, when a job has a second toolchain
setup action, or when a workflow step or Makefile recipe selects a toolchain by hand
(`cargo +x`, `rustup`, `RUSTUP_TOOLCHAIN`). `rust-version` in `Cargo.toml` is the MSRV, not the
pin. The `test.yml` `msrv` job installs it, installs no extra components, and sets
`RUSTUP_TOOLCHAIN` to it, since rustup would otherwise let the file override the action's
default; its log prints `rustc --version --verbose`.

To bump the pin, open a deliberate PR that changes `channel` in `rust-toolchain.toml` and the
`toolchain:` value in each workflow (plus any changed `components` or `targets`) in the same commit, runs the full gate (`make all`) and
reads the Linux `coverage` job: region accounting can change and expose gaps that need tests, not
exclusions. The `release.yml` pin is unexecuted until a tag runs it.
