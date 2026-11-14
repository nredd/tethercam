//! The `ImageCaptureCore` backend wiring, driven through scripted and simulated boundaries.
//! No native code runs: `run_with` stands in for the main-thread runtime.
//!
//! References:
//! - `docs/macos-camera.md`, Ownership and deadlines

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::cli::Command;
use crate::sony::op;
use crate::supervisor::testing::{LogCapture, camera};
use crate::transport::icc::fake::{Call, Fake, Script, Shared, SimBoundary, SimFaults};
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

fn scripted(setup: impl FnOnce(&mut Script)) -> (Fake, Shared) {
    let (fake, shared) = Fake::new();
    setup(&mut script(&shared));
    (fake, shared)
}

fn options(command: Command) -> Options {
    Options {
        command,
        config: None,
        simulate: None,
        no_discovery: true,
    }
}

fn free_cfg() -> Config {
    let mut cfg = Config::generate().unwrap();
    cfg.rtsp_address = "127.0.0.1".parse().unwrap();
    cfg.onvif_address = cfg.rtsp_address;
    let a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    cfg.rtsp_port = a.local_addr().unwrap().port();
    cfg.onvif_port = b.local_addr().unwrap().port();
    cfg
}

#[test]
fn every_discovery_failure_maps_to_a_sanitized_open_category() {
    for (error, expected) in [
        (IccError::Permission, OpenError::Permission),
        (IccError::Busy, OpenError::Busy),
        (IccError::AdmissionExpired, OpenError::Busy),
        (IccError::DeadlineExceeded, OpenError::Busy),
        (IccError::Removed, OpenError::Disconnected),
        (IccError::Disconnected, OpenError::Disconnected),
        (IccError::NotFound, OpenError::Disconnected),
        (IccError::Quarantined, OpenError::Disconnected),
        (IccError::Ambiguous, OpenError::Discovery),
        (IccError::NativeFailure, OpenError::Discovery),
        (IccError::Runtime, OpenError::Discovery),
    ] {
        assert_eq!(open_error(error), expected, "{error:?}");
    }
}

/// The outcome of one connect, reduced to what the policy decides.
async fn attempt(factory: &mut IccFactory) -> Result<bool, OpenError> {
    factory.connect().await.map(|camera| camera.is_none())
}

