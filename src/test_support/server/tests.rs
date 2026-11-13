//! Generic engine fault/deadline/resource and cleanup contracts, without a kernel adapter.
//!
//! References:
//! - <https://docs.kernel.org/usb/usbip_protocol.html>
use super::*;
use crate::test_support::{Control, Identity, Sony, Synthetic};
use std::future::{Future, ready};

struct FaultModel {
    error: TransportError,
    invalid: bool,
}
impl DeviceModel for FaultModel {
    fn identity(&self) -> Identity {
        let mut identity = Synthetic::default().identity();
        if self.invalid {
            identity.device.clear();
        }
        identity
    }
    fn transfer(
        &mut self, _endpoint: u32, _direction: u32, _payload: &[u8], _bytes: &mut [u8],
    ) -> impl Future<Output = Result<usize, TransportError>> + Send {
        ready(Err(self.error.clone()))
    }
    fn control(
        &mut self, _setup: [u8; 8], _payload: &[u8],
    ) -> impl Future<Output = Result<Control, Error>> + Send {
        ready(Err(io::Error::other("injected control").into()))
    }
}

fn urb(endpoint: u32, direction: u32, length: usize) -> Urb {
    Urb {
        command: 1,
        seq: 1,
        direction,
        endpoint,
        length,
        unlink: 0,
        setup: [0; 8],
    }
}

