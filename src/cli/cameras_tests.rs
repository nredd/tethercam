//! The `cameras` listing through scripted boundaries. No native code runs, and a device is only
//! ever opened when `--describe` asked for it.
//!
//! References:
//! - `docs/macos-camera.md`, Listing and selecting cameras

use super::*;
use crate::config::Secret;
use crate::ptp::session::code;
use crate::supervisor::OpenError;
use crate::supervisor::testing::LogCapture;
use crate::transport::icc::IccReply;
use crate::transport::icc::fake::{Call, Fake, Script, Shared, Step, ok_reply};
use crate::transport::icc::run_with;

const A: &str = "synthetic-secret-a";
const B: &str = "synthetic-secret-b";
const C: &str = "synthetic-secret-c";
const SERIAL: &str = "SYNTHETIC-DEVICE-SERIAL";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn device(name: &str) -> IccDeviceId {
    IccDeviceId::new(name.into()).unwrap()
}

fn script(shared: &Shared) -> std::sync::MutexGuard<'_, Script> {
    shared.lock().unwrap()
}

/// A `GetDeviceInfo` data phase carrying `manufacturer`, `model` and a serial that must never
/// be printed.
fn info(manufacturer: &str, model: &str) -> Step {
    let dataset = DeviceInfo {
        manufacturer: manufacturer.into(),
        model: model.into(),
        serial_number: SERIAL.into(),
        ..DeviceInfo::default()
    };
    Step::Done(Ok(IccReply {
        data: dataset.to_bytes().unwrap(),
        ..ok_reply()
    }))
}

fn refusal() -> Step {
    Step::Done(Ok(IccReply {
        code: code::OPERATION_NOT_SUPPORTED,
        ..ok_reply()
    }))
}

fn scripted(attached: &[&str], setup: impl FnOnce(&mut Script)) -> (Fake, Shared) {
    let (fake, shared) = Fake::new();
    {
        let mut script = script(&shared);
        script
            .discover
            .push_back(Ok(attached.iter().map(|name| device(name)).collect()));
        setup(&mut script);
    }
    (fake, shared)
}

fn config(camera_id: Option<&str>) -> Config {
    let mut cfg = Config::generate().unwrap();
    cfg.camera_backend = crate::config::CameraBackend::Icc;
    cfg.camera_id = camera_id.map(|id| Secret::new(id.into()));
    cfg
}

/// What a listing printed, how it ended and what it logged.
struct Listing {
    result: Result<(), Error>,
    out: String,
    logs: String,
}

fn listing_with(
    fake: Fake, cfg: &Config, describe: bool, stopped: bool, out: &mut (dyn Write + Send),
) -> (Result<(), Error>, String) {
    let capture = LogCapture::new();
    let dispatch = capture.dispatch();
    let result = run_with(fake, |handle| {
        tracing::dispatcher::with_default(&dispatch, || {
            runtime().block_on(async {
                let stop: Pin<Box<dyn Future<Output = ()>>> = if stopped {
                    Box::pin(std::future::ready(()))
                } else {
                    Box::pin(std::future::pending())
                };
                list(handle, cfg, describe, out, stop).await
            })
        })
    })
    .unwrap();
    (result, capture.text())
}

fn run(fake: Fake, cfg: &Config, describe: bool, stopped: bool) -> Listing {
    let mut out = Vec::new();
    let (result, logs) = listing_with(fake, cfg, describe, stopped, &mut out);
    Listing {
        result,
        out: String::from_utf8(out).unwrap(),
        logs,
    }
}

/// The listing is the one sink for identifiers: nothing else may carry them or device text.
fn assert_quiet(listing: &Listing) {
    for secret in ["synthetic-secret", SERIAL, "Synthetic Maker"] {
        assert!(!listing.logs.contains(secret), "{}", listing.logs);
        if let Err(error) = &listing.result {
            let shown = format!("{error} {error:?}");
            assert!(!shown.contains(secret), "{shown}");
        }
    }
    assert!(!listing.out.contains(SERIAL), "{}", listing.out);
}

fn closes(shared: &Shared) -> usize {
    script(shared)
        .calls
        .iter()
        .filter(|call| **call == Call::Close)
        .count()
}

fn dropped(shared: &Shared) -> bool {
    script(shared).calls.contains(&Call::LeaseDropped)
}

#[test]
fn no_camera_lists_none_and_opens_nothing() {
    let (fake, shared) = scripted(&[], |_| {});
    let listing = run(fake, &config(None), true, false);
    listing.result.as_ref().unwrap();
    assert_eq!(listing.out, "Cameras found: 0\n");
    assert_eq!(script(&shared).calls, [Call::Discover]);
    assert_quiet(&listing);
}

