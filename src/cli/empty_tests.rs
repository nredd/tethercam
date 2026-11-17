//! Bounded recovery from an empty discovery, through scripted boundaries. Synthetic only: no
//! native code runs and none of this says anything about a real camera.
//!
//! References:
//! - `docs/macos-camera.md`, Listing and selecting cameras

use std::time::Instant;

use super::*;
use crate::cli::icc::IccFactory;
use crate::ptp::session::{PtpSession, SessionConfig};
use crate::supervisor::testing::LogCapture;
use crate::supervisor::{CameraFactory, OpenError};
use crate::transport::icc::fake::{Call, Fake, Script, Shared};
use crate::transport::icc::{Selector, run_with};

const WANTED: &str = "synthetic-secret-wanted";
const OTHER: &str = "synthetic-secret-other";

fn device(name: &str) -> IccDeviceId {
    IccDeviceId::new(name.into()).unwrap()
}

fn script(shared: &Shared) -> std::sync::MutexGuard<'_, Script> {
    shared.lock().unwrap()
}

fn scripted(answers: Vec<Result<Vec<IccDeviceId>, IccError>>) -> (Fake, Shared) {
    let (fake, shared) = Fake::new();
    script(&shared).discover.extend(answers);
    (fake, shared)
}

fn paused() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap()
}

fn discovers(shared: &Shared) -> usize {
    script(shared)
        .calls
        .iter()
        .filter(|call| **call == Call::Discover)
        .count()
}

/// What one `connect` of a recovering factory did, and how long it took on the paused clock.
struct Attempt {
    outcome: Result<Option<()>, OpenError>,
    elapsed: Duration,
    fatal: bool,
}

fn connect_once(
    answers: Vec<Result<Vec<IccDeviceId>, IccError>>, selector: Selector, policy: EmptyRecovery,
) -> (Attempt, Shared, LogCapture) {
    let (fake, shared) = scripted(answers);
    let logs = LogCapture::new();
    let dispatch = logs.dispatch();
    let attempt = run_with(fake, |handle| {
        tracing::dispatcher::with_default(&dispatch, || {
            paused().block_on(async {
                let (factory, fatal) = IccFactory::new(handle, selector);
                let mut factory = factory.recovering(policy);
                let started = tokio::time::Instant::now();
                let outcome = factory.connect().await.map(|camera| camera.map(|_| ()));
                Attempt {
                    outcome,
                    elapsed: started.elapsed(),
                    fatal: *fatal.borrow(),
                }
            })
        })
    })
    .unwrap();
    (attempt, shared, logs)
}

/// Every window used in full plus every pause: the most empty snapshots can cost.
fn worst_case(policy: EmptyRecovery) -> Duration {
    DISCOVERY_WINDOW * u32::from(policy.attempts()) + policy.pause * u32::from(policy.retries)
}

fn exact() -> Selector {
    Selector::Exact(device(WANTED))
}

#[test]
fn the_policy_is_bounded_and_covers_one_shot_commands_only() {
    assert_eq!(EmptyRecovery::for_command(Command::Run), EmptyRecovery::OFF);
    for command in [
        Command::Probe,
        Command::Doctor,
        Command::Cameras { describe: false },
        Command::Cameras { describe: true },
    ] {
        assert_eq!(EmptyRecovery::for_command(command), EmptyRecovery::ONE_SHOT);
    }
    assert_eq!(EmptyRecovery::OFF.attempts(), 1);
    assert_eq!(EmptyRecovery::ONE_SHOT.attempts(), 3);
    assert_eq!(worst_case(EmptyRecovery::OFF), DISCOVERY_WINDOW);
    // Three full windows and two pauses.
    assert_eq!(worst_case(EmptyRecovery::ONE_SHOT), Duration::from_secs(8));
    // The count cannot overflow, whatever the policy.
    let huge = EmptyRecovery {
        retries: u8::MAX,
        pause: Duration::ZERO,
    };
    assert_eq!(huge.attempts(), u8::MAX);
}

