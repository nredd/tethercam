//! Deterministic connection failure tests over real sockets and narrowly injected OS failures.
//!
//! References:
//! - [RFC 2326, RTSP 1.0](https://www.rfc-editor.org/rfc/rfc2326)
//! - [RFC 3550 section 6.4.1 (sender reports)](https://www.rfc-editor.org/rfc/rfc3550#section-6.4.1)
//! - `docs/architecture.md`, "RTSP server contract"

use std::collections::VecDeque;
use std::future::poll_fn;
use std::sync::atomic::AtomicU64;
use std::task::Poll;

use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time::timeout;

use super::*;
use crate::rtsp::digest::{Algorithm, Authenticator, ClientDigest, Credentials};
use crate::rtsp::server::ServerConfig;
use crate::rtsp::source::testing::{TestSource, close_queue};
use crate::rtsp::source::{Fanout, ScriptedSource};

const WAIT: Duration = Duration::from_secs(5);

async fn connection(
    source: Arc<ScriptedSource>,
) -> (Connection<ScriptedSource>, TcpStream, watch::Sender<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (stream, addr) = listener.accept().await.unwrap();
    let local = stream.local_addr().unwrap();
    let config = ServerConfig::default();
    let shared = Arc::new(Shared {
        track: source.track(),
        fanout: Fanout::new(source, config.client_queue),
        auth: Authenticator::new(
            Credentials::new("camera", "synthetic-password", "test").unwrap(),
            config.nonce_lifetime,
        ),
        sessions: Arc::new(Semaphore::new(config.max_sessions)),
        next_connection: AtomicU64::new(1),
        config,
    });
    let (stop, rx) = watch::channel(false);
    (Connection::new(stream, addr, local, shared, rx), peer, stop)
}

fn request(bytes: &[u8]) -> Request {
    let Some((Incoming::Request(request), _)) =
        parse_incoming(bytes, &crate::rtsp::message::Limits::default()).unwrap()
    else {
        panic!("expected request");
    };
    request
}

#[test]
#[should_panic(expected = "expected request")]
fn request_fixture_rejects_interleaved_messages() {
    let _ = request(b"$\0\0\0");
}

fn authorized(connection: &Connection<ScriptedSource>) -> Request {
    let now = Instant::now();
    connection
        .shared
        .auth
        .challenge_with(connection.id, false, now, &mut |bytes| {
            bytes.fill(0);
            Ok(())
        })
        .unwrap();
    let auth = ClientDigest {
        algorithm: Algorithm::Sha256,
        username: "camera",
        password: "synthetic-password",
        realm: "test",
        nonce: "00000000000000000000000000000000",
        method: "OPTIONS",
        uri: "/stream",
        nc: 1,
        cnonce: "synthetic-client",
    }
    .header_value();
    request(
        format!("OPTIONS /stream RTSP/1.0\r\nCSeq: 1\r\nAuthorization: {auth}\r\n\r\n").as_bytes(),
    )
}

async fn setup(connection: &mut Connection<ScriptedSource>) {
    let reply = connection.setup(&request(b"SETUP /stream/trackID=0 RTSP/1.0\r\nCSeq: 1\r\nTransport: RTP/AVP/TCP;interleaved=0-1\r\n\r\n")).await;
    assert_eq!(reply.status, 200);
}

fn play(connection: &mut Connection<ScriptedSource>) {
    let id = &connection.session.as_ref().unwrap().id;
    let request =
        request(format!("PLAY /stream RTSP/1.0\r\nCSeq: 2\r\nSession: {id}\r\n\r\n").as_bytes());
    assert_eq!(connection.play(&request).status, 200);
    // Repeated PLAY is idempotent and must retain the existing subscription.
    assert_eq!(connection.play(&request).status, 200);
    assert_eq!(connection.shared.fanout.subscribers(), 1);
}

fn packet(size: usize) -> Packet {
    let mut bytes = vec![0x80, 96, 0, 1, 0, 0, 0, 7, 0xca, 0xfe, 0xba, 0xbe];
    bytes.resize(size, 0xaa);
    Packet::new(bytes)
}

fn entropy_failure(_: &mut [u8]) -> Result<(), getrandom::Error> {
    Err(getrandom::Error::UNSUPPORTED)
}

