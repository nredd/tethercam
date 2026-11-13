//! Real network/media/actor integration, entirely over synthetic Sony USB transactions.
//!
//! References:
//! - `docs/architecture.md`, CLI and supervisor contract

#[cfg(test)]
use tethercam as camera_api;
#[cfg(test)]
#[path = "support/runtime_client.rs"]
mod client;
#[cfg(test)]
#[path = "support/runtime_client_properties.rs"]
mod client_properties;

#[cfg(test)]
mod suite {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::io::Write;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use jpeg_encoder::{ColorType, Encoder as JpegEncoder, SamplingFactor};
    use tethercam::config::{Config, Secret};
    use tethercam::media::{Encoder, EncoderConfig, Profile, SoftwareH264Encoder, annexb};
    use tethercam::ptp::session::{Session, SessionConfig, code};
    use tethercam::rtp::{Header, jpeg::JpegInfo};
    use tethercam::supervisor::{CameraFactory, OpenError};
    use tethercam::transport::sim::{
        SimCamera,
        sony::{SonySimConfig, sony_camera},
    };
    use tethercam::transport::{Endpoint, sim::Fault};
    use tokio::io::AsyncWriteExt;
    use tokio::sync::oneshot;
    use tokio::time::timeout;

    use super::client::{Client, PASSWORD, USER, annex_b, authorization, nonce};
    const WAIT: Duration = Duration::from_secs(15);

