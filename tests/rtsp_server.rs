//! RTSP server over real loopback sockets with a synthetic `MediaSource`.
//!
//! Every server binds `127.0.0.1:0`; nothing here uses a fixed port.

#[cfg(test)]
mod suite {
    use std::fmt::Write as _;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use tethercam::rtsp::message::Limits;
    use tethercam::rtsp::source::ScriptedSource;
    use tethercam::rtsp::{
        Algorithm, ClientDigest, Credentials, MediaKind, Packet, RtspServer, ServerConfig,
        TrackInfo,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpStream, UdpSocket};
    use tokio::sync::{mpsc, oneshot};
    use tokio::task::JoinHandle;
    use tokio::time::timeout;

    const USER: &str = "camera";
    const PASS: &str = "correct-horse-battery";
    const REALM: &str = "tethercam";
    const WAIT: Duration = Duration::from_secs(5);

    struct Counter {
        source: Arc<ScriptedSource>,
        starts: bool,
    }

    impl Counter {
        fn load(&self, _: Ordering) -> usize {
            if self.starts {
                self.source.starts()
            } else {
                self.source.stops()
            }
        }
    }

    struct TestSource {
        tx: mpsc::UnboundedSender<Packet>,
        source: Arc<ScriptedSource>,
        starts: Counter,
        stops: Counter,
    }

    impl TestSource {
        fn new() -> Self {
            let (source, tx) = ScriptedSource::channel(track("H264"));
            let source = Arc::new(source);
            Self {
                tx,
                starts: Counter {
                    source: Arc::clone(&source),
                    starts: true,
                },
                stops: Counter {
                    source: Arc::clone(&source),
                    starts: false,
                },
                source,
            }
        }

        fn packet(seq: u16) -> Vec<u8> {
            let mut p = vec![0x80, 96, 0, 0, 0, 0, 0x0b, 0xb8, 0xca, 0xfe, 0xba, 0xbe];
            p[2..4].copy_from_slice(&seq.to_be_bytes());
            p.extend_from_slice(b"frame-data");
            p
        }

        fn push(&self, seq: u16) {
            let _ = self.tx.send(Packet::new(Self::packet(seq)));
        }
    }

    fn track(codec: &str) -> TrackInfo {
        TrackInfo {
            kind: MediaKind::Video,
            payload_type: 96,
            codec: codec.into(),
            clock_rate: 90_000,
            fmtp: Some("packetization-mode=1".into()),
            control: "trackID=0".into(),
        }
    }

    impl std::ops::Deref for TestSource {
        type Target = Arc<ScriptedSource>;

        fn deref(&self) -> &Self::Target {
            &self.source
        }
    }

    struct Running {
        addr: SocketAddr,
        source: TestSource,
        stop: Option<oneshot::Sender<()>>,
        task: JoinHandle<()>,
    }

    impl Running {
        fn url(&self, path: &str) -> String {
            format!("rtsp://{}{path}", self.addr)
        }

        async fn shutdown(mut self) {
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
            timeout(WAIT, self.task).await.unwrap().unwrap();
        }
    }

    fn config() -> ServerConfig {
        ServerConfig {
            rtcp_interval: Duration::from_millis(100),
            ..ServerConfig::default()
        }
    }

