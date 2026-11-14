//! Camera selection on the `ImageCaptureCore` backend: `camera_id` against zero, one and several
//! attached devices, through scripted boundaries. Nothing native runs.
//!
//! References:
//! - `docs/macos-camera.md`, Listing and selecting cameras

use super::*;
use crate::cli::Command;
use crate::config::Secret;
use crate::ptp::session::PtpSession;
use crate::supervisor::testing::{LogCapture, camera};
use crate::transport::icc::fake::{Call, Fake, Script, Shared, SimBoundary};
use crate::transport::icc::run_with;

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

fn exact(name: &str) -> Selector {
    Selector::Exact(device(name))
}

/// What one connect did: `None` when it waits, else the camera was opened and closed again.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Waiting,
    Selected,
    Refused(OpenError),
}

/// Discover `attached` under `selector`, then open and close whatever was selected.
fn connect(selector: Selector, attached: &[&str], logs: &LogCapture) -> (Outcome, Shared) {
    let (fake, shared) = Fake::new();
    script(&shared)
        .discover
        .push_back(Ok(attached.iter().map(|name| device(name)).collect()));
    let dispatch = logs.dispatch();
    let outcome = run_with(fake, |handle| {
        tracing::dispatcher::with_default(&dispatch, || {
            runtime().block_on(async {
                let (mut factory, fatal) = IccFactory::new(handle, selector);
                let outcome = match factory.connect().await {
                    Ok(None) => Outcome::Waiting,
                    Err(error) => Outcome::Refused(error),
                    Ok(Some(found)) => {
                        let mut session =
                            factory.session(found, SessionConfig::default(), Instant::now);
                        session.open().await.unwrap();
                        session.close().await.unwrap();
                        factory.release(factory.recover(session));
                        Outcome::Selected
                    }
                };
                assert!(!*fatal.borrow());
                outcome
            })
        })
    })
    .unwrap();
    (outcome, shared)
}