    fn jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut state = 0x1234_5678_u32;
        let rgb: Vec<_> = (0..usize::from(width) * usize::from(height) * 3)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                u8::try_from(state >> 24).unwrap()
            })
            .collect();
        let mut encoder = JpegEncoder::new(&mut bytes, 85);
        encoder.set_sampling_factor(SamplingFactor::F_2_2);
        encoder.encode(&rgb, width, height, ColorType::Rgb).unwrap();
        bytes
    }

    struct Factory {
        cameras: VecDeque<SimCamera>,
        released: Arc<Mutex<Vec<SimCamera>>>,
    }
    impl CameraFactory for Factory {
        type Camera = SimCamera;
        type Session = tethercam::ptp::session::Session<Self::Camera>;

        fn session(
            &self, camera: Self::Camera, mut cfg: tethercam::ptp::session::SessionConfig,
            clock: fn() -> std::time::Instant,
        ) -> Self::Session {
            let _ = &mut cfg;
            Self::Session::new(camera, cfg).with_clock(clock)
        }
        fn recover(&self, session: Self::Session) -> Self::Camera {
            session.into_transport()
        }

        fn connect(&mut self) -> impl Future<Output = Result<Option<SimCamera>, OpenError>> + Send {
            std::future::ready(Ok(self.cameras.pop_front()))
        }
        fn release(&mut self, camera: SimCamera) {
            self.released.lock().unwrap().push(camera);
        }
        fn pause(&self, delay: Duration) -> impl Future<Output = ()> + Send {
            tokio::time::sleep(delay)
        }
    }

    // Observe normal explicit ready output, not a replacement pipeline callback.
    struct Ready(Option<oneshot::Sender<()>>);
    impl Write for Ready {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.starts_with(b"Serving")
                && let Some(tx) = self.0.take()
            {
                let _ = tx.send(());
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Cameras the test hands over while the service runs, as a USB hot-plug would.
    struct Hotplug {
        queue: Arc<Mutex<VecDeque<SimCamera>>>,
        released: Arc<Mutex<Vec<SimCamera>>>,
    }
    impl CameraFactory for Hotplug {
        type Camera = SimCamera;
        type Session = tethercam::ptp::session::Session<Self::Camera>;

        fn session(
            &self, camera: Self::Camera, cfg: tethercam::ptp::session::SessionConfig,
            clock: fn() -> std::time::Instant,
        ) -> Self::Session {
            Self::Session::new(camera, cfg).with_clock(clock)
        }
        fn recover(&self, session: Self::Session) -> Self::Camera {
            session.into_transport()
        }
        fn connect(&mut self) -> impl Future<Output = Result<Option<SimCamera>, OpenError>> + Send {
            std::future::ready(Ok(self.queue.lock().unwrap().pop_front()))
        }
        fn release(&mut self, camera: SimCamera) {
            self.released.lock().unwrap().push(camera);
        }
        fn pause(&self, delay: Duration) -> impl Future<Output = ()> + Send {
            tokio::time::sleep(delay.min(Duration::from_millis(200)))
        }
    }

    /// Fires on the "waiting" line, i.e. once the listeners are bound with no camera.
    struct Listening(Option<oneshot::Sender<()>>);
    impl Write for Listening {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.starts_with(b"Listening")
                && let Some(tx) = self.0.take()
            {
                let _ = tx.send(());
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn cfg() -> Config {
        let mut cfg = Config::generate().unwrap();
        cfg.rtsp_address = "127.0.0.1".parse().unwrap();
        cfg.onvif_address = cfg.rtsp_address;
        let rtsp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let http = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        cfg.rtsp_port = rtsp.local_addr().unwrap().port();
        cfg.onvif_port = http.local_addr().unwrap().port();
        cfg.credentials.username = Secret::new(USER.into());
        cfg.credentials.password = Secret::new(PASSWORD.into());
        cfg
    }

    async fn soap(cfg: &Config, operation: &str) -> String {
        let mut http = Client::open(SocketAddr::new(cfg.onvif_address, cfg.onvif_port))
            .await
            .unwrap();
        let path = "/onvif/media_service";
        let body = format!(
            "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:trt=\"http://www.onvif.org/ver10/media/wsdl\"><s:Body>{operation}</s:Body></s:Envelope>"
        );
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/soap+xml\r\nContent-Length: {}\r\n",
            body.len()
        );
        http.socket
            .write_all(format!("{head}\r\n{body}").as_bytes())
            .await
            .unwrap();
        let challenge = http.reply().await.unwrap();
        assert_eq!(challenge.status, 401);
        let authorization = authorization(
            "POST",
            path,
            nonce(challenge.header("www-authenticate").unwrap()).unwrap(),
            1,
        );
        http.socket
            .write_all(
                format!("{head}Authorization: {authorization}\r\nConnection: close\r\n\r\n{body}")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let reply = http.reply().await.unwrap();
        assert_eq!(reply.status, 200);
        String::from_utf8(reply.body).unwrap()
    }

    async fn uri(cfg: &Config, operation: &str, token: &str) -> String {
        let body = soap(
            cfg,
            &format!(
                "<trt:{operation}><trt:ProfileToken>{token}</trt:ProfileToken></trt:{operation}>"
            ),
        )
        .await;
        let document = roxmltree::Document::parse(&body).unwrap();
        document
            .descendants()
            .find(|n| n.tag_name().name() == "Uri")
            .unwrap()
            .text()
            .unwrap()
            .into()
    }

    async fn snapshot(cfg: &Config, still: &[u8]) {
        let uri = uri(cfg, "GetSnapshotUri", "main").await;
        let address = SocketAddr::new(cfg.onvif_address, cfg.onvif_port);
        let path = uri.strip_prefix(&format!("http://{address}")).unwrap();
        let mut http = Client::open(address).await.unwrap();
        http.socket
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let challenge = http.reply().await.unwrap();
        assert_eq!(challenge.status, 401);
        let authorization = authorization(
            "GET",
            path,
            nonce(challenge.header("www-authenticate").unwrap()).unwrap(),
            1,
        );
        http.socket.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: {authorization}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let reply = http.reply().await.unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, still);
    }

    fn inspect_h264(cfg: &Config, packets: &[Vec<u8>]) {
        let access_unit = annex_b(packets).unwrap();
        let nals: Vec<_> = annexb::nal_units(&access_unit).collect();
        let kinds: Vec<_> = nals
            .iter()
            .filter_map(|n| annexb::nal_unit_type(n))
            .collect();
        assert!(kinds.contains(&7));
        assert!(kinds.contains(&8));
        assert!(kinds.contains(&5));
        let encoder = SoftwareH264Encoder::configure(EncoderConfig {
            width: 64,
            height: 48,
            fps: cfg.streams.frame_rate,
            bitrate_bps: cfg.streams.bitrate,
            keyframe_interval: cfg.streams.frame_rate * 2,
            profile: Profile::Baseline,
        })
        .unwrap();
        assert_eq!(
            nals.iter()
                .find(|n| annexb::nal_unit_type(n) == Some(7))
                .copied()
                .unwrap(),
            encoder.sps()
        );
        assert!(packets.len() > 3); // The noisy scene exercises actual FU-A fragmentation.
    }

    fn inspect_jpeg(original: &[u8], packets: &[Vec<u8>]) {
        let expected = JpegInfo::parse(original).unwrap();
        let mut scan = Vec::new();
        for (index, packet) in packets.iter().enumerate() {
            let header = Header::decode(packet).unwrap();
            assert_eq!(header.payload_type, 26);
            let payload = &packet[12..];
            let offset =
                usize::try_from(u32::from_be_bytes([0, payload[1], payload[2], payload[3]]))
                    .unwrap();
            assert_eq!(offset, scan.len());
            assert_eq!(payload[4], 1);
            assert_eq!((payload[6], payload[7]), (8, 6));
            let skip = if index == 0 {
                assert_eq!(&payload[12..76], expected.tables()[0]);
                assert_eq!(&payload[76..140], expected.tables()[1]);
                140
            } else {
                8
            };
            scan.extend_from_slice(&payload[skip..]);
        }
        assert_eq!(scan, expected.scan);
        assert!(packets.len() > 1);
    }

    async fn consume(cfg: &Config, live: &[u8], still: &[u8], released: &Mutex<Vec<SimCamera>>) {
        let profiles = soap(cfg, "<trt:GetProfiles/>").await;
        assert!(profiles.contains("token=\"main\""));
        assert!(profiles.contains("token=\"mjpeg\""));
        let main_uri = uri(cfg, "GetStreamUri", "main").await;
        let jpeg_uri = uri(cfg, "GetStreamUri", "mjpeg").await;
        let addr = SocketAddr::new(cfg.rtsp_address, cfg.rtsp_port);
        assert_eq!(main_uri, format!("rtsp://{addr}/live/main"));
        assert_eq!(jpeg_uri, format!("rtsp://{addr}/live/mjpeg"));
        let mut main = Client::open(addr).await.unwrap();
        main.authenticate(&main_uri).await.unwrap();
        let sdp = main.request("DESCRIBE", &main_uri, "").await.unwrap();
        assert_eq!(sdp.status, 200);
        assert!(String::from_utf8(sdp.body).unwrap().contains("H264/90000"));
        let mut mjpeg = Client::open(addr).await.unwrap();
        mjpeg.authenticate(&jpeg_uri).await.unwrap();
        let sdp = mjpeg.request("DESCRIBE", &jpeg_uri, "").await.unwrap();
        assert!(String::from_utf8(sdp.body).unwrap().contains("JPEG/90000"));
        main.play(&main_uri).await.unwrap();
        mjpeg.play(&jpeg_uri).await.unwrap();
        inspect_h264(cfg, &main.frame(96).await.unwrap());
        inspect_jpeg(live, &mjpeg.frame(26).await.unwrap());
        let expected = if cfg.streams.full_resolution_snapshot {
            still
        } else {
            live
        };
        snapshot(cfg, expected).await;
        timeout(WAIT, async {
            while released.lock().unwrap().is_empty() {
                let _ = main.frame(96).await.unwrap();
            }
        })
        .await
        .unwrap();
        let _after_reconnect = main.frame(96).await.unwrap();
    }

    #[tokio::test]
    async fn both_authenticated_mounts_snapshot_reconnect_and_close() {
        scenario(false).await;
    }

    #[tokio::test]
    async fn opt_in_full_resolution_snapshots_serve_the_still() {
        scenario(true).await;
    }

    async fn scenario(full_resolution: bool) {
        let mut cfg = cfg();
        cfg.streams.full_resolution_snapshot = full_resolution;
        let live = jpeg(64, 48);
        let still = jpeg(128, 96);
        let sim = SonySimConfig {
            warmup_reads: 0,
            live_view_jpeg: live.clone(),
            still: still.clone(),
            ..SonySimConfig::default()
        };
        let mut first = sony_camera(sim.clone());
        first.inject(Endpoint::BulkOut, 80, Fault::Disconnect);
        let released = Arc::new(Mutex::new(Vec::new()));
        let factory = Factory {
            cameras: VecDeque::from([first, sony_camera(sim)]),
            released: Arc::clone(&released),
        };
        let (stop, stopped) = oneshot::channel();
        let (ready, ready_rx) = oneshot::channel();
        let server = Box::pin(async {
            tethercam::supervisor::serve(
                factory,
                &cfg,
                None,
                async {
                    let _ = stopped.await;
                },
                &mut Ready(Some(ready)),
            )
            .await
        });
        let clients = async {
            ready_rx.await.unwrap();
            consume(&cfg, &live, &still, &released).await;
            stop.send(()).unwrap();
        };
        let (result, ()) = timeout(Duration::from_secs(30), async {
            tokio::join!(server, clients)
        })
        .await
        .unwrap();
        result.unwrap();
        let healthy = {
            let mut cameras = released.lock().unwrap();
            assert_eq!(cameras.len(), 2);
            cameras.pop().unwrap()
        };
        assert_eq!(healthy.reset_count(), 0);
        assert_eq!(
            healthy
                .commands()
                .iter()
                .filter(|c| c.code == code::CLOSE_SESSION)
                .count(),
            1
        );
        let mut session = Session::new(healthy, SessionConfig::default());
        session.open().await.unwrap();
        session.close().await.unwrap();
        assert_eq!(session.transport().reset_count(), 0);
    }
    /// An authenticated `GET` on the ONVIF port; returns status and body.
    async fn authed_get(cfg: &Config, path: &str) -> (u16, String) {
        let address = SocketAddr::new(cfg.onvif_address, cfg.onvif_port);
        let mut http = Client::open(address).await.unwrap();
        let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n");
        http.socket
            .write_all(format!("{request}\r\n").as_bytes())
            .await
            .unwrap();
        let challenge = http.reply().await.unwrap();
        assert_eq!(challenge.status, 401);
        let authorization = authorization(
            "GET",
            path,
            nonce(challenge.header("www-authenticate").unwrap()).unwrap(),
            1,
        );
        http.socket
            .write_all(
                format!("{request}Authorization: {authorization}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let reply = http.reply().await.unwrap();
        (reply.status, String::from_utf8(reply.body).unwrap())
    }

    /// Poll the status endpoint until the camera state is `state`.
    async fn wait_for_state(cfg: &Config, state: &str) {
        let wanted = format!("\"camera\":\"{state}\"");
        timeout(WAIT, async {
            loop {
                let (status, body) = authed_get(cfg, "/tethercam/status").await;
                assert_eq!(status, 200);
                if body.contains(&wanted) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("status never became {state}"));
    }

    /// `DESCRIBE` on a fresh authenticated connection; returns the reply.
    async fn describe(cfg: &Config, path: &str) -> super::client::Reply {
        let addr = SocketAddr::new(cfg.rtsp_address, cfg.rtsp_port);
        let uri = format!("rtsp://{addr}{path}");
        let mut rtsp = Client::open(addr).await.unwrap();
        rtsp.authenticate(&uri).await.unwrap();
        rtsp.request("DESCRIBE", &uri, "").await.unwrap()
    }

    async fn media_status(cfg: &Config, operation: &str) -> u16 {
        let mut http = Client::open(SocketAddr::new(cfg.onvif_address, cfg.onvif_port))
            .await
            .unwrap();
        let body = format!(
            "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:trt=\"http://www.onvif.org/ver10/media/wsdl\"><s:Body>{operation}</s:Body></s:Envelope>"
        );
        let head = format!(
            "POST /onvif/media_service HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/soap+xml\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        http.socket
            .write_all(format!("{head}{body}").as_bytes())
            .await
            .unwrap();
        http.reply().await.unwrap().status
    }

    #[tokio::test]
    async fn service_waits_without_a_camera_becomes_ready_and_survives_unplug() {
        let cfg = cfg();
        let live = jpeg(64, 48);
        let sim = SonySimConfig {
            warmup_reads: 0,
            live_view_jpeg: live.clone(),
            ..SonySimConfig::default()
        };
        let mut first = sony_camera(sim.clone());
        first.inject(Endpoint::BulkOut, 18, Fault::Disconnect);
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let released = Arc::new(Mutex::new(Vec::new()));
        let factory = Hotplug {
            queue: Arc::clone(&queue),
            released: Arc::clone(&released),
        };
        let (stop, stopped) = oneshot::channel();
        let (listening, listening_rx) = oneshot::channel();
        let server = Box::pin(async {
            tethercam::supervisor::serve(
                factory,
                &cfg,
                None,
                async {
                    let _ = stopped.await;
                },
                &mut Listening(Some(listening)),
            )
            .await
        });
        let clients = async {
            listening_rx.await.unwrap();
            // Waiting: reachable, and every surface says so instead of hanging or guessing.
            let reply = describe(&cfg, "/live/main").await;
            assert_eq!(reply.status, 503);
            assert_eq!(reply.header("retry-after"), Some("5"));
            assert_eq!(media_status(&cfg, "<trt:GetProfiles/>").await, 503);
            wait_for_state(&cfg, "waiting").await;
            let (waiting, body) = authed_get(&cfg, "/tethercam/status").await;
            assert_eq!((waiting, body.as_str()), (200, "{\"camera\":\"waiting\"}"));

            // A camera appears: profiles, SDP and video come from its first frame.
            queue.lock().unwrap().push_back(first);
            wait_for_state(&cfg, "ready").await;
            // Status flips with the camera session; the streams publish a moment later.
            timeout(WAIT, async {
                while describe(&cfg, "/live/main").await.status != 200 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .unwrap();
            let profiles = soap(&cfg, "<trt:GetProfiles/>").await;
            assert!(profiles.contains("token=\"main\""));
            assert!(profiles.contains("Width>64<"));
            assert_eq!(describe(&cfg, "/live/main").await.status, 200);
            let main_uri = uri(&cfg, "GetStreamUri", "main").await;
            let addr = SocketAddr::new(cfg.rtsp_address, cfg.rtsp_port);
            let mut main = Client::open(addr).await.unwrap();
            main.authenticate(&main_uri).await.unwrap();
            main.play(&main_uri).await.unwrap();
            let _ = main.frame(96).await.unwrap();

            // It goes away: back to waiting, listeners and the published format stay up, and a
            // snapshot is refused rather than served stale.
            wait_for_state(&cfg, "waiting").await;
            assert_eq!(released.lock().unwrap().len(), 1);
            assert_eq!(describe(&cfg, "/live/main").await.status, 200);
            let snapshot_uri = uri(&cfg, "GetSnapshotUri", "main").await;
            let path = snapshot_uri
                .strip_prefix(&format!("http://{}:{}", cfg.onvif_address, cfg.onvif_port))
                .unwrap();
            assert_eq!(authed_get(&cfg, path).await.0, 503);

            // It returns: ready again and the same client's stream resumes.
            queue.lock().unwrap().push_back(sony_camera(sim.clone()));
            wait_for_state(&cfg, "ready").await;
            let _ = timeout(WAIT, main.frame(96)).await.unwrap().unwrap();
            stop.send(()).unwrap();
        };
        let (result, ()) = timeout(Duration::from_secs(60), async {
            tokio::join!(server, clients)
        })
        .await
        .unwrap();
        result.unwrap();
        assert_eq!(released.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_camera_returning_with_another_resolution_fails_the_run_instead_of_being_served() {
        let cfg = cfg();
        let sim = |width, height| SonySimConfig {
            warmup_reads: 0,
            live_view_jpeg: jpeg(width, height),
            ..SonySimConfig::default()
        };
        let mut first = sony_camera(sim(64, 48));
        first.inject(Endpoint::BulkOut, 18, Fault::Disconnect);
        let queue = Arc::new(Mutex::new(VecDeque::from([first])));
        let released = Arc::new(Mutex::new(Vec::new()));
        let factory = Hotplug {
            queue: Arc::clone(&queue),
            released: Arc::clone(&released),
        };
        let (ready, ready_rx) = oneshot::channel();
        let mut out = Ready(Some(ready));
        let server = Box::pin(tethercam::supervisor::serve(
            factory,
            &cfg,
            None,
            std::future::pending(),
            &mut out,
        ));
        let clients = async {
            ready_rx.await.unwrap();
            let main_uri = uri(&cfg, "GetStreamUri", "main").await;
            let addr = SocketAddr::new(cfg.rtsp_address, cfg.rtsp_port);
            let mut main = Client::open(addr).await.unwrap();
            main.authenticate(&main_uri).await.unwrap();
            main.play(&main_uri).await.unwrap();
            wait_for_state(&cfg, "waiting").await;
            queue.lock().unwrap().push_back(sony_camera(sim(32, 32)));
        };
        let (result, ()) = timeout(Duration::from_secs(60), async {
            tokio::join!(server, clients)
        })
        .await
        .unwrap();
        assert!(matches!(
            result,
            Err(tethercam::supervisor::RunError::Pipeline(_))
        ));
        // Both bodies were closed cleanly; the second was never streamed.
        assert_eq!(released.lock().unwrap().len(), 2);
    }
}