#[test]
fn a_plain_listing_prints_identifiers_with_indexes_and_opens_nothing() {
    let (fake, shared) = scripted(&[A, B, C], |_| {});
    let listing = run(fake, &config(None), false, false);
    listing.result.as_ref().unwrap();
    let lines: Vec<&str> = listing.out.lines().collect();
    assert_eq!(
        &lines[..4],
        [
            "Cameras found: 3",
            "1  synthetic-secret-a",
            "2  synthetic-secret-b",
            "3  synthetic-secret-c",
        ]
    );
    assert!(
        lines[4].starts_with("Set camera_id in the configuration"),
        "{lines:?}"
    );
    assert!(lines[4].contains("--describe") && lines[4].contains("trust prompt"));
    assert_eq!(lines.len(), 5);
    // Without `--describe` no device is opened, so a phone is never prompted.
    assert_eq!(script(&shared).calls, [Call::Discover]);
    // Identifiers go to the user's output only: no log line carries one.
    assert!(
        !listing.logs.contains("synthetic-secret"),
        "{}",
        listing.logs
    );
}

#[test]
fn the_selected_camera_is_marked_and_a_missing_one_is_called_out() {
    let (fake, shared) = scripted(&[A, B, C], |_| {});
    let listing = run(fake, &config(Some(B)), false, false);
    listing.result.as_ref().unwrap();
    assert!(
        listing
            .out
            .contains("2  synthetic-secret-b  (selected by camera_id)\n"),
        "{}",
        listing.out
    );
    assert_eq!(listing.out.matches("selected by").count(), 1);
    assert!(!listing.out.contains("Set camera_id"), "{}", listing.out);
    assert_eq!(script(&shared).calls, [Call::Discover]);

    let (fake, shared) = scripted(&[A, C], |_| {});
    let listing = run(fake, &config(Some(B)), false, false);
    listing.result.as_ref().unwrap();
    assert!(
        listing.out.contains("camera_id is set but matches none"),
        "{}",
        listing.out
    );
    assert!(listing.out.contains("never falls back"), "{}", listing.out);
    assert!(!listing.out.contains("selected by"));
    assert_eq!(script(&shared).calls, [Call::Discover]);

    // A key with nothing attached says the same.
    let (fake, _) = scripted(&[], |_| {});
    let listing = run(fake, &config(Some(B)), false, false);
    assert!(
        listing.out.contains("camera_id is set but matches none"),
        "{}",
        listing.out
    );
}

#[test]
fn describe_prints_make_and_model_never_the_serial_and_confirms_every_close() {
    let (fake, shared) = scripted(&[A, B], |s| {
        s.transact.push_back(info("Synthetic Maker", "Model One"));
        s.transact.push_back(info("Synthetic Maker", "Model Two"));
    });
    let listing = run(fake, &config(Some(B)), true, false);
    listing.result.as_ref().unwrap();
    assert_eq!(
        listing.out.lines().collect::<Vec<_>>(),
        [
            "Cameras found: 2",
            "1  synthetic-secret-a  Synthetic Maker Model One",
            "2  synthetic-secret-b  Synthetic Maker Model Two  (selected by camera_id)",
        ]
    );
    assert_eq!(script(&shared).opened, [device(A), device(B)]);
    let operations: Vec<u16> = script(&shared)
        .calls
        .iter()
        .filter_map(|call| match call {
            Call::Transact { operation, .. } => Some(*operation),
            _ => None,
        })
        .collect();
    // Read-only and standard: GetDeviceInfo and nothing else, once per device.
    assert_eq!(operations, [code::GET_DEVICE_INFO, code::GET_DEVICE_INFO]);
    assert_eq!(closes(&shared), 2);
    assert!(!dropped(&shared));
    assert_eq!(script(&shared).live_leases, 0);
    assert_quiet(&listing);
    // With the descriptions shown there is no hint to add them.
    assert!(!listing.out.contains("--describe"));
}

#[test]
fn a_refusing_or_failing_device_is_reported_and_the_listing_goes_on() {
    let (fake, shared) = scripted(&[A, B, C, "synthetic-secret-d"], |s| {
        s.open.push_back(Err(IccError::Permission));
        s.transact.push_back(refusal());
        s.transact
            .push_back(Step::Done(Err(IccError::NativeFailure)));
        s.transact.push_back(info("Synthetic Maker", "Model Four"));
    });
    let listing = run(fake, &config(None), true, false);
    listing.result.as_ref().unwrap();
    assert_eq!(
        listing.out.lines().take(5).collect::<Vec<_>>(),
        [
            "Cameras found: 4",
            "1  synthetic-secret-a  not a supported camera (access restricted)",
            "2  synthetic-secret-b  not a supported camera (refused)",
            "3  synthetic-secret-c  not a supported camera (failed)",
            "4  synthetic-secret-d  Synthetic Maker Model Four",
        ]
    );
    // The failed open held nothing, and the three opened sessions each closed with confirmation.
    assert_eq!(script(&shared).opened.len(), 4);
    assert_eq!(closes(&shared), 3);
    assert!(!dropped(&shared));
    assert_eq!(script(&shared).live_leases, 0);
    assert_quiet(&listing);
}