async fn reset(peer: TcpStream, connection: &Connection<ScriptedSource>) {
    socket2::SockRef::from(&peer)
        .set_linger(Some(Duration::ZERO))
        .unwrap();
    drop(peer);
    timeout(WAIT, connection.stream.readable())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn entropy_failure_answers_500_without_creating_session() {
    let source = TestSource::new();
    let (mut conn, mut peer, _stop) = connection(Arc::clone(&source)).await;
    conn.fill_random = entropy_failure;
    assert!(
        conn.handle_request(&request(b"OPTIONS /stream RTSP/1.0\r\nCSeq: 1\r\n\r\n"))
            .await
            .unwrap()
    );
    let mut reply = [0; 12];
    timeout(WAIT, peer.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&reply, b"RTSP/1.0 500");
    let reply = conn.setup(&request(b"SETUP /stream/trackID=0 RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP/TCP;interleaved=0-1\r\n\r\n")).await;
    assert_eq!(reply.status, 500);
    assert!(conn.session.is_none());
    assert_eq!(
        conn.shared.sessions.available_permits(),
        conn.shared.config.max_sessions
    );
}

#[tokio::test]
async fn describe_revalidates_track_from_source() {
    let source = TestSource::new();
    let mut track = source.track();
    track.clock_rate = 0;
    let invalid = Arc::new(ScriptedSource::new(track, vec![], false, None));
    let (conn, _peer, _stop) = connection(invalid).await;
    assert_eq!(
        conn.describe(&request(b"DESCRIBE /stream RTSP/1.0\r\nCSeq: 1\r\n\r\n"))
            .status,
        500
    );
}

#[tokio::test]
async fn request_writes_propagate_peer_errors() {
    let source = TestSource::new();
    let (mut conn, peer, _stop) = connection(Arc::clone(&source)).await;
    let authenticated = authorized(&conn);
    reset(peer, &conn).await;
    // Some platforms already consider a reset socket disconnected at shutdown.
    let _ = conn.stream.shutdown().await;
    for request in [
        request(b"OPTIONS /stream RTSP/1.0\r\n\r\n"),
        request(b"OPTIONS /stream RTSP/1.0\r\nCSeq: 1\r\n\r\n"),
        authenticated,
    ] {
        assert!(conn.handle_request(&request).await.is_err());
    }
    conn.buf = b"OPTIONS /stream RTSP/1.0\r\n\r\n".to_vec();
    assert!(conn.drive().await.is_err());
}

#[tokio::test]
async fn reset_is_a_typed_read_failure_including_drive() {
    let source = TestSource::new();
    let (mut conn, peer, _stop) = connection(Arc::clone(&source)).await;
    reset(peer, &conn).await;
    let error = timeout(WAIT, read_more(&mut conn.stream, &mut conn.buf))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.0.kind(), io::ErrorKind::ConnectionReset);
    let (mut conn, peer, _stop) = connection(Arc::clone(&source)).await;
    reset(peer, &conn).await;
    let error = timeout(WAIT, conn.drive()).await.unwrap().unwrap_err();
    assert_eq!(error.0.kind(), io::ErrorKind::ConnectionReset);
}

#[tokio::test]
async fn stalled_tcp_write_has_bounded_typed_deadline() {
    let source = TestSource::new();
    let (mut conn, peer, _stop) = connection(Arc::clone(&source)).await;
    socket2::SockRef::from(&conn.stream)
        .set_send_buffer_size(4096)
        .unwrap();
    socket2::SockRef::from(&peer)
        .set_recv_buffer_size(4096)
        .unwrap();
    // The peer never reads, and the write exceeds both finite socket buffers.
    let data = vec![0; 1024 * 1024];
    let error = timeout(
        WAIT,
        write_all(&mut conn.stream, &data, Duration::from_millis(1)),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.0.kind(), io::ErrorKind::TimedOut);
}