#[tokio::test]
async fn engine_maps_transport_faults_and_keeps_permanent_budgets() {
    for error in [
        TransportError::Timeout,
        TransportError::Stall,
        TransportError::Disconnected,
        TransportError::Other("injected".into()),
    ] {
        let raw = Arc::new(Mutex::new(FaultModel {
            error: error.clone(),
            invalid: false,
        }));
        let model = model_io::shared(&raw);
        let mut state = State::new();
        model.identity().await.validate().expect("valid model");
        let response = execute(&urb(4, 1, 8), &[], &mut state, model.as_ref())
            .await
            .expect("execute");
        assert_eq!(
            response.is_none(),
            matches!(
                error,
                TransportError::Timeout | TransportError::Disconnected
            )
        );
        assert_eq!(state.disconnect, error == TransportError::Disconnected);
        if let Some(response) = response {
            assert_ne!(wire::word(&response, 20).expect("status"), 0);
        }
        state.hold_out = true;
        assert!(
            execute(&urb(4, 0, 0), &[], &mut state, model.as_ref())
                .await
                .expect("hold")
                .is_none()
        );
        state.retained_work = MAX_RETAINED_WORK;
        assert!(
            execute(&urb(4, 0, 0), &[], &mut state, model.as_ref())
                .await
                .is_err()
        );
        assert!(
            control(&urb(0, 0, 0), &[], &mut state, model.as_ref())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn generic_control_validation_and_synthetic_roundtrip() {
    let server = Server::start(0, Synthetic::default()).await.expect("start");
    let model = model_io::shared(&server.model);
    let mut state = server.state.lock().await;
    execute(&urb(4, 0, 3), &[9, 8, 7], &mut state, model.as_ref())
        .await
        .expect("out");
    let response = execute(&urb(5, 1, 8), &[], &mut state, model.as_ref())
        .await
        .expect("in")
        .expect("reply");
    assert_eq!(response.get(48..), Some(&[9, 8, 7][..]));
    assert_eq!(wire::word(&response, 24).expect("length"), 3);
    for (setup, direction, length) in [
        ([0x80, 6, 0, 1, 0, 0, 18, 0], 1, 18),
        ([0x80, 6, 0, 1, 0, 0, 18, 0], 0, 18),
        ([0x80, 6, 0, 1, 0, 0, 18, 0], 1, 17),
        ([0, 9, 1, 0, 0, 0, 0, 0], 0, 0),
        ([0, 9, 2, 0, 0, 0, 0, 0], 0, 0),
        ([0, 9, 1, 1, 0, 0, 0, 0], 0, 0),
        ([0, 9, 1, 0, 1, 0, 0, 0], 0, 0),
        ([0, 9, 1, 0, 0, 0, 1, 0], 0, 1),
        ([1, 11, 1, 0, 0, 0, 0, 0], 0, 0),
        ([1, 11, 1, 1, 0, 0, 0, 0], 0, 0),
        ([1, 11, 1, 0, 1, 0, 0, 0], 0, 0),
        ([1, 11, 1, 0, 0, 0, 1, 0], 0, 1),
        ([0x81, 10, 0, 0, 1, 0, 1, 0], 1, 1),
        ([0x80, 0, 0, 0, 0, 0, 2, 0], 1, 2),
        ([0xff, 99, 0, 0, 0, 0, 0, 0], 1, 0),
    ] {
        let mut request = urb(0, direction, length);
        request.setup = setup;
        assert!(
            execute(&request, &vec![0; length], &mut state, model.as_ref())
                .await
                .expect("control")
                .is_some()
        );
    }
    drop(state);
    let address = server.address;
    assert!(
        Server::start(address.port(), Synthetic::default())
            .await
            .is_err()
    );
    server.shutdown().await.expect("shutdown");
    assert!(TcpStream::connect(address).await.is_err());
    assert!(
        Server::start(
            0,
            FaultModel {
                error: TransportError::Timeout,
                invalid: true
            }
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn bounded_error_categories_and_shutdown_results_are_explicit() {
    let mut server = Server::start(0, Sony::default()).await.expect("start");
    for kind in [
        io::ErrorKind::UnexpectedEof,
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::BrokenPipe,
    ] {
        record_error(&server.state, io::Error::from(kind).into()).await;
    }
    assert_eq!(server.state.lock().await.errors, [] as [String; 0]);
    for _ in 0..66 {
        record_error(
            &server.state,
            io::Error::other("not retained payload").into(),
        )
        .await;
    }
    assert_eq!(server.state.lock().await.errors.len(), 64);
    server.task.abort();
    server.task = tokio::spawn(async { Err(io::Error::other("injected accept").into()) });
    tokio::task::yield_now().await;
    assert!(server.shutdown().await.is_err());
    let mut server = Server::start(0, Sony::default()).await.expect("start");
    server.task.abort();
    server.task = tokio::spawn(async {
        panic!("injected server task");
    });
    tokio::task::yield_now().await;
    assert!(server.shutdown().await.is_err());
}

#[tokio::test(start_paused = true)]
async fn writes_have_deadlines_and_cancel_flush_preserves_interrupts() {
    let (mut writer, mut reader) = tokio::io::duplex(8);
    write(&mut writer, &[1]).await.expect("write");
    assert_eq!(reader.read_u8().await.expect("read"), 1);
    assert!(write(&mut writer, &[0; 16]).await.is_err());
    drop(reader);
    assert!(write(&mut writer, &[1]).await.is_err());
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind");
    let mut peer = TcpStream::connect(listener.local_addr().expect("address"))
        .await
        .expect("connect");
    let (socket, _) = listener.accept().await.expect("accept");
    let (_, mut writer) = socket.into_split();
    let mut pending = Pending::from([
        (urb(1, 1, 0), vec![], Instant::now()),
        (urb(3, 1, 0), vec![], Instant::now()),
        (urb(2, 0, 0), vec![], Instant::now()),
    ]);
    cancel_bulk(
        &mut pending,
        &mut writer,
        &[1, 2],
        SocketOps::default().write,
    )
    .await
    .expect("flush");
    assert_eq!(pending.len(), 1);
    let mut replies = [0; 96];
    peer.read_exact(&mut replies).await.expect("replies");
    assert_eq!(
        wire::word(&replies, 20).expect("status"),
        u32::from_be_bytes((-104_i32).to_be_bytes())
    );
}

#[tokio::test]
async fn native_socket_failures_and_task_join_failures_propagate_without_invalid_descriptors() {
    let ops = SocketOps {
        address: |_| Err(io::Error::other("address")),
        ..SocketOps::default()
    };
    assert!(
        Server::start_with(0, Synthetic::default(), ops)
            .await
            .is_err()
    );
    let ops = SocketOps {
        accept: |_| Box::pin(ready(Err(io::Error::other("accept")))),
        ..SocketOps::default()
    };
    let server = Server::start_with(0, Synthetic::default(), ops)
        .await
        .expect("bind");
    tokio::task::yield_now().await;
    assert!(server.shutdown().await.is_err());
    let ops = SocketOps {
        nodelay: |_| Err(io::Error::other("nodelay")),
        ..SocketOps::default()
    };
    let server = Server::start_with(0, Synthetic::default(), ops)
        .await
        .expect("bind");
    let mut socket = TcpStream::connect(server.address).await.expect("connect");
    assert!(socket.read_u8().await.is_err());
    assert_eq!(server.state.lock().await.errors.len(), 1);
    server.shutdown().await.expect("shutdown");
}

#[tokio::test(start_paused = true)]
async fn bounded_frame_reader_rejects_partial_or_stalled_headers_and_payloads() {
    let (mut tx, mut rx) = tokio::io::duplex(64);
    assert!(Framed::new(&mut rx).next().await.is_err());
    tx.write_all(&[0; 48]).await.expect("invalid header");
    assert!(Framed::new(&mut rx).next().await.is_err());
    let mut header = Vec::new();
    for value in [1_u32, 1, 0x0001_0001, 0, 4, 0, 1, 0, 0, 0, 0, 0] {
        header.extend_from_slice(&value.to_be_bytes());
    }
    tx.write_all(&header).await.expect("header");
    assert!(Framed::new(&mut rx).next().await.is_err());
    tx.write_all(&header).await.expect("header");
    tx.shutdown().await.expect("EOF");
    assert!(Framed::new(&mut rx).next().await.is_err());
    assert!(Framed::new(&mut rx).next().await.is_err());
}

struct BoundaryModel {
    hang: bool,
}
impl DeviceModel for BoundaryModel {
    fn identity(&self) -> Identity {
        Synthetic::default().identity()
    }
    async fn transfer(
        &mut self, _endpoint: u32, _direction: u32, _payload: &[u8], _bytes: &mut [u8],
    ) -> Result<usize, TransportError> {
        if self.hang {
            std::future::pending().await
        } else {
            Ok(usize::MAX)
        }
    }
    async fn control(&mut self, _setup: [u8; 8], _payload: &[u8]) -> Result<Control, Error> {
        if self.hang {
            std::future::pending().await
        } else {
            Err(io::Error::other("injected").into())
        }
    }
}

#[tokio::test(start_paused = true)]
async fn model_futures_are_deadlined_and_oversized_model_results_are_rejected() {
    for hang in [false, true] {
        let raw = Arc::new(Mutex::new(BoundaryModel { hang }));
        let model = model_io::shared(&raw);
        let mut state = State::new();
        model.identity().await.validate().expect("valid model");
        let response = execute(&urb(4, 1, 8), &[], &mut state, model.as_ref()).await;
        if hang {
            assert!(response.expect("timeout").is_none());
        } else {
            assert!(response.is_err());
        }
        assert!(
            control(&urb(0, 0, 0), &[], &mut state, model.as_ref())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn negotiation_eof_errors_and_explicit_server_abort_are_observable() {
    let server = Server::start(0, Synthetic::default()).await.expect("start");
    let model = model_io::shared(&server.model);
    for imported in [false, true] {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind");
        let mut peer = TcpStream::connect(listener.local_addr().expect("address"))
            .await
            .expect("connect");
        let (mut socket, _) = listener.accept().await.expect("accept");
        if imported {
            peer.write_all(&wire::operation(0x8003, 0))
                .await
                .expect("operation");
        }
        peer.shutdown().await.expect("EOF");
        assert!(
            negotiate(
                &mut socket,
                &server.state,
                SocketOps::default().write,
                model.as_ref()
            )
            .await
            .is_err()
        );
    }
    server.task.abort();
    server.shutdown().await.expect("explicit abort");
}

struct OversizedAfterPending(usize);
impl DeviceModel for OversizedAfterPending {
    fn identity(&self) -> Identity {
        Synthetic::default().identity()
    }
    fn transfer(
        &mut self, _endpoint: u32, _direction: u32, _payload: &[u8], _bytes: &mut [u8],
    ) -> impl Future<Output = Result<usize, TransportError>> + Send {
        self.0 += 1;
        ready(if self.0 == 1 {
            Err(TransportError::Timeout)
        } else {
            Ok(usize::MAX)
        })
    }
    fn control(
        &mut self, _setup: [u8; 8], _payload: &[u8],
    ) -> impl Future<Output = Result<Control, Error>> + Send {
        ready(Ok(Control {
            data: None,
            flush_bulk: false,
        }))
    }
}

#[tokio::test]
async fn model_result_corruption_after_pending_cannot_escape_the_transfer_bound() {
    let mut model = OversizedAfterPending(0);
    assert!(
        model
            .control([0; 8], &[])
            .await
            .expect("unknown control")
            .data
            .is_none()
    );
    let server = Server::start(0, model).await.expect("server");
    let mut socket = super::network_tests::imported(&server).await;
    socket
        .write_all(&super::network_tests::request(5, 1, 1, 16, [0; 8]))
        .await
        .expect("pending");
    assert!(
        timeout(Duration::from_secs(2), socket.read_u8())
            .await
            .expect("close deadline")
            .is_err()
    );
    server.shutdown().await.expect("stop");
}

#[tokio::test(start_paused = true)]
async fn fragmented_headers_and_payloads_survive_repeated_select_cancellation() {
    let (mut writer, reader) = tokio::io::duplex(128);
    let mut framed = Framed::new(reader);
    let header = super::network_tests::request(4, 0, 1, 3, [0; 8]);
    writer.write_all(&header[..4]).await.expect("prefix");
    assert!(
        timeout(Duration::from_millis(1), framed.next())
            .await
            .is_err()
    );
    writer.write_all(&header[4..]).await.expect("header suffix");
    writer.write_all(&[7]).await.expect("payload prefix");
    assert!(
        timeout(Duration::from_millis(1), framed.next())
            .await
            .is_err()
    );
    writer.write_all(&[8, 9]).await.expect("payload suffix");
    let (urb, payload) = framed.next().await.expect("same complete frame");
    assert_eq!(urb.seq, 1);
    assert_eq!(payload, vec![7, 8, 9]);
    let started = Instant::now();
    for _ in 0..2 {
        assert!(
            timeout(Duration::from_secs(10), framed.next())
                .await
                .is_err()
        );
    }
    assert!(framed.next().await.is_err());
    assert_eq!(started.elapsed(), Duration::from_secs(30));
    writer.shutdown().await.expect("EOF");
    assert!(framed.next().await.is_err());
}

struct BrokenRead;
impl tokio::io::AsyncRead for BrokenRead {
    fn poll_read(
        self: std::pin::Pin<&mut Self>, _cx: &mut std::task::Context<'_>,
        _buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::task::Poll::Ready(Err(io::Error::other("injected read")))
    }
}

#[tokio::test]
async fn framed_io_errors_preserve_the_header_and_payload_failure_category() {
    assert!(Framed::new(BrokenRead).next().await.is_err());
    let mut reader = Framed {
        reader: Box::new(BrokenRead),
        state: FrameState::Payload(urb(4, 0, 8), Vec::new()),
        deadline: Instant::now() + LIMIT,
    };
    assert!(reader.next().await.is_err());
}

#[tokio::test]
async fn sony_alternate_configuration_and_control_history_bounds_are_shared_protocol_rules() {
    let raw = Arc::new(Mutex::new(Sony::default()));
    let model = model_io::shared(&raw);
    let mut state = State::new();
    let mut request = urb(0, 0, 0);
    request.setup = [1, 11, 1, 0, 0, 0, 0, 0];
    let reply = control(&request, &[], &mut state, model.as_ref())
        .await
        .expect("alternate one");
    assert_eq!(wire::word(&reply, 20).expect("success"), 0);
    request.direction = 1;
    request.length = 1;
    request.setup = [0x81, 10, 0, 0, 0, 0, 1, 0];
    assert_eq!(
        control(&request, &[], &mut state, model.as_ref())
            .await
            .expect("get alt")
            .get(48..),
        Some(&[1][..])
    );
    request.setup = [0x80, 8, 0, 0, 0, 0, 1, 0];
    assert_eq!(
        control(&request, &[], &mut state, model.as_ref())
            .await
            .expect("get config")
            .get(48..),
        Some(&[1][..])
    );
    state.controls.resize(4096, [0; 8]);
    assert!(
        control(&request, &[], &mut state, model.as_ref())
            .await
            .is_err()
    );
}
