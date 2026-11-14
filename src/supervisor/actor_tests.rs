//! Deterministic session lifecycle, snapshots and reconnect tests.
//!
//! References:
//! - `docs/architecture.md`, CLI and supervisor contract

#[test]
fn snapshots_accept_jpeg_but_never_label_raw_objects_as_jpeg() {
    let logs = LogCapture::new();
    tracing::dispatcher::with_default(&logs.dispatch(), || {
        for (format, data) in [
            (0x3801, vec![0xff, 0xd8, 0xff, 0xd9]),
            (0x3000, vec![0xff, 0xd8]),
            (0x3801, b"synthetic-snapshot-secret".to_vec()),
        ] {
            let image = CapturedImage {
                info: crate::ptp::ObjectInfo {
                    object_format: format,
                    ..crate::ptp::ObjectInfo::default()
                },
                data: data.clone(),
            };
            let result = snapshot_image(image);
            assert_eq!(
                result.is_ok(),
                format == 0x3801 && data.starts_with(&[0xff, 0xd8])
            );
        }
    });
    let logs = logs.text();
    assert_eq!(logs.matches("snapshot_format_rejected").count(), 2);
    assert!(!logs.contains("[255, 216"));
    assert!(!logs.contains("synthetic-snapshot-secret"));
}

proptest::proptest! {
    #[test]
    fn snapshot_bytes_never_panic(data in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..4096), format in proptest::prelude::any::<u16>()) {
        let _ = snapshot_image(CapturedImage { info: crate::ptp::ObjectInfo { object_format: format, ..crate::ptp::ObjectInfo::default() }, data });
    }
}

#[tokio::test]
async fn raw_snapshot_rejection_keeps_the_same_camera_session_usable() {
    let sim = sony_camera(SonySimConfig {
        warmup_reads: 0,
        still: vec![1, 2, 3],
        ..SonySimConfig::default()
    });
    let (actor, control) = actor(Factory::new(vec![sim]));
    let actor = actor.with_still_snapshots(true);
    let (factory, ()) = tokio::join!(Box::pin(actor.run()), async {
        ready(&control).await;
        for _ in 0..2 {
            let control = control.clone();
            assert_eq!(
                tokio::task::spawn_blocking(move || control.snapshot("main"))
                    .await
                    .unwrap(),
                Err(SnapshotError)
            );
        }
        assert!(format!("{:?}", control.status()).contains("Ready"));
        control.shutdown();
    });
    assert_eq!(factory.released.lock().unwrap().len(), 1);
    assert_eq!(factory.released.lock().unwrap()[0].reset_count(), 0);
}

#[tokio::test]
async fn stop_during_an_in_flight_frame_finishes_retries_then_closes() {
    let (actor, control) = actor(Factory::new(vec![sony_camera(SonySimConfig {
        warmup_reads: 0,
        live_view_denied: 2,
        ..SonySimConfig::default()
    })]));
    let (factory, ()) = tokio::join!(Box::pin(actor.run()), async {
        tokio::time::sleep(Duration::from_millis(1)).await;
        control.shutdown();
    });
    let cameras = factory.released.lock().unwrap();
    assert_eq!(cameras[0].reset_count(), 0);
    assert_eq!(
        cameras[0].commands().last().unwrap().code,
        crate::ptp::session::code::CLOSE_SESSION
    );
    assert!(
        cameras[0]
            .commands()
            .iter()
            .filter(|c| c.code == 0x1009)
            .count()
            >= 3
    );
}