#[tokio::test]
async fn rtp_ignores_invalid_or_undeliverable_packets_without_stats() {
    let source = TestSource::new();
    let (mut conn, _peer, _stop) = connection(Arc::clone(&source)).await;
    let valid = packet(13);
    conn.send_rtp(&valid).await.unwrap();
    conn.send_report().await.unwrap();
    setup(&mut conn).await;
    conn.send_rtp(&valid).await.unwrap();
    conn.send_report().await.unwrap();
    play(&mut conn);
    conn.send_report().await.unwrap();
    conn.send_rtp(&Packet::new(vec![0])).await.unwrap();
    conn.send_rtp(&packet(65_536)).await.unwrap();
    let session = conn.session.as_mut().unwrap();
    session.spec = TransportSpec::Udp {
        rtp_port: 0,
        rtcp_port: 0,
    };
    // Defensive missing-transport state must not claim delivery or emit reports.
    conn.send_rtp(&valid).await.unwrap();
    conn.session
        .as_mut()
        .unwrap()
        .playing
        .as_mut()
        .unwrap()
        .stats = Some(SendStats {
        ssrc: 1,
        last_timestamp: 7,
        last_at: Instant::now(),
        packets: 1,
        octets: 1,
    });
    conn.send_report().await.unwrap();
    conn.session
        .as_mut()
        .unwrap()
        .playing
        .as_mut()
        .unwrap()
        .stats = None;
    let ip = conn.local.ip();
    // Sending to an IPv6 destination through an IPv4 socket fails on every platform.
    let dest = "[::1]:9".parse().unwrap();
    conn.session.as_mut().unwrap().udp = Some(bind_udp_pair(ip, None, dest, dest).await.unwrap());
    conn.send_rtp(&valid).await.unwrap();
    assert!(
        conn.session
            .as_ref()
            .unwrap()
            .playing
            .as_ref()
            .unwrap()
            .stats
            .is_none()
    );
    conn.session
        .as_mut()
        .unwrap()
        .playing
        .as_mut()
        .unwrap()
        .stats = Some(SendStats {
        ssrc: 1,
        last_timestamp: 7,
        last_at: Instant::now(),
        packets: 1,
        octets: 1,
    });
    conn.send_report().await.unwrap();
}

#[tokio::test]
async fn media_writes_and_drive_events_propagate_write_failures() {
    let source = TestSource::new();
    let (mut conn, _peer, _stop) = connection(Arc::clone(&source)).await;
    setup(&mut conn).await;
    play(&mut conn);
    conn.stream.shutdown().await.unwrap();
    assert!(conn.send_rtp(&packet(13)).await.is_err());
    source.push(1);
    let error = timeout(WAIT, conn.drive()).await.unwrap().unwrap_err();
    assert_eq!(error.0.kind(), io::ErrorKind::BrokenPipe);
    conn.session
        .as_mut()
        .unwrap()
        .playing
        .as_mut()
        .unwrap()
        .stats = Some(SendStats {
        ssrc: 1,
        last_timestamp: 7,
        last_at: Instant::now(),
        packets: 1,
        octets: 1,
    });
    assert!(conn.send_report().await.is_err());
    conn.session
        .as_mut()
        .unwrap()
        .playing
        .as_mut()
        .unwrap()
        .next_report = tokio::time::Instant::now();
    let error = timeout(WAIT, conn.drive()).await.unwrap().unwrap_err();
    assert_eq!(error.0.kind(), io::ErrorKind::BrokenPipe);
}

