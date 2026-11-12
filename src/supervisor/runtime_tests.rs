//! Startup, failure propagation and discovery worker lifecycle over synthetic inputs.
//!
//! References:
//! - `docs/architecture.md`, CLI and supervisor contract

#[tokio::test]
async fn shutdown_arriving_at_ready_output_stops_before_the_first_rtsp_poll() {
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
    .await
    .unwrap();
    // The ready notification is one-shot, but subsequent diagnostic writes remain valid.
    out.write_all(b"secondary diagnostic").unwrap();
    assert!(std::net::TcpListener::bind((cfg.rtsp_address, cfg.rtsp_port)).is_ok());
    assert!(std::net::TcpListener::bind((cfg.onvif_address, cfg.onvif_port)).is_ok());
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

struct FlushFailure;
impl Write for FlushFailure {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("synthetic flush failure"))
    }
}

#[tokio::test]
async fn ready_output_flush_failure_closes_started_listeners_and_session() {
    let cfg = cfg();
    assert!(
        serve(
            Factory::new(vec![camera()]),
            &cfg,
            None,
            std::future::pending(),
            &mut FlushFailure
        )
        .await
        .is_err()
    );
    assert!(std::net::TcpListener::bind((cfg.rtsp_address, cfg.rtsp_port)).is_ok());
    assert!(std::net::TcpListener::bind((cfg.onvif_address, cfg.onvif_port)).is_ok());
}

use crate as camera_api;
#[path = "../../tests/support/runtime_client.rs"]
mod client;

use super::*;
use crate::config::Secret;
use crate::supervisor::testing::{Factory, camera, jpeg};
use crate::transport::sim::sony::{SonySimConfig, sony_camera};
use tokio::sync::oneshot;

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
async fn shutdown_before_camera_and_invalid_config_do_not_start_listeners() {
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
    assert_eq!(out, [] as [u8; 0]);
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
    let mut full: &mut [u8] = &mut [];
    assert!(
        serve(factory, &cfg, None, std::future::pending(), &mut full)
            .await
            .is_err()
    );
    assert_eq!(released.lock().unwrap().len(), 1);
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
async fn startup_mount_selection_and_credential_validation_are_consistent() {
    let control = control();
    for flags in [(true, true), (true, false), (false, true)] {
        let mut cfg = cfg();
        cfg.streams.h264 = flags.0;
        cfg.streams.mjpeg = flags.1;
        let (_pipeline, rtsp, http) = start_servers(&cfg, &control, &jpeg(32, 32)).await.unwrap();
        assert_eq!(rtsp.local_addr().unwrap().port(), cfg.rtsp_port);
        drop(http);
    }
    let mut cfg = cfg();
    cfg.credentials.username = Secret::new("invalid:username".into());
    assert!(start_servers(&cfg, &control, &jpeg(32, 32)).await.is_err());
    cfg.credentials.username = Secret::new(String::new());
    assert!(start_servers(&cfg, &control, &jpeg(32, 32)).await.is_err());
    cfg.streams.h264 = false;
    cfg.streams.mjpeg = false;
    assert!(start_servers(&cfg, &control, &jpeg(32, 32)).await.is_err());
    let error = crate::onvif::service::SetupError::Auth(crate::onvif::auth::AuthError::Entropy);
    assert!(start_http(&cfg, &control, Err(error)).is_err());
}

struct Ready(Option<oneshot::Sender<()>>);
impl Write for Ready {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(tx) = self.0.take() {
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
