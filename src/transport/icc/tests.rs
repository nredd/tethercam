//! Deterministic race, failure and end-to-end tests for the ICC session layers.

use std::future::Future;
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use super::fake::{Call, Fake, Script, Shared, SimBoundary, Step, ok_reply};
use super::{
    IccDeviceId, IccError, IccHandle, IccReply, IccSession, IccTimeout, MAX_DATA_BYTES,
    read_only_sample, run_with,
};
use crate::ptp::ContainerError;
use crate::ptp::session::{Event, PtpSession, SessionConfig, SessionError};
use crate::sony::{SonyCamera, SonyConfig, SonyEvent, prop};
use crate::transport::Endpoint;
use crate::transport::sim::Fault;
use crate::transport::sim::sony::{SonySimConfig, sony_camera};

fn block<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .expect("runtime")
        .block_on(future)
}

fn script(shared: &Shared) -> std::sync::MutexGuard<'_, Script> {
    shared.lock().expect("script")
}

fn run<T: Send>(
    setup: impl FnOnce(&mut Script), user: impl FnOnce(IccHandle) -> T + Send,
) -> (T, Shared) {
    let (fake, shared) = Fake::new();
    setup(&mut script(&shared));
    let out = run_with(fake, user).expect("owner ran");
    (out, shared)
}

fn id() -> IccDeviceId {
    IccDeviceId::new("synthetic-camera".into()).expect("valid")
}

fn t() -> IccTimeout {
    IccTimeout::default()
}

fn session_with(handle: IccHandle, config: SessionConfig) -> IccSession {
    IccSession::new(handle, id(), config, Instant::now)
}

fn session(handle: IccHandle) -> IccSession {
    session_with(handle, SessionConfig::default())
}

fn icc(error: IccError) -> SessionError {
    SessionError::Icc(error)
}

fn reply(code: u16, params: &[u32], data: &[u8]) -> IccReply {
    IccReply {
        code,
        params: params.to_vec(),
        data: data.to_vec(),
    }
}

fn done(result: Result<IccReply, IccError>) -> Step {
    Step::Done(result)
}

fn ptp_event(code: u16, params: &[u32]) -> Vec<u8> {
    let mut wire = vec![11];
    wire.extend(
        u32::try_from(12 + 4 * params.len())
            .expect("small")
            .to_le_bytes(),
    );
    wire.extend(4_u16.to_le_bytes());
    wire.extend(code.to_le_bytes());
    wire.extend(0_u32.to_le_bytes());
    for param in params {
        wire.extend(param.to_le_bytes());
    }
    wire
}

fn calls(shared: &Shared) -> Vec<Call> {
    script(shared).calls.clone()
}

fn transacts(shared: &Shared) -> usize {
    calls(shared)
        .iter()
        .filter(|call| matches!(call, Call::Transact { .. }))
        .count()
}

// ---- bridge ------------------------------------------------------------------------------

#[test]
fn the_bridge_carries_every_request_kind() {
    let (out, shared) = run(
        |s| {
            s.discover.push_back(Ok(vec![id()]));
            s.boundary_events.push_back(Ok(Some(vec![1])));
            s.lease_events.push_back(Ok(Some(vec![2])));
            s.transact.push_back(done(Ok(reply(0x2001, &[4], &[5, 6]))));
        },
        |mut handle| {
            block(async move {
                let found = handle.discover(Duration::from_secs(1), t()).await.unwrap();
                let before = handle.poll_event().await.unwrap();
                handle.open(id(), t()).await.unwrap();
                let held = handle.poll_event().await.unwrap();
                let got = handle
                    .transact(0x1001, vec![1], vec![9], t())
                    .await
                    .unwrap();
                handle.close(t()).await.unwrap();
                (found, before, held, got)
            })
        },
    );
    assert_eq!(out.0, [id()]);
    assert_eq!((out.1, out.2), (Some(vec![1]), Some(vec![2])));
    assert_eq!(out.3, reply(0x2001, &[4], &[5, 6]));
    assert_eq!(
        calls(&shared),
        [
            Call::Discover,
            Call::Event,
            Call::Open,
            Call::Event,
            Call::Transact {
                operation: 0x1001,
                params: vec![1]
            },
            Call::Close,
        ]
    );
    assert_eq!(script(&shared).live_leases, 0);
}

#[test]
fn requests_without_a_lease_are_refused_and_do_not_reach_the_boundary() {
    let (out, shared) = run(
        |s| s.open.push_back(Err(IccError::NativeFailure)),
        |mut handle| {
            block(async move {
                (
                    handle.transact(0x1001, vec![], vec![], t()).await,
                    handle.close(t()).await,
                    handle.open(id(), t()).await,
                )
            })
        },
    );
    assert_eq!(out.0, Err(IccError::NotOpen));
    assert_eq!(out.1, Err(IccError::NotOpen));
    assert_eq!(out.2, Err(IccError::NativeFailure));
    assert_eq!(calls(&shared), [Call::Open]);
}

