//! Startup, failure propagation and discovery worker lifecycle over synthetic inputs.
//!
//! References:
//! - `docs/architecture.md`, CLI and supervisor contract

#[tokio::test]
async fn shutdown_arriving_at_ready_output_stops_before_the_first_rtsp_poll() {
    let logs = LogCapture::new();
    let cfg = cfg();
    let (ready, ready_rx) = oneshot::channel();
    let mut out = Ready(Some(ready));
    serve(
        Factory::new(vec![camera()]),
        &cfg,
        None,
        async {
            ready_rx.await.unwrap();
        },
        &mut out,
    )
    .with_subscriber(logs.dispatch())
    .await
    .unwrap();
    let logs = logs.text();
    assert!(logs.contains("waiting_camera"));
    assert!(logs.contains("runtime_listening"));
    assert!(logs.contains("runtime_ready"));
    assert_eq!(logs.matches("runtime_stopped").count(), 1);
    assert!(!logs.contains("runtime_failed"));
    // One final counts line, after the actor finished, and none from the five-minute reporter.
    assert_eq!(logs.matches("event=\"run_counters_final\"").count(), 1);
    assert_eq!(logs.matches("event=\"run_counters\"").count(), 0);
    assert!(logs.contains("camera=stopped"));
    assert!(!logs.contains(cfg.credentials.username.expose()));
    assert!(!logs.contains(cfg.credentials.password.expose()));
    // The ready notification is one-shot, but subsequent diagnostic writes remain valid.
    out.write_all(b"secondary diagnostic").unwrap();
    assert!(std::net::TcpListener::bind((cfg.rtsp_address, cfg.rtsp_port)).is_ok());
    assert!(std::net::TcpListener::bind((cfg.onvif_address, cfg.onvif_port)).is_ok());
}

fn actor_control() -> (Supervisor<Factory>, Control) {
    Supervisor::new(
        Factory::new(vec![camera()]),
        crate::sony::SonyConfig::default(),
        SessionConfig::default(),
        Duration::from_millis(10),
    )
    .unwrap()
}

#[tokio::test(start_paused = true)]
async fn counters_are_logged_on_a_fixed_period_until_stop() {
    let logs = LogCapture::new();
    let (_actor, control) = actor_control();
    let work = async {
        tokio::time::sleep(Duration::from_secs(650)).await;
        control.shutdown();
    };
    async { tokio::join!(report_counters(&control, COUNTERS_INTERVAL), work) }
        .with_subscriber(logs.dispatch())
        .await;
    let logs = logs.text();
    // The first line is one period after start, not at start: 300 s and 600 s, not 650 s.
    assert_eq!(logs.matches("event=\"run_counters\"").count(), 2);
    assert!(logs.contains("camera=waiting"));
    assert!(logs.contains("battery_percent=n/a power=n/a"));
    assert!(logs.contains("uptime_secs="));
    assert!(logs.contains("frames_fetched=0 frames_served=0 frames_dropped=0"));
    assert!(logs.contains("reconnects=0 session_restarts=0 connect_misses=0"));
    assert!(logs.contains("snapshots_served=0 snapshots_refused=0"));
    assert_eq!(COUNTERS_INTERVAL, Duration::from_secs(300));

    // Already stopped: no wait for a period, no line.
    let quiet = LogCapture::new();
    let (_actor, control) = actor_control();
    control.shutdown();
    report_counters(&control, COUNTERS_INTERVAL)
        .with_subscriber(quiet.dispatch())
        .await;
    assert_eq!(quiet.text(), "");
}

#[tokio::test(start_paused = true)]
async fn the_counts_line_reports_battery_and_power_without_identity() {
    let logs = LogCapture::new();
    let (actor, control) = actor_control();
    let work = async {
        // Ready at once, and past the first telemetry refresh, which reads a settled battery.
        tokio::time::sleep(Duration::from_secs(6)).await;
        log_counters(&control, "run_counters");
        control.shutdown();
    };
    let _ = tokio::join!(Box::pin(actor.run()), work.with_subscriber(logs.dispatch()));
    let logs = logs.text();
    assert!(logs.contains("camera=ready"), "{logs}");
    assert!(!logs.contains("battery_percent=n/a"), "{logs}");
    assert!(!logs.contains("power=n/a"), "{logs}");
    assert!(logs.contains("frames_fetched=1"), "{logs}");
}