#[tokio::test]
async fn report_deadline_runs_without_new_packets_and_closed_queue_is_pending() {
    let source = TestSource::new();
    let (mut conn, mut peer, stop) = connection(Arc::clone(&source)).await;
    setup(&mut conn).await;
    play(&mut conn);
    conn.send_rtp(&packet(13)).await.unwrap();
    conn.send_rtp(&packet(13)).await.unwrap();
    conn.session
        .as_mut()
        .unwrap()
        .playing
        .as_mut()
        .unwrap()
        .next_report = tokio::time::Instant::now();
    // Force a report event through the real drive loop, then stop it once bytes arrive.
    let mut bytes = vec![0; 52 + 2 * 17];
    timeout(WAIT, peer.read_exact(&mut bytes))
        .await
        .unwrap()
        .unwrap(); // initial RTP and RTCP
    let read = async {
        let mut header = [0; 4];
        timeout(WAIT, peer.read_exact(&mut header))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(header, [b'$', 1, 0, 48]);
        stop.send(true).unwrap();
    };
    let (result, ()) = tokio::join!(conn.drive(), read);
    result.unwrap();
    close_queue(
        &mut conn
            .session
            .as_mut()
            .unwrap()
            .playing
            .as_mut()
            .unwrap()
            .subscription,
    );
    conn.session
        .as_mut()
        .unwrap()
        .playing
        .as_mut()
        .unwrap()
        .stats = None;
    let event = next_event(&mut conn.session);
    tokio::pin!(event);
    poll_fn(|cx| {
        assert!(event.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn udp_allocation_retries_and_preserves_each_os_error() {
    let ip: IpAddr = "127.0.0.1".parse().unwrap();
    let dest = SocketAddr::new(ip, 9);
    // Every scripted successful bind is a real port-0 socket. Port observations are
    // controlled independently, so no ephemeral-port parity or availability is assumed.
    let mut sockets = VecDeque::new();
    for _ in 0..5 {
        sockets.push_back(Ok(UdpSocket::bind((ip, 0)).await.unwrap()));
    }
    sockets.insert(
        2,
        Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "synthetic collision",
        )),
    );
    let mut requested = Vec::new();
    let mut bind = |addr| -> UdpBind {
        requested.push(addr);
        let result = sockets.pop_front().unwrap();
        Box::pin(std::future::ready(result))
    };
    let mut ports = VecDeque::from([Ok(50001), Ok(50002), Ok(50004), Ok(50004), Ok(50005)]);
    let mut port = |_: &UdpSocket| ports.pop_front().unwrap();
    let pair = bind_udp_pair_with(ip, None, dest, dest, &mut bind, &mut port)
        .await
        .unwrap();
    assert_eq!(pair.server_ports, (50004, 50005));
    assert_eq!(
        requested.iter().map(SocketAddr::port).collect::<Vec<_>>(),
        [0, 0, 50003, 0, 50005]
    );

    let mut fail_bind = |_: SocketAddr| -> UdpBind {
        Box::pin(std::future::ready(Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "synthetic bind failure",
        ))))
    };
    let error = bind_udp_pair_with(ip, None, dest, dest, &mut fail_bind, &mut udp_port)
        .await
        .err()
        .unwrap();
    assert_eq!(error.0.kind(), io::ErrorKind::PermissionDenied);
    for observations in [
        vec![Err(io::Error::other("ephemeral address"))],
        vec![Ok(50000), Err(io::Error::other("RTP address"))],
        vec![Ok(50000), Ok(50000), Err(io::Error::other("RTCP address"))],
    ] {
        let mut observations = VecDeque::from(observations);
        let mut port = |_: &UdpSocket| observations.pop_front().unwrap();
        let error = bind_udp_pair_with(
            ip,
            None,
            dest,
            dest,
            &mut |_| bind_udp_socket(SocketAddr::new(ip, 0)),
            &mut port,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.0.kind(), io::ErrorKind::Other);
    }
    let mut odd = |_: &UdpSocket| Ok(50001);
    let error = bind_udp_pair_with(ip, None, dest, dest, &mut bind_udp_socket, &mut odd)
        .await
        .err()
        .unwrap();
    assert_eq!(error.0.kind(), io::ErrorKind::AddrNotAvailable);
    for range in [50001..=50001, 65535..=65535] {
        assert!(
            bind_udp_pair_with(ip, Some(&range), dest, dest, &mut fail_bind, &mut udp_port)
                .await
                .is_err()
        );
    }
    let mut sockets = VecDeque::new();
    for _ in 0..2 {
        sockets.push_back(Ok(UdpSocket::bind((ip, 0)).await.unwrap()));
    }
    let mut bind =
        |_: SocketAddr| -> UdpBind { Box::pin(std::future::ready(sockets.pop_front().unwrap())) };
    let pair = bind_udp_pair_with(
        ip,
        Some(&(50000..=50001)),
        dest,
        dest,
        &mut bind,
        &mut udp_port,
    )
    .await
    .unwrap();
    assert_eq!(pair.media_dest, dest);
    assert_eq!(pair.control_dest, dest);
}