#[test]
fn discovery_outcomes_leave_the_handle_with_the_factory() {
    let logs = LogCapture::new();
    let (fake, _) = scripted(|s| {
        s.discover.push_back(Ok(Vec::new()));
        s.discover.push_back(Ok(vec![
            device("synthetic-secret-a"),
            device("synthetic-secret-b"),
        ]));
        s.discover.push_back(Err(IccError::Permission));
        s.discover.push_back(Err(IccError::DeadlineExceeded));
        s.discover.push_back(Ok(vec![device("synthetic-secret-a")]));
    });
    let dispatch = logs.dispatch();
    let out = run_with(fake, |handle| {
        tracing::dispatcher::with_default(&dispatch, || {
            runtime().block_on(async {
                let (mut factory, fatal) = IccFactory::new(handle);
                let mut seen = Vec::new();
                for _ in 0..4 {
                    seen.push(attempt(&mut factory).await);
                }
                let camera = factory.connect().await.unwrap().unwrap();
                // A second connect while a camera is outstanding must not reach the boundary.
                seen.push(attempt(&mut factory).await);
                factory.release(camera);
                (seen, *fatal.borrow())
            })
        })
    })
    .unwrap();
    assert_eq!(
        out,
        (
            vec![
                Ok(true),
                Err(OpenError::Discovery),
                Err(OpenError::Permission),
                Err(OpenError::Busy),
                Err(OpenError::Busy),
            ],
            false
        )
    );
    let text = logs.text();
    assert!(
        text.contains(r#"event="icc_ambiguous_cameras" candidates=2"#),
        "{text}"
    );
    assert!(text.contains(r#"event="icc_connect_failed" category=camera access restricted"#));
    assert!(!text.contains("synthetic-secret"));
}

#[test]
fn a_quarantine_is_terminal_and_refuses_every_later_discovery() {
    let logs = LogCapture::new();
    let (fake, shared) = scripted(|s| s.discover.push_back(Err(IccError::Removed)));
    let dispatch = logs.dispatch();
    let out = run_with(fake, |handle| {
        tracing::dispatcher::with_default(&dispatch, || {
            runtime().block_on(async {
                let (mut factory, fatal) = IccFactory::new(handle);
                let first = attempt(&mut factory).await;
                let flagged = *fatal.borrow();
                let second = attempt(&mut factory).await;
                (first, flagged, second)
            })
        })
    })
    .unwrap();
    assert_eq!(
        out,
        (
            Err(OpenError::Disconnected),
            true,
            Err(OpenError::Disconnected)
        )
    );
    // Only the first attempt reached the native boundary.
    assert_eq!(script(&shared).calls, [Call::Discover]);
    assert!(logs.text().contains("icc_runtime_quarantined"));
}

fn paused_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap()
}

#[test]
fn a_run_of_busy_answers_past_the_limit_is_fatal_through_the_quarantine_flag() {
    let logs = LogCapture::new();
    let (fake, shared) = scripted(|s| s.discover.extend((0..8).map(|_| Err(IccError::Busy))));
    let dispatch = logs.dispatch();
    let out = run_with(fake, |handle| {
        tracing::dispatcher::with_default(&dispatch, || {
            paused_runtime().block_on(async {
                let (mut factory, fatal) = IccFactory::new(handle);
                // Busy at the start of the run, then up to one tick short of the limit.
                let first = attempt(&mut factory).await;
                tokio::time::advance(BUSY_LIMIT.saturating_sub(Duration::from_secs(1))).await;
                let short = attempt(&mut factory).await;
                let before = *fatal.borrow();
                tokio::time::advance(Duration::from_secs(1)).await;
                let limit = attempt(&mut factory).await;
                let after = *fatal.borrow();
                // Fatal is terminal: nothing more reaches the boundary.
                let refused = attempt(&mut factory).await;
                (first, short, before, limit, after, refused)
            })
        })
    })
    .unwrap();
    assert_eq!(
        out,
        (
            Err(OpenError::Busy),
            Err(OpenError::Busy),
            false,
            Err(OpenError::Busy),
            true,
            Err(OpenError::Disconnected)
        )
    );
    assert_eq!(
        script(&shared).calls,
        [Call::Discover, Call::Discover, Call::Discover]
    );
    let text = logs.text();
    assert!(
        text.contains(r#"event="icc_busy_limit_exceeded" limit_secs=120"#),
        "{text}"
    );
    assert!(text.contains("icc_runtime_quarantined"), "{text}");
}

#[test]
fn any_other_outcome_ends_a_run_of_busy_answers() {
    let (fake, _) = scripted(|s| {
        s.discover.push_back(Err(IccError::Busy));
        s.discover.push_back(Err(IccError::Busy));
        // Settled: the run ends, so the busy answers after it start a new one.
        s.discover.push_back(Ok(Vec::new()));
        s.discover.extend((0..3).map(|_| Err(IccError::Busy)));
    });
    let flags = run_with(fake, |handle| {
        paused_runtime().block_on(async {
            let (mut factory, fatal) = IccFactory::new(handle);
            let mut flags = Vec::new();
            for _ in 0..6 {
                let _ = attempt(&mut factory).await;
                // Each step is 80% of the limit. Without the reset the fourth answer would
                // already be 288 s into one run; with it only the sixth is past the limit.
                tokio::time::advance(BUSY_LIMIT * 4 / 5).await;
                flags.push(*fatal.borrow());
            }
            flags
        })
    })
    .unwrap();
    assert_eq!(flags, [false, false, false, false, false, true]);
}

#[test]
fn sessions_reopen_on_the_one_runtime_after_a_confirmed_close() {
    let boundary = SimBoundary::new(camera());
    let out = run_with(boundary, |handle| {
        runtime().block_on(async {
            use crate::sony::{SonyCamera, SonyConfig};
            let (mut factory, fatal) = IccFactory::new(handle);
            let mut protocols = Vec::new();
            for _ in 0..2 {
                let found = factory.connect().await.unwrap().unwrap();
                let mut session =
                    factory.session(found, SessionConfig::default(), std::time::Instant::now);
                let config = SonyConfig {
                    poll_interval: Duration::ZERO,
                    ..SonyConfig::default()
                };
                let ran =
                    SonyCamera::run(&mut session, config, async |camera| Ok(camera.protocol()))
                        .await;
                protocols.push(ran.is_ok());
                let recovered = factory.recover(session);
                factory.release(recovered);
            }
            (protocols, *fatal.borrow())
        })
    })
    .unwrap();
    assert_eq!(out, (vec![true, true], false));
}

#[test]
fn a_quarantined_session_flags_the_factory_when_released() {
    let boundary = SimBoundary::with_faults(
        camera(),
        SimFaults {
            close: Some(IccError::CleanupFailure),
            ..SimFaults::default()
        },
    );
    let out = run_with(boundary, |handle| {
        runtime().block_on(async {
            let (mut factory, fatal) = IccFactory::new(handle);
            let found = factory.connect().await.unwrap().unwrap();
            let mut session =
                factory.session(found, SessionConfig::default(), std::time::Instant::now);
            let opened = crate::ptp::session::PtpSession::open(&mut session).await;
            let closed = crate::ptp::session::PtpSession::close(&mut session).await;
            let recovered = factory.recover(session);
            let before = *fatal.borrow();
            factory.release(recovered);
            (opened.is_ok(), closed.is_err(), before, *fatal.borrow())
        })
    })
    .unwrap();
    assert_eq!(out, (true, true, false, true));
}

fn probe_through(boundary: SimBoundary, logs: &LogCapture) -> (Result<(), Error>, String) {
    let plan = Plan {
        options: options(Command::Probe),
        cfg: free_cfg(),
        dispatch: logs.dispatch(),
    };
    let mut out = Vec::new();
    let result = run_with(boundary, |handle| worker(handle, &plan, &mut out)).unwrap();
    (result, String::from_utf8(out).unwrap())
}

#[test]
fn probe_runs_the_production_stack_over_the_boundary() {
    let (result, out) = probe_through(SimBoundary::new(camera()), &LogCapture::new());
    result.unwrap();
    assert!(out.starts_with("Camera: PC Remote; protocol V200"), "{out}");
}

#[test]
fn a_probe_without_the_camera_is_a_plain_failure_not_a_quarantine() {
    let (fake, _) = scripted(|s| s.discover.push_back(Ok(Vec::new())));
    let plan = Plan {
        options: options(Command::Probe),
        cfg: free_cfg(),
        dispatch: LogCapture::new().dispatch(),
    };
    let mut out = Vec::new();
    let result = run_with(fake, |handle| worker(handle, &plan, &mut out)).unwrap();
    assert!(matches!(result, Err(Error::NoCamera)), "{result:?}");
}

#[test]
fn run_serves_until_stopped_and_closes_the_session_in_order() {
    let boundary = SimBoundary::new(camera());
    let cfg = free_cfg();
    let sink = Sink::default();
    let mut out = sink.clone();
    let result = run_with(boundary, |handle| {
        runtime().block_on(async {
            let stop = served(&sink);
            drive_until(
                handle,
                &cfg,
                &options(Command::Run),
                &mut out,
                Box::pin(stop),
            )
            .await
        })
    })
    .unwrap();
    result.unwrap();
    // Listeners came up first and answered "not ready"; the ICC session's first frame then made
    // the stream ready.
    let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    let waiting = text.find("waiting for a camera").expect("waiting line");
    let serving = text
        .find("Serving authenticated RTSP and ONVIF")
        .expect("ready line");
    assert!(waiting < serving, "{text}");
}

/// Resolves once the application has printed `needle`.
async fn printed(sink: &Sink, needle: &str) {
    while !String::from_utf8_lossy(&sink.0.lock().unwrap()).contains(needle) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Resolves once the application reports the stream ready, i.e. the first frame arrived.
async fn served(sink: &Sink) {
    printed(sink, "Serving authenticated").await;
}

#[test]
fn with_no_camera_the_process_comes_up_waiting_and_stops_in_order() {
    let (fake, shared) = Fake::new();
    let cfg = free_cfg();
    let sink = Sink::default();
    let mut out = sink.clone();
    let result = run_with(fake, |handle| {
        runtime().block_on(async {
            let stop = printed(&sink, "waiting for a camera");
            drive_until(
                handle,
                &cfg,
                &options(Command::Run),
                &mut out,
                Box::pin(stop),
            )
            .await
        })
    })
    .unwrap();
    result.unwrap();
    let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    assert!(!text.contains("Serving authenticated"), "{text}");
    // Only discovery ever reached the boundary: no session was opened without a camera.
    assert!(
        script(&shared)
            .calls
            .iter()
            .all(|call| *call == Call::Discover)
    );
}

/// Run with no stop request: only a quarantine can end it.
fn run_until_quarantined(faults: SimFaults) -> Result<(), Error> {
    let boundary = SimBoundary::with_faults(camera(), faults);
    let cfg = free_cfg();
    let mut out = Vec::new();
    run_with(boundary, |handle| {
        runtime().block_on(drive_until(
            handle,
            &cfg,
            &options(Command::Run),
            &mut out,
            Box::pin(std::future::pending()),
        ))
    })
    .unwrap()
}

#[test]
fn a_native_command_that_never_completes_ends_the_run_as_quarantined_after_the_busy_limit() {
    // The retired command's completion never arrives, so every discovery is answered busy. The
    // supervisor's backoff keeps retrying until the bound turns it into the orderly exit.
    let logs = LogCapture::new();
    let (fake, shared) = scripted(|s| s.discover.extend((0..500).map(|_| Err(IccError::Busy))));
    let dispatch = logs.dispatch();
    let cfg = free_cfg();
    let mut out = Vec::new();
    let result = run_with(fake, |handle| {
        tracing::dispatcher::with_default(&dispatch, || {
            paused_runtime().block_on(drive_until(
                handle,
                &cfg,
                &options(Command::Run),
                &mut out,
                Box::pin(std::future::pending()),
            ))
        })
    })
    .unwrap();
    assert!(matches!(result, Err(Error::Quarantined)), "{result:?}");
    // 100 ms doubling to the 5 s cap reaches 120 s on about the 30th attempt.
    let attempts = script(&shared).calls.len();
    assert!((25..=35).contains(&attempts), "{attempts}");
    assert!(logs.text().contains("icc_busy_limit_exceeded"));
}

#[test]
fn the_backend_slows_reconnects_after_a_refused_handshake() {
    let (fake, _) = Fake::new();
    let delay = run_with(fake, |handle| IccFactory::new(handle).0.refusal_delay()).unwrap();
    assert_eq!(delay, Some(REFUSAL_DELAY));
    assert!(REFUSAL_DELAY > Duration::from_secs(5));
}

#[test]
fn a_removal_mid_connect_shuts_the_application_down_as_quarantined() {
    let result = run_until_quarantined(SimFaults {
        transact: Some((op::GET_OBJECT, None, IccError::Removed)),
        ..SimFaults::default()
    });
    assert!(matches!(result, Err(Error::Quarantined)), "{result:?}");
}

#[test]
fn a_failed_close_quarantines_even_when_the_stop_was_requested() {
    let boundary = SimBoundary::with_faults(
        camera(),
        SimFaults {
            close: Some(IccError::CleanupFailure),
            ..SimFaults::default()
        },
    );
    let cfg = free_cfg();
    let sink = Sink::default();
    let mut out = sink.clone();
    let result = run_with(boundary, |handle| {
        runtime().block_on(async {
            let stop = served(&sink);
            drive_until(
                handle,
                &cfg,
                &options(Command::Run),
                &mut out,
                Box::pin(stop),
            )
            .await
        })
    })
    .unwrap();
    assert!(matches!(result, Err(Error::Quarantined)), "{result:?}");
}

#[test]
fn the_worker_reports_runtime_and_signal_setup_failures() {
    let plan = Plan {
        options: options(Command::Probe),
        cfg: free_cfg(),
        dispatch: LogCapture::new().dispatch(),
    };
    let mut out = Vec::new();
    // Never reached when the runtime failed first; called once here so it is exercised.
    let idle = || -> StopSignal { Ok(Box::pin(std::future::pending())) };
    assert!(idle().is_ok());
    let (fake, _) = Fake::new();
    let failed = run_with(fake, |handle| {
        worker_with(
            handle,
            &plan,
            &mut out,
            Err(io::Error::other("synthetic reactor failure")),
            idle,
        )
    })
    .unwrap();
    assert!(matches!(failed, Err(Error::Io(_))));
    let (fake, _) = Fake::new();
    let failed = run_with(fake, |handle| {
        worker_with(handle, &plan, &mut out, Ok(runtime()), || {
            Err(io::Error::other("synthetic signal failure"))
        })
    })
    .unwrap();
    assert!(matches!(failed, Err(Error::Io(_))));
}

/// A `Write + Send + 'static` sink the test can read after the entry has consumed it.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Each test that inspects a termination owns its static: the lib tests may share one process.
static TERMINATED: AtomicU8 = AtomicU8::new(0);
static UNEXPECTED: AtomicU8 = AtomicU8::new(0);

fn record_termination(code: u8) {
    TERMINATED.store(code, Ordering::SeqCst);
}

fn record_unexpected_termination(code: u8) {
    UNEXPECTED.store(code, Ordering::SeqCst);
}

#[test]
fn launch_runs_the_worker_on_the_native_side_with_logging_and_stdout_carried_over() {
    // Reset the sentinel; the call also exercises the recorder a correct run never triggers.
    record_unexpected_termination(0);
    let logs = LogCapture::new();
    let sink = Sink::default();
    let result = launch_with(
        &options(Command::Probe),
        &free_cfg(),
        &logs.dispatch(),
        sink.clone(),
        |entry| run_with(SimBoundary::new(camera()), entry),
        record_unexpected_termination,
    );
    result.unwrap();
    assert!(
        String::from_utf8(sink.0.lock().unwrap().clone())
            .unwrap()
            .starts_with("Camera: PC Remote")
    );
    assert_eq!(UNEXPECTED.load(Ordering::SeqCst), 0);
}

#[test]
fn launch_terminates_with_the_restart_status_after_a_quarantine() {
    let logs = LogCapture::new();
    let sink = Sink::default();
    TERMINATED.store(0, Ordering::SeqCst);
    let result = launch_with(
        &options(Command::Probe),
        &free_cfg(),
        &logs.dispatch(),
        sink.clone(),
        |entry| {
            let faults = SimFaults {
                close: Some(IccError::CleanupFailure),
                ..SimFaults::default()
            };
            run_with(SimBoundary::with_faults(camera(), faults), entry)
        },
        record_termination,
    );
    assert!(matches!(result, Err(Error::Quarantined)), "{result:?}");
    assert_eq!(TERMINATED.load(Ordering::SeqCst), QUARANTINE_EXIT);
    let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains("Error: native camera runtime quarantined"),
        "{text}"
    );
    // The worker thread logged into the dispatcher it was handed, not the main thread's.
    assert!(logs.text().contains("icc_runtime_quarantined"));
}

#[test]
fn launch_reports_an_unavailable_runtime_without_running_anything() {
    let result = launch_with(
        &options(Command::Probe),
        &free_cfg(),
        &LogCapture::new().dispatch(),
        Sink::default(),
        |_entry| Err(IccError::Runtime),
        record_unexpected_termination,
    );
    assert!(matches!(result, Err(Error::IccUnavailable)));
}

#[test]
fn launch_does_not_blame_the_platform_when_the_worker_failed_after_it_ran() {
    let sink = Sink::default();
    let result = launch_with(
        &options(Command::Probe),
        &free_cfg(),
        &LogCapture::new().dispatch(),
        sink.clone(),
        |entry| {
            run_with(SimBoundary::new(camera()), |handle| {
                // The application ran to completion, then the worker thread failed.
                let _ = entry(handle);
                std::panic::resume_unwind(Box::new("synthetic worker failure"))
            })
        },
        record_unexpected_termination,
    );
    let error = result.unwrap_err();
    assert!(matches!(error, Error::IccFailed), "{error:?}");
    assert!(!error.to_string().contains("unavailable"), "{error}");
    assert!(
        String::from_utf8(sink.0.lock().unwrap().clone())
            .unwrap()
            .starts_with("Camera: PC Remote")
    );
}

#[test]
fn the_real_entry_point_refuses_before_any_camera_access_off_the_main_thread() {
    // Off macOS the native entry point is a stub. On macOS the real one checks for the main thread
    // before it touches anything native, so a test thread (never `main`) cannot start it.
    // The guard keeps that true even if a harness ever ran a test inline.
    assert_ne!(std::thread::current().name(), Some("main"));
    let result = launch(
        &options(Command::Run),
        &free_cfg(),
        &LogCapture::new().dispatch(),
    );
    assert!(matches!(result, Err(Error::IccUnavailable)));
}

#[test]
fn terminate_exits_the_process_with_the_given_status() {
    const CHILD: &str = "TETHERCAM_TEST_TERMINATE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        terminate(QUARANTINE_EXIT);
        return;
    }
    let (_, module) = module_path!().split_once("::").unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("{module}::terminate_exits_the_process_with_the_given_status"),
        ])
        .arg("--nocapture")
        .env(CHILD, "1")
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(i32::from(QUARANTINE_EXIT)));
}

#[test]
fn backend_errors_are_static_and_name_no_device() {
    for error in [Error::IccUnavailable, Error::IccFailed, Error::Quarantined] {
        let text = error.to_string();
        assert!(text.contains("icc") || text.contains("native"), "{text}");
    }
}