#[tokio::test]
async fn malformed_live_frame_through_ptp_stops_networks_and_closes_the_camera() {
    use crate::transport::sim::sony::Defect;
    use tokio::io::AsyncReadExt;
    let cfg = cfg();
    let camera = sony_camera(SonySimConfig {
        warmup_reads: 0,
        live_view_jpeg: jpeg(32, 32),
        defect: Defect::BadLiveViewAfterFirst,
        ..SonySimConfig::default()
    });
    let factory = Factory::new(vec![camera]);
    let released = Arc::clone(&factory.released);
    let (ready, ready_rx) = oneshot::channel();
    let mut out = Ready(Some(ready));
    let server = Box::pin(serve(factory, &cfg, None, std::future::pending(), &mut out));
    let (result, ()) = tokio::join!(server, async {
        ready_rx.await.unwrap();
        let address = SocketAddr::new(cfg.rtsp_address, cfg.rtsp_port);
        let uri = format!("rtsp://{address}/live/main");
        let mut consumer = client::Client::open(address).await.unwrap();
        consumer.authenticate(&uri).await.unwrap();
        consumer.play(&uri).await.unwrap();
        let mut tail = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            consumer.socket.read_to_end(&mut tail),
        )
        .await
        .unwrap()
        .unwrap();
    });
    assert!(result.is_err());
    let cameras = released.lock().unwrap();
    assert_eq!(cameras.len(), 1);
    assert_eq!(cameras[0].reset_count(), 0);
    assert_eq!(
        cameras[0].commands().last().unwrap().code,
        crate::ptp::session::code::CLOSE_SESSION
    );
}

