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
    let (_actor, control) = actor(Factory::new(Vec::new()));
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
    let logs = logs.text();
    assert_eq!(logs.matches("camera_open_failed").count(), 1);
    assert!(logs.contains("reason=camera disconnected"));
    assert_eq!(logs.matches("camera_disconnected").count(), 1);
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

#[tokio::test]
async fn serializes_live_frames_and_full_resolution_snapshots_then_reads_power() {
    let (actor, control) = actor(Factory::new(vec![camera()]));
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
async fn capture_failure_closes_and_reconnects_without_abandoning_a_session() {
    let unsupported = sony_camera(SonySimConfig {
        warmup_reads: 0,
        shutter: Shutter::NoControls,
        ..SonySimConfig::default()
    });
    let (actor, control) = actor(Factory::new(vec![unsupported, camera()]));
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
        assert_eq!(released.lock().unwrap().len(), 1);
        ready(&control).await;
        control.shutdown();
    };
    let (factory, ()) = tokio::join!(Box::pin(actor.run()), work);
    assert_eq!(factory.released.lock().unwrap().len(), 2);
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