#[test]
fn a_deadline_leaves_cleanup_unsettled_so_the_rest_are_listed_but_not_opened() {
    // A deadline on the command retires the session: nothing to close, and a new one would only
    // be refused as busy.
    let (fake, shared) = scripted(&[A, B, C], |s| {
        s.transact.push_back(info("Synthetic Maker", "Model One"));
        s.transact
            .push_back(Step::Done(Err(IccError::DeadlineExceeded)));
    });
    let listing = run(fake, &config(None), true, false);
    listing.result.as_ref().unwrap();
    assert_eq!(
        listing.out.lines().take(4).collect::<Vec<_>>(),
        [
            "Cameras found: 3",
            "1  synthetic-secret-a  Synthetic Maker Model One",
            "2  synthetic-secret-b  not a supported camera (timed out)",
            "3  synthetic-secret-c  not described (native cleanup pending)",
        ]
    );
    assert_eq!(script(&shared).opened, [device(A), device(B)]);
    assert_quiet(&listing);

    // The same for a deadline while opening.
    let (fake, shared) = scripted(&[A, B], |s| {
        s.open.push_back(Err(IccError::DeadlineExceeded));
    });
    let listing = run(fake, &config(None), true, false);
    listing.result.as_ref().unwrap();
    assert!(
        listing
            .out
            .contains("1  synthetic-secret-a  not a supported camera (timed out)")
    );
    assert!(
        listing
            .out
            .contains("2  synthetic-secret-b  not described (native cleanup pending)")
    );
    assert_eq!(script(&shared).opened, [device(A)]);

    // Not describing at all: the stop reason only shows next to descriptions.
    assert_eq!(skipped(false, "x"), None);
}

#[test]
fn a_failed_close_quarantines_and_ends_the_command_as_quarantined() {
    let (fake, shared) = scripted(&[A, B], |s| {
        s.transact.push_back(info("Synthetic Maker", "Model One"));
        s.close.push_back(Err(IccError::CleanupFailure));
    });
    let listing = run(fake, &config(None), true, false);
    assert!(
        matches!(listing.result, Err(Error::Quarantined)),
        "{:?}",
        listing.result
    );
    // What was learned is still printed, and the rest is listed, not opened.
    assert!(
        listing
            .out
            .contains("1  synthetic-secret-a  Synthetic Maker Model One")
    );
    assert!(
        listing
            .out
            .contains("2  synthetic-secret-b  not described (native runtime quarantined)")
    );
    assert_eq!(script(&shared).opened, [device(A)]);
    assert_quiet(&listing);

    // A removal while opening quarantines too.
    let (fake, _) = scripted(&[A], |s| s.open.push_back(Err(IccError::Removed)));
    let listing = run(fake, &config(None), true, false);
    assert!(matches!(listing.result, Err(Error::Quarantined)));
    assert!(
        listing
            .out
            .contains("1  synthetic-secret-a  not a supported camera (failed)")
    );
}

#[test]
fn unusable_device_information_is_not_a_supported_camera() {
    let empty = DeviceInfo::default().to_bytes().unwrap();
    let (fake, shared) = scripted(&[A, B, C], |s| {
        s.transact.push_back(Step::Done(Ok(IccReply {
            data: vec![1, 2, 3],
            ..ok_reply()
        })));
        s.transact.push_back(Step::Done(Ok(IccReply {
            data: empty,
            ..ok_reply()
        })));
        s.transact.push_back(Step::Done(Ok(ok_reply())));
    });
    let listing = run(fake, &config(None), true, false);
    listing.result.as_ref().unwrap();
    assert_eq!(
        listing
            .out
            .matches("not a supported camera (no device information)")
            .count(),
        3
    );
    assert_eq!(closes(&shared), 3);
    assert_quiet(&listing);
}

#[test]
fn device_text_is_cleaned_before_it_reaches_the_terminal() {
    assert_eq!(clean("  Model \u{1b}[31mX\u{7}  "), "Model ?[31mX?");
    assert_eq!(clean(&"y".repeat(500)).len(), MAX_LABEL);
    assert_eq!(clean("\t"), "?");
    assert_eq!(
        label(&info_bytes("Maker", "")),
        Description::Camera("Maker".into())
    );
    assert_eq!(
        label(&info_bytes("", "Model")),
        Description::Camera("Model".into())
    );
    assert_eq!(
        label(&info_bytes(" ", "")),
        Description::Unsupported("no device information")
    );
}