#[test]
fn a_held_lease_refuses_discovery_and_a_second_open() {
    let (out, _) = run(
        |_| {},
        |mut handle| {
            block(async move {
                handle.open(id(), t()).await.unwrap();
                (
                    handle.discover(Duration::from_secs(1), t()).await,
                    handle.open(id(), t()).await,
                )
            })
        },
    );
    assert_eq!(out, (Err(IccError::Busy), Err(IccError::Busy)));
}

#[test]
fn dropping_the_handle_with_a_lease_requests_close_without_confirming() {
    let ((), shared) = run(
        |_| {},
        |mut handle| {
            block(async move {
                handle.open(id(), t()).await.unwrap();
            });
        },
    );
    assert_eq!(calls(&shared), [Call::Open, Call::LeaseDropped]);
    assert_eq!(script(&shared).live_leases, 0);
}

#[test]
fn retiring_failures_give_up_the_lease_and_others_keep_it() {
    let (out, shared) = run(
        |s| {
            s.transact.push_back(done(Err(IccError::NativeFailure)));
            s.transact.push_back(done(Err(IccError::DeadlineExceeded)));
        },
        |mut handle| {
            block(async move {
                handle.open(id(), t()).await.unwrap();
                let native = handle.transact(0x1001, vec![], vec![], t()).await;
                let late = handle.transact(0x1001, vec![], vec![], t()).await;
                let gone = handle.close(t()).await;
                (native, late, gone)
            })
        },
    );
    assert_eq!(
        out,
        (
            Err(IccError::NativeFailure),
            Err(IccError::DeadlineExceeded),
            Err(IccError::NotOpen)
        )
    );
    assert_eq!(transacts(&shared), 2);
    assert!(calls(&shared).contains(&Call::LeaseDropped));
}

#[test]
fn a_cancelled_caller_neither_backlogs_nor_overlaps_native_work() {
    let (entered_tx, entered_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    let (out, shared) = run(
        |s| {
            s.transact.push_back(Step::Hold {
                entered: entered_tx,
                release: release_rx,
                result: Ok(reply(0x2001, &[], &[1])),
            });
            s.transact.push_back(done(Ok(reply(0x2001, &[], &[2]))));
        },
        |mut handle| {
            block(async move {
                handle.open(id(), t()).await.unwrap();
                let slow = tokio::time::timeout(
                    Duration::from_millis(50),
                    handle.transact(0x1001, vec![], vec![], t()),
                )
                .await;
                assert!(slow.is_err(), "the caller stopped waiting");
                entered_rx.recv().expect("native work started");
                // The abandoned operation still runs: nothing else is admitted.
                let during = (
                    handle.poll_event().await,
                    handle.transact(0x1001, vec![], vec![], t()).await,
                    handle.close(t()).await,
                );
                release_tx.send(()).expect("release");
                let settled = loop {
                    match handle.transact(0x1001, vec![], vec![], t()).await {
                        Err(IccError::Busy) => std::thread::sleep(Duration::from_millis(1)),
                        other => break other,
                    }
                };
                // The lease survived the abandoned call, and exactly one op ran per request.
                (during, settled)
            })
        },
    );
    assert_eq!(
        out.0,
        (
            Err(IccError::Busy),
            Err(IccError::Busy),
            Err(IccError::Busy)
        )
    );
    assert_eq!(out.1, Ok(reply(0x2001, &[], &[2])));
    assert_eq!(transacts(&shared), 2);
}

#[test]
fn an_owner_that_dies_mid_request_reports_disconnect_and_releases_the_gate() {
    let (fake, shared) = Fake::new();
    script(&shared).panic_on_discover = true;
    let seen = std::sync::Mutex::new(Vec::new());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_with(fake, |mut handle| {
            let first = block(handle.discover(Duration::from_secs(1), t()));
            // The gate was released, so the failure is stable rather than Busy.
            let second = block(handle.poll_event());
            seen.lock()
                .expect("seen")
                .extend([first.map(|_| ()), second.map(|_| ())]);
        })
    }));
    assert!(result.is_err(), "the owner's panic propagates");
    assert_eq!(
        *seen.lock().expect("seen"),
        [Err(IccError::Disconnected), Err(IccError::Disconnected)]
    );
}

#[test]
fn a_panicking_user_is_reported_not_propagated() {
    let (fake, _) = Fake::new();
    let result = run_with::<_, _, ()>(fake, |_handle: IccHandle| {
        std::panic::resume_unwind(Box::new("synthetic"))
    });
    assert_eq!(result, Err(IccError::Runtime));
}

// ---- session -----------------------------------------------------------------------------