#[tokio::test]
async fn request_success_and_connection_exit_paths() {
    let source = TestSource::new();
    let (mut conn, mut peer, _stop) = connection(Arc::clone(&source)).await;
    assert!(
        conn.handle_request(&request(b"OPTIONS /stream RTSP/1.0\r\n\r\n"))
            .await
            .unwrap()
    );
    assert!(String::from_utf8_lossy(&conn.out).contains("400 Bad Request"));
    let authenticated = authorized(&conn);
    assert!(conn.handle_request(&authenticated).await.unwrap());
    assert!(String::from_utf8_lossy(&conn.out).contains("200 OK"));
    let mut no_session: Option<Session<ScriptedSource>> = None;
    let event = next_event(&mut no_session);
    tokio::pin!(event);
    poll_fn(|cx| {
        assert!(event.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    // A half-closed peer is EOF, not an error.
    peer.shutdown().await.unwrap();
    timeout(WAIT, conn.drive()).await.unwrap().unwrap();
    let (mut conn, _peer, _stop) = connection(Arc::clone(&source)).await;
    Arc::get_mut(&mut conn.shared)
        .unwrap()
        .config
        .max_auth_failures = 0;
    conn.buf = b"OPTIONS /stream RTSP/1.0\r\nCSeq: 1\r\n\r\n".to_vec();
    conn.drive().await.unwrap();
    let (mut conn, _peer, _stop) = connection(Arc::clone(&source)).await;
    conn.buf = b"OPTIONS * RTSP/2.0\r\n\r\n".to_vec();
    conn.drive().await.unwrap();
    assert!(String::from_utf8_lossy(&conn.out).contains("505"));
    let (mut conn, mut peer, _stop) = connection(Arc::clone(&source)).await;
    Arc::get_mut(&mut conn.shared).unwrap().config.idle_timeout = Duration::ZERO;
    conn.drive().await.unwrap();
    // Read bytes in drive, consume an interleaved frame, then hit EOF.
    Arc::get_mut(&mut conn.shared).unwrap().config.idle_timeout = WAIT;
    peer.write_all(b"$\0\0\0").await.unwrap();
    peer.shutdown().await.unwrap();
    conn.drive().await.unwrap();
}

#[tokio::test]
async fn play_and_describe_validate_paths_and_session() {
    let source = TestSource::new();
    let (mut conn, _peer, _stop) = connection(Arc::clone(&source)).await;
    for (path, status) in [("/stream", 200), ("/other", 404)] {
        let req = request(format!("DESCRIBE {path} RTSP/1.0\r\nCSeq: 1\r\n\r\n").as_bytes());
        assert_eq!(conn.describe(&req).status, status);
    }
    let req = request(b"PLAY /stream RTSP/1.0\r\nCSeq: 1\r\nSession: wrong\r\n\r\n");
    assert_eq!(conn.play(&req).status, 454);
    setup(&mut conn).await;
    assert_eq!(conn.play(&req).status, 454);
    for (path, status) in [
        ("/other", 404),
        ("/stream/trackID=0", 200),
        ("/stream", 200),
    ] {
        let id = &conn.session.as_ref().unwrap().id;
        let req =
            request(format!("PLAY {path} RTSP/1.0\r\nCSeq: 1\r\nSession: {id}\r\n\r\n").as_bytes());
        assert_eq!(conn.play(&req).status, status);
    }
}

#[tokio::test]
async fn setup_validates_and_replaces_transport_without_leaking_permits() {
    let source = TestSource::new();
    let (mut conn, _peer, _stop) = connection(Arc::clone(&source)).await;
    for (path, headers, status) in [
        ("/other", "", 404),
        ("/stream/trackID=0", "", 400),
        ("/stream/trackID=0", "Transport: invalid\r\n", 461),
        (
            "/stream/trackID=0",
            "Transport: RTP/AVP/TCP;interleaved=0-1\r\nSession: wrong\r\n",
            454,
        ),
    ] {
        let req = request(format!("SETUP {path} RTSP/1.0\r\nCSeq: 1\r\n{headers}\r\n").as_bytes());
        assert_eq!(conn.setup(&req).await.status, status);
    }
    let udp = request(b"SETUP /stream/trackID=0 RTSP/1.0\r\nCSeq: 1\r\nTransport: RTP/AVP;client_port=50000-50001\r\n\r\n");
    assert_eq!(conn.setup(&udp).await.status, 200);
    assert_eq!(
        conn.session
            .as_ref()
            .unwrap()
            .udp
            .as_ref()
            .unwrap()
            .media_dest
            .ip(),
        conn.peer.ip()
    );
    assert_eq!(conn.setup(&udp).await.status, 455);
    let id = conn.session.as_ref().unwrap().id.clone();
    let replace = request(format!("SETUP /stream/trackID=0 RTSP/1.0\r\nCSeq: 1\r\nTransport: RTP/AVP/TCP;interleaved=0-1\r\nSession: {id}\r\n\r\n").as_bytes());
    assert_eq!(conn.setup(&replace).await.status, 200);
    assert!(conn.session.as_ref().unwrap().udp.is_none());
    play(&mut conn);
    assert_eq!(conn.setup(&replace).await.status, 455);
    conn.session = None;
    let permits = Arc::clone(&conn.shared.sessions)
        .acquire_many_owned(u32::try_from(conn.shared.config.max_sessions).unwrap())
        .await
        .unwrap();
    assert_eq!(conn.setup(&udp).await.status, 503);
    drop(permits);
    // A held pair makes the configured range deterministically unavailable.
    let ip = conn.local.ip();
    let dest = conn.peer;
    let pair = bind_udp_pair(ip, None, dest, dest).await.unwrap();
    Arc::get_mut(&mut conn.shared).unwrap().config.udp_ports =
        Some(pair.server_ports.0..=pair.server_ports.1);
    assert_eq!(conn.setup(&udp).await.status, 500);
}