fn info_bytes(manufacturer: &str, model: &str) -> Vec<u8> {
    DeviceInfo {
        manufacturer: manufacturer.into(),
        model: model.into(),
        ..DeviceInfo::default()
    }
    .to_bytes()
    .unwrap()
}

#[test]
fn failure_categories_are_static_and_name_nothing() {
    for (error, expected) in [
        (SessionError::Icc(IccError::Permission), "access restricted"),
        (SessionError::Icc(IccError::DeadlineExceeded), "timed out"),
        (SessionError::Icc(IccError::AdmissionExpired), "timed out"),
        (SessionError::DeadlineExceeded(Duration::ZERO), "timed out"),
        (
            SessionError::Response {
                code: 0x2005,
                params: Vec::new(),
            },
            "refused",
        ),
        (SessionError::Icc(IccError::NativeFailure), "failed"),
        (SessionError::NotOpen, "failed"),
    ] {
        assert_eq!(category(&error), expected, "{error:?}");
    }
}

#[test]
fn a_stop_is_honoured_between_devices_and_never_mid_command() {
    let (fake, shared) = scripted(&[A, B], |_| {});
    let listing = run(fake, &config(None), true, true);
    listing.result.as_ref().unwrap();
    assert!(
        listing
            .out
            .contains("1  synthetic-secret-a  not described (stopped)"),
        "{}",
        listing.out
    );
    assert!(
        listing
            .out
            .contains("2  synthetic-secret-b  not described (stopped)")
    );
    assert_eq!(script(&shared).calls, [Call::Discover]);

    // A plain listing has no waits, so a stop changes nothing.
    let (fake, _) = scripted(&[A], |_| {});
    let listing = run(fake, &config(None), false, true);
    assert!(
        listing.out.contains("1  synthetic-secret-a\n"),
        "{}",
        listing.out
    );
}

#[test]
fn discovery_failures_are_sanitized_categories() {
    for (error, expected) in [
        (IccError::Permission, OpenError::Permission),
        (IccError::Busy, OpenError::Busy),
        (IccError::NativeFailure, OpenError::Discovery),
    ] {
        let (fake, shared) = Fake::new();
        script(&shared).discover.push_back(Err(error));
        let listing = run(fake, &config(None), true, false);
        assert!(matches!(listing.result, Err(Error::Open(found)) if found == expected));
        assert_eq!(listing.out, "");
        assert_quiet(&listing);
    }
    let (fake, shared) = Fake::new();
    script(&shared).discover.push_back(Err(IccError::Removed));
    let listing = run(fake, &config(None), false, false);
    assert!(matches!(listing.result, Err(Error::Quarantined)));
}

#[test]
fn the_listing_is_bounded_to_the_native_device_limit() {
    let names: Vec<String> = (0..=MAX_DEVICES)
        .map(|n| format!("synthetic-secret-{n}"))
        .collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let (fake, shared) = scripted(&refs, |_| {});
    let listing = run(fake, &config(None), true, false);
    assert!(matches!(
        listing.result,
        Err(Error::Open(OpenError::Discovery))
    ));
    assert_eq!(listing.out, "");
    assert_eq!(script(&shared).calls, [Call::Discover]);

    let (fake, _) = scripted(&refs[..MAX_DEVICES], |_| {});
    let listing = run(fake, &config(None), false, false);
    listing.result.as_ref().unwrap();
    assert!(listing.out.starts_with("Cameras found: 64\n1  "));
    assert!(listing.out.contains("\n64  synthetic-secret-63\n"));
}

/// Fails the output operation (write or flush) numbered `fail_at`, so each `?` is reached.
struct FailAt {
    fail_at: usize,
    ops: usize,
}

impl FailAt {
    fn step(&mut self) -> std::io::Result<()> {
        self.ops += 1;
        if self.ops == self.fail_at {
            Err(std::io::Error::other("synthetic output failure"))
        } else {
            Ok(())
        }
    }
}

impl Write for FailAt {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.step().map(|()| bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.step()
    }
}

#[test]
fn every_output_failure_is_reported_as_io() {
    // Header, each line part, the marker, the flush and both footers.
    for (selection, attached) in [(None, &[A][..]), (Some(A), &[A][..]), (Some(B), &[A][..])] {
        let mut failures = 0;
        for fail_at in 1..=40 {
            let (fake, _) = scripted(attached, |s| s.transact.push_back(info("Maker", "Model")));
            let mut out = FailAt { fail_at, ops: 0 };
            let (result, _) = listing_with(fake, &config(selection), true, false, &mut out);
            if matches!(result, Err(Error::Io(_))) {
                failures += 1;
            } else {
                assert!(result.is_ok(), "{result:?}");
                assert!(out.ops < fail_at);
                break;
            }
        }
        assert!(failures >= 6, "{failures}");
    }
}
