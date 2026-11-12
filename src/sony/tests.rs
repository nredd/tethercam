//! Driver tests against the simulated Sony body, for both protocol generations.

use std::time::{Duration, Instant};

use super::*;
use crate::ptp::session::{Session, SessionConfig, SessionError};
use crate::ptp::{DevicePropCode, Value};
use crate::transport::sim::sony::{
    Defect, LiveViewMode, Priority, Shutter, SonySimConfig, Version300, sony_camera,
};
use crate::transport::sim::{Fault, SimCamera};
use crate::transport::{Endpoint, TransportError};

fn session(sim: SonySimConfig) -> Session<SimCamera> {
    Session::new(sony_camera(sim), SessionConfig::default())
}

fn fast() -> SonyConfig {
    SonyConfig {
        poll_interval: Duration::ZERO,
        ..SonyConfig::default()
    }
}

fn body(version: u16) -> SonySimConfig {
    SonySimConfig {
        max_version: version,
        ..SonySimConfig::default()
    }
}

/// Command codes the camera received, in order.
fn codes(session: &Session<SimCamera>) -> Vec<u16> {
    session
        .transport()
        .commands()
        .iter()
        .map(|c| c.code)
        .collect()
}

fn assert_closed(session: &Session<SimCamera>) {
    assert!(!session.is_open());
    assert!(!session.transport().session_open());
    assert!(!session.transport().is_wedged());
}