#[test]
fn a_session_opens_transacts_and_closes_with_confirmation() {
    let (out, shared) = run(
        |s| {
            s.transact
                .push_back(done(Ok(reply(0x2001, &[7], &[1, 2, 3]))));
            s.transact.push_back(done(Ok(reply(0x2001, &[], &[4]))));
        },
        |handle| {
            block(async move {
                let mut s = session(handle);
                assert!(!s.is_open());
                assert_eq!(
                    s.close().await,
                    Ok(()),
                    "closing a closed session is a no-op"
                );
                s.open().await.unwrap();
                assert!(s.is_open());
                assert_eq!(s.open().await, Err(SessionError::AlreadyOpen));
                let outcome = s.transaction(0x1009, &[1, 2], Some(&[9])).await.unwrap();
                let info = s.get_device_info().await.unwrap();
                s.close().await.unwrap();
                assert!(!s.is_open());
                // Same process, same bridge: reopen after confirmed close.
                s.open().await.unwrap();
                s.close().await.unwrap();
                let _ = s.into_handle();
                (outcome, info)
            })
        },
    );
    assert_eq!(out.0.params, [7]);
    assert_eq!(out.0.data, [1, 2, 3]);
    assert_eq!(out.1, [4]);
    let log = calls(&shared);
    assert_eq!(log.iter().filter(|c| **c == Call::Close).count(), 2);
    assert!(log.contains(&Call::Transact {
        operation: 0x1001,
        params: vec![]
    }));
}

#[test]
fn get_device_info_reports_its_failure() {
    let (out, _) = run(
        |s| s.transact.push_back(done(Err(IccError::NativeFailure))),
        |handle| {
            block(async move {
                let mut s = session(handle);
                s.open().await.unwrap();
                s.get_device_info().await
            })
        },
    );
    assert_eq!(out, Err(icc(IccError::NativeFailure)));
}

#[test]
fn the_session_clock_is_injected() {
    fn fixed() -> Instant {
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *BASE.get_or_init(Instant::now)
    }
    let (fake, _) = Fake::new();
    run_with(fake, |handle| {
        let s = IccSession::new(handle, id(), SessionConfig::default(), fixed);
        assert_eq!(s.now(), fixed());
        // Debug never prints the device identity.
        assert!(!format!("{s:?}").contains("synthetic-camera"));
    })
    .expect("ran");
}

#[test]
fn raw_session_operations_and_bad_arguments_never_reach_native_code() {
    let config = SessionConfig {
        max_data_out: 4,
        max_data_len: 2,
        ..SessionConfig::default()
    };
    let (out, shared) = run(
        |s| {
            s.transact
                .push_back(done(Ok(reply(0x2001, &[], &[1, 2, 3]))));
        },
        |handle| {
            block(async move {
                let mut s = session_with(handle, config);
                assert_eq!(
                    s.transaction(0x1009, &[], None).await,
                    Err(SessionError::NotOpen)
                );
                s.open().await.unwrap();
                let mut out = Vec::new();
                for operation in [0x1002, 0x1003] {
                    out.push(s.transaction(operation, &[], None).await);
                }
                out.push(s.transaction(0x1009, &[0; 6], None).await);
                out.push(s.transaction(0x1009, &[], Some(&[0; 5])).await);
                out.push(
                    s.transaction_within(Duration::ZERO, 0x1009, &[], None)
                        .await,
                );
                out.push(
                    s.transaction_within(Duration::from_secs(7200), 0x1009, &[], None)
                        .await,
                );
                // Incoming data beyond the configured cap fails without hurting the session.
                out.push(s.transaction(0x1009, &[], None).await);
                assert!(s.is_open());
                out
            })
        },
    );
    let raw = icc(IccError::RawSessionOperation);
    assert_eq!(
        out.iter()
            .map(|r| r.clone().map(|_| ()))
            .collect::<Vec<_>>(),
        [
            Err(raw.clone()),
            Err(raw),
            Err(SessionError::Container(ContainerError::TooManyParams(6))),
            Err(SessionError::DataTooLarge {
                announced: 5,
                limit: 4
            }),
            Err(icc(IccError::AdmissionExpired)),
            Err(icc(IccError::InvalidInput)),
            Err(SessionError::DataTooLarge {
                announced: 3,
                limit: 2
            }),
        ]
    );
    assert_eq!(transacts(&shared), 1);
}

#[test]
fn outgoing_data_is_capped_by_the_native_bound_even_when_configured_higher() {
    let config = SessionConfig {
        max_data_out: usize::MAX,
        ..SessionConfig::default()
    };
    let (out, shared) = run(
        |_| {},
        |handle| {
            block(async move {
                let mut s = session_with(handle, config);
                s.open().await.unwrap();
                let big = vec![0_u8; MAX_DATA_BYTES + 1];
                s.transaction(0x1009, &[], Some(&big)).await
            })
        },
    );
    assert_eq!(
        out,
        Err(SessionError::DataTooLarge {
            announced: MAX_DATA_BYTES + 1,
            limit: MAX_DATA_BYTES
        })
    );
    assert_eq!(transacts(&shared), 0);
}