#[tokio::test(start_paused = true)]
async fn frame_event_and_telemetry_transport_failures_reconnect_without_cancellation() {
    use crate::transport::sim::sony::Priority;
    use crate::transport::{Endpoint, sim::Fault};
    let cfg = SonySimConfig {
        priority: Priority::Absent,
        warmup_reads: 0,
        live_view_jpeg: crate::supervisor::testing::jpeg(32, 32),
        ..SonySimConfig::default()
    };
    let (baseline, control) = actor(Factory::new(vec![sony_camera(cfg.clone())]));
    let (baseline, ()) = tokio::join!(Box::pin(baseline.run()), async {
        ready(&control).await;
        control.shutdown();
    });
    let startup_commands = baseline.released.lock().unwrap()[0].commands().len() - 1;
    for mode in 0..3 {
        let mut camera = sony_camera(cfg.clone());
        let (endpoint, after) = if mode == 1 {
            (Endpoint::InterruptIn, 0)
        } else {
            (Endpoint::BulkOut, startup_commands)
        };
        camera.inject(endpoint, after, Fault::Disconnect);
        let mut factory = Factory::new(vec![camera]);
        if mode == 0 {
            for _ in 0..8 {
                factory.steps.push_front(Ok(None));
            }
        }
        let pauses = Arc::clone(&factory.pauses);
        let (actor, control) = super::tests::actor(factory);
        let (factory, ()) = tokio::join!(Box::pin(actor.run()), async {
            ready(&control).await;
            let mut status = control.status.clone();
            status.borrow_and_update();
            if mode == 0 {
                control.demand.send_replace([1, 0]);
            }
            status.changed().await.unwrap();
            assert_eq!(*status.borrow(), CameraStatus::Disconnected);
            assert_eq!(
                pauses.lock().unwrap().last(),
                Some(&Duration::from_millis(100))
            );
            control.shutdown();
        });
        assert_eq!(factory.released.lock().unwrap().len(), 1);
        assert_eq!(factory.released.lock().unwrap()[0].reset_count(), 0);
    }
}

use crate::transport::sim::sony::LiveViewMode;

#[tokio::test(start_paused = true)]
async fn readiness_and_first_frame_errors_close_before_retry() {
    let unsupported = sony_camera(SonySimConfig {
        live_view: LiveViewMode::Unsupported,
        ..SonySimConfig::default()
    });
    let (actor, control) = actor(Factory::new(vec![unsupported, camera()]));
    let (factory, ()) = tokio::join!(Box::pin(actor.run()), async {
        ready(&control).await;
        control.shutdown();
    });
    assert_eq!(factory.released.lock().unwrap().len(), 2);
    let (mut actor, control) = super::tests::actor(Factory::new(vec![camera()]));
    actor.sony.live_view_timeout = Duration::ZERO;
    let released = Arc::clone(&actor.factory.released);
    let (factory, ()) = tokio::join!(Box::pin(actor.run()), async {
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(released.lock().unwrap().len(), 1);
        control.shutdown();
    });
    assert_eq!(factory.released.lock().unwrap()[0].reset_count(), 0);
}