    async fn start(config: ServerConfig) -> Running {
        let source = TestSource::new();
        let creds = Credentials::new(USER, PASS, REALM).unwrap();
        let server = RtspServer::bind(
            "127.0.0.1:0".parse().unwrap(),
            Arc::clone(&source),
            creds,
            config,
        )
        .await
        .unwrap();
        let addr = server.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            server
                .serve(async move {
                    let _ = stopped.await;
                })
                .await;
        });
        Running {
            addr,
            source,
            stop: Some(stop),
            task,
        }
    }

    struct Reply {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl Reply {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }

        fn all(&self, name: &str) -> Vec<&str> {
            self.headers
                .iter()
                .filter(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
                .collect()
        }

        fn session(&self) -> String {
            let v = self.header("session").unwrap();
            v.split(';').next().unwrap().to_owned()
        }
    }

    struct Client {
        stream: TcpStream,
        buf: Vec<u8>,
        cseq: u32,
        nonce: Option<String>,
        nc: u32,
        algorithm: Algorithm,
        password: String,
        /// Answer like an RFC 2069 client: no `qop`, `nc` or `cnonce`.
        legacy: bool,
    }

    impl Client {
        async fn connect(addr: SocketAddr) -> Self {
            Self {
                stream: TcpStream::connect(addr).await.unwrap(),
                buf: Vec::new(),
                cseq: 0,
                nonce: None,
                nc: 0,
                algorithm: Algorithm::Sha256,
                password: PASS.into(),
                legacy: false,
            }
        }

        async fn fill(&mut self) -> usize {
            let mut chunk = [0_u8; 4096];
            let n = timeout(WAIT, self.stream.read(&mut chunk))
                .await
                .expect("read timed out")
                .unwrap_or(0);
            self.buf.extend_from_slice(&chunk[..n]);
            n
        }

        async fn send_raw(&mut self, bytes: &[u8]) {
            self.stream.write_all(bytes).await.unwrap();
        }

        /// Send a request with an explicit `Authorization` value (or none).
        async fn send(
            &mut self, method: &str, uri: &str, auth: Option<String>, extra: &[(&str, &str)],
            body: &str,
        ) {
            self.cseq += 1;
            let mut req = format!("{method} {uri} RTSP/1.0\r\nCSeq: {}\r\n", self.cseq);
            if let Some(auth) = auth {
                let _ = write!(req, "Authorization: {auth}\r\n");
            }
            for (name, value) in extra {
                let _ = write!(req, "{name}: {value}\r\n");
            }
            if !body.is_empty() {
                let _ = write!(req, "Content-Length: {}\r\n", body.len());
            }
            req.push_str("\r\n");
            req.push_str(body);
            self.send_raw(req.as_bytes()).await;
        }

        async fn read_reply(&mut self) -> Reply {
            loop {
                if let Some(end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8(self.buf[..end].to_vec()).unwrap();
                    let mut lines = head.split("\r\n");
                    let status_line = lines.next().unwrap();
                    assert!(status_line.starts_with("RTSP/1.0 "), "{status_line}");
                    let status: u16 = status_line[9..12].parse().unwrap();
                    let headers: Vec<(String, String)> = lines
                        .map(|l| {
                            let (n, v) = l.split_once(':').unwrap();
                            (n.to_owned(), v.trim().to_owned())
                        })
                        .collect();
                    let len: usize = headers
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
                        .map_or(0, |(_, v)| v.parse().unwrap());
                    while self.buf.len() < end + 4 + len {
                        assert!(self.fill().await > 0, "closed mid-body");
                    }
                    let body =
                        String::from_utf8(self.buf[end + 4..end + 4 + len].to_vec()).unwrap();
                    self.buf.drain(..end + 4 + len);
                    return Reply {
                        status,
                        headers,
                        body,
                    };
                }
                assert!(self.fill().await > 0, "connection closed before a reply");
            }
        }

        /// Unauthenticated `OPTIONS` to learn the nonce, as a real client would.
        async fn login(&mut self, uri: &str) {
            self.send("OPTIONS", uri, None, &[], "").await;
            let reply = self.read_reply().await;
            assert_eq!(reply.status, 401);
            self.take_challenge(&reply);
        }

        fn take_challenge(&mut self, reply: &Reply) {
            let wanted = self.algorithm.token();
            let challenge = reply
                .all("www-authenticate")
                .into_iter()
                .find(|ch| ch.contains(&format!("algorithm={wanted}")))
                .expect("challenge for algorithm");
            let start = challenge.find("nonce=\"").unwrap() + 7;
            let rest = &challenge[start..];
            self.nonce = Some(rest[..rest.find('"').unwrap()].to_owned());
            self.nc = 0;
        }

        fn authorization(&mut self, method: &str, uri: &str) -> String {
            self.nc += 1;
            let digest = ClientDigest {
                algorithm: self.algorithm,
                username: USER,
                password: &self.password,
                realm: REALM,
                nonce: self.nonce.as_deref().unwrap(),
                method,
                uri,
                nc: self.nc,
                cnonce: "5f1d3a7c",
            };
            if self.legacy {
                digest.legacy_header_value()
            } else {
                digest.header_value()
            }
        }

        async fn request(&mut self, method: &str, uri: &str, extra: &[(&str, &str)]) -> Reply {
            let auth = self.authorization(method, uri);
            self.send(method, uri, Some(auth), extra, "").await;
            self.read_reply().await
        }

        async fn read_frame(&mut self) -> (u8, Vec<u8>) {
            loop {
                if self.buf.len() >= 4 {
                    assert_eq!(
                        self.buf[0],
                        b'$',
                        "expected interleaved frame, got {:?}",
                        &self.buf[..self.buf.len().min(16)]
                    );
                    let len = usize::from(u16::from_be_bytes([self.buf[2], self.buf[3]]));
                    if self.buf.len() >= 4 + len {
                        let channel = self.buf[1];
                        let payload = self.buf[4..4 + len].to_vec();
                        self.buf.drain(..4 + len);
                        return (channel, payload);
                    }
                }
                assert!(self.fill().await > 0, "connection closed before a frame");
            }
        }

        async fn is_closed(&mut self) -> bool {
            loop {
                match timeout(WAIT, self.stream.read(&mut [0_u8; 256])).await {
                    Ok(Ok(0) | Err(_)) => return true,
                    Ok(Ok(_)) => {}
                    Err(_) => return false,
                }
            }
        }
    }

    async fn wait_for(what: &str, cond: impl Fn() -> bool) {
        for _ in 0..200 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }

    #[tokio::test]
    async fn tcp_interleaved_flow() {
        let server = start(config()).await;
        let base = server.url("/stream");
        let track = server.url("/stream/trackID=0");
        let mut client = Client::connect(server.addr).await;
        client.login(&base).await;

        // OPTIONS itself needs credentials, and works with them.
        let reply = client.request("OPTIONS", &base, &[]).await;
        assert_eq!(reply.status, 200);
        assert!(reply.header("public").unwrap().contains("GET_PARAMETER"));

        let reply = client
            .request("DESCRIBE", &base, &[("Accept", "application/sdp")])
            .await;
        assert_eq!(reply.status, 200);
        assert_eq!(reply.header("content-type"), Some("application/sdp"));
        assert!(reply.body.contains("m=video 0 RTP/AVP 96"));
        assert!(reply.body.contains("a=rtpmap:96 H264/90000"));
        assert!(reply.body.contains("a=fmtp:96 packetization-mode=1"));
        assert!(reply.body.contains("a=control:trackID=0"));
        assert_eq!(
            server.source.starts.load(Ordering::SeqCst),
            0,
            "DESCRIBE must not start the camera"
        );

        let reply = client
            .request(
                "SETUP",
                &track,
                &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1")],
            )
            .await;
        assert_eq!(reply.status, 200);
        assert_eq!(
            reply.header("transport"),
            Some("RTP/AVP/TCP;unicast;interleaved=0-1")
        );
        let session = reply.session();
        assert_eq!(session.len(), 16);
        assert!(session.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(reply.header("session").unwrap().ends_with(";timeout=60"));

        let reply = client
            .request("PLAY", &base, &[("Session", &session)])
            .await;
        assert_eq!(reply.status, 200);
        wait_for("camera start", || {
            server.source.starts.load(Ordering::SeqCst) == 1
        })
        .await;

        for seq in 1..=3 {
            server.source.push(seq);
        }
        let mut seen = Vec::new();
        let mut reports = 0;
        while seen.len() < 3 {
            let (channel, payload) = client.read_frame().await;
            match channel {
                0 => seen.push(payload),
                1 => {
                    assert_eq!(payload[1], 200, "RTCP sender report");
                    reports += 1;
                }
                other => panic!("unexpected channel {other}"),
            }
        }
        for (i, payload) in seen.iter().enumerate() {
            assert_eq!(payload, &TestSource::packet(u16::try_from(i + 1).unwrap()));
        }
        // The first report goes out with the first packet; keep reading until we have seen it.
        while reports == 0 {
            let (channel, payload) = client.read_frame().await;
            if channel == 1 {
                assert_eq!(payload[1], 200);
                reports += 1;
            }
        }

        let reply = client
            .request("GET_PARAMETER", &base, &[("Session", &session)])
            .await;
        assert_eq!(reply.status, 200);
        // Media frames may be interleaved with the reply; drain any before the next reply.
        let reply = client
            .request_skipping_frames("TEARDOWN", &base, &[("Session", &session)])
            .await;
        assert_eq!(reply.status, 200);
        wait_for("camera stop", || {
            server.source.stops.load(Ordering::SeqCst) == 1
        })
        .await;
        server.shutdown().await;
    }

    impl Client {
        /// Like [`Client::request`] but discards interleaved frames that arrive before the reply.
        async fn request_skipping_frames(
            &mut self, method: &str, uri: &str, extra: &[(&str, &str)],
        ) -> Reply {
            let auth = self.authorization(method, uri);
            self.send(method, uri, Some(auth), extra, "").await;
            loop {
                if self.buf.first() == Some(&b'$') {
                    self.read_frame().await;
                } else if self.buf.starts_with(b"RTSP/1.0") {
                    return self.read_reply().await;
                } else {
                    assert!(self.fill().await > 0, "closed before reply");
                }
            }
        }
    }

    #[tokio::test]
    async fn udp_flow_with_server_port_allocation() {
        let server = start(config()).await;
        let base = server.url("/stream");
        let track = server.url("/stream/trackID=0");
        let media = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let control = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (media_port, control_port) = (
            media.local_addr().unwrap().port(),
            control.local_addr().unwrap().port(),
        );

        let mut client = Client::connect(server.addr).await;
        client.algorithm = Algorithm::Md5;
        client.login(&base).await;
        let description = client
            .request("DESCRIBE", &base, &[("Accept", "application/sdp")])
            .await;
        assert_eq!(description.status, 200);
        assert_eq!(description.header("content-type"), Some("application/sdp"));
        assert!(description.body.contains("a=control:trackID=0"));
        assert_eq!(server.source.starts.load(Ordering::SeqCst), 0);
        let transport = format!("RTP/AVP;unicast;client_port={media_port}-{control_port}");
        let reply = client
            .request("SETUP", &track, &[("Transport", &transport)])
            .await;
        assert_eq!(reply.status, 200);
        let transport_header = reply.header("transport").unwrap();
        assert!(
            transport_header.contains(&format!("client_port={media_port}-{control_port}")),
            "{transport_header}"
        );
        let sp = transport_header.split("server_port=").nth(1).unwrap();
        let (a, b) = sp.split_once('-').unwrap();
        let (a, b): (u16, u16) = (a.parse().unwrap(), b.parse().unwrap());
        assert!(a != 0 && a % 2 == 0 && b == a + 1, "server ports {a}-{b}");
        let session = reply.session();

        assert_eq!(
            client
                .request("PLAY", &base, &[("Session", &session)])
                .await
                .status,
            200
        );
        wait_for("camera start", || {
            server.source.starts.load(Ordering::SeqCst) == 1
        })
        .await;
        server.source.push(7);
        server.source.push(8);

        let mut buf = [0_u8; 2048];
        for seq in [7, 8] {
            let (n, from) = timeout(WAIT, media.recv_from(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(from.port(), a, "RTP comes from the allocated server port");
            assert_eq!(&buf[..n], &TestSource::packet(seq)[..]);
        }
        let (n, from) = timeout(WAIT, control.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(from.port(), b);
        assert_eq!(buf[1], 200, "sender report");
        assert!(n >= 28);
        assert_eq!(&buf[4..8], &0xcafe_babe_u32.to_be_bytes());

        assert_eq!(
            client
                .request("TEARDOWN", &base, &[("Session", &session)])
                .await
                .status,
            200
        );
        wait_for("camera stop", || {
            server.source.stops.load(Ordering::SeqCst) == 1
        })
        .await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn every_method_requires_credentials_including_options() {
        let server = start(config()).await;
        let base = server.url("/stream");
        let mut client = Client::connect(server.addr).await;
        for method in [
            "OPTIONS",
            "DESCRIBE",
            "SETUP",
            "PLAY",
            "TEARDOWN",
            "GET_PARAMETER",
            "PAUSE",
            "BOGUS",
        ] {
            client.send(method, &base, None, &[], "").await;
            let reply = client.read_reply().await;
            assert_eq!(reply.status, 401, "{method}");
            let challenges = reply.all("www-authenticate");
            assert_eq!(challenges.len(), 2, "{method}");
            assert!(
                challenges[0].contains("algorithm=SHA-256")
                    && challenges[1].contains("algorithm=MD5")
            );
            assert!(
                challenges
                    .iter()
                    .all(|ch| ch.starts_with("Digest ") && ch.contains("qop=\"auth\""))
            );
            assert!(reply.header("cseq").is_some());
        }
        assert_eq!(server.source.starts.load(Ordering::SeqCst), 0);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn wrong_and_replayed_credentials_are_rejected() {
        let server = start(config()).await;
        let base = server.url("/stream");
        let mut client = Client::connect(server.addr).await;
        client.login(&base).await;

        client.password = "wrong".into();
        let reply = client.request("OPTIONS", &base, &[]).await;
        assert_eq!(reply.status, 401);
        assert!(!reply.header("www-authenticate").unwrap().contains("stale"));

        // Replay: the same header twice. The first use is accepted, the second is not.
        client.password = PASS.into();
        client.take_challenge(&reply);
        let auth = client.authorization("DESCRIBE", &base);
        client
            .send("DESCRIBE", &base, Some(auth.clone()), &[], "")
            .await;
        assert_eq!(client.read_reply().await.status, 200);
        client.send("DESCRIBE", &base, Some(auth), &[], "").await;
        let second = client.read_reply().await;
        assert_eq!(second.status, 401);

        // A digest for another method or URI is not transferable.
        client.take_challenge(&second);
        let for_options = client.authorization("OPTIONS", &base);
        client
            .send("DESCRIBE", &base, Some(for_options), &[], "")
            .await;
        assert_eq!(client.read_reply().await.status, 401);
        server.shutdown().await;
    }

    fn legacy_client(mut client: Client) -> Client {
        client.algorithm = Algorithm::Md5;
        client.legacy = true;
        client
    }

    #[tokio::test]
    async fn qop_less_md5_client_completes_a_full_session() {
        let server = start(config()).await;
        let base = server.url("/stream");
        let track = server.url("/stream/trackID=0");
        let mut client = legacy_client(Client::connect(server.addr).await);
        client.login(&base).await;
        // One nonce serves every request, as live555-based clients do.
        assert_eq!(client.request("OPTIONS", &base, &[]).await.status, 200);
        let reply = client.request("DESCRIBE", &base, &[]).await;
        assert_eq!(reply.status, 200);
        assert!(reply.body.contains("m=video"));
        let reply = client
            .request(
                "SETUP",
                &track,
                &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1")],
            )
            .await;
        assert_eq!(reply.status, 200);
        let session = reply.session();
        let reply = client
            .request("PLAY", &base, &[("Session", &session)])
            .await;
        assert_eq!(reply.status, 200);
        server.source.push(1);
        let (channel, payload) = loop {
            let frame = client.read_frame().await;
            if frame.0 == 0 {
                break frame;
            }
        };
        assert_eq!((channel, payload), (0, TestSource::packet(1)));
        let reply = client
            .request_skipping_frames("TEARDOWN", &base, &[("Session", &session)])
            .await;
        assert_eq!(reply.status, 200);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn qop_less_md5_wrong_password_is_rejected() {
        let server = start(config()).await;
        let base = server.url("/stream");
        let mut client = legacy_client(Client::connect(server.addr).await);
        client.login(&base).await;
        client.password = "wrong".into();
        let reply = client.request("OPTIONS", &base, &[]).await;
        assert_eq!(reply.status, 401);
        assert!(
            !reply.header("www-authenticate").unwrap().contains("stale"),
            "a wrong password must never look stale"
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn qop_less_nonce_from_another_connection_is_stale_and_not_a_strike() {
        // One strike would close the connection: stale retries must not count.
        let server = start(ServerConfig {
            max_auth_failures: 1,
            ..config()
        })
        .await;
        let base = server.url("/stream");
        let mut owner = legacy_client(Client::connect(server.addr).await);
        owner.login(&base).await;
        assert_eq!(owner.request("OPTIONS", &base, &[]).await.status, 200);

        let mut other = legacy_client(Client::connect(server.addr).await);
        // The first, unauthenticated request does not count either.
        other.send("OPTIONS", &base, None, &[], "").await;
        assert_eq!(other.read_reply().await.status, 401);
        // A valid digest over the other connection's nonce: stale, then a fresh nonce works.
        other.nonce = owner.nonce.clone();
        let reply = other.request("OPTIONS", &base, &[]).await;
        assert_eq!(reply.status, 401);
        assert!(
            reply
                .all("www-authenticate")
                .iter()
                .all(|ch| ch.ends_with("stale=true"))
        );
        other.take_challenge(&reply);
        assert_eq!(other.request("OPTIONS", &base, &[]).await.status, 200);
        assert_eq!(other.request("DESCRIBE", &base, &[]).await.status, 200);
        // The owner is unaffected.
        assert_eq!(owner.request("DESCRIBE", &base, &[]).await.status, 200);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn expired_nonce_gets_stale_challenge_and_retry_succeeds() {
        let server = start(ServerConfig {
            nonce_lifetime: Duration::from_millis(150),
            ..config()
        })
        .await;
        let base = server.url("/stream");
        let mut client = Client::connect(server.addr).await;
        client.login(&base).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let reply = client.request("OPTIONS", &base, &[]).await;
        assert_eq!(reply.status, 401);
        assert!(
            reply
                .all("www-authenticate")
                .iter()
                .all(|ch| ch.ends_with("stale=true"))
        );
        client.take_challenge(&reply);
        assert_eq!(client.request("OPTIONS", &base, &[]).await.status, 200);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn repeated_bad_credentials_close_the_connection() {
        let server = start(ServerConfig {
            max_auth_failures: 3,
            ..config()
        })
        .await;
        let base = server.url("/stream");
        let mut client = Client::connect(server.addr).await;
        client.login(&base).await;
        client.password = "wrong".into();
        for _ in 0..3 {
            assert_eq!(client.request("OPTIONS", &base, &[]).await.status, 401);
        }
        assert!(client.is_closed().await);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn malformed_and_oversize_requests_are_rejected_without_killing_the_server() {
        let server = start(ServerConfig {
            limits: Limits {
                max_head_bytes: 512,
                max_headers: 8,
                max_body_bytes: 64,
            },
            ..config()
        })
        .await;

        let mut cases: Vec<(&str, Vec<u8>, u16)> = vec![
            (
                "garbage",
                b"\x00\x01\x02 not rtsp at all\r\n\r\n".to_vec(),
                400,
            ),
            ("http", b"GET / HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(), 400),
            (
                "future version",
                b"OPTIONS * RTSP/2.0\r\nCSeq: 1\r\n\r\n".to_vec(),
                505,
            ),
            (
                "huge header",
                format!("OPTIONS * RTSP/1.0\r\nX: {}\r\n\r\n", "a".repeat(600)).into_bytes(),
                413,
            ),
            ("unterminated head", vec![b'A'; 700], 413),
            (
                "huge body",
                b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 65\r\n\r\n".to_vec(),
                413,
            ),
            (
                "bad length",
                b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nContent-Length: -5\r\n\r\n".to_vec(),
                400,
            ),
            ("oversize interleaved frame", b"$\x00\xff\xff".to_vec(), 413),
        ];
        let many = format!("OPTIONS * RTSP/1.0\r\n{}\r\n", "H: 1\r\n".repeat(9));
        cases.push(("too many headers", many.into_bytes(), 413));

        for (name, bytes, status) in cases {
            let mut client = Client::connect(server.addr).await;
            client.send_raw(&bytes).await;
            let reply = client.read_reply().await;
            assert_eq!(reply.status, status, "{name}");
            assert!(client.is_closed().await, "{name}: connection should close");
        }

        // A request without CSeq gets 400 but the connection stays usable.
        let mut client = Client::connect(server.addr).await;
        client.send_raw(b"OPTIONS * RTSP/1.0\r\n\r\n").await;
        assert_eq!(client.read_reply().await.status, 400);
        let base = server.url("/stream");
        client.login(&base).await;
        assert_eq!(client.request("OPTIONS", &base, &[]).await.status, 200);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn protocol_errors_after_authentication() {
        let server = start(config()).await;
        let base = server.url("/stream");
        let track = server.url("/stream/trackID=0");
        let mut client = Client::connect(server.addr).await;
        client.login(&base).await;

        assert_eq!(
            client
                .request("DESCRIBE", &server.url("/other"), &[])
                .await
                .status,
            404
        );
        assert_eq!(
            client
                .request(
                    "SETUP",
                    &base,
                    &[("Transport", "RTP/AVP/TCP;interleaved=0-1")]
                )
                .await
                .status,
            404
        );
        assert_eq!(client.request("SETUP", &track, &[]).await.status, 400);
        assert_eq!(
            client
                .request("SETUP", &track, &[("Transport", "RTP/AVP;multicast")])
                .await
                .status,
            461
        );
        assert_eq!(client.request("PLAY", &base, &[]).await.status, 454);
        assert_eq!(
            client
                .request("PLAY", &base, &[("Session", "deadbeefdeadbeef")])
                .await
                .status,
            454
        );
        assert_eq!(
            client
                .request("TEARDOWN", &base, &[("Session", "deadbeefdeadbeef")])
                .await
                .status,
            454
        );
        assert_eq!(client.request("PAUSE", &base, &[]).await.status, 501);
        assert_eq!(
            client.request("GET_PARAMETER", &base, &[]).await.status,
            200
        );
        assert_eq!(
            client
                .request("GET_PARAMETER", &base, &[("Session", "nope")])
                .await
                .status,
            454
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn setup_renegotiation_and_state_errors() {
        let server = start(config()).await;
        let base = server.url("/stream");
        let track = server.url("/stream/trackID=0");
        let mut client = Client::connect(server.addr).await;
        client.login(&base).await;

        let reply = client
            .request(
                "SETUP",
                &track,
                &[("Transport", "RTP/AVP/TCP;interleaved=0-1")],
            )
            .await;
        assert_eq!(reply.status, 200);
        let session = reply.session();
        // A second SETUP without the session is a state error; with it, it re-negotiates.
        assert_eq!(
            client
                .request(
                    "SETUP",
                    &track,
                    &[("Transport", "RTP/AVP/TCP;interleaved=2-3")]
                )
                .await
                .status,
            455
        );
        let again = client
            .request(
                "SETUP",
                &track,
                &[
                    ("Transport", "RTP/AVP/TCP;interleaved=2-3"),
                    ("Session", &session),
                ],
            )
            .await;
        assert_eq!(again.status, 200);
        assert_eq!(again.session(), session);
        assert_eq!(
            client
                .request("PLAY", &server.url("/nope"), &[("Session", &session)])
                .await
                .status,
            404
        );
        // GET_PARAMETER with a body asks for parameters we do not have.
        let auth = client.authorization("GET_PARAMETER", &base);
        client
            .send(
                "GET_PARAMETER",
                &base,
                Some(auth),
                &[("Session", &session)],
                "packets_received",
            )
            .await;
        assert_eq!(client.read_reply().await.status, 451);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn session_ids_are_unique_per_session() {
        let server = start(config()).await;
        let base = server.url("/stream");
        let track = server.url("/stream/trackID=0");
        let mut ids = Vec::new();
        for _ in 0..4 {
            let mut client = Client::connect(server.addr).await;
            client.login(&base).await;
            let reply = client
                .request(
                    "SETUP",
                    &track,
                    &[("Transport", "RTP/AVP/TCP;interleaved=0-1")],
                )
                .await;
            ids.push(reply.session());
        }
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 4);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn setup_rejects_wrong_sessions_and_renegotiation_while_playing() {
        let server = start(config()).await;
        let base = server.url("/stream");
        let track = server.url("/stream/trackID=0");
        let transport = ("Transport", "RTP/AVP/TCP;interleaved=0-1");
        let mut client = Client::connect(server.addr).await;
        client.login(&base).await;
        assert_eq!(
            client
                .request("SETUP", &track, &[transport, ("Session", "wrong")])
                .await
                .status,
            454
        );
        let setup = client.request("SETUP", &track, &[transport]).await;
        let session = setup.session();
        assert_eq!(
            client
                .request("SETUP", &track, &[transport, ("Session", "wrong")])
                .await
                .status,
            454
        );
        assert_eq!(
            client
                .request("PLAY", &base, &[("Session", &session)])
                .await
                .status,
            200
        );
        assert_eq!(
            client
                .request("SETUP", &track, &[transport, ("Session", &session)])
                .await
                .status,
            455
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn setup_reports_exhausted_udp_port_range() {
        let server = start(ServerConfig {
            udp_ports: Some(u16::MAX..=u16::MAX),
            ..config()
        })
        .await;
        let base = server.url("/stream");
        let track = server.url("/stream/trackID=0");
        let mut client = Client::connect(server.addr).await;
        client.login(&base).await;
        let reply = client
            .request(
                "SETUP",
                &track,
                &[("Transport", "RTP/AVP;unicast;client_port=5000-5001")],
            )
            .await;
        assert_eq!(reply.status, 500);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn interleaved_client_frames_before_and_after_setup() {
        let server = start(config()).await;
        let base = server.url("/stream");
        let track = server.url("/stream/trackID=0");
        let mut client = Client::connect(server.addr).await;
        client.login(&base).await;
        client.send_raw(b"$\x01\x00\x01x").await;
        assert_eq!(client.request("OPTIONS", &base, &[]).await.status, 200);
        let setup = client
            .request(
                "SETUP",
                &track,
                &[("Transport", "RTP/AVP/TCP;interleaved=0-1")],
            )
            .await;
        assert_eq!(setup.status, 200);
        client.send_raw(b"$\x01\x00\x01x").await;
        assert_eq!(client.request("OPTIONS", &base, &[]).await.status, 200);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn sessions_belong_to_their_connection() {
        let server = start(config()).await;
        let base = server.url("/stream");
        let track = server.url("/stream/trackID=0");
        let mut owner = Client::connect(server.addr).await;
        owner.login(&base).await;
        let session = owner
            .request(
                "SETUP",
                &track,
                &[("Transport", "RTP/AVP/TCP;interleaved=0-1")],
            )
            .await
            .session();
        let mut other = Client::connect(server.addr).await;
        other.login(&base).await;
        assert_eq!(
            other
                .request("PLAY", &base, &[("Session", &session)])
                .await
                .status,
            454
        );
        assert_eq!(
            other
                .request("TEARDOWN", &base, &[("Session", &session)])
                .await
                .status,
            454
        );
        assert_eq!(server.source.starts.load(Ordering::SeqCst), 0);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn session_and_connection_limits_are_enforced() {
        let server = start(ServerConfig {
            max_sessions: 1,
            max_connections: 2,
            ..config()
        })
        .await;
        let base = server.url("/stream");
        let track = server.url("/stream/trackID=0");
        let tcp = [("Transport", "RTP/AVP/TCP;interleaved=0-1")];

        let mut first = Client::connect(server.addr).await;
        first.login(&base).await;
        assert_eq!(first.request("SETUP", &track, &tcp).await.status, 200);

        let mut second = Client::connect(server.addr).await;
        second.login(&base).await;
        assert_eq!(second.request("SETUP", &track, &tcp).await.status, 503);

        // Third connection exceeds max_connections and is closed at accept.
        let mut third = Client::connect(server.addr).await;
        assert!(third.is_closed().await);

        // Releasing the session lets the second client in.
        drop(first);
        let mut retry = None;
        for _ in 0..100 {
            let reply = second.request("SETUP", &track, &tcp).await;
            if reply.status == 200 {
                retry = Some(reply);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(retry.is_some(), "session slot was never released");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn disconnect_without_teardown_cleans_up() {
        let server = start(config()).await;
        let base = server.url("/stream");
        let track = server.url("/stream/trackID=0");
        let mut client = Client::connect(server.addr).await;
        client.login(&base).await;
        let session = client
            .request(
                "SETUP",
                &track,
                &[("Transport", "RTP/AVP/TCP;interleaved=0-1")],
            )
            .await
            .session();
        assert_eq!(
            client
                .request("PLAY", &base, &[("Session", &session)])
                .await
                .status,
            200
        );
        wait_for("camera start", || {
            server.source.starts.load(Ordering::SeqCst) == 1
        })
        .await;
        drop(client);
        wait_for("camera stop", || {
            server.source.stops.load(Ordering::SeqCst) == 1
        })
        .await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn silent_sessions_time_out() {
        let server = start(ServerConfig {
            session_timeout: Duration::from_millis(300),
            ..config()
        })
        .await;
        let base = server.url("/stream");
        let track = server.url("/stream/trackID=0");
        let mut client = Client::connect(server.addr).await;
        client.login(&base).await;
        let session = client
            .request(
                "SETUP",
                &track,
                &[("Transport", "RTP/AVP/TCP;interleaved=0-1")],
            )
            .await
            .session();
        assert_eq!(
            client
                .request("PLAY", &base, &[("Session", &session)])
                .await
                .status,
            200
        );
        wait_for("camera start", || {
            server.source.starts.load(Ordering::SeqCst) == 1
        })
        .await;
        assert!(client.is_closed().await);
        wait_for("camera stop", || {
            server.source.stops.load(Ordering::SeqCst) == 1
        })
        .await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn connections_that_never_authenticate_are_dropped() {
        let server = start(ServerConfig {
            idle_timeout: Duration::from_millis(200),
            ..config()
        })
        .await;
        let mut client = Client::connect(server.addr).await;
        // A slow partial request does not extend the deadline.
        client.send_raw(b"OPTIONS * RTSP/1.0\r\n").await;
        assert!(client.is_closed().await);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn two_clients_each_get_every_packet() {
        let server = start(config()).await;
        let base = server.url("/stream");
        let track = server.url("/stream/trackID=0");
        let mut clients = Vec::new();
        for _ in 0..2 {
            let mut client = Client::connect(server.addr).await;
            client.login(&base).await;
            let session = client
                .request(
                    "SETUP",
                    &track,
                    &[("Transport", "RTP/AVP/TCP;interleaved=0-1")],
                )
                .await
                .session();
            assert_eq!(
                client
                    .request("PLAY", &base, &[("Session", &session)])
                    .await
                    .status,
                200
            );
            clients.push(client);
        }
        assert_eq!(
            server.source.starts.load(Ordering::SeqCst),
            1,
            "one start for both clients"
        );
        for seq in 0..5 {
            server.source.push(seq);
        }
        for client in &mut clients {
            let mut got = 0;
            while got < 5 {
                let (channel, payload) = client.read_frame().await;
                if channel == 0 {
                    assert_eq!(payload, TestSource::packet(got));
                    got += 1;
                }
            }
        }
        drop(clients);
        wait_for("camera stop", || {
            server.source.stops.load(Ordering::SeqCst) == 1
        })
        .await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn bind_reports_track_and_address_errors_and_exposes_fanout() {
        let creds = || Credentials::new(USER, PASS, REALM).unwrap();
        let (invalid, _tx) = ScriptedSource::channel(track(""));
        let invalid = Arc::new(invalid);
        let addr = "127.0.0.1:0".parse().unwrap();
        assert!(
            RtspServer::bind(addr, invalid, creds(), config())
                .await
                .is_err()
        );
        let server = RtspServer::bind(addr, TestSource::new().source, creds(), config())
            .await
            .unwrap();
        assert_eq!(server.fanout().dropped_packets(), 0);
        assert!(
            RtspServer::bind(
                server.local_addr().unwrap(),
                TestSource::new().source,
                creds(),
                config()
            )
            .await
            .is_err()
        );
    }

    #[test]
    fn response_reason_phrases_include_less_common_statuses() {
        use tethercam::rtsp::message::reason_phrase;
        assert_eq!(reason_phrase(405), "Method Not Allowed");
        assert_eq!(reason_phrase(500), "Internal Server Error");
        assert_eq!(reason_phrase(999), "Unknown");
    }

    #[test]
    fn bind_rejects_bad_configuration() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            for config in [
                ServerConfig {
                    mount_path: "stream".into(),
                    ..config()
                },
                ServerConfig {
                    mount_path: "/stream/".into(),
                    ..config()
                },
                ServerConfig {
                    max_sessions: 0,
                    ..config()
                },
                ServerConfig {
                    max_sessions: usize::MAX,
                    ..config()
                },
                ServerConfig {
                    max_connections: usize::MAX,
                    ..config()
                },
                ServerConfig {
                    client_queue: usize::MAX,
                    ..config()
                },
                ServerConfig {
                    client_queue: 0,
                    ..config()
                },
                ServerConfig {
                    idle_timeout: Duration::MAX,
                    ..config()
                },
                ServerConfig {
                    session_timeout: Duration::MAX,
                    ..config()
                },
                ServerConfig {
                    rtcp_interval: Duration::MAX,
                    ..config()
                },
                ServerConfig {
                    write_timeout: Duration::ZERO,
                    ..config()
                },
                ServerConfig {
                    nonce_lifetime: Duration::MAX,
                    ..config()
                },
            ] {
                let result = RtspServer::bind(
                    "127.0.0.1:0".parse().unwrap(),
                    TestSource::new().source,
                    Credentials::new(USER, PASS, REALM).unwrap(),
                    config,
                )
                .await;
                assert!(result.is_err());
            }
        });
    }
}