/// Fails the write, or the flush after it, of the line starting with `line`.
struct LineFailure {
    line: &'static [u8],
    on_flush: bool,
    hit: bool,
}
impl LineFailure {
    fn new(line: &'static str, on_flush: bool) -> Self {
        Self {
            line: line.as_bytes(),
            on_flush,
            hit: false,
        }
    }
}
impl Write for LineFailure {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // `writeln!` with arguments arrives in pieces; only the first starts with the line.
        self.hit |= bytes.starts_with(self.line);
        if self.hit && !self.on_flush {
            return Err(io::Error::other("synthetic write failure"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.hit {
            return Err(io::Error::other("synthetic flush failure"));
        }
        Ok(())
    }
}

#[tokio::test]
async fn ready_output_flush_failure_closes_started_listeners_and_session() {
    let logs = LogCapture::new();
    let cfg = cfg();
    let factory = Factory::new(vec![camera()]);
    let released = Arc::clone(&factory.released);
    assert!(
        serve(
            factory,
            &cfg,
            None,
            std::future::pending(),
            &mut LineFailure::new("Serving", true)
        )
        .with_subscriber(logs.dispatch())
        .await
        .is_err()
    );
    let logs = logs.text();
    assert_eq!(logs.matches("runtime_failed").count(), 1);
    assert!(!logs.contains("runtime_stopped"));
    assert!(!logs.contains("synthetic flush failure"));
    assert!(!logs.contains(&format!("{:?}", jpeg(32, 32))));
    assert!(!logs.contains(cfg.credentials.username.expose()));
    assert!(!logs.contains(cfg.credentials.password.expose()));
    let cameras = released.lock().unwrap();
    assert_eq!(cameras.len(), 1);
    assert_eq!(cameras[0].reset_count(), 0);
    assert_eq!(
        cameras[0].commands().last().unwrap().code,
        crate::ptp::session::code::CLOSE_SESSION
    );
    assert!(std::net::TcpListener::bind((cfg.rtsp_address, cfg.rtsp_port)).is_ok());
    assert!(std::net::TcpListener::bind((cfg.onvif_address, cfg.onvif_port)).is_ok());
}

use crate as camera_api;
#[path = "../../tests/support/runtime_client.rs"]
mod client;

use super::*;
use crate::config::Secret;
use crate::supervisor::testing::{Factory, LogCapture, camera, jpeg};
use crate::transport::sim::sony::{SonySimConfig, sony_camera};
use tokio::sync::oneshot;
use tracing::instrument::WithSubscriber;

fn cfg() -> Config {
    let mut cfg = Config::generate().unwrap();
    cfg.rtsp_address = "127.0.0.1".parse().unwrap();
    cfg.onvif_address = cfg.rtsp_address;
    let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let second = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    cfg.rtsp_port = first.local_addr().unwrap().port();
    cfg.onvif_port = second.local_addr().unwrap().port();
    cfg.credentials.username = Secret::new(client::USER.into());
    cfg.credentials.password = Secret::new(client::PASSWORD.into());
    cfg
}

fn control() -> Control {
    Supervisor::new(
        Factory::new(Vec::new()),
        crate::sony::SonyConfig::default(),
        SessionConfig::default(),
        Duration::from_millis(10),
    )
    .unwrap()
    .1
}

fn discovery_cfg(address: &str) -> discovery::Config {
    discovery::Config::new(
        discovery::EndpointReference::from_uuid(uuid::Uuid::from_u128(1)),
        Vec::new(),
        vec![discovery::Interface {
            addr: address.parse().unwrap(),
            prefix_len: 32,
            index: 0,
        }],
        discovery::ServiceAddress {
            port: 8080,
            path: "/onvif/device_service".into(),
        },
    )
    .unwrap()
}

#[test]
fn profiles_match_enabled_codecs_actual_dimensions_rate_and_mounts() {
    let mut cfg = cfg();
    for flags in [(true, true), (true, false), (false, true)] {
        cfg.streams.h264 = flags.0;
        cfg.streams.mjpeg = flags.1;
        let model = model(&cfg, 64, 48).unwrap();
        assert_eq!(
            model.profiles().len(),
            usize::from(flags.0) + usize::from(flags.1)
        );
        assert_eq!(model.rtsp_port(), cfg.rtsp_port);
        for profile in model.profiles() {
            assert_eq!(
                (profile.width, profile.height, profile.frame_rate),
                (64, 48, cfg.streams.frame_rate)
            );
            assert_eq!(profile.rtsp_path, format!("/live/{}", profile.token));
        }
    }
    cfg.streams.h264 = false;
    cfg.streams.mjpeg = false;
    assert!(model(&cfg, 64, 48).is_err());
}

#[tokio::test]
async fn shutdown_before_camera_only_reports_waiting_and_invalid_config_fails() {
    let mut cfg = cfg();
    let mut out = Vec::new();
    serve(
        Factory::new(Vec::new()),
        &cfg,
        None,
        std::future::ready(()),
        &mut out,
    )
    .await
    .unwrap();
    let out_text = String::from_utf8(out.clone()).unwrap();
    assert!(out_text.starts_with("Listening for RTSP and ONVIF; waiting for a camera."));
    assert!(!out_text.contains("Serving"));
    out.clear();
    cfg.onvif_port = 0;
    assert!(
        serve(
            Factory::new(Vec::new()),
            &cfg,
            None,
            std::future::ready(()),
            &mut out
        )
        .await
        .is_err()
    );
    cfg.streams.frame_rate = 0;
    assert!(
        serve(
            Factory::new(Vec::new()),
            &cfg,
            None,
            std::future::ready(()),
            &mut out
        )
        .await
        .is_err()
    );
    let control = control();
    let mut shutdown: Shutdown<'_> = Box::pin(std::future::pending());
    assert!(
        serve_network(&cfg, &control, None, &mut shutdown, &mut out)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn startup_failures_still_close_the_camera_session() {
    let cfg = cfg();
    let released;
    {
        let invalid = sony_camera(SonySimConfig {
            warmup_reads: 0,
            ..SonySimConfig::default()
        });
        let factory = Factory::new(vec![invalid]);
        released = Arc::clone(&factory.released);
        let mut out = Vec::new();
        assert!(
            serve(factory, &cfg, None, std::future::pending(), &mut out)
                .await
                .is_err()
        );
    }
    assert_eq!(released.lock().unwrap()[0].reset_count(), 0);
    let factory = Factory::new(vec![camera()]);
    let released = Arc::clone(&factory.released);
    let mut failing = LineFailure::new("Serving", false);
    assert!(
        serve(factory, &cfg, None, std::future::pending(), &mut failing)
            .await
            .is_err()
    );
    assert_eq!(released.lock().unwrap().len(), 1);
    // The waiting line fails before any camera exists; nothing is left bound.
    for on_flush in [false, true] {
        let mut failing = LineFailure::new("Listening", on_flush);
        assert!(
            serve(
                Factory::new(Vec::new()),
                &cfg,
                None,
                std::future::pending(),
                &mut failing
            )
            .await
            .is_err()
        );
    }
    assert!(std::net::TcpListener::bind((cfg.rtsp_address, cfg.rtsp_port)).is_ok());
    assert!(std::net::TcpListener::bind((cfg.onvif_address, cfg.onvif_port)).is_ok());
    let occupied = std::net::TcpListener::bind((cfg.rtsp_address, cfg.rtsp_port)).unwrap();
    assert!(
        serve(
            Factory::new(vec![camera()]),
            &cfg,
            None,
            std::future::pending(),
            &mut Vec::new()
        )
        .await
        .is_err()
    );
    drop(occupied);
    let occupied = std::net::TcpListener::bind((cfg.onvif_address, cfg.onvif_port)).unwrap();
    assert!(
        serve(
            Factory::new(vec![camera()]),
            &cfg,
            None,
            std::future::pending(),
            &mut Vec::new()
        )
        .await
        .is_err()
    );
    drop(occupied);
}

#[tokio::test]
async fn listeners_bind_before_any_camera_and_validate_credentials() {
    let control = control();
    let cfg = cfg();
    let (listeners, http) = start_listeners(&cfg, &control).await.unwrap();
    assert_eq!(listeners.rtsp.local_addr().unwrap().port(), cfg.rtsp_port);
    assert!(listeners.rtsp.fanout().is_none());
    assert_eq!(http.local_addr().port(), cfg.onvif_port);
    drop(http);
    let mut cfg = cfg;
    cfg.credentials.username = Secret::new("invalid:username".into());
    assert!(start_listeners(&cfg, &control).await.is_err());
    cfg.credentials.username = Secret::new(String::new());
    assert!(start_listeners(&cfg, &control).await.is_err());
    let error = crate::onvif::service::SetupError::Auth(crate::onvif::auth::AuthError::Entropy);
    assert!(start_http(&cfg, &control, Err(error)).is_err());
}

type Publishers = (
    StreamPublisher<crate::supervisor::RuntimeSource>,
    ModelPublisher,
);

/// Pending RTSP and ONVIF publishers; `mount` is the RTSP primary path.
async fn pending_publishers(
    cfg: &Config, mount: &str,
) -> (RtspServer<crate::supervisor::RuntimeSource>, Publishers) {
    let credentials = crate::rtsp::digest::Credentials::new("user", "password", "test").unwrap();
    let (rtsp, streams) = RtspServer::bind_pending(
        SocketAddr::new(cfg.rtsp_address, 0),
        credentials,
        ServerConfig {
            mount_path: mount.into(),
            ..ServerConfig::default()
        },
    )
    .await
    .unwrap();
    let (_, model) = OnvifService::pending(
        crate::onvif::auth::Credentials::new("user", "password").unwrap(),
        Arc::new(crate::onvif::time::SystemClock),
    )
    .unwrap();
    (rtsp, (streams, model))
}

/// Run the real actor over one synthetic camera and `serve_live` over `publishers` until it
/// finishes on its own (an error) or the camera has produced its first frame.
async fn live_with(cfg: &Config, publishers: Publishers) -> Result<(), RunError> {
    let (actor, control) = Supervisor::new(
        Factory::new(vec![camera()]),
        crate::sony::SonyConfig::default(),
        SessionConfig::default(),
        Duration::from_millis(10),
    )
    .unwrap();
    let mut out = Vec::new();
    let live = async {
        let result = serve_live(cfg, &control, &mut out, publishers, None).await;
        control.shutdown();
        result
    };
    let (_, result) = tokio::join!(Box::pin(actor.run()), live);
    result
}

#[tokio::test]
async fn a_format_the_model_or_mounts_cannot_describe_fails_the_run_not_the_listeners() {
    let mut cfg = cfg();
    // The primary mount collides with the extra JPEG mount: the publisher refuses it.
    let (_rtsp, publishers) = pending_publishers(&cfg, "/live/mjpeg").await;
    assert_eq!(
        live_with(&cfg, publishers).await.unwrap_err().to_string(),
        "RTSP listener unavailable"
    );
    // No codec leaves nothing to describe: the model is refused.
    cfg.streams.h264 = false;
    cfg.streams.mjpeg = false;
    let (_rtsp, publishers) = pending_publishers(&cfg, "/live/main").await;
    assert_eq!(
        live_with(&cfg, publishers).await.unwrap_err().to_string(),
        "invalid network camera description"
    );
}

#[tokio::test]
async fn published_mounts_follow_the_enabled_codecs() {
    for (h264, mjpeg, present, absent) in [
        (true, true, &["/live/main", "/live/mjpeg"][..], &[][..]),
        (true, false, &["/live/main"][..], &["/live/mjpeg"][..]),
        (false, true, &["/live/mjpeg"][..], &["/live/main"][..]),
    ] {
        let mut cfg = cfg();
        cfg.streams.h264 = h264;
        cfg.streams.mjpeg = mjpeg;
        let (stop, stopped) = oneshot::channel();
        let (ready, ready_rx) = oneshot::channel();
        let mut out = Ready(Some(ready));
        let runtime = Box::pin(serve(
            Factory::new(vec![camera()]),
            &cfg,
            None,
            async {
                let _ = stopped.await;
            },
            &mut out,
        ));
        let (result, ()) = tokio::join!(runtime, async {
            ready_rx.await.unwrap();
            let address = SocketAddr::new(cfg.rtsp_address, cfg.rtsp_port);
            let mut consumer = client::Client::open(address).await.unwrap();
            let first = format!("rtsp://{address}{}", present[0]);
            consumer.authenticate(&first).await.unwrap();
            for path in present {
                let uri = format!("rtsp://{address}{path}");
                assert_eq!(
                    consumer.request("DESCRIBE", &uri, "").await.unwrap().status,
                    200
                );
            }
            for path in absent {
                let uri = format!("rtsp://{address}{path}");
                assert_eq!(
                    consumer.request("DESCRIBE", &uri, "").await.unwrap().status,
                    404
                );
            }
            drop(consumer);
            stop.send(()).unwrap();
        });
        result.unwrap();
    }
}

/// Fires on the "serving" line, i.e. once a camera defined the streams.
struct Ready(Option<oneshot::Sender<()>>);
impl Write for Ready {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.starts_with(b"Serving")
            && let Some(tx) = self.0.take()
        {
            let _ = tx.send(());
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn normal_runtime_and_loopback_only_discovery_shut_down_together() {
    let cfg = cfg();
    let (stop, stopped) = oneshot::channel();
    let (ready, ready_rx) = oneshot::channel();
    let mut out = Ready(Some(ready));
    let runtime = Box::pin(serve(
        Factory::new(vec![camera()]),
        &cfg,
        Some(discovery_cfg("127.0.0.1")),
        async {
            let _ = stopped.await;
        },
        &mut out,
    ));
    let (result, ()) = tokio::join!(runtime, async {
        ready_rx.await.unwrap();
        let address = SocketAddr::new(cfg.rtsp_address, cfg.rtsp_port);
        let uri = format!("rtsp://{address}/live/main");
        let mut consumer = client::Client::open(address).await.unwrap();
        consumer.authenticate(&uri).await.unwrap();
        let sdp = consumer.request("DESCRIBE", &uri, "").await.unwrap();
        assert!(String::from_utf8(sdp.body).unwrap().contains("H264/90000"));
        consumer.play(&uri).await.unwrap();
        let frame = consumer.frame(96).await.unwrap();
        let bytes = client::annex_b(&frame).unwrap();
        assert!(crate::media::annexb::contains_idr(&bytes));
        drop(consumer);
        stop.send(()).unwrap();
    });
    result.unwrap();
    // Socket binding errors are real, deterministic nonlocal-interface failures, not a bypass
    // of the discovery engine or a multicast announcement on a physical interface.
    assert!(
        serve(
            Factory::new(vec![camera()]),
            &cfg,
            Some(discovery_cfg("192.0.2.254")),
            std::future::pending(),
            &mut Vec::new()
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn discovery_worker_failures_and_prior_shutdown_are_joined() {
    let control = control();
    let failure = tokio::spawn(async {
        Err(discovery::Error::Io(io::Error::other(
            "synthetic receive failure",
        )))
    });
    assert!(
        finish_discovery(&control, failure, Arc::new(AtomicBool::new(false)))
            .await
            .is_err()
    );
    assert!(control.stopping());
    let cancelled = tokio::spawn(std::future::pending::<Result<(), discovery::Error>>());
    cancelled.abort();
    assert!(
        finish_discovery(&control, cancelled, Arc::new(AtomicBool::new(true)))
            .await
            .is_err()
    );
    let handle = tokio::spawn(std::future::pending::<()>());
    handle.abort();
    assert!(worker_result(handle.await).is_err());
    let responder = Responder::new(
        discovery_cfg("127.0.0.1"),
        discovery::SystemEntropy::default(),
        discovery::UdpTransport::bind(&[]).unwrap(),
        discovery::SystemClock,
    );
    run_discovery(&control, Some(responder)).await.unwrap();
    run_discovery(&control, None).await.unwrap();
}

#[tokio::test]
async fn first_frame_waits_for_a_frame_and_reports_an_actor_that_is_gone() {
    // The supervisor was dropped: no frame can ever come.
    assert_eq!(
        first_frame(&control()).await.unwrap_err().to_string(),
        "camera actor stopped before startup"
    );
    let (actor, control) = Supervisor::new(
        Factory::new(vec![camera()]),
        crate::sony::SonyConfig::default(),
        SessionConfig::default(),
        Duration::from_millis(10),
    )
    .unwrap();
    let work = async {
        let frame = first_frame(&control).await.unwrap().unwrap();
        assert!(frame.starts_with(&[0xff, 0xd8]));
        control.shutdown();
        // After a stop request there is nothing left to wait for.
        assert!(first_frame(&control).await.unwrap().is_none());
    };
    let _ = tokio::join!(Box::pin(actor.run()), work);
}

#[tokio::test]
async fn waiting_service_answers_rtsp_503_through_the_runtime_source() {
    let cfg = cfg();
    let (stop, stopped) = oneshot::channel();
    let (listening, listening_rx) = oneshot::channel();
    let mut out = Listening(Some(listening));
    let runtime = Box::pin(serve(
        Factory::new(Vec::new()),
        &cfg,
        None,
        async {
            let _ = stopped.await;
        },
        &mut out,
    ));
    let (result, ()) = tokio::join!(runtime, async {
        listening_rx.await.unwrap();
        let address = SocketAddr::new(cfg.rtsp_address, cfg.rtsp_port);
        let uri = format!("rtsp://{address}/live/main");
        let mut consumer = client::Client::open(address).await.unwrap();
        consumer.authenticate(&uri).await.unwrap();
        let reply = consumer.request("DESCRIBE", &uri, "").await.unwrap();
        assert_eq!(reply.status, 503);
        assert_eq!(reply.header("retry-after"), Some("5"));
        let setup = "Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n";
        let track = format!("{uri}/trackID=0");
        assert_eq!(
            consumer
                .request("SETUP", &track, setup)
                .await
                .unwrap()
                .status,
            503
        );
        drop(consumer);
        stop.send(()).unwrap();
    });
    result.unwrap();
}

/// Fires on the "waiting" line.
struct Listening(Option<oneshot::Sender<()>>);
impl Write for Listening {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.starts_with(b"Listening")
            && let Some(tx) = self.0.take()
        {
            let _ = tx.send(());
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
