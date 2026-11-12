//! Real loopback admission, generic enumeration, cancellation, re-import and cleanup tests.
use super::*;
use crate::test_support::{Sony, Synthetic};

pub(super) async fn imported<M: DeviceModel>(server: &Server<M>) -> TcpStream {
    let mut socket = TcpStream::connect(server.address).await.expect("connect");
    socket
        .write_all(&wire::operation(0x8003, 0))
        .await
        .expect("import");
    let mut bus = [0; 32];
    bus[..3].copy_from_slice(b"1-1");
    socket.write_all(&bus).await.expect("bus");
    let mut response = [0; 320];
    socket.read_exact(&mut response).await.expect("imported");
    assert_eq!(&response[..8], wire::operation(3, 0));
    socket
}

pub(super) fn request(
    endpoint: u32, direction: u32, seq: u32, length: u32, setup: [u8; 8],
) -> Vec<u8> {
    let mut bytes = Vec::new();
    for value in [
        1_u32,
        seq,
        0x0001_0001,
        direction,
        endpoint,
        0,
        length,
        0,
        0,
        0,
    ] {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    bytes.extend_from_slice(&setup);
    bytes
}

#[tokio::test]
async fn independent_non_sony_personality_enumerates_and_echoes_over_real_sockets() {
    let server = Server::start(0, Synthetic::default()).await.expect("start");
    let mut socket = imported(&server).await;
    socket
        .write_all(&request(4, 0, 1, 3, [0; 8]))
        .await
        .expect("submit out");
    socket.write_all(&[7, 8, 9]).await.expect("payload");
    let mut reply = [0; 48];
    socket.read_exact(&mut reply).await.expect("out reply");
    socket
        .write_all(&request(5, 1, 2, 16, [0; 8]))
        .await
        .expect("submit in");
    let mut reply = [0; 51];
    socket.read_exact(&mut reply).await.expect("in reply");
    assert_eq!(&reply[48..], &[7, 8, 9]);
    let state = Arc::clone(&server.state);
    let pending = Arc::clone(&state.lock().await.pending_bulk_in);
    drop(socket);
    timeout(Duration::from_secs(2), async {
        while state.lock().await.imported.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("release import");
    let _again = imported(&server).await;
    server.shutdown().await.expect("shutdown");
    assert!(!state.lock().await.imported.load(Ordering::Acquire));
    assert_eq!(pending.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn negotiation_rejects_invalid_headers_status_bus_and_unknown_operations() {
    let server = Server::start(0, Synthetic::default()).await.expect("start");
    for header in [
        [0; 8].to_vec(),
        wire::operation(0x8005, 1),
        wire::operation(0x8006, 0),
    ] {
        let mut socket = TcpStream::connect(server.address).await.expect("connect");
        socket.write_all(&header).await.expect("bad header");
        assert!(socket.read_u8().await.is_err());
    }
    for index in [0, 5] {
        let mut socket = TcpStream::connect(server.address).await.expect("connect");
        socket
            .write_all(&wire::operation(0x8003, 0))
            .await
            .expect("header");
        let mut bus = [0; 32];
        bus[..3].copy_from_slice(b"1-1");
        bus[index] = 255;
        socket.write_all(&bus).await.expect("bad bus");
        let mut response = [0; 8];
        socket.read_exact(&mut response).await.expect("rejection");
        assert_eq!(response.to_vec(), wire::operation(3, 1));
    }
    server.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn duplicate_pending_sequence_fails_and_class_reset_flushes_only_bulk() {
    let server = Server::start(0, Sony::default()).await.expect("start");
    let mut socket = imported(&server).await;
    for seq in [1, 1] {
        socket
            .write_all(&request(1, 1, seq, 16, [0; 8]))
            .await
            .expect("submit");
    }
    assert!(socket.read_u8().await.is_err());
    assert!(!server.state.lock().await.imported.load(Ordering::Acquire));
    let mut socket = imported(&server).await;
    socket
        .write_all(&request(1, 1, 3, 16, [0; 8]))
        .await
        .expect("pending bulk");
    socket
        .write_all(&request(3, 1, 4, 16, [0; 8]))
        .await
        .expect("pending interrupt");
    socket
        .write_all(&request(0, 0, 5, 0, [0x21, 0x66, 0, 0, 0, 0, 0, 0]))
        .await
        .expect("class reset");
    let mut response = [0; 96];
    socket
        .read_exact(&mut response)
        .await
        .expect("reset and cancelled bulk");
    assert_eq!(wire::word(&response, 4).expect("sequence"), 5);
    assert_eq!(wire::word(&response, 52).expect("cancelled sequence"), 3);
    server.state.lock().await.disconnect = true;
    server.changed.notify_one();
    assert!(socket.read_u8().await.is_err());
    server.shutdown().await.expect("shutdown");
}

#[tokio::test(start_paused = true)]
async fn bounded_slow_peer_and_writer_deadlines_are_real() {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("listener");
    let mut socket = TcpStream::connect(listener.local_addr().expect("address"))
        .await
        .expect("connect");
    let (peer, _) = listener.accept().await.expect("accept");
    assert!(read::<8>(&mut socket).await.is_err());
    assert!(read::<32>(&mut socket).await.is_err());
    drop(peer);
    assert!(read::<32>(&mut socket).await.is_err());
}

thread_local! { static FAIL_WRITE_AT: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) }; }

fn injecting_write<'a>(writer: &'a mut Writer, bytes: &'a [u8]) -> WriteFuture<'a> {
    let fail = FAIL_WRITE_AT.with(|count| {
        let remaining = count.get();
        count.set(remaining.saturating_sub(1));
        remaining == 1
    });
    if fail {
        Box::pin(std::future::ready(Err(io::Error::other("injected write"))))
    } else {
        Box::pin(write(writer, bytes))
    }
}

#[tokio::test]
async fn management_replies_propagate_real_write_errors_and_release_import_leases() {
    for mode in 0..4 {
        FAIL_WRITE_AT.with(|count| count.set(1));
        let ops = SocketOps {
            write: injecting_write,
            ..SocketOps::default()
        };
        let server = Server::start_with(0, Sony::default(), ops)
            .await
            .expect("start");
        if mode == 2 {
            server
                .state
                .lock()
                .await
                .imported
                .store(true, Ordering::Release);
        }
        let mut socket = TcpStream::connect(server.address).await.expect("connect");
        socket
            .write_all(&wire::operation(if mode == 0 { 0x8005 } else { 0x8003 }, 0))
            .await
            .expect("operation");
        if mode != 0 {
            let mut bus = [0; 32];
            bus[..3].copy_from_slice(if mode == 1 { b"bad" } else { b"1-1" });
            socket.write_all(&bus).await.expect("bus");
        }
        assert!(
            timeout(Duration::from_secs(2), socket.read_u8())
                .await
                .expect("close deadline")
                .is_err()
        );
        if mode != 2 {
            assert!(!server.state.lock().await.imported.load(Ordering::Acquire));
        }
        server.shutdown().await.expect("stop");
    }
}

#[tokio::test]
async fn submit_unlink_and_reset_flush_write_failures_cancel_all_owned_work() {
    for mode in 0..3 {
        FAIL_WRITE_AT.with(|count| count.set(if mode == 2 { 3 } else { 2 }));
        let ops = SocketOps {
            write: injecting_write,
            ..SocketOps::default()
        };
        let server = Server::start_with(0, Sony::default(), ops)
            .await
            .expect("start");
        let mut socket = imported(&server).await;
        if mode == 0 {
            socket
                .write_all(&request(2, 0, 1, 0, [0; 8]))
                .await
                .expect("out");
        } else {
            socket
                .write_all(&request(1, 1, 1, 16, [0; 8]))
                .await
                .expect("pending bulk");
            if mode == 1 {
                let mut unlink = request(0, 0, 2, 0, [0; 8]);
                unlink[..4].copy_from_slice(&2_u32.to_be_bytes());
                unlink[20..24].copy_from_slice(&1_u32.to_be_bytes());
                socket.write_all(&unlink).await.expect("unlink");
            } else {
                socket
                    .write_all(&request(0, 0, 2, 0, [0x21, 0x66, 0, 0, 0, 0, 0, 0]))
                    .await
                    .expect("reset");
                let mut response = [0; 48];
                socket.read_exact(&mut response).await.expect("reset reply");
            }
        }
        assert!(
            timeout(Duration::from_secs(2), socket.read_u8())
                .await
                .expect("close deadline")
                .is_err()
        );
        server.shutdown().await.expect("stop");
    }
}

#[tokio::test]
async fn pending_completion_write_failure_is_fatal_and_admission_is_bounded() {
    FAIL_WRITE_AT.with(|count| count.set(3));
    let server = Server::start_with(
        0,
        Synthetic::default(),
        SocketOps {
            write: injecting_write,
            ..SocketOps::default()
        },
    )
    .await
    .expect("start");
    let mut socket = imported(&server).await;
    socket
        .write_all(&request(5, 1, 1, 16, [0; 8]))
        .await
        .expect("pending");
    socket
        .write_all(&request(4, 0, 2, 1, [0; 8]))
        .await
        .expect("out");
    socket.write_all(&[9]).await.expect("payload");
    let mut response = [0; 48];
    socket.read_exact(&mut response).await.expect("out reply");
    assert!(
        timeout(Duration::from_secs(2), socket.read_u8())
            .await
            .expect("close deadline")
            .is_err()
    );
    server.shutdown().await.expect("stop");
    let server = Server::start(0, Synthetic::default()).await.expect("start");
    let mut peers = Vec::new();
    for _ in 0..4 {
        peers.push(TcpStream::connect(server.address).await.expect("slow peer"));
    }
    let mut rejected = TcpStream::connect(server.address)
        .await
        .expect("excess peer");
    assert!(
        timeout(Duration::from_secs(2), rejected.read_u8())
            .await
            .expect("admission deadline")
            .is_err()
    );
    drop(peers);
    server.shutdown().await.expect("stop");
}

#[tokio::test(start_paused = true)]
async fn held_out_work_expires_and_deadline_write_failure_is_fatal() {
    FAIL_WRITE_AT.with(|count| count.set(2));
    let server = Server::start_with(
        0,
        Synthetic::default(),
        SocketOps {
            write: injecting_write,
            ..SocketOps::default()
        },
    )
    .await
    .expect("start");
    server.state.lock().await.hold_out = true;
    let mut socket = imported(&server).await;
    socket
        .write_all(&request(4, 0, 1, 0, [0; 8]))
        .await
        .expect("held out");
    assert!(socket.read_u8().await.is_err());
    server.shutdown().await.expect("stop");
}

#[tokio::test]
async fn generic_devlist_and_duplicate_import_replies_preserve_the_original_lease() {
    let server = Server::start(0, Synthetic::default())
        .await
        .expect("server");
    let mut socket = TcpStream::connect(server.address).await.expect("list");
    socket
        .write_all(&wire::operation(0x8005, 0))
        .await
        .expect("devlist");
    let mut reply = [0; 328];
    socket.read_exact(&mut reply).await.expect("list reply");
    assert_eq!(reply.get(312..314), Some(&[255, 255][..]));
    let _original = imported(&server).await;
    let mut duplicate = TcpStream::connect(server.address).await.expect("duplicate");
    duplicate
        .write_all(&wire::operation(0x8003, 0))
        .await
        .expect("import");
    let mut bus = [0; 32];
    bus[..3].copy_from_slice(b"1-1");
    duplicate.write_all(&bus).await.expect("bus");
    let mut rejected = [0; 8];
    duplicate.read_exact(&mut rejected).await.expect("reject");
    assert_eq!(rejected.to_vec(), wire::operation(3, 1));
    assert!(server.state.lock().await.imported.load(Ordering::Acquire));
    server.shutdown().await.expect("stop");
}

#[tokio::test]
async fn unknown_unlink_is_harmless_and_exhausted_exports_reject_before_model_execution() {
    let server = Server::start(0, Synthetic::default())
        .await
        .expect("server");
    let mut socket = imported(&server).await;
    let mut unlink = request(0, 0, 1, 0, [0; 8]);
    unlink[..4].copy_from_slice(&2_u32.to_be_bytes());
    unlink[20..24].copy_from_slice(&999_u32.to_be_bytes());
    socket.write_all(&unlink).await.expect("unknown unlink");
    let mut reply = [0; 48];
    socket.read_exact(&mut reply).await.expect("unlink reply");
    assert_eq!(wire::word(&reply, 20).expect("status"), 0);
    assert_eq!(server.state.lock().await.unlinked, 0);
    server.state.lock().await.retained_work = MAX_RETAINED_WORK;
    socket
        .write_all(&request(4, 0, 2, 0, [0; 8]))
        .await
        .expect("exhausted submit");
    assert!(
        timeout(Duration::from_secs(2), socket.read_u8())
            .await
            .expect("rejection deadline")
            .is_err()
    );
    server.shutdown().await.expect("stop");
}