#[test]
fn camera_refusals_stay_protocol_results() {
    let (out, _) = run(
        |s| {
            s.transact.push_back(done(Ok(reply(0x2019, &[3], &[]))));
        },
        |handle| {
            block(async move {
                let mut s = session(handle);
                s.open().await.unwrap();
                let refused = s.transaction(0x1009, &[], None).await;
                (refused, s.is_open())
            })
        },
    );
    assert_eq!(
        out,
        (
            Err(SessionError::Response {
                code: 0x2019,
                params: vec![3]
            }),
            true
        )
    );
}

#[test]
fn a_malformed_completion_faults_the_session_but_close_still_confirms() {
    let (out, shared) = run(
        |s| {
            s.transact.push_back(done(Ok(reply(0x2001, &[0; 6], &[]))));
            s.transact
                .push_back(done(Err(IccError::MalformedCompletion)));
        },
        |handle| {
            block(async move {
                let mut s = session(handle);
                s.open().await.unwrap();
                let first = s.transaction(0x1009, &[], None).await;
                let after = s.transaction(0x1009, &[], None).await;
                let reopen = s.open().await;
                s.close().await.unwrap();
                s.open().await.unwrap();
                (first, after, reopen)
            })
        },
    );
    assert_eq!(out.0.map(|_| ()), Err(icc(IccError::MalformedCompletion)));
    assert_eq!(out.1.map(|_| ()), Err(SessionError::NotOpen));
    assert_eq!(out.2, Err(SessionError::AlreadyOpen));
    assert_eq!(transacts(&shared), 1);
    assert!(calls(&shared).contains(&Call::Close));
}

#[test]
fn a_command_deadline_retires_the_lease_and_reuse_waits_for_native_cleanup() {
    let (out, shared) = run(
        |s| {
            s.transact.push_back(done(Err(IccError::DeadlineExceeded)));
            // The native owner keeps refusing a replacement until cleanup settles.
            s.open.push_back(Ok(()));
            s.open.push_back(Err(IccError::Busy));
            s.open.push_back(Ok(()));
        },
        |handle| {
            block(async move {
                let mut s = session(handle);
                s.open().await.unwrap();
                let deadline = s.transaction(0x1009, &[], None).await;
                // No duplicate close while native cleanup is unresolved.
                let close = s.close().await;
                let work = s.transaction(0x1009, &[], None).await;
                let busy = s.open().await;
                let again = s.open().await;
                (
                    deadline.map(|_| ()),
                    close,
                    work.map(|_| ()),
                    busy,
                    again,
                    s.is_open(),
                )
            })
        },
    );
    assert_eq!(out.0, Err(icc(IccError::DeadlineExceeded)));
    assert_eq!(out.1, Err(icc(IccError::Busy)));
    assert_eq!(out.2, Err(SessionError::NotOpen));
    assert_eq!(out.3, Err(icc(IccError::Busy)));
    assert_eq!(out.4, Ok(()));
    assert!(out.5);
    let log = calls(&shared);
    assert!(!log.contains(&Call::Close), "never a second close: {log:?}");
    assert_eq!(log.iter().filter(|c| **c == Call::Open).count(), 3);
}

#[test]
fn quarantine_is_terminal_and_submits_nothing_more() {
    let (out, shared) = run(
        |s| s.transact.push_back(done(Err(IccError::Quarantined))),
        |handle| {
            block(async move {
                let mut s = session(handle);
                s.open().await.unwrap();
                let first = s.transaction(0x1009, &[], None).await;
                (
                    first.map(|_| ()),
                    s.transaction(0x1009, &[], None).await.map(|_| ()),
                    s.open().await,
                    s.close().await,
                    s.poll_event().await.map(|_| ()),
                    s.is_open(),
                )
            })
        },
    );
    let q = Err(icc(IccError::Quarantined));
    assert_eq!(out.0, q);
    assert_eq!(out.1, q);
    assert_eq!(out.2, q);
    assert_eq!(out.3, q);
    assert_eq!(out.4, q);
    assert!(!out.5);
    assert_eq!(transacts(&shared), 1);
    assert_eq!(
        calls(&shared).iter().filter(|c| **c == Call::Open).count(),
        1
    );
}

#[test]
fn admission_expiry_and_gate_busy_leave_the_session_usable() {
    let (out, _) = run(
        |s| {
            s.transact.push_back(done(Err(IccError::AdmissionExpired)));
            s.transact.push_back(done(Err(IccError::InvalidInput)));
        },
        |handle| {
            block(async move {
                let mut s = session(handle);
                s.open().await.unwrap();
                let a = s.transaction(0x1009, &[], None).await.map(|_| ());
                let b = s.transaction(0x1009, &[], None).await.map(|_| ());
                let c = s.transaction(0x1009, &[], None).await.map(|_| ());
                (a, b, c, s.is_open())
            })
        },
    );
    assert_eq!(out.0, Err(icc(IccError::AdmissionExpired)));
    assert_eq!(out.1, Err(icc(IccError::InvalidInput)));
    assert_eq!(out.2, Ok(()));
    assert!(out.3);
}