#[tokio::test]
async fn stop_during_readiness_finishes_the_operation_then_closes() {
    let (actor, control) = actor(Factory::new(vec![sony_camera(SonySimConfig::default())]));
    let (factory, ()) = tokio::join!(Box::pin(actor.run()), async {
        tokio::time::sleep(Duration::from_millis(1)).await;
        control.shutdown();
    });
    let cameras = factory.released.lock().unwrap();
    assert_eq!(
        cameras[0].commands().last().unwrap().code,
        crate::ptp::session::code::CLOSE_SESSION
    );
    assert_eq!(cameras[0].reset_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn shutdown_does_not_cancel_a_shutter_wait_or_skip_release() {
    let no_image = sony_camera(SonySimConfig {
        warmup_reads: 0,
        shutter: Shutter::NoImage,
        ..SonySimConfig::default()
    });
    let (mut actor, control) = actor(Factory::new(vec![no_image]));
    actor.still_snapshots.store(true, Ordering::Relaxed);
    actor.sony.capture_timeout = Duration::from_millis(100);
    actor.sony.poll_interval = Duration::from_millis(10);
    let (factory, ()) = tokio::join!(
        Box::pin(
            actor
                .with_clock(|| tokio::time::Instant::now().into_std())
                .run()
        ),
        async {
            ready(&control).await;
            let (reply, result) = blocking::sync_channel(1);
            control.snapshots.try_send(reply).unwrap();
            tokio::time::sleep(Duration::from_millis(1)).await;
            control.shutdown();
            tokio::time::sleep(Duration::from_millis(110)).await;
            assert_eq!(result.try_recv().unwrap(), Err(SnapshotError));
        }
    );
    let cameras = factory.released.lock().unwrap();
    let camera = &cameras[0];
    assert_eq!(camera.reset_count(), 0);
    assert_eq!(
        camera.commands().last().unwrap().code,
        crate::ptp::session::code::CLOSE_SESSION
    );
    let controls: Vec<_> = camera
        .commands()
        .iter()
        .filter(|c| c.code == crate::sony::op::SDIO_CONTROL_DEVICE)
        .collect();
    assert_eq!(controls.len(), 4); // half/full press, then both releases before close
    assert_eq!(
        camera
            .received_data()
            .iter()
            .rev()
            .take(2)
            .cloned()
            .collect::<Vec<_>>(),
        vec![vec![1, 0], vec![1, 0]]
    );
}

#[test]
fn snapshot_reply_timeout_does_not_cancel_the_queued_operation() {
    let (actor, control) = actor(Factory::new(Vec::new()));
    assert_eq!(control.request_snapshot(Duration::ZERO), Err(SnapshotError));
    actor.frames.send_replace(Some(Arc::from(vec![1_u8])));
    assert_eq!(control.request_snapshot(Duration::ZERO), Err(SnapshotError));
}

use tracing::instrument::WithSubscriber;

use super::*;
use crate::supervisor::testing::{Factory, LogCapture, camera};
use crate::transport::sim::sony::{Shutter, SonySimConfig, sony_camera};

fn actor(factory: Factory) -> (Supervisor<Factory>, Control) {
    Supervisor::new(
        factory,
        SonyConfig::default(),
        SessionConfig::default(),
        Duration::from_millis(10),
    )
    .unwrap()
}

async fn ready(control: &Control) {
    let mut status = control.status.clone();
    while !matches!(*status.borrow_and_update(), CameraStatus::Ready { .. }) {
        status.changed().await.unwrap();
    }
}

#[test]
fn validates_intervals_and_debug_only_reports_telemetry() {
    for interval in [Duration::ZERO, Duration::from_secs(2)] {
        assert_eq!(
            std::mem::discriminant(
                &Supervisor::new(
                    Factory::new(Vec::new()),
                    SonyConfig::default(),
                    SessionConfig::default(),
                    interval
                )
                .err()
                .unwrap()
            ),
            std::mem::discriminant(&SupervisorError::FrameInterval)
        );
    }
    let (_, control) = actor(Factory::new(Vec::new()));
    assert_eq!(control.status(), CameraStatus::Disconnected);
    assert!(format!("{control:?}").contains("Disconnected"));
    assert_eq!(control.snapshot("main"), Err(SnapshotError));
    assert_eq!(control.counters().snapshots_refused, 1);
}

#[tokio::test(start_paused = true)]
async fn stop_before_start_and_after_factory_open_never_open_sessions() {
    let (actor, control) = actor(Factory::new(vec![camera()]));
    control.shutdown();
    let factory = actor.with_clock(Instant::now).run().await;
    assert_eq!(factory.steps.len(), 1);
    assert_eq!(control.status(), CameraStatus::Stopped);
    let (mut actor, control) = super::tests::actor(Factory::new(vec![camera()]));
    actor.factory.stop_on_connect = Some(control.stop.clone());
    let factory = actor.run().await;
    assert_eq!(factory.released.lock().unwrap()[0].commands(), []);
}

#[tokio::test(start_paused = true)]
async fn reconnects_with_capped_backoff_and_closes_failed_handshakes() {
    let logs = LogCapture::new();
    let bad = sony_camera(SonySimConfig {
        model: "synthetic-private-model".into(),
        advertise_sdio: false,
        ..SonySimConfig::default()
    });
    let mut factory = Factory::new(vec![bad, camera()]);
    for _ in 0..8 {
        factory.steps.push_front(Ok(None));
    }
    factory.steps.push_front(Err(OpenError::Disconnected));
    let pauses = Arc::clone(&factory.pauses);
    let (actor, control) = actor(factory);
    let work = async {
        ready(&control).await;
        assert!(pauses.lock().unwrap().contains(&Duration::from_secs(5)));
        control.shutdown();
    };
    let (factory, ()) = tokio::join!(actor.run().with_subscriber(logs.dispatch()), work);
    let counts = control.counters();
    // Eight absent cameras and one open error, then a handshake that fails, then a good one.
    assert_eq!(
        (
            counts.connect_misses,
            counts.session_restarts,
            counts.reconnects
        ),
        (9, 1, 0)
    );
    assert!(counts.frames_fetched >= 1);
    let logs = logs.text();
    assert_eq!(logs.matches("camera_open_failed").count(), 1);
    assert!(logs.contains("reason=camera disconnected"));
    assert_eq!(logs.matches("camera_disconnected").count(), 1);
    assert_eq!(logs.matches("camera_ready").count(), 1);
    assert!(!logs.contains("synthetic-private-model"));
    let cameras = factory.released.lock().unwrap();
    assert_eq!(cameras.len(), 2);
    for camera in cameras.iter() {
        assert_eq!(camera.reset_count(), 0);
        assert_eq!(
            camera.commands().last().unwrap().code,
            crate::ptp::session::code::CLOSE_SESSION
        );
    }
}

/// The shared factory with a backend-specific pause after a refused handshake.
struct Refusing(Factory);

impl CameraFactory for Refusing {
    type Camera = <Factory as CameraFactory>::Camera;
    type Session = <Factory as CameraFactory>::Session;

    fn connect(&mut self) -> impl Future<Output = Result<Option<Self::Camera>, OpenError>> + Send {
        self.0.connect()
    }
    fn session(
        &self, camera: Self::Camera, cfg: SessionConfig, clock: fn() -> Instant,
    ) -> Self::Session {
        self.0.session(camera, cfg, clock)
    }
    fn recover(&self, session: Self::Session) -> Self::Camera {
        self.0.recover(session)
    }
    fn release(&mut self, camera: Self::Camera) {
        self.0.release(camera);
    }
    fn pause(&self, delay: Duration) -> impl Future<Output = ()> + Send {
        self.0.pause(delay)
    }
    fn refusal_delay(&self) -> Option<Duration> {
        Some(Duration::from_secs(60))
    }
}

#[tokio::test(start_paused = true)]
async fn a_refused_handshake_waits_the_backends_refusal_delay_not_the_short_backoff() {
    let logs = LogCapture::new();
    let refused = || {
        sony_camera(SonySimConfig {
            advertise_sdio: false,
            ..SonySimConfig::default()
        })
    };
    let factory = Factory::new(vec![refused(), refused(), camera()]);
    let pauses = Arc::clone(&factory.pauses);
    let (actor, control) = Supervisor::new(
        Refusing(factory),
        SonyConfig::default(),
        SessionConfig::default(),
        Duration::from_millis(10),
    )
    .unwrap();
    let work = async {
        ready(&control).await;
        control.shutdown();
    };
    let (_, ()) = tokio::join!(actor.run().with_subscriber(logs.dispatch()), work);
    // Two refusals paused the full delay; the ordinary backoff did not apply to them.
    assert_eq!(
        pauses.lock().unwrap()[..2],
        [Duration::from_secs(60), Duration::from_secs(60)]
    );
    assert_eq!(logs.text().matches("camera_disconnected").count(), 2);
    // A refused handshake is a session that ended with an error, not a missed connection, and
    // the first session that reaches live view afterwards is not a reconnect.
    let counts = control.counters();
    assert_eq!(
        (
            counts.session_restarts,
            counts.connect_misses,
            counts.reconnects
        ),
        (2, 0, 0)
    );
}

#[tokio::test(start_paused = true)]
async fn a_failure_that_is_not_a_refusal_keeps_the_ordinary_backoff() {
    // A backend refusal delay applies only to a refused handshake, never to a lost camera.
    let factory = Factory::new(vec![camera()]);
    let pauses = Arc::clone(&factory.pauses);
    let mut factory = factory;
    factory.steps.push_front(Err(OpenError::Disconnected));
    let (actor, control) = Supervisor::new(
        Refusing(factory),
        SonyConfig::default(),
        SessionConfig::default(),
        Duration::from_millis(10),
    )
    .unwrap();
    let work = async {
        ready(&control).await;
        control.shutdown();
    };
    tokio::join!(actor.run(), work);
    assert_eq!(pauses.lock().unwrap()[0], Duration::from_millis(100));
}

#[tokio::test]
async fn serializes_live_frames_and_full_resolution_snapshots_then_reads_power() {
    let (actor, control) = actor(Factory::new(vec![camera()]));
    let actor = actor.with_still_snapshots(true);
    let work = async {
        ready(&control).await;
        control.demand.send_replace([1, 1]);
        let mut frames = control.frames.clone();
        frames.changed().await.unwrap();
        let snapshot_control = control.clone();
        let still = tokio::task::spawn_blocking(move || snapshot_control.snapshot("main"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still, crate::supervisor::testing::jpeg(64, 64));
        control.demand.send_replace([0, 0]);
        // A subsequent refresh, not an initial zero-valued warm-up battery read.
        tokio::time::sleep(Duration::from_millis(5100)).await;
        assert!(format!("{:?}", control.status()).contains("battery: Some"));
        control.shutdown();
    };
    let (factory, ()) = tokio::join!(Box::pin(actor.run()), work);
    assert_eq!(factory.released.lock().unwrap()[0].reset_count(), 0);
}

#[tokio::test]
async fn failed_still_is_refused_and_the_stream_stays_up() {
    let unsupported = sony_camera(SonySimConfig {
        warmup_reads: 0,
        shutter: Shutter::NoControls,
        ..SonySimConfig::default()
    });
    // The warning's enabled branch only runs, and is only covered, under a live subscriber.
    let logs = LogCapture::new();
    let (actor, control) = actor(Factory::new(vec![unsupported]));
    let actor = actor.with_still_snapshots(true);
    let released = Arc::clone(&actor.factory.released);
    let work = async {
        ready(&control).await;
        let snapshot_control = control.clone();
        assert_eq!(
            tokio::task::spawn_blocking(move || snapshot_control.snapshot("main"))
                .await
                .unwrap(),
            Err(SnapshotError)
        );
        // Same session: nothing was released or reconnected, and live view still advances.
        assert_eq!(released.lock().unwrap().len(), 0);
        assert!(format!("{:?}", control.status()).contains("Ready"));
        let mut frames = control.frames.clone();
        frames.borrow_and_update();
        control.demand.send_replace([1, 0]);
        frames.changed().await.unwrap();
        control.shutdown();
    };
    let (factory, ()) = tokio::join!(actor.run().with_subscriber(logs.dispatch()), work);
    assert_eq!(factory.released.lock().unwrap().len(), 1);
    let logs = logs.text();
    assert_eq!(logs.matches("snapshot_still_failed").count(), 1);
}

#[tokio::test]
async fn a_still_that_times_out_after_a_clean_release_keeps_the_session() {
    let no_image = sony_camera(SonySimConfig {
        warmup_reads: 0,
        shutter: Shutter::NoImage,
        ..SonySimConfig::default()
    });
    let (mut actor, control) = actor(Factory::new(vec![no_image]));
    actor.still_snapshots.store(true, Ordering::Relaxed);
    actor.sony.capture_timeout = Duration::from_millis(50);
    actor.sony.poll_interval = Duration::from_millis(10);
    let work = async {
        ready(&control).await;
        let snapshot_control = control.clone();
        assert_eq!(
            tokio::task::spawn_blocking(move || snapshot_control.snapshot("main"))
                .await
                .unwrap(),
            Err(SnapshotError)
        );
        assert!(format!("{:?}", control.status()).contains("Ready"));
        control.shutdown();
    };
    let (factory, ()) = tokio::join!(Box::pin(actor.run()), work);
    assert_eq!(factory.released.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_refused_shutter_release_ends_the_session_instead_of_leaving_it_pressed() {
    let refused = sony_camera(SonySimConfig {
        warmup_reads: 0,
        shutter: Shutter::ReleaseRefused,
        ..SonySimConfig::default()
    });
    let (actor, control) = actor(Factory::new(vec![refused, camera()]));
    let actor = actor.with_still_snapshots(true);
    let work = async {
        ready(&control).await;
        let mut status = control.status.clone();
        status.borrow_and_update();
        let snapshot_control = control.clone();
        assert_eq!(
            tokio::task::spawn_blocking(move || snapshot_control.snapshot("main"))
                .await
                .unwrap(),
            Err(SnapshotError)
        );
        status.changed().await.unwrap();
        assert_eq!(*status.borrow(), CameraStatus::Disconnected);
        ready(&control).await;
        control.shutdown();
    };
    let (factory, ()) = tokio::join!(Box::pin(actor.run()), work);
    let cameras = factory.released.lock().unwrap();
    assert_eq!(cameras.len(), 2);
    assert_eq!(
        cameras[0].commands().last().unwrap().code,
        crate::ptp::session::code::CLOSE_SESSION
    );
}

#[tokio::test(start_paused = true)]
async fn a_snapshot_that_raced_the_slot_clearing_is_refused_at_the_next_attempt() {
    let (actor, control) = actor(Factory::new(Vec::new()));
    let pauses = Arc::clone(&actor.factory.pauses);
    let work = async {
        // `join!` polls the actor first, and with no camera it runs straight into its backoff,
        // so its session-end rejection has already happened.
        assert_eq!(pauses.lock().unwrap().len(), 1);
        let (tx, rx) = blocking::sync_channel(1);
        control.snapshots.try_send(tx).unwrap();
        // A blocking task would inhibit the paused clock's auto-advance; step it by hand.
        tokio::time::advance(Duration::from_millis(100)).await;
        let reply = loop {
            match rx.try_recv() {
                Err(blocking::TryRecvError::Empty) => tokio::task::yield_now().await,
                other => break other,
            }
        };
        assert_eq!(reply.unwrap(), Err(SnapshotError));
        control.shutdown();
    };
    let (_, ()) = tokio::join!(Box::pin(actor.run()), work);
}

fn shutter_or_still_commands(camera: &crate::transport::sim::SimCamera) -> usize {
    camera
        .commands()
        .iter()
        .filter(|c| {
            c.code == crate::sony::codes::op::SDIO_CONTROL_DEVICE
                || c.params.first() == Some(&crate::sony::codes::handle::CAPTURED_IMAGE)
        })
        .count()
}

fn live_view_fetches(camera: &crate::transport::sim::SimCamera) -> usize {
    camera
        .commands()
        .iter()
        .filter(|c| {
            c.code == crate::sony::codes::op::GET_OBJECT
                && c.params.first() == Some(&crate::sony::codes::handle::LIVE_VIEW)
        })
        .count()
}

#[tokio::test]
async fn live_snapshots_never_release_the_shutter_or_stall_streaming() {
    let (actor, control) = actor(Factory::new(vec![camera()]));
    let work = async {
        ready(&control).await;
        control.demand.send_replace([1, 1]);
        let mut frames = control.frames.clone();
        frames.changed().await.unwrap();
        for _ in 0..5 {
            let snapshot_control = control.clone();
            let snapshot = tokio::task::spawn_blocking(move || snapshot_control.snapshot("main"))
                .await
                .unwrap();
            assert_eq!(snapshot.unwrap(), crate::supervisor::testing::jpeg(32, 32));
        }
        frames.borrow_and_update();
        frames.changed().await.unwrap();
        control.shutdown();
    };
    let (factory, ()) = tokio::join!(Box::pin(actor.run()), work);
    assert_eq!(
        shutter_or_still_commands(&factory.released.lock().unwrap()[0]),
        0
    );
    let counts = control.counters();
    assert_eq!((counts.snapshots_served, counts.snapshots_refused), (5, 0));
}

#[tokio::test]
async fn idle_snapshots_reuse_a_fresh_frame_and_fetch_when_stale() {
    let (actor, control) = actor(Factory::new(vec![camera()]));
    let work = async {
        ready(&control).await;
        let snapshot = || {
            let snapshot_control = control.clone();
            async move {
                tokio::task::spawn_blocking(move || snapshot_control.snapshot("main"))
                    .await
                    .unwrap()
                    .unwrap()
            }
        };
        // Nobody streams: the startup frame is fresh, so no camera I/O happens.
        assert_eq!(snapshot().await, crate::supervisor::testing::jpeg(32, 32));
        tokio::time::sleep(SNAPSHOT_MAX_AGE + Duration::from_millis(200)).await;
        // Stale: one live-view fetch (no shutter), after which the new frame is fresh again.
        assert_eq!(snapshot().await, crate::supervisor::testing::jpeg(32, 32));
        assert_eq!(snapshot().await, crate::supervisor::testing::jpeg(32, 32));
        control.shutdown();
    };
    let (factory, ()) = tokio::join!(Box::pin(actor.run()), work);
    let cameras = factory.released.lock().unwrap();
    assert_eq!(live_view_fetches(&cameras[0]), 2);
    assert_eq!(shutter_or_still_commands(&cameras[0]), 0);
}

#[tokio::test]
async fn failed_stale_refresh_refuses_the_snapshot_and_reconnects() {
    use crate::transport::{Endpoint, sim::Fault};
    let cfg = SonySimConfig {
        warmup_reads: 0,
        live_view_jpeg: crate::supervisor::testing::jpeg(32, 32),
        ..SonySimConfig::default()
    };
    let (baseline, control) = actor(Factory::new(vec![sony_camera(cfg.clone())]));
    let (baseline, ()) = tokio::join!(Box::pin(baseline.run()), async {
        ready(&control).await;
        control.shutdown();
    });
    // Bulk-out operations during startup: every command but the close, plus each data phase.
    let startup = {
        let cameras = baseline.released.lock().unwrap();
        let commands = cameras[0].commands();
        let data_phases = commands
            .iter()
            .filter(|c| c.code == crate::sony::codes::op::SDIO_SET_EXT_DEVICE_PROP_VALUE)
            .count();
        commands.len() - 1 + data_phases
    };
    let mut failing = sony_camera(cfg.clone());
    failing.inject(Endpoint::BulkOut, startup, Fault::Disconnect);
    let (actor, control) = actor(Factory::new(vec![failing, sony_camera(cfg)]));
    let work = async {
        ready(&control).await;
        tokio::time::sleep(SNAPSHOT_MAX_AGE + Duration::from_millis(200)).await;
        let snapshot_control = control.clone();
        assert_eq!(
            tokio::task::spawn_blocking(move || snapshot_control.snapshot("main"))
                .await
                .unwrap(),
            Err(SnapshotError)
        );
        ready(&control).await;
        control.shutdown();
    };
    let (factory, ()) = tokio::join!(Box::pin(actor.run()), work);
    assert_eq!(factory.released.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn snapshots_queued_when_a_session_ends_are_refused() {
    use crate::transport::{Endpoint, sim::Fault};
    let mut failing = camera();
    failing.inject(Endpoint::BulkOut, 0, Fault::Disconnect);
    let (actor, control) = actor(Factory::new(vec![failing]));
    let (tx, rx) = blocking::sync_channel(1);
    control.snapshots.try_send(tx).unwrap();
    let work = async {
        let reply = tokio::task::spawn_blocking(move || rx.recv())
            .await
            .unwrap();
        assert_eq!(reply.unwrap(), Err(SnapshotError));
        control.shutdown();
    };
    let (_, ()) = tokio::join!(Box::pin(actor.run()), work);
}

#[tokio::test]
async fn closed_command_channel_and_dropped_handles_clean_up() {
    let (actor, control) = actor(Factory::new(vec![camera()]));
    let work = async {
        ready(&control).await;
        drop(control.snapshots);
        // Keep the other senders alive to exercise the command-channel EOF itself.
        tokio::time::sleep(Duration::from_millis(10)).await;
        control.stop.send_replace(true);
    };
    let (factory, ()) = tokio::join!(Box::pin(actor.run()), work);
    assert_eq!(factory.released.lock().unwrap().len(), 1);
    let (actor, control) = super::tests::actor(Factory::new(Vec::new()));
    drop(control);
    let factory = actor.run().await;
    assert_eq!(factory.pauses.lock().unwrap().len(), 0);
}

#[tokio::test]
async fn snapshot_admission_is_bounded_and_shutdown_drains_replies() {
    let (actor, control) = actor(Factory::new(Vec::new()));
    actor.frames.send_replace(Some(Arc::from(vec![1_u8])));
    let mut replies = Vec::new();
    for _ in 0..4 {
        let (tx, rx) = blocking::sync_channel(1);
        control.snapshots.try_send(tx).unwrap();
        replies.push(rx);
    }
    assert_eq!(control.snapshot("main"), Err(SnapshotError));
    control.shutdown();
    let _ = actor.run().await;
    for reply in replies {
        assert_eq!(reply.recv().unwrap(), Err(SnapshotError));
    }
}

#[test]
fn live_view_snapshots_get_a_short_reply_budget_and_stills_the_long_one() {
    let (actor, control) = actor(Factory::new(Vec::new()));
    assert_eq!(control.snapshot_budget(), Duration::from_secs(10));
    let _ = actor.with_still_snapshots(true);
    assert_eq!(control.snapshot_budget(), Duration::from_secs(360));
    let (actor, control) = super::tests::actor(Factory::new(Vec::new()));
    let _ = actor.with_still_snapshots(true).with_still_snapshots(false);
    assert_eq!(control.snapshot_budget(), Duration::from_secs(10));
}

#[test]
fn status_json_reports_state_and_power_without_identity() {
    use crate::sony::{Battery, PowerSource};
    let ready = |battery| CameraStatus::Ready { battery };
    let battery = |percent, source| {
        Some(Battery {
            percent,
            indicator: Some(0x0b),
            source,
        })
    };
    assert_eq!(
        CameraStatus::Disconnected.to_json(),
        r#"{"camera":"waiting"}"#
    );
    assert_eq!(CameraStatus::Stopped.to_json(), r#"{"camera":"stopped"}"#);
    assert_eq!(ready(None).to_json(), r#"{"camera":"ready"}"#);
    assert_eq!(
        ready(battery(Some(87), PowerSource::Usb)).to_json(),
        r#"{"camera":"ready","battery_percent":87,"power":"usb"}"#
    );
    assert_eq!(
        ready(battery(None, PowerSource::Battery)).to_json(),
        r#"{"camera":"ready","power":"battery"}"#
    );
    assert_eq!(
        ready(battery(Some(0), PowerSource::NoBattery)).to_json(),
        r#"{"camera":"ready","battery_percent":0,"power":"no_battery"}"#
    );
    assert_eq!(
        ready(battery(None, PowerSource::Unknown)).to_json(),
        r#"{"camera":"ready","power":"unknown"}"#
    );
}

const COUNT_KEYS: [&str; 9] = [
    "uptime_secs",
    "frames_fetched",
    "frames_served",
    "frames_dropped",
    "reconnects",
    "session_restarts",
    "connect_misses",
    "snapshots_served",
    "snapshots_refused",
];

/// The status document with every count verified as a number and removed, so the camera state
/// members can be compared exactly.
fn state_members(control: &Control) -> (String, serde_json::Map<String, serde_json::Value>) {
    let json = control.status_json();
    let mut document: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&json).unwrap();
    for key in COUNT_KEYS {
        assert!(document.remove(key).unwrap().is_u64(), "{key}");
    }
    // Existing members stay first, in their original order, so older parsers are unaffected.
    assert!(json.starts_with(r#"{"camera":""#));
    (json, document)
}

#[tokio::test]
async fn control_serves_status_json_through_the_status_source_seam() {
    let (actor, control) = actor(Factory::new(vec![camera()]));
    let (_, waiting) = state_members(&control);
    assert_eq!(
        serde_json::Value::from(waiting),
        serde_json::json!({"camera": "waiting"})
    );
    let work = async {
        ready(&control).await;
        let (json, ready) = state_members(&control);
        assert_eq!(ready["camera"], "ready");
        assert!(json.contains("\"frames_fetched\":1"));
        control.shutdown();
    };
    let _ = tokio::join!(Box::pin(actor.run()), work);
    let (_, stopped) = state_members(&control);
    assert_eq!(
        serde_json::Value::from(stopped),
        serde_json::json!({"camera": "stopped"})
    );
}

#[tokio::test(start_paused = true)]
async fn a_session_lost_after_ready_counts_a_restart_and_the_next_ready_a_reconnect() {
    use crate::transport::sim::sony::Priority;
    use crate::transport::{Endpoint, sim::Fault};
    let cfg = SonySimConfig {
        priority: Priority::Absent,
        warmup_reads: 0,
        live_view_jpeg: crate::supervisor::testing::jpeg(32, 32),
        ..SonySimConfig::default()
    };
    let mut first = sony_camera(cfg.clone());
    first.inject(Endpoint::InterruptIn, 0, Fault::Disconnect);
    let (actor, control) = actor(Factory::new(vec![first, sony_camera(cfg)]));
    let work = async {
        ready(&control).await;
        let mut status = control.status.clone();
        status.borrow_and_update();
        status.changed().await.unwrap();
        assert_eq!(*status.borrow(), CameraStatus::Disconnected);
        ready(&control).await;
        control.shutdown();
    };
    let _ = tokio::join!(Box::pin(actor.run()), work);
    let counts = control.counters();
    assert_eq!(
        (
            counts.reconnects,
            counts.session_restarts,
            counts.frames_fetched
        ),
        (1, 1, 2)
    );
}