#[test]
fn an_empty_snapshot_then_the_exact_camera_is_selected_after_one_pause() {
    let (attempt, shared, logs) = connect_once(
        vec![Ok(Vec::new()), Ok(vec![device(WANTED)])],
        exact(),
        EmptyRecovery::ONE_SHOT,
    );
    assert!(
        matches!(attempt.outcome, Ok(Some(()))),
        "{:?}",
        attempt.outcome
    );
    assert_eq!(attempt.elapsed, EmptyRecovery::ONE_SHOT.pause);
    assert_eq!(discovers(&shared), 2);
    // Selected, not opened: connecting never opens a session.
    assert_eq!(script(&shared).opened, Vec::new());
    let text = logs.text();
    assert!(
        text.contains(r#"event="icc_discovery_recovered" attempts=2"#),
        "{text}"
    );
    assert!(!text.contains("synthetic-"), "{text}");
}

#[test]
fn recovery_stops_at_the_first_non_empty_snapshot() {
    // Empty twice, then the camera: the third and last attempt is used and no fourth is made.
    let (attempt, shared, _) = connect_once(
        vec![
            Ok(Vec::new()),
            Ok(Vec::new()),
            Ok(vec![device(WANTED)]),
            Ok(vec![device(OTHER)]),
        ],
        exact(),
        EmptyRecovery::ONE_SHOT,
    );
    assert!(matches!(attempt.outcome, Ok(Some(()))));
    assert_eq!(attempt.elapsed, EmptyRecovery::ONE_SHOT.pause * 2);
    assert_eq!(discovers(&shared), 3);
}

#[test]
fn a_non_empty_snapshot_without_the_selected_camera_is_final_and_never_falls_back() {
    // Another device is attached and the selected one is not: absent, not empty. One discovery,
    // no pause, nothing opened.
    let (attempt, shared, logs) = connect_once(
        vec![Ok(vec![device(OTHER)]), Ok(vec![device(WANTED)])],
        exact(),
        EmptyRecovery::ONE_SHOT,
    );
    assert!(matches!(attempt.outcome, Ok(None)), "{:?}", attempt.outcome);
    assert_eq!(attempt.elapsed, Duration::ZERO);
    assert_eq!(script(&shared).calls, [Call::Discover]);
    assert_eq!(script(&shared).opened, Vec::new());
    assert!(!logs.text().contains("icc_discovery_recovered"));
}

#[test]
fn empty_then_only_another_device_is_absent_and_never_opens_it() {
    let (attempt, shared, logs) = connect_once(
        vec![Ok(Vec::new()), Ok(vec![device(OTHER)])],
        exact(),
        EmptyRecovery::ONE_SHOT,
    );
    // The selected camera is still absent after the retry: it waits, never the other body.
    assert!(matches!(attempt.outcome, Ok(None)), "{:?}", attempt.outcome);
    assert_eq!(script(&shared).calls, [Call::Discover, Call::Discover]);
    assert_eq!(script(&shared).opened, Vec::new());
    assert!(logs.text().contains("icc_discovery_recovered"));
}

#[test]
fn empty_then_several_is_still_ambiguous_for_a_sole_candidate() {
    let (attempt, shared, _) = connect_once(
        vec![Ok(Vec::new()), Ok(vec![device(WANTED), device(OTHER)])],
        Selector::SoleCandidate,
        EmptyRecovery::ONE_SHOT,
    );
    assert_eq!(attempt.outcome.unwrap_err(), OpenError::Ambiguous);
    assert_eq!(discovers(&shared), 2);
    assert_eq!(script(&shared).opened, Vec::new());
}

#[test]
fn an_always_empty_listing_is_bounded_in_attempts_and_wall_time() {
    let (attempt, shared, logs) = connect_once(
        // More answers than attempts: the surplus must stay unread.
        vec![Ok(Vec::new()); 6],
        exact(),
        EmptyRecovery::ONE_SHOT,
    );
    assert_eq!(
        attempt.outcome.unwrap_err(),
        OpenError::EmptyListing { attempts: 3 }
    );
    assert_eq!(discovers(&shared), 3);
    assert_eq!(script(&shared).discover.len(), 3);
    assert_eq!(attempt.elapsed, EmptyRecovery::ONE_SHOT.pause * 2);
    // Native answering every window on time costs the windows too, never more than the bound.
    assert!(
        attempt.elapsed + DISCOVERY_WINDOW * 3 <= worst_case(EmptyRecovery::ONE_SHOT),
        "{:?}",
        attempt.elapsed
    );
    assert!(!attempt.fatal);
    assert!(!logs.text().contains("synthetic-"), "{}", logs.text());
}

#[test]
fn the_diagnostic_says_what_is_known_and_what_is_not() {
    let text = OpenError::EmptyListing { attempts: 3 }.to_string();
    for part in [
        "listed no cameras in 3 discovery attempts",
        "cause is not established",
        "seen with the camera attached",
        "PC Remote",
    ] {
        assert!(text.contains(part), "{text}");
    }
}

#[test]
fn an_error_is_never_retried_and_nothing_is_submitted_after_it() {
    for error in [
        IccError::Busy,
        IccError::AdmissionExpired,
        IccError::DeadlineExceeded,
        IccError::Permission,
        IccError::NativeFailure,
        IccError::Quarantined,
    ] {
        let (attempt, shared, _) = connect_once(
            vec![Err(error), Ok(vec![device(WANTED)])],
            exact(),
            EmptyRecovery::ONE_SHOT,
        );
        assert!(attempt.outcome.is_err(), "{error:?}");
        assert_eq!(script(&shared).calls, [Call::Discover], "{error:?}");
        assert_eq!(attempt.elapsed, Duration::ZERO, "{error:?}");
    }
}

#[test]
fn an_error_after_an_empty_snapshot_ends_the_recovery_with_that_error() {
    let (attempt, shared, _) = connect_once(
        vec![
            Ok(Vec::new()),
            Err(IccError::Busy),
            Ok(vec![device(WANTED)]),
        ],
        exact(),
        EmptyRecovery::ONE_SHOT,
    );
    assert_eq!(attempt.outcome.unwrap_err(), OpenError::Busy);
    assert_eq!(discovers(&shared), 2);
    // The camera behind the error is left unread: native work may still be pending.
    assert_eq!(script(&shared).discover.len(), 1);
}

#[test]
fn a_quarantine_during_recovery_is_still_fatal() {
    let (attempt, _, _) = connect_once(
        vec![Ok(Vec::new()), Err(IccError::Quarantined)],
        exact(),
        EmptyRecovery::ONE_SHOT,
    );
    assert!(attempt.fatal);
    assert!(attempt.outcome.is_err());
}

#[test]
fn the_service_policy_discovers_once_and_keeps_waiting() {
    let (attempt, shared, _) = connect_once(
        vec![Ok(Vec::new()), Ok(vec![device(WANTED)])],
        exact(),
        EmptyRecovery::OFF,
    );
    // Empty is "wait" for the supervisor, as before: no retry here, no error, no delay.
    assert!(matches!(attempt.outcome, Ok(None)), "{:?}", attempt.outcome);
    assert_eq!(attempt.elapsed, Duration::ZERO);
    assert_eq!(script(&shared).calls, [Call::Discover]);
}

#[test]
fn pending_native_work_submits_no_discovery_even_when_recovering() {
    let (fake, shared) = scripted(vec![Ok(vec![device(WANTED)])]);
    let outcome = run_with(fake, |handle| {
        paused().block_on(async {
            let (factory, _) = IccFactory::new(handle, exact());
            let mut factory = factory.recovering(EmptyRecovery::ONE_SHOT);
            // The camera is outstanding: its handle is with the caller, not the factory.
            let camera = factory.connect().await.unwrap().unwrap();
            let started = tokio::time::Instant::now();
            let pending = factory.connect().await.err();
            let elapsed = started.elapsed();
            factory.release(camera);
            (pending, elapsed)
        })
    })
    .unwrap();
    assert_eq!(outcome.0, Some(OpenError::Busy));
    assert_eq!(outcome.1, Duration::ZERO);
    assert_eq!(script(&shared).calls, [Call::Discover]);
}

#[test]
fn a_recovered_camera_is_opened_and_closed_in_order_and_the_handle_returns() {
    let (fake, shared) = scripted(vec![
        Ok(Vec::new()),
        Ok(vec![device(WANTED)]),
        Ok(vec![device(WANTED)]),
    ]);
    run_with(fake, |handle| {
        paused().block_on(async {
            let (factory, _) = IccFactory::new(handle, exact());
            let mut factory = factory.recovering(EmptyRecovery::ONE_SHOT);
            let camera = factory.connect().await.unwrap().unwrap();
            let mut session = factory.session(camera, SessionConfig::default(), Instant::now);
            session.open().await.unwrap();
            session.close().await.unwrap();
            let camera = factory.recover(session);
            factory.release(camera);
            // Cleanup left the handle usable: the next connect reaches the boundary again.
            assert!(factory.connect().await.unwrap().is_some());
        });
    })
    .unwrap();
    let script = script(&shared);
    let steps: Vec<&Call> = script
        .calls
        .iter()
        .filter(|call| matches!(call, Call::Discover | Call::Open | Call::Close))
        .collect();
    assert_eq!(
        steps,
        [
            &Call::Discover,
            &Call::Discover,
            &Call::Open,
            &Call::Close,
            &Call::Discover,
        ]
    );
    assert_eq!(script.live_leases, 0);
    assert_eq!(script.opened, [device(WANTED)]);
}

#[test]
fn the_first_discovery_is_never_delayed() {
    let (fake, shared) = scripted(vec![Ok(vec![device(WANTED)])]);
    let snapshot = run_with(fake, |mut handle| {
        paused().block_on(async {
            let started = tokio::time::Instant::now();
            let snapshot = discover(&mut handle, EmptyRecovery::ONE_SHOT).await;
            (snapshot, started.elapsed())
        })
    })
    .unwrap();
    assert_eq!(snapshot.1, Duration::ZERO);
    let snapshot = snapshot.0.unwrap();
    assert_eq!(snapshot.attempts, 1);
    assert!(!snapshot.exhausted_empty());
    assert_eq!(discovers(&shared), 1);
}