#[test]
fn open_failures_map_to_the_right_state() {
    let (out, _) = run(
        |s| {
            s.open.push_back(Err(IccError::NativeFailure));
            s.open.push_back(Err(IccError::DeadlineExceeded));
            s.open.push_back(Err(IccError::Quarantined));
        },
        |handle| {
            block(async move {
                let mut s = session(handle);
                // A failed open leaves nothing open, so it can simply be retried.
                let failed = s.open().await;
                // After a deadline the open is retained natively; the state is Retired.
                let late = s.open().await;
                let retired = s.transaction(0x1009, &[], None).await.map(|_| ());
                let quarantined = s.open().await;
                let terminal = s.open().await;
                (failed, late, retired, quarantined, terminal)
            })
        },
    );
    assert_eq!(out.0, Err(icc(IccError::NativeFailure)));
    assert_eq!(out.1, Err(icc(IccError::DeadlineExceeded)));
    assert_eq!(out.2, Err(SessionError::NotOpen));
    assert_eq!(out.3, Err(icc(IccError::Quarantined)));
    assert_eq!(out.4, Err(icc(IccError::Quarantined)));
}

#[test]
fn close_failures_retire_or_quarantine_and_never_reissue_a_close() {
    let cases: [(Result<(), IccError>, SessionError, bool); 5] = [
        (
            Err(IccError::DeadlineExceeded),
            icc(IccError::DeadlineExceeded),
            false,
        ),
        (
            Err(IccError::AdmissionExpired),
            icc(IccError::AdmissionExpired),
            false,
        ),
        (Err(IccError::NotOpen), icc(IccError::NotOpen), false),
        (Err(IccError::Quarantined), icc(IccError::Quarantined), true),
        // Any other native failure to close is a cleanup failure and quarantines.
        (
            Err(IccError::NativeFailure),
            icc(IccError::CleanupFailure),
            true,
        ),
    ];
    for (scripted, expected, terminal) in cases {
        let (out, shared) = run(
            |s| s.close.push_back(scripted),
            |handle| {
                block(async move {
                    let mut s = session(handle);
                    s.open().await.unwrap();
                    let first = s.close().await;
                    let second = s.close().await;
                    (first, second)
                })
            },
        );
        assert_eq!(out.0, Err(expected.clone()));
        let again = if terminal {
            icc(IccError::Quarantined)
        } else {
            icc(IccError::Busy)
        };
        assert_eq!(out.1, Err(again));
        let closes = calls(&shared).iter().filter(|c| **c == Call::Close).count();
        assert_eq!(closes, 1, "{expected:?}");
    }
}