#[tokio::test]
async fn handshake_negotiates_down_to_an_older_body() {
    let mut session = session(body(200));
    SonyCamera::run(&mut session, fast(), async |camera| {
        assert_eq!(camera.protocol(), Protocol::V200);
        assert_eq!(camera.device_info().model, "ILCE-SIM");
        assert_eq!(camera.ext_info().version, 200);
        assert_eq!(camera.quirks(), Quirks::default());
        let caps = camera.capabilities();
        assert_eq!(caps.protocol(), Protocol::V200);
        for capability in [
            Capability::LiveView,
            Capability::StillCapture,
            Capability::SetProperty,
            Capability::Battery,
            Capability::PriorityMode,
        ] {
            assert!(caps.supports(capability), "{capability:?}");
        }
        // 2.00 lists the controls inside the property stream, with a single enum list.
        assert!(camera.properties().contains(prop::SHUTTER_FULL_PRESS));
        let aspect = camera.properties().get(DevicePropCode(0xD211)).unwrap();
        assert_eq!(
            aspect.form,
            SonyForm::Enumeration {
                settable: vec![Value::UInt8(1), Value::UInt8(2)],
                reported: Vec::new(),
            }
        );
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        codes(&session),
        [
            0x1002, 0x1001, 0x9201, 0x9201, 0x9202, 0x9201, 0x9209, 0x9205, 0x1003
        ]
    );
    let commands = session.transport().commands();
    assert_eq!(commands[2].params, [1, 0, 0]);
    assert_eq!(commands[3].params, [2, 0, 0]);
    assert_eq!(commands[4].params, [300, 1]);
    assert_eq!(commands[5].params, [3, 0, 0]);
    assert_eq!(commands[6].params, Vec::<u32>::new());
    assert_eq!(commands[7].params, [0xD25A]);
    // Priority mode is written as the declared `INT8`, value 1.
    assert_eq!(session.transport().received_data(), [vec![1]]);
    assert_closed(&session);
}

#[tokio::test]
async fn handshake_with_a_newer_body_uses_the_3_00_layout() {
    let mut session = session(body(300));
    SonyCamera::run(&mut session, fast(), async |camera| {
        assert_eq!(camera.protocol(), Protocol::V300);
        assert!(!camera.properties().contains(prop::SHUTTER_FULL_PRESS));
        let aspect = camera.properties().get(DevicePropCode(0xD211)).unwrap();
        assert_eq!(
            aspect.form,
            SonyForm::Enumeration {
                settable: vec![Value::UInt8(1), Value::UInt8(2)],
                reported: vec![Value::UInt8(1), Value::UInt8(2)],
            }
        );
        assert!(camera.capabilities().supports(Capability::StillCapture));
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(session.transport().commands()[6].params, [0, 1]);
    assert_closed(&session);
}

#[tokio::test]
async fn a_body_can_be_asked_for_the_older_generation() {
    let mut session = session(body(300));
    let config = SonyConfig {
        protocol: Protocol::V200,
        ..fast()
    };
    SonyCamera::run(&mut session, config, async |camera| {
        assert_eq!(camera.protocol(), Protocol::V200);
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(session.transport().commands()[4].params, [200, 1]);
}

#[tokio::test]
async fn ext_info_is_retried_while_busy_or_empty() {
    let sim = SonySimConfig {
        ext_info_busy: 2,
        ext_info_empty: 2,
        ..body(200)
    };
    let mut session = session(sim);
    SonyCamera::run(&mut session, fast(), async |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(codes(&session).iter().filter(|c| **c == 0x9202).count(), 5);
    assert_closed(&session);
}

#[tokio::test]
async fn ext_info_gives_up_and_the_session_still_closes() {
    let sim = SonySimConfig {
        ext_info_empty: 9,
        ..body(200)
    };
    let mut session = session(sim);
    let config = SonyConfig {
        protocol: Protocol::V200,
        ext_info_attempts: 3,
        ..fast()
    };
    let result = SonyCamera::run(&mut session, config, async |_| Ok(())).await;
    assert_eq!(result, Err(SonyError::ExtInfoUnavailable { attempts: 3 }));
    assert_eq!(codes(&session).iter().filter(|c| **c == 0x9202).count(), 3);
    assert_closed(&session);
}

#[tokio::test]
async fn ext_info_transport_failure_is_not_retried() {
    let mut camera = sony_camera(body(200));
    // OpenSession, GetDeviceInfo (data + response), phase 1, phase 2 (data + response each),
    // then the ext-info data phase is read number 7.
    camera.inject(Endpoint::BulkIn, 7, Fault::Timeout);
    let mut session = Session::new(camera, SessionConfig::default());
    let config = SonyConfig {
        protocol: Protocol::V200,
        ..fast()
    };
    let result = SonyCamera::run(&mut session, config, async |_| Ok(())).await;
    assert_eq!(
        result,
        Err(SonyError::Session(SessionError::Transport(
            TransportError::Timeout
        )))
    );
    assert_eq!(codes(&session).iter().filter(|c| **c == 0x9202).count(), 1);
    assert_closed(&session);
}

#[tokio::test]
async fn a_body_outside_pc_remote_mode_is_refused() {
    let sim = SonySimConfig {
        advertise_sdio: false,
        ..body(200)
    };
    let mut session = session(sim);
    let result = SonyCamera::run(&mut session, fast(), async |_| Ok(())).await;
    assert_eq!(result, Err(SonyError::SdioUnsupported));
    assert_eq!(codes(&session), [0x1002, 0x1001, 0x1003]);
    assert_closed(&session);
}

#[tokio::test]
async fn an_undocumented_old_protocol_is_refused() {
    let mut session = session(body(150));
    let result = SonyCamera::run(&mut session, fast(), async |_| Ok(())).await;
    assert_eq!(result, Err(SonyError::UnsupportedProtocol(150)));
    assert_closed(&session);
}

#[tokio::test]
async fn the_body_error_wins_over_a_clean_close() {
    let mut session = session(body(200));
    let result: Result<(), SonyError> = SonyCamera::run(&mut session, fast(), async |_| {
        Err(SonyError::LiveViewNotReady)
    })
    .await;
    assert_eq!(result, Err(SonyError::LiveViewNotReady));
    assert_closed(&session);
}

#[tokio::test]
async fn set_property_round_trips_through_the_change_event() {
    for version in [200, 300] {
        let mut session = session(body(version));
        SonyCamera::run(&mut session, fast(), async |camera| {
            let aspect = DevicePropCode(0xD211);
            assert_eq!(camera.properties().integer(aspect), Some(0));
            camera.refresh_properties().await?;
            assert_eq!(camera.properties().integer(aspect), Some(0));
            // The handshake wrote priority mode, and the camera confirmed it with an event.
            assert_eq!(
                camera.pump_events().await?,
                [SonyEvent::PropertyChanged(prop::PRIORITY_MODE)]
            );
            camera.set_property(aspect, &Value::UInt8(1)).await?;
            let events = camera.pump_events().await?;
            assert_eq!(events, [SonyEvent::PropertyChanged(aspect)]);
            assert_eq!(camera.properties().integer(aspect), Some(1));
            camera
                .set_property(DevicePropCode(0xD1CF), &Value::String("tethercam".into()))
                .await?;
            assert_eq!(camera.pump_events().await?.len(), 1);
            assert_eq!(camera.pump_events().await?, Vec::<SonyEvent>::new());
            Ok(())
        })
        .await
        .unwrap();
        assert_closed(&session);
    }
}

#[tokio::test]
async fn set_property_validates_before_sending() {
    let mut session = session(body(200));
    SonyCamera::run(&mut session, fast(), async |camera| {
        let unknown = DevicePropCode(0xD999);
        assert_eq!(
            camera.set_property(unknown, &Value::UInt8(1)).await,
            Err(SonyError::UnknownProperty(unknown))
        );
        assert_eq!(
            camera
                .set_property(prop::LIVE_VIEW_STATUS, &Value::UInt8(1))
                .await,
            Err(SonyError::NotWritable(prop::LIVE_VIEW_STATUS))
        );
        assert_eq!(
            camera
                .set_property(DevicePropCode(0xD211), &Value::UInt16(1))
                .await,
            Err(SonyError::TypeMismatch {
                code: DevicePropCode(0xD211),
                expected: 0x0002,
                actual: 0x0004,
            })
        );
        let too_long = Value::String("x".repeat(300));
        assert!(matches!(
            camera.set_property(DevicePropCode(0xD1CF), &too_long).await,
            Err(SonyError::Codec(_))
        ));
        Ok(())
    })
    .await
    .unwrap();
    // Only the handshake wrote anything.
    assert_eq!(session.transport().received_data().len(), 1);
    assert_closed(&session);
}

#[tokio::test]
async fn control_rejects_unlisted_codes_and_unencodable_values() {
    let mut session = session(body(200));
    SonyCamera::run(&mut session, fast(), async |camera| {
        let unknown = DevicePropCode(0xD999);
        assert_eq!(
            camera.control(unknown, &Value::UInt16(2)).await,
            Err(SonyError::UnsupportedControl(unknown))
        );
        assert!(matches!(
            camera
                .control(prop::SHUTTER_HALF_PRESS, &Value::String("x".repeat(300)))
                .await,
            Err(SonyError::Codec(_))
        ));
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn live_view_waits_for_ready_then_hands_out_frames() {
    for version in [200, 300] {
        let jpeg = vec![0xFF, 0xD8, 7, 7, 7, 0xFF, 0xD9];
        let sim = SonySimConfig {
            live_view_jpeg: jpeg.clone(),
            ..body(version)
        };
        let mut session = session(sim);
        SonyCamera::run(&mut session, fast(), async |camera| {
            // Right after connecting: not ready, and property values read as zero.
            assert_eq!(
                camera.fetch_live_view_frame().await,
                Err(SonyError::LiveViewNotReady)
            );
            assert_eq!(camera.properties().integer(prop::LIVE_VIEW_STATUS), Some(0));
            assert_eq!(camera.battery().unwrap().percent, Some(0));
            camera.wait_live_view_ready().await?;
            assert_eq!(camera.properties().integer(prop::LIVE_VIEW_STATUS), Some(1));
            for _ in 0..3 {
                assert_eq!(camera.next_live_view_frame().await?, jpeg);
            }
            let battery = camera.battery().unwrap();
            assert_eq!(battery.percent, Some(80));
            assert_eq!(battery.source, PowerSource::Usb);
            Ok(())
        })
        .await
        .unwrap();
        assert_closed(&session);
    }
}

#[tokio::test]
async fn live_view_retries_denied_fetches_within_its_budget() {
    let sim = SonySimConfig {
        live_view_denied: 3,
        warmup_reads: 0,
        ..body(200)
    };
    let mut session = session(sim);
    SonyCamera::run(&mut session, fast(), async |camera| {
        assert!(camera.next_live_view_frame().await.is_ok());
        Ok(())
    })
    .await
    .unwrap();
    let fetches = codes(&session).iter().filter(|c| **c == 0x1009).count();
    assert_eq!(fetches, 4);
}

#[tokio::test]
async fn live_view_budget_runs_out_unless_the_model_quirk_extends_it() {
    for (model, succeeds) in [("ILCE-9999", false), ("ZV-1", true)] {
        let sim = SonySimConfig {
            model: model.to_owned(),
            live_view_denied: 5,
            warmup_reads: 0,
            ..body(200)
        };
        let mut session = session(sim);
        let config = SonyConfig {
            live_view_timeout: Duration::from_millis(12),
            poll_interval: Duration::from_millis(5),
            ..fast()
        };
        SonyCamera::run(&mut session, config, async |camera| {
            let frame = camera.next_live_view_frame().await;
            if succeeds {
                assert!(frame.is_ok());
            } else {
                assert_eq!(frame, Err(SonyError::LiveViewNotReady));
            }
            Ok(())
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn live_view_readiness_errors() {
    let cases = [
        (LiveViewMode::Unsupported, 2, SonyError::LiveViewUnsupported),
        (LiveViewMode::Absent, 2, SonyError::LiveViewUnsupported),
        (LiveViewMode::Works, u32::MAX, SonyError::LiveViewNotReady),
    ];
    for (live_view, warmup_reads, expected) in cases {
        let sim = SonySimConfig {
            warmup_reads,
            live_view,
            ..body(200)
        };
        ELAPSED.set(Duration::ZERO);
        let mut session = session(sim).with_clock(ticking_now);
        // Only the never-ready case needs a short deadline; the others answer at once.
        let config = SonyConfig {
            live_view_ready_timeout: if warmup_reads == u32::MAX {
                Duration::from_millis(20)
            } else {
                Duration::from_secs(10)
            },
            ..fast()
        };
        SonyCamera::run(&mut session, config, async |camera| {
            assert_eq!(camera.wait_live_view_ready().await, Err(expected.clone()));
            Ok(())
        })
        .await
        .unwrap();
        assert_closed(&session);
    }
}

#[tokio::test]
async fn a_malformed_live_view_object_is_an_error() {
    let sim = SonySimConfig {
        live_view_jpeg: vec![1, 2, 3, 4],
        warmup_reads: 0,
        ..body(200)
    };
    let mut session = session(sim);
    SonyCamera::run(&mut session, fast(), async |camera| {
        assert_eq!(
            camera.fetch_live_view_frame().await,
            Err(SonyError::LiveView(LiveViewError::NotJpeg))
        );
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn capture_presses_waits_releases_and_downloads() {
    for (version, event) in [(200, true), (300, true), (200, false), (300, false)] {
        let sim = SonySimConfig {
            emit_object_added: event,
            ..body(version)
        };
        let still = sim.still.clone();
        let mut session = session(sim);
        SonyCamera::run(&mut session, fast(), async |camera| {
            let image = camera.capture_still().await?;
            assert_eq!(image.data, still);
            assert_eq!(image.info.filename, "DSC00001.JPG");
            assert_eq!(image.info.object_format, 0x3801);
            // The download cleared the camera's memory.
            camera.refresh_properties().await?;
            assert_eq!(camera.properties().integer(prop::OBJECT_IN_MEMORY), Some(0));
            Ok(())
        })
        .await
        .unwrap();
        let writes: Vec<&Vec<u8>> = session.transport().received_data().iter().skip(1).collect();
        // Half press, full press, release full, release half, as `UINT16`.
        assert_eq!(writes, [&vec![2, 0], &vec![2, 0], &vec![1, 0], &vec![1, 0]]);
        let controls: Vec<u32> = session
            .transport()
            .commands()
            .iter()
            .filter(|c| c.code == 0x9207)
            .map(|c| c.params[0])
            .collect();
        assert_eq!(controls, [0xD2C1, 0xD2C2, 0xD2C2, 0xD2C1]);
        assert_closed(&session);
    }
}

#[tokio::test]
async fn still_downloads_can_outlast_normal_transaction_timeout() {
    for stale_image in [false, true] {
        ELAPSED.set(Duration::ZERO);
        let still = vec![42; 512 * 1024];
        let sim = SonySimConfig {
            warmup_reads: 0,
            stale_image,
            still: still.clone(),
            ..body(200)
        };
        let session_config = SessionConfig {
            transaction_timeout: Duration::from_millis(20),
            ..SessionConfig::default()
        };
        let mut session = Session::new(sony_camera(sim), session_config).with_clock(ticking_now);
        let config = SonyConfig {
            download_timeout: Duration::from_secs(1),
            ..fast()
        };
        SonyCamera::run(&mut session, config, async |camera| {
            let before = ELAPSED.get();
            let image = camera.capture_still().await?;
            assert_eq!(image.data, still);
            assert!(ELAPSED.get().saturating_sub(before) > Duration::from_millis(20));
            assert!(camera.session.is_open());
            assert_eq!(camera.session.transport().reset_count(), 0);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(
            codes(&session)
                .iter()
                .filter(|c| **c == op::GET_OBJECT)
                .count(),
            if stale_image { 2 } else { 1 }
        );
        assert_eq!(session.transport().reset_count(), 0);
        assert_eq!(codes(&session).last(), Some(&0x1003));
        assert_closed(&session);
    }
}

#[tokio::test]
async fn capture_discards_a_stale_image_first() {
    let sim = SonySimConfig {
        stale_image: true,
        ..body(200)
    };
    let mut session = session(sim);
    SonyCamera::run(&mut session, fast(), async |camera| {
        // Values read as zero until the camera warms up, which would hide the stale image.
        camera.wait_live_view_ready().await?;
        assert!(camera.capture_still().await.is_ok());
        Ok(())
    })
    .await
    .unwrap();
    let downloads = codes(&session).iter().filter(|c| **c == 0x1009).count();
    assert_eq!(downloads, 2, "one discard, one real download");
}

thread_local! {
    /// Fake time elapsed on this test thread.
    static ELAPSED: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
}

fn fake_now() -> Instant {
    static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *BASE.get_or_init(Instant::now) + ELAPSED.get()
}

fn advance(by: Duration) {
    ELAPSED.set(ELAPSED.get() + by);
}

fn ticking_now() -> Instant {
    let now = fake_now();
    advance(Duration::from_millis(1));
    now
}

/// A session whose clock only moves when an idle interrupt read spends its timeout.
fn fake_clock_session(sim: SonySimConfig) -> Session<SimCamera> {
    ELAPSED.set(Duration::ZERO);
    Session::new(
        sony_camera(sim).with_idle_sink(advance),
        SessionConfig::default(),
    )
    .with_clock(fake_now)
}

#[tokio::test]
async fn capture_ends_at_its_deadline_on_the_injected_clock_and_releases_the_shutter() {
    let sim = SonySimConfig {
        shutter: Shutter::NoImage,
        ..body(200)
    };
    let mut session = fake_clock_session(sim);
    let config = SonyConfig {
        capture_timeout: Duration::from_secs(1),
        event_poll_timeout: Duration::from_millis(100),
        ..fast()
    };
    SonyCamera::run(&mut session, config, async |camera| {
        assert_eq!(
            camera.capture_still().await,
            Err(SonyError::CaptureTimeout {
                timeout: Duration::from_secs(1)
            })
        );
        Ok(())
    })
    .await
    .unwrap();
    // The one-second wait plus the polls before it, nowhere near the 5 s transfer timeout
    // per poll that real hardware would have charged.
    let elapsed = ELAPSED.get();
    assert!(elapsed >= Duration::from_secs(1), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
    let writes: Vec<&Vec<u8>> = session.transport().received_data().iter().skip(1).collect();
    assert_eq!(writes, [&vec![2, 0], &vec![2, 0], &vec![1, 0], &vec![1, 0]]);
    assert_closed(&session);
}

#[tokio::test]
async fn event_refresh_is_skipped_after_budget_expires() {
    let mut session = fake_clock_session(body(200));
    let config = SonyConfig {
        event_poll_timeout: Duration::from_millis(50),
        ..fast()
    };
    SonyCamera::run(&mut session, config, async |camera| {
        let before = camera.session.transport().commands().len();
        let started = camera.session.now();
        let events = camera
            .pump_events_inner(Some((started, Duration::from_millis(10))))
            .await?;
        assert_eq!(events, [SonyEvent::PropertyChanged(prop::PRIORITY_MODE)]);
        assert_eq!(camera.session.transport().commands().len(), before);
        assert_eq!(camera.session.transport().reset_count(), 0);
        camera.refresh_properties().await?;
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(codes(&session).last(), Some(&0x1003));
    assert_eq!(session.transport().reset_count(), 0);
}

#[tokio::test]
async fn pumping_events_costs_at_most_the_short_poll_timeout() {
    let mut session = fake_clock_session(body(200));
    let config = SonyConfig {
        event_poll_timeout: Duration::from_millis(50),
        ..fast()
    };
    SonyCamera::run(&mut session, config, async |camera| {
        camera.wait_live_view_ready().await?;
        for _ in 0..3 {
            let before = ELAPSED.get();
            camera.next_live_view_frame().await?;
            camera.pump_events().await?;
            assert!(ELAPSED.get().saturating_sub(before) <= Duration::from_millis(50));
        }
        Ok(())
    })
    .await
    .unwrap();
    // The session's own 5 s transfer timeout was never charged.
    assert!(ELAPSED.get() < Duration::from_secs(1));
}

#[tokio::test]
async fn capture_needs_the_shutter_controls() {
    let sim = SonySimConfig {
        shutter: Shutter::NoControls,
        ..body(200)
    };
    let mut session = session(sim);
    SonyCamera::run(&mut session, fast(), async |camera| {
        assert!(!camera.capabilities().supports(Capability::StillCapture));
        assert_eq!(
            camera.capture_still().await,
            Err(SonyError::CaptureUnsupported)
        );
        Ok(())
    })
    .await
    .unwrap();
}

/// A whole session, for the fault sweep.
async fn workflow(session: &mut Session<SimCamera>) -> Result<(), SonyError> {
    SonyCamera::run(session, fast(), async |camera| {
        camera.wait_live_view_ready().await?;
        camera.next_live_view_frame().await?;
        camera.next_live_view_frame().await?;
        camera
            .set_property(DevicePropCode(0xD211), &Value::UInt8(1))
            .await?;
        camera.pump_events().await?;
        camera.capture_still().await?;
        camera.pump_events().await?;
        Ok(())
    })
    .await
}

#[tokio::test]
async fn every_failure_point_closes_the_session_cleanly() {
    for (stale_image, emit_object_added, shutter, version_300) in [
        (false, true, Shutter::Works, Version300::Negotiates),
        (true, false, Shutter::Works, Version300::Rejects(0x201D)),
        (false, false, Shutter::AfterRelease, Version300::Stalls),
    ] {
        sweep(stale_image, emit_object_added, shutter, version_300).await;
    }
}

async fn sweep(
    stale_image: bool, emit_object_added: bool, shutter: Shutter, version_300: Version300,
) {
    let faults = [
        Fault::Stall,
        Fault::Timeout,
        Fault::ShortRead(3),
        Fault::Disconnect,
    ];
    let endpoints = [Endpoint::BulkOut, Endpoint::BulkIn, Endpoint::InterruptIn];
    let (mut failures, mut successes) = (0, 0);
    for fault in faults {
        for endpoint in endpoints {
            for after in 0..160 {
                let sim = SonySimConfig {
                    warmup_reads: 1,
                    stale_image,
                    emit_object_added,
                    shutter,
                    version_300,
                    ..body(300)
                };
                let mut camera = sony_camera(sim);
                camera.inject(endpoint, after, fault);
                let mut session = Session::new(camera, SessionConfig::default());
                match workflow(&mut session).await {
                    Ok(()) => successes += 1,
                    Err(_) => failures += 1,
                }
                assert!(!session.is_open(), "{fault:?} {endpoint:?} {after}");
                if fault != Fault::Disconnect {
                    assert!(
                        !session.transport().session_open(),
                        "session left open: {fault:?} {endpoint:?} {after}"
                    );
                    assert!(
                        !session.transport().is_wedged(),
                        "camera wedged: {fault:?} {endpoint:?} {after}"
                    );
                }
            }
        }
    }
    assert!(failures > 100, "{failures} failures");
    assert!(successes > 100, "{successes} successes");
}

#[tokio::test]
async fn zero_wait_budgets_do_not_start_operations() {
    let sim = SonySimConfig {
        shutter: Shutter::NoImage,
        ..body(200)
    };
    let mut session = fake_clock_session(sim);
    let config = SonyConfig {
        live_view_ready_timeout: Duration::ZERO,
        live_view_timeout: Duration::ZERO,
        capture_timeout: Duration::ZERO,
        ..fast()
    };
    SonyCamera::run(&mut session, config, async |camera| {
        let before = camera.session.transport().commands().len();
        assert_eq!(
            camera.wait_live_view_ready().await,
            Err(SonyError::LiveViewNotReady)
        );
        assert_eq!(
            camera.next_live_view_frame().await,
            Err(SonyError::LiveViewNotReady)
        );
        assert_eq!(camera.session.transport().commands().len(), before);
        let capture_start = camera.session.transport().commands().len();
        assert_eq!(
            camera.capture_still().await,
            Err(SonyError::CaptureTimeout {
                timeout: Duration::ZERO
            })
        );
        let capture_commands = &camera.session.transport().commands()[capture_start..];
        assert_eq!(
            capture_commands.last().unwrap().code,
            op::SDIO_CONTROL_DEVICE
        );
        camera.refresh_properties().await?;
        assert!(camera.session.is_open());
        assert_eq!(camera.session.transport().reset_count(), 0);
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(session.transport().reset_count(), 0);
    assert_eq!(codes(&session).last(), Some(&0x1003));
    assert_closed(&session);

    let mut session = fake_clock_session(body(200));
    let config = SonyConfig {
        protocol: Protocol::V200,
        ext_info_timeout: Duration::ZERO,
        ..fast()
    };
    assert_eq!(
        SonyCamera::run(&mut session, config, async |_| Ok(())).await,
        Err(SonyError::ExtInfoUnavailable { attempts: 0 })
    );
    assert_eq!(ext_info_requests(&session), Vec::<Vec<u32>>::new());
    assert_eq!(session.transport().reset_count(), 0);
    assert_eq!(codes(&session).last(), Some(&0x1003));
    assert_closed(&session);
}

#[tokio::test]
async fn started_transaction_can_finish_after_wait_budget() {
    ELAPSED.set(Duration::ZERO);
    let sim = SonySimConfig {
        warmup_reads: 0,
        ..body(200)
    };
    let mut session = session(sim).with_clock(ticking_now);
    let config = SonyConfig {
        live_view_ready_timeout: Duration::from_millis(2),
        ..fast()
    };
    SonyCamera::run(&mut session, config, async |camera| {
        let before = ELAPSED.get();
        camera.wait_live_view_ready().await?;
        assert!(ELAPSED.get().saturating_sub(before) > Duration::from_millis(2));
        assert_eq!(camera.session.transport().reset_count(), 0);
        assert!(camera.session.is_open());
        camera.refresh_properties().await?;
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(session.transport().reset_count(), 0);
    assert_eq!(codes(&session).last(), Some(&0x1003));
}

#[tokio::test]
async fn slow_wait_transactions_preserve_the_session() {
    for (frame, millis) in [(false, 5), (true, 10), (true, 20)] {
        ELAPSED.set(Duration::ZERO);
        let mut session = session(body(200)).with_clock(ticking_now);
        let config = SonyConfig {
            live_view_ready_timeout: Duration::from_millis(5),
            live_view_timeout: Duration::from_millis(millis),
            ..fast()
        };
        SonyCamera::run(&mut session, config, async |camera| {
            let result = if frame {
                camera.next_live_view_frame().await.map(|_| ())
            } else {
                camera.wait_live_view_ready().await
            };
            assert_eq!(result, Err(SonyError::LiveViewNotReady));
            assert_eq!(camera.session.transport().reset_count(), 0);
            assert!(camera.session.is_open());
            camera.refresh_properties().await?;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(session.transport().reset_count(), 0);
        assert_eq!(codes(&session).last(), Some(&0x1003));
        assert_closed(&session);
    }
}

#[tokio::test]
async fn capture_budget_covers_event_and_property_reads() {
    for millis in [1, 3, 5, 10, 20, 40] {
        ELAPSED.set(Duration::ZERO);
        let sim = SonySimConfig {
            shutter: Shutter::NoImage,
            ..body(200)
        };
        let mut session = session(sim).with_clock(ticking_now);
        let config = SonyConfig {
            capture_timeout: Duration::from_millis(millis),
            event_poll_timeout: Duration::ZERO,
            ..fast()
        };
        SonyCamera::run(&mut session, config, async |camera| {
            assert_eq!(
                camera.capture_still().await,
                Err(SonyError::CaptureTimeout {
                    timeout: Duration::from_millis(millis)
                })
            );
            assert_eq!(camera.session.transport().reset_count(), 0);
            assert!(camera.session.is_open());
            camera.refresh_properties().await?;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(session.transport().reset_count(), 0);
        assert_eq!(codes(&session).last(), Some(&0x1003));
        assert_closed(&session);
    }
}

#[tokio::test]
async fn a_disconnect_mid_frame_surfaces_and_closes() {
    let sim = SonySimConfig {
        warmup_reads: 0,
        live_view_jpeg: vec![0xFF, 0xD8, 9, 0xFF, 0xD9],
        ..body(200)
    };
    let mut camera = sony_camera(sim);
    // Handshake reads: see `ext_info_transport_failure_is_not_retried`, plus phase 3, the
    // property table and the priority write; the live-view data phase is read 13.
    camera.inject(Endpoint::BulkIn, 13, Fault::Disconnect);
    let mut session = Session::new(camera, SessionConfig::default());
    let result = SonyCamera::run(&mut session, fast(), async |camera| {
        camera.fetch_live_view_frame().await
    })
    .await;
    assert_eq!(
        result,
        Err(SonyError::Session(SessionError::Transport(
            TransportError::Disconnected
        )))
    );
    assert!(!session.is_open());
}

#[tokio::test]
async fn a_garbage_device_info_is_an_error() {
    let camera = sony_camera(body(200)).with_device_info(vec![1, 2, 3]);
    let mut session = Session::new(camera, SessionConfig::default());
    let result = SonyCamera::run(&mut session, fast(), async |_| Ok(())).await;
    assert!(matches!(result, Err(SonyError::Codec(_))));
    assert_closed(&session);
}

#[tokio::test]
async fn a_body_without_priority_mode_is_not_written_to() {
    let sim = SonySimConfig {
        priority: Priority::Absent,
        ..body(200)
    };
    let mut session = session(sim);
    SonyCamera::run(&mut session, fast(), async |camera| {
        assert!(!camera.capabilities().supports(Capability::PriorityMode));
        Ok(())
    })
    .await
    .unwrap();
    assert!(!codes(&session).contains(&0x9205));
    assert_closed(&session);
}

#[tokio::test]
async fn a_truncated_property_table_is_an_error() {
    let sim = SonySimConfig {
        defect: Defect::TruncatedTable,
        ..body(200)
    };
    let mut session = session(sim);
    let result = SonyCamera::run(&mut session, fast(), async |_| Ok(())).await;
    assert!(matches!(result, Err(SonyError::Codec(_))));
    assert_closed(&session);
}

#[tokio::test]
async fn a_garbage_object_info_fails_the_capture_but_releases_the_shutter() {
    let sim = SonySimConfig {
        defect: Defect::BadObjectInfo,
        ..body(200)
    };
    let mut session = session(sim);
    SonyCamera::run(&mut session, fast(), async |camera| {
        assert!(matches!(
            camera.capture_still().await,
            Err(SonyError::Codec(_))
        ));
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(session.transport().received_data().len(), 5);
    assert_closed(&session);
}

#[tokio::test]
async fn capture_waits_for_an_image_that_only_appears_after_release() {
    for emit_object_added in [true, false] {
        let sim = SonySimConfig {
            shutter: Shutter::AfterRelease,
            stale_image: true,
            emit_object_added,
            ..body(200)
        };
        let still = sim.still.clone();
        let mut session = session(sim);
        SonyCamera::run(&mut session, fast(), async |camera| {
            camera.wait_live_view_ready().await?;
            let image = camera.capture_still().await?;
            assert_eq!(image.data, still);
            Ok(())
        })
        .await
        .unwrap();
        // Stale discard, then the real download, after both releases.
        let order = codes(&session);
        let last_control = order.iter().rposition(|c| *c == 0x9207).unwrap();
        let last_download = order.iter().rposition(|c| *c == 0x1009).unwrap();
        assert!(last_download > last_control);
        assert_closed(&session);
    }
}

/// Commands with this code, in order.
fn ext_info_requests(session: &Session<SimCamera>) -> Vec<Vec<u32>> {
    session
        .transport()
        .commands()
        .iter()
        .filter(|c| c.code == 0x9202)
        .map(|c| c.params.clone())
        .collect()
}

#[tokio::test]
async fn a_rejected_3_00_request_falls_back_to_2_00_on_the_same_session() {
    for response in [0x201D, 0x2005, 0x2002, 0x200F] {
        let sim = SonySimConfig {
            version_300: Version300::Rejects(response),
            ..body(300)
        };
        let mut session = session(sim);
        SonyCamera::run(&mut session, fast(), async |camera| {
            assert_eq!(camera.protocol(), Protocol::V200);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(
            ext_info_requests(&session),
            [[300, 1], [200, 1]],
            "{response:#x}"
        );
        let opens = codes(&session).iter().filter(|c| **c == 0x1002).count();
        assert_eq!(opens, 1, "the session was not abandoned");
        assert_eq!(session.transport().reset_count(), 0);
        assert_closed(&session);
        assert_eq!(*codes(&session).last().unwrap(), 0x1003);
    }
}

#[tokio::test]
async fn a_stalled_3_00_request_is_recovered_and_retried_at_2_00() {
    let sim = SonySimConfig {
        version_300: Version300::Stalls,
        ..body(300)
    };
    let mut session = session(sim);
    SonyCamera::run(&mut session, fast(), async |camera| {
        assert_eq!(camera.protocol(), Protocol::V200);
        assert!(camera.capabilities().supports(Capability::LiveView));
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(ext_info_requests(&session), [[300, 1], [200, 1]]);
    // The reset ended the session, so it was reopened and the handshake replayed.
    assert!(session.transport().reset_count() >= 1);
    let opens = codes(&session).iter().filter(|c| **c == 0x1002).count();
    assert_eq!(opens, 2);
    let phases = codes(&session).iter().filter(|c| **c == 0x9201).count();
    assert_eq!(phases, 5, "phases 1 and 2 twice, then 3");
    assert_closed(&session);
}

#[tokio::test]
async fn refusing_both_generations_names_both_attempts() {
    let sim = SonySimConfig {
        version_300: Version300::RefusesAll(0x201D),
        ..body(300)
    };
    let mut session = session(sim);
    let result = SonyCamera::run(&mut session, fast(), async |_| Ok(())).await;
    let refusal = || {
        Box::new(SonyError::Session(SessionError::Response {
            code: 0x201D,
            params: Vec::new(),
        }))
    };
    assert_eq!(
        result,
        Err(SonyError::ProtocolRefused {
            requested: Protocol::V300,
            requested_error: refusal(),
            fallback: Protocol::V200,
            fallback_error: refusal(),
        })
    );
    assert_eq!(ext_info_requests(&session), [[300, 1], [200, 1]]);
    assert_closed(&session);
}

#[tokio::test]
async fn starting_at_2_00_makes_a_single_attempt() {
    let config = SonyConfig {
        protocol: Protocol::V200,
        ..fast()
    };
    let sim = SonySimConfig {
        version_300: Version300::Stalls,
        ..body(300)
    };
    let mut session = session(sim);
    SonyCamera::run(&mut session, config.clone(), async |camera| {
        assert_eq!(camera.protocol(), Protocol::V200);
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(ext_info_requests(&session), [[200, 1]]);
    assert_eq!(session.transport().reset_count(), 0);

    let sim = SonySimConfig {
        version_300: Version300::RefusesAll(0x201D),
        ..body(300)
    };
    let mut session = self::session(sim);
    let result = SonyCamera::run(&mut session, config, async |_| Ok(())).await;
    assert_eq!(
        result,
        Err(SonyError::Session(SessionError::Response {
            code: 0x201D,
            params: Vec::new()
        }))
    );
    assert_eq!(ext_info_requests(&session), [[200, 1]]);
    assert_closed(&session);
}

#[tokio::test]
async fn a_model_quirk_chooses_the_first_generation() {
    let sim = SonySimConfig {
        model: "ILCE-7M3".to_owned(),
        version_300: Version300::Stalls,
        ..body(200)
    };
    let mut session = session(sim);
    SonyCamera::run(&mut session, fast(), async |camera| {
        assert_eq!(camera.quirks().start_protocol, Some(Protocol::V200));
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(ext_info_requests(&session), [[200, 1]]);
    assert_eq!(session.transport().reset_count(), 0);
}

#[tokio::test]
async fn deadline_on_first_version_reopens_and_replays() {
    let mut camera = sony_camera(body(300)).with_idle_sink(advance);
    camera.inject(Endpoint::BulkIn, 7, Fault::Timeout);
    ELAPSED.set(Duration::ZERO);
    let session_config = SessionConfig {
        transaction_timeout: Duration::from_millis(20),
        ..SessionConfig::default()
    };
    let mut session = Session::new(camera, session_config).with_clock(fake_now);
    let config = fast();
    SonyCamera::run(&mut session, config, async |camera| {
        assert_eq!(camera.protocol(), Protocol::V200);
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(ext_info_requests(&session), [[300, 1], [200, 1]]);
    assert_eq!(codes(&session).iter().filter(|c| **c == 0x1002).count(), 2);
    assert_eq!(codes(&session).iter().filter(|c| **c == 0x9201).count(), 5);
    assert_closed(&session);
}

#[tokio::test]
async fn malformed_fallback_is_not_a_refusal() {
    let sim = SonySimConfig {
        version_300: Version300::Rejects(0x201D),
        defect: Defect::BadExtInfo,
        ..body(300)
    };
    let mut session = fake_clock_session(sim);
    let result = SonyCamera::run(&mut session, fast(), async |_| Ok(())).await;
    assert!(matches!(result, Err(SonyError::Codec(_))));
    assert_eq!(ext_info_requests(&session), [[300, 1], [200, 1]]);
    assert_eq!(session.transport().reset_count(), 0);
    assert_closed(&session);
}

#[tokio::test]
async fn disconnect_on_fallback_is_not_a_refusal() {
    let sim = SonySimConfig {
        version_300: Version300::Rejects(0x201D),
        ..body(300)
    };
    let mut camera = sony_camera(sim);
    camera.inject(Endpoint::BulkIn, 8, Fault::Disconnect);
    let mut session = Session::new(camera, SessionConfig::default()).with_clock(fake_now);
    let result = SonyCamera::run(&mut session, fast(), async |_| Ok(())).await;
    assert_eq!(
        result,
        Err(SonyError::Session(SessionError::Transport(
            TransportError::Disconnected
        )))
    );
    assert!(!session.is_open());
}

#[tokio::test]
async fn a_disconnect_during_negotiation_is_not_a_refusal() {
    let mut camera = sony_camera(body(300));
    camera.inject(Endpoint::BulkIn, 7, Fault::Disconnect);
    let mut session = Session::new(camera, SessionConfig::default());
    let result = SonyCamera::run(&mut session, fast(), async |_| Ok(())).await;
    assert_eq!(
        result,
        Err(SonyError::Session(SessionError::Transport(
            TransportError::Disconnected
        )))
    );
    assert_eq!(ext_info_requests(&session), [[300, 1]]);
}

#[tokio::test]
async fn a_persistently_busy_3_00_request_falls_back_too() {
    let sim = SonySimConfig {
        version_300: Version300::Rejects(0x2019),
        ..body(300)
    };
    let mut session = session(sim);
    let config = SonyConfig {
        ext_info_attempts: 3,
        ..fast()
    };
    SonyCamera::run(&mut session, config, async |camera| {
        assert_eq!(camera.protocol(), Protocol::V200);
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        ext_info_requests(&session),
        [[300, 1], [300, 1], [300, 1], [200, 1]]
    );
    assert_closed(&session);
}

#[tokio::test]
async fn endless_empty_answers_for_both_generations_name_both_attempts() {
    let sim = SonySimConfig {
        ext_info_empty: u32::MAX,
        ..body(300)
    };
    let mut session = session(sim);
    let config = SonyConfig {
        ext_info_attempts: 2,
        ..fast()
    };
    let unavailable = || Box::new(SonyError::ExtInfoUnavailable { attempts: 2 });
    let result = SonyCamera::run(&mut session, config, async |_| Ok(())).await;
    assert_eq!(
        result,
        Err(SonyError::ProtocolRefused {
            requested: Protocol::V300,
            requested_error: unavailable(),
            fallback: Protocol::V200,
            fallback_error: unavailable(),
        })
    );
    assert_closed(&session);
}

#[tokio::test]
async fn ext_info_retries_stop_at_their_deadline() {
    let sim = SonySimConfig {
        ext_info_busy: u32::MAX,
        ..body(200)
    };
    let mut session = session(sim);
    let config = SonyConfig {
        protocol: Protocol::V200,
        ext_info_attempts: 100_000,
        ext_info_timeout: Duration::from_millis(20),
        poll_interval: Duration::from_millis(5),
        ..fast()
    };
    let result = SonyCamera::run(&mut session, config, async |_| Ok(())).await;
    let Err(SonyError::ExtInfoUnavailable { attempts }) = result else {
        panic!("expected ExtInfoUnavailable, got {result:?}");
    };
    assert!((1..1000).contains(&attempts), "{attempts} attempts");
    assert_closed(&session);
}