#[test]
fn with_no_key_zero_wait_one_is_used_and_several_are_refused() {
    let logs = LogCapture::new();
    let (waiting, shared) = connect(Selector::SoleCandidate, &[], &logs);
    assert_eq!(waiting, Outcome::Waiting);
    assert_eq!(script(&shared).calls, [Call::Discover]);

    let (one, shared) = connect(Selector::SoleCandidate, &["synthetic-secret-a"], &logs);
    assert_eq!(one, Outcome::Selected);
    assert_eq!(script(&shared).opened, [device("synthetic-secret-a")]);

    let several = [
        "synthetic-secret-a",
        "synthetic-secret-b",
        "synthetic-secret-c",
    ];
    let (refused, shared) = connect(Selector::SoleCandidate, &several, &logs);
    assert_eq!(refused, Outcome::Refused(OpenError::Ambiguous));
    // Nothing was opened: a refusal never touches any of them.
    assert_eq!(script(&shared).calls, [Call::Discover]);
    assert_eq!(script(&shared).opened, Vec::new());

    // The refusal names the listing command and carries a count, never an identifier.
    let message = OpenError::Ambiguous.to_string();
    assert!(message.contains("tethercam cameras"), "{message}");
    let text = logs.text();
    assert!(
        text.contains(r#"event="icc_ambiguous_cameras" candidates=3"#),
        "{text}"
    );
    assert!(!text.contains("synthetic-secret"), "{text}");
}

#[test]
fn a_key_selects_exactly_that_device_and_ignores_every_other() {
    let logs = LogCapture::new();
    let (alone, shared) = connect(exact("synthetic-secret-b"), &["synthetic-secret-b"], &logs);
    assert_eq!(alone, Outcome::Selected);
    assert_eq!(script(&shared).opened, [device("synthetic-secret-b")]);

    for attached in [
        [
            "synthetic-secret-a",
            "synthetic-secret-b",
            "synthetic-secret-c",
        ],
        [
            "synthetic-secret-b",
            "synthetic-secret-a",
            "synthetic-secret-c",
        ],
        [
            "synthetic-secret-c",
            "synthetic-secret-a",
            "synthetic-secret-b",
        ],
    ] {
        let (outcome, shared) = connect(exact("synthetic-secret-b"), &attached, &logs);
        assert_eq!(outcome, Outcome::Selected);
        // Only the selected one was ever opened, wherever it sat in the list.
        assert_eq!(script(&shared).opened, [device("synthetic-secret-b")]);
    }
    assert!(!logs.text().contains("synthetic-secret"));
}

#[test]
fn an_absent_key_waits_and_never_falls_back_to_another_device() {
    let logs = LogCapture::new();
    for attached in [
        &[][..],
        &["synthetic-secret-a"][..],
        &["synthetic-secret-a", "synthetic-secret-c"][..],
    ] {
        let (outcome, shared) = connect(exact("synthetic-secret-b"), attached, &logs);
        assert_eq!(outcome, Outcome::Waiting);
        assert_eq!(script(&shared).calls, [Call::Discover]);
        assert_eq!(script(&shared).opened, Vec::new());
    }
    let text = logs.text();
    assert!(
        !text.contains("synthetic-secret") && !text.contains("icc_"),
        "{text}"
    );
}

#[test]
fn a_key_listed_twice_is_ambiguous_not_a_free_choice() {
    let twice = ["synthetic-secret-b", "synthetic-secret-b"];
    let (outcome, shared) = connect(exact("synthetic-secret-b"), &twice, &LogCapture::new());
    assert_eq!(outcome, Outcome::Refused(OpenError::Ambiguous));
    assert_eq!(script(&shared).opened, Vec::new());
}

#[test]
fn the_configuration_bound_is_the_boundary_bound() {
    assert_eq!(
        config::MAX_CAMERA_ID_BYTES,
        crate::transport::icc::MAX_ID_BYTES
    );
}

#[test]
fn the_selector_follows_the_configuration_and_never_echoes_the_key() {
    let mut cfg = Config::generate().unwrap();
    assert_eq!(selector(&cfg).unwrap(), Selector::SoleCandidate);
    cfg.camera_id = Some(Secret::new("synthetic-secret-b".into()));
    assert_eq!(selector(&cfg).unwrap(), exact("synthetic-secret-b"));
    // Unvalidated values are refused under the same key, with the value redacted.
    for bad in [String::new(), "SYNTHETIC\0ID".into(), "S".repeat(1025)] {
        cfg.camera_id = Some(Secret::new(bad));
        let error = selector(&cfg).unwrap_err();
        assert!(matches!(error, Error::Config(_)), "{error:?}");
        let shown = format!("{error} {error:?}");
        assert!(shown.contains("camera_id"), "{shown}");
        assert!(
            !shown.contains("SYNTHETIC") && !shown.contains("SSSS"),
            "{shown}"
        );
    }
}

fn plan(command: Command, camera_id: Option<&str>, logs: &LogCapture) -> Plan {
    let mut cfg = Config::generate().unwrap();
    cfg.rtsp_address = "127.0.0.1".parse().unwrap();
    cfg.onvif_address = cfg.rtsp_address;
    let a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    cfg.rtsp_port = a.local_addr().unwrap().port();
    cfg.onvif_port = b.local_addr().unwrap().port();
    cfg.camera_backend = config::CameraBackend::Icc;
    cfg.camera_id = camera_id.map(|id| Secret::new(id.into()));
    Plan {
        options: Options {
            command,
            config: None,
            simulate: None,
            no_discovery: true,
        },
        cfg,
        dispatch: logs.dispatch(),
    }
}

#[test]
fn probe_and_doctor_use_the_configured_key() {
    for command in [Command::Probe, Command::Doctor] {
        let logs = LogCapture::new();
        let present = plan(command, Some("synthetic-sony"), &logs);
        let mut out = Vec::new();
        let result = run_with(SimBoundary::new(camera()), |handle| {
            worker(handle, &present, &mut out)
        })
        .unwrap();
        result.unwrap();
        assert!(
            String::from_utf8(out)
                .unwrap()
                .contains("Camera: PC Remote")
        );

        // The only attached camera is not the selected one: it waits and nothing is opened.
        let absent = plan(command, Some("synthetic-selected"), &logs);
        let (fake, shared) = Fake::new();
        script(&shared)
            .discover
            .push_back(Ok(vec![device("synthetic-other")]));
        let mut out = Vec::new();
        let result = run_with(fake, |handle| worker(handle, &absent, &mut out)).unwrap();
        assert!(matches!(result, Err(Error::NoCamera)), "{result:?}");
        assert_eq!(out, Vec::<u8>::new());
        assert_eq!(script(&shared).calls, [Call::Discover]);
        assert!(!logs.text().contains("synthetic-"));
    }
}

#[test]
fn an_invalid_key_fails_before_any_native_access() {
    let logs = LogCapture::new();
    let bad = plan(Command::Probe, Some(""), &logs);
    let (fake, shared) = Fake::new();
    let mut out = Vec::new();
    let result = run_with(fake, |handle| worker(handle, &bad, &mut out)).unwrap();
    assert!(matches!(result, Err(Error::Config(_))), "{result:?}");
    assert_eq!(script(&shared).calls, Vec::new());
}

#[test]
fn the_cameras_command_runs_through_the_worker_on_the_same_boundary() {
    let logs = LogCapture::new();
    for (describe, expect) in [
        (false, "1  synthetic-sony\n"),
        (true, "1  synthetic-sony  "),
    ] {
        let listing = plan(Command::Cameras { describe }, None, &logs);
        let mut out = Vec::new();
        let result = run_with(SimBoundary::new(camera()), |handle| {
            worker(handle, &listing, &mut out)
        })
        .unwrap();
        result.unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("Cameras found: 1\n"), "{text}");
        assert!(text.contains(expect), "{text}");
    }
    // Identifiers are for the user's terminal only.
    assert!(!logs.text().contains("synthetic-sony"), "{}", logs.text());
}