#[test]
fn a_gate_busy_close_can_be_retried_once_the_abandoned_work_settles() {
    let (entered_tx, entered_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    let (out, shared) = run(
        |s| {
            s.transact.push_back(Step::Hold {
                entered: entered_tx,
                release: release_rx,
                result: Ok(ok_reply()),
            });
        },
        |handle| {
            block(async move {
                let mut s = session(handle);
                s.open().await.unwrap();
                let slow = tokio::time::timeout(
                    Duration::from_millis(50),
                    s.transaction(0x1009, &[], None),
                )
                .await;
                assert!(slow.is_err());
                entered_rx.recv().expect("started");
                let busy = s.close().await;
                release_tx.send(()).expect("release");
                loop {
                    match s.close().await {
                        Err(SessionError::Icc(IccError::Busy)) => {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        other => return (busy, other),
                    }
                }
            })
        },
    );
    assert_eq!(out, (Err(icc(IccError::Busy)), Ok(())));
    assert_eq!(
        calls(&shared).iter().filter(|c| **c == Call::Close).count(),
        1
    );
}

#[test]
fn an_oversized_transaction_timeout_is_rejected_before_submission() {
    let config = SessionConfig {
        transaction_timeout: Duration::from_secs(7200),
        ..SessionConfig::default()
    };
    let (out, shared) = run(
        |_| {},
        |handle| {
            block(async move {
                let mut s = session_with(handle, config);
                s.open().await
            })
        },
    );
    assert_eq!(out, Err(icc(IccError::InvalidInput)));
    assert_eq!(calls(&shared), Vec::<Call>::new());

    let config = SessionConfig {
        transaction_timeout: Duration::ZERO,
        ..SessionConfig::default()
    };
    let (out, _) = run(
        |_| {},
        |handle| {
            block(async move {
                let mut s = session_with(handle, config);
                s.open().await
            })
        },
    );
    assert_eq!(out, Err(icc(IccError::InvalidInput)));
}

#[test]
fn closing_with_an_unrepresentable_timeout_is_rejected() {
    let (out, _) = run(
        |_| {},
        |handle| {
            block(async move {
                let mut s = session(handle);
                s.open().await.unwrap();
                s.config.transaction_timeout = Duration::ZERO;
                s.close().await
            })
        },
    );
    assert_eq!(out, Err(icc(IccError::InvalidInput)));
}

// ---- events ------------------------------------------------------------------------------

#[test]
fn ptp_events_arrive_and_advisory_observations_are_skipped() {
    let (out, shared) = run(
        |s| {
            s.lease_events.push_back(Ok(Some(vec![3])));
            s.lease_events.push_back(Ok(Some(vec![8])));
            s.lease_events.push_back(Ok(Some(ptp_event(0xC201, &[5]))));
        },
        |handle| {
            block(async move {
                let mut s = session(handle);
                assert_eq!(s.poll_event().await, Err(SessionError::NotOpen));
                s.open().await.unwrap();
                let event = s.poll_event_within(Duration::ZERO).await;
                // Empty queue: one poll with a zero wait, several while waiting.
                let none = s.poll_event_within(Duration::from_millis(25)).await;
                let default_wait = s.poll_event().await;
                (event, none, default_wait, s.is_open())
            })
        },
    );
    assert_eq!(
        out.0,
        Ok(Some(Event {
            code: 0xC201,
            transaction_id: 0,
            params: vec![5]
        }))
    );
    assert_eq!((out.1, out.2), (Ok(None), Ok(None)));
    assert!(out.3);
    let polls = calls(&shared).iter().filter(|c| **c == Call::Event).count();
    assert!(polls > 5, "waiting polls repeatedly: {polls}");
}

#[test]
fn a_flood_of_advisory_events_is_bounded_per_poll() {
    let (out, shared) = run(
        |s| {
            for _ in 0..70 {
                s.lease_events.push_back(Ok(Some(vec![1])));
            }
        },
        |handle| {
            block(async move {
                let mut s = session(handle);
                s.open().await.unwrap();
                s.poll_event_within(Duration::from_secs(10)).await
            })
        },
    );
    assert_eq!(out, Ok(None));
    assert_eq!(
        calls(&shared).iter().filter(|c| **c == Call::Event).count(),
        64
    );
}

#[test]
fn event_failures_fail_closed_with_the_right_effect() {
    let cases = [
        (Ok(Some(vec![5])), IccError::Removed, true),
        (Ok(Some(vec![10])), IccError::Permission, false),
        (Ok(Some(vec![6])), IccError::NativeFailure, false),
        (Ok(Some(vec![12])), IccError::EventOverflow, false),
        (Ok(Some(vec![99])), IccError::MalformedEvent, false),
        (Ok(Some(vec![11, 1, 2])), IccError::MalformedEvent, false),
        (Err(IccError::EventOverflow), IccError::EventOverflow, false),
    ];
    for (event, expected, quarantined) in cases {
        let (out, shared) = run(
            |s| s.lease_events.push_back(event),
            |handle| {
                block(async move {
                    let mut s = session(handle);
                    s.open().await.unwrap();
                    let failed = s.poll_event_within(Duration::ZERO).await;
                    let work = s.transaction(0x1009, &[], None).await.map(|_| ());
                    let close = s.close().await;
                    (failed, work, close)
                })
            },
        );
        assert_eq!(out.0, Err(icc(expected)), "{expected:?}");
        if quarantined {
            // Removal: nothing more is submitted, not even a close.
            assert_eq!(out.1, Err(icc(IccError::Quarantined)));
            assert_eq!(out.2, Err(icc(IccError::Quarantined)));
            assert_eq!(transacts(&shared), 0);
        } else {
            // Event loss faults the session: no transactions, but close still confirms.
            assert_eq!(out.1, Err(SessionError::NotOpen));
            assert_eq!(out.2, Ok(()));
        }
    }
}

#[test]
fn polling_failures_from_the_bridge_apply_their_effect() {
    let (out, _) = run(
        |s| s.lease_events.push_back(Err(IccError::Disconnected)),
        |handle| {
            block(async move {
                let mut s = session(handle);
                s.open().await.unwrap();
                (s.poll_event_within(Duration::MAX).await, s.is_open())
            })
        },
    );
    assert_eq!(out, (Err(icc(IccError::Disconnected)), false));
}

// ---- end to end --------------------------------------------------------------------------

fn sony_over_icc(max_version: u16) {
    let sim = SonySimConfig {
        max_version,
        ..SonySimConfig::default()
    };
    let boundary = SimBoundary::new(sony_camera(sim));
    let out = run_with(boundary, |handle| {
        block(async move {
            let mut session = session(handle);
            let config = SonyConfig {
                poll_interval: Duration::ZERO,
                ..SonyConfig::default()
            };
            let result = SonyCamera::run(&mut session, config, async |camera| {
                assert_eq!(camera.device_info().model, "ILCE-SIM");
                camera.refresh_properties().await.expect("refresh");
                // The handshake wrote priority mode; the camera confirms it with an event.
                let events = camera.pump_events().await.expect("events");
                assert_eq!(events, [SonyEvent::PropertyChanged(prop::PRIORITY_MODE)]);
                camera.wait_live_view_ready().await.expect("ready");
                let frame = camera.next_live_view_frame().await.expect("frame");
                assert_eq!(&frame[..2], [0xff, 0xd8]);
                Ok(())
            })
            .await;
            let closed = !session.is_open();
            // Reopen on the same bridge after the confirmed close.
            let again = SonyCamera::run(&mut session, config_fast(), async |_| Ok(())).await;
            (result, closed, again)
        })
    })
    .expect("owner ran");
    assert_eq!(out.0, Ok(()));
    assert!(out.1);
    assert_eq!(out.2, Ok(()));
}

fn config_fast() -> SonyConfig {
    SonyConfig {
        poll_interval: Duration::ZERO,
        ..SonyConfig::default()
    }
}

#[test]
fn the_sony_driver_runs_unchanged_over_icc_for_both_protocol_generations() {
    sony_over_icc(200);
    sony_over_icc(300);
}

#[test]
fn the_simulated_boundary_models_discovery_and_exclusive_ownership() {
    let boundary = SimBoundary::new(sony_camera(SonySimConfig::default()));
    let out = run_with(boundary, |mut handle| {
        block(async move {
            let found = handle.discover(Duration::from_secs(1), t()).await;
            let idle = handle.poll_event().await;
            handle.open(id(), t()).await.unwrap();
            let held = handle.open(id(), t()).await;
            (
                found.map(|f| f.len()),
                idle,
                held,
                handle.poll_event().await,
            )
        })
    })
    .expect("ran");
    assert_eq!(out.0, Ok(1));
    assert_eq!(out.1, Ok(None));
    assert_eq!(out.2, Err(IccError::Busy));
    assert_eq!(out.3, Ok(None));
}

#[test]
fn simulated_camera_refusals_and_faults_surface_as_the_right_categories() {
    let mut camera = sony_camera(SonySimConfig::default());
    camera.inject(Endpoint::BulkOut, 0, Fault::Disconnect);
    let out = run_with(SimBoundary::new(camera), |mut handle| {
        block(async move { handle.open(id(), t()).await })
    })
    .expect("ran");
    assert_eq!(out, Err(IccError::NativeFailure));

    // An unsupported operation is the camera's refusal: a response, not a failure.
    let boundary = SimBoundary::new(sony_camera(SonySimConfig::default()));
    let out = run_with(boundary, |mut handle| {
        block(async move {
            handle.open(id(), t()).await.unwrap();
            handle.transact(0x9999, vec![], vec![], t()).await
        })
    })
    .expect("ran");
    assert_eq!(out.expect("a response").code, 0x2005);

    // Losing the cable after the session opened fails the next command, the poll and the close.
    let mut camera = sony_camera(SonySimConfig::default());
    camera.inject(Endpoint::BulkOut, 1, Fault::Disconnect);
    camera.inject(Endpoint::InterruptIn, 0, Fault::Disconnect);
    let out = run_with(SimBoundary::new(camera), |mut handle| {
        block(async move {
            handle.open(id(), t()).await.unwrap();
            (
                handle.poll_event().await,
                handle
                    .transact(0x1001, vec![], vec![], t())
                    .await
                    .map(|_| ()),
            )
        })
    })
    .expect("ran");
    assert_eq!(out.0, Err(IccError::NativeFailure));
    assert_eq!(out.1, Err(IccError::NativeFailure));

    let mut camera = sony_camera(SonySimConfig::default());
    camera.inject(Endpoint::BulkOut, 1, Fault::Disconnect);
    let out = run_with(SimBoundary::new(camera), |mut handle| {
        block(async move {
            handle.open(id(), t()).await.unwrap();
            handle.close(t()).await
        })
    })
    .expect("ran");
    assert_eq!(out, Err(IccError::NativeFailure));
}

// ---- sample ------------------------------------------------------------------------------

fn sample(setup: impl FnOnce(&mut Script), cycles: usize) -> (super::Receipt, Shared) {
    run(setup, move |handle| {
        block(async move { read_only_sample(handle, &id(), cycles).await })
    })
}

#[test]
fn the_read_only_sample_counts_cycles_and_prints_nothing_identifying() {
    let (receipt, shared) = sample(
        |s| {
            s.discover.push_back(Ok(vec![id()]));
            s.transact.push_back(done(Ok(reply(0x2001, &[], &[1]))));
            s.transact.push_back(done(Ok(reply(0x2001, &[], &[2]))));
        },
        2,
    );
    assert!(receipt.passed(2));
    assert_eq!((receipt.candidates, receipt.confirmed_closes), (1, 2));
    assert_eq!(receipt.response_codes, [0x2001, 0x2001]);
    let mut out = Vec::new();
    receipt.write_to(&mut out).expect("write");
    assert_eq!(
        String::from_utf8(out).expect("utf8"),
        "candidates=1 cycles=2 device_info_ok=2 confirmed_closes=2 \
         response_codes=0x2001,0x2001 result=ok\n"
    );
    // Only discovery, open, GetDeviceInfo and close ever reach the boundary.
    let operations: Vec<_> = calls(&shared)
        .into_iter()
        .filter_map(|call| match call {
            Call::Transact { operation, .. } => Some(operation),
            _ => None,
        })
        .collect();
    assert_eq!(operations, [0x1001, 0x1001]);
    assert!(!calls(&shared).contains(&Call::LeaseDropped));
}

#[test]
fn the_sample_stops_at_the_first_failure_and_still_closes() {
    let selection = |found: Vec<IccDeviceId>| sample(|s| s.discover.push_back(Ok(found)), 1);
    let (none, shared) = selection(Vec::new());
    assert_eq!(none.failure, Some(IccError::NotFound.into()));
    assert!(!calls(&shared).contains(&Call::Open));
    let (many, _) = selection(vec![id(), id()]);
    assert_eq!(many.failure, Some(IccError::Ambiguous.into()));
    assert_eq!(many.candidates, 2);

    let (failed, _) = sample(|s| s.discover.push_back(Err(IccError::Busy)), 1);
    assert_eq!(
        failed.failure.as_ref().map(super::SampleError::category),
        Some("discovery")
    );
    assert!(!failed.passed(1));

    let (refused, shared) = sample(
        |s| {
            s.discover.push_back(Ok(vec![id()]));
            s.transact.push_back(done(Ok(reply(0x2019, &[], &[]))));
        },
        2,
    );
    assert_eq!(refused.response_codes, [0x2019]);
    assert_eq!(refused.confirmed_closes, 1, "closed even after a refusal");
    assert_eq!(
        refused.failure.as_ref().map(super::SampleError::category),
        Some("session")
    );
    assert!(calls(&shared).contains(&Call::Close));
    let mut out = Vec::new();
    refused.write_to(&mut out).expect("write");
    assert!(
        String::from_utf8(out)
            .expect("utf8")
            .ends_with("result=failed category=session\n")
    );

    let (empty, _) = sample(
        |s| {
            s.discover.push_back(Ok(vec![id()]));
            s.transact.push_back(done(Ok(reply(0x2001, &[], &[]))));
        },
        1,
    );
    assert_eq!(empty.failure, Some(super::SampleError::EmptyDeviceInfo));
    assert_eq!(
        empty.failure.as_ref().map(super::SampleError::category),
        Some("empty_device_info")
    );

    let (broken, _) = sample(
        |s| {
            s.discover.push_back(Ok(vec![id()]));
            s.transact.push_back(done(Err(IccError::NativeFailure)));
        },
        1,
    );
    assert_eq!(broken.response_codes, Vec::<u16>::new());
    assert_eq!(
        broken.confirmed_closes, 1,
        "a faulted session still closes with confirmation"
    );
    assert_eq!(
        broken.failure.as_ref().map(super::SampleError::category),
        Some("session")
    );

    let (open_failed, _) = sample(
        |s| {
            s.discover.push_back(Ok(vec![id()]));
            s.open.push_back(Err(IccError::NativeFailure));
        },
        1,
    );
    assert_eq!(open_failed.cycles, 1);
    assert_eq!(open_failed.confirmed_closes, 0);

    let (close_failed, _) = sample(
        |s| {
            s.discover.push_back(Ok(vec![id()]));
            s.close.push_back(Err(IccError::NativeFailure));
        },
        1,
    );
    assert_eq!(close_failed.device_info_ok, 0);

    assert_eq!(close_failed.confirmed_closes, 0);
    assert_eq!(close_failed.response_codes, [0x2001]);
    assert!(!close_failed.passed(1));
}

#[test]
fn an_unfinished_sample_does_not_pass() {
    let receipt = super::Receipt::default();
    assert!(!receipt.passed(1));
    assert!(receipt.passed(0));
    let mut out = Vec::new();
    receipt.write_to(&mut out).expect("write");
    assert_eq!(
        String::from_utf8(out).expect("utf8"),
        "candidates=0 cycles=0 device_info_ok=0 confirmed_closes=0 response_codes= result=ok\n"
    );
    let mut broken = [0_u8; 0];
    assert!(receipt.write_to(&mut broken.as_mut_slice()).is_err());
}
