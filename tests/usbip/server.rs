//! Loopback-only, test-owned USB/IP export backed by the real simulated Sony responder.
//!
//! References:
//! - <https://docs.kernel.org/usb/usbip_protocol.html>
//! - <https://www.usb.org/document-library/still-image-capture-device-definition-10-and-errata-16-mar-2007>

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tethercam::transport::sim::SimCamera;
use tethercam::transport::sim::sony::{SonySimConfig, sony_camera};
use tethercam::transport::{Endpoint, Transport, TransportError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Notify, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};

use super::wire::{self, Urb};

const LIMIT: Duration = Duration::from_secs(5);
const MAX_PENDING: usize = 32;
const MAX_MEMORY: usize = 4 * 1024 * 1024;
// Permanent, export-lifetime admission credits also bound SimCamera histories and queued
// responses. Each OUT reserves four copies of its input, two maximum generated replies,
// and bookkeeping. Credits are never refunded by read, unlink, reset, or re-import.
const MAX_RETAINED_WORK: usize = 128 * 1024 * 1024;
type Pending = VecDeque<(Urb, Vec<u8>, Instant)>;

#[derive(Default)]
struct Cancellation {
    flush_bulk: bool,
    busy_status: bool,
}

pub(crate) struct State {
    pub camera: SimCamera,
    pub product: u16,
    pub class_stall: bool,
    pub status: u16,
    retained_work: usize,
    cancellation: Cancellation,
    configuration: u8,
    alternate: u8,
    pub hold_out: bool,
    pub disconnect: bool,
    pub controls: Vec<[u8; 8]>,
    pub submitted: usize,
    pub pending_bulk_in: Arc<AtomicUsize>,
    pub unlinked: usize,
    pub errors: Vec<String>,
    imported: Arc<AtomicBool>,
}

pub(crate) struct Server {
    pub state: Arc<Mutex<State>>,
    pub changed: Arc<Notify>,
    pub address: std::net::SocketAddr,
    task: JoinHandle<()>,
}

impl Server {
    pub(crate) async fn start(port: u16) -> io::Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
        let address = listener.local_addr()?;
        let state = Arc::new(Mutex::new(State {
            camera: sony_camera(SonySimConfig::default()).with_max_payload(wire::MAX_TRANSFER),
            product: 0x0c34,
            class_stall: false,
            status: 0x2001,
            retained_work: 0,
            cancellation: Cancellation::default(),
            configuration: 1,
            alternate: 0,
            hold_out: false,
            disconnect: false,
            controls: Vec::new(),
            submitted: 0,
            pending_bulk_in: Arc::new(AtomicUsize::new(0)),
            unlinked: 0,
            errors: Vec::new(),
            imported: Arc::new(AtomicBool::new(false)),
        }));
        let changed = Arc::new(Notify::new());
        let shared = Arc::clone(&state);
        let notification = Arc::clone(&changed);
        let task = tokio::spawn(async move {
            let permits = Arc::new(Semaphore::new(4));
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                while tasks.try_join_next().is_some() {}
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((socket, peer)) = accepted else { break; };
                        if !peer.ip().is_loopback() { continue; }
                        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else { continue; };
                        let state = Arc::clone(&shared);
                        let changed = Arc::clone(&notification);
                        tasks.spawn(async move {
                            let _permit = permit;
                            if let Err(error) = connection(socket, &state, &changed).await
                                && !matches!(error.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe) {
                                let mut state = state.lock().await;
                                if state.errors.len() < 64 { state.errors.push(error.to_string()); }
                            }
                        });
                    }
                    Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                }
            }
        });
        Ok(Self {
            state,
            changed,
            address,
            task,
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Bound a reply even when the loopback peer stops consuming its socket.
async fn write(writer: &mut (impl tokio::io::AsyncWrite + Unpin), bytes: &[u8]) -> io::Result<()> {
    timeout(LIMIT, writer.write_all(bytes)).await??;
    Ok(())
}

async fn read<const N: usize>(socket: &mut TcpStream) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    timeout(Duration::from_secs(30), socket.read_exact(&mut bytes)).await??;
    Ok(bytes)
}

struct ImportLease(Arc<AtomicBool>);
impl Drop for ImportLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

async fn negotiate(
    socket: &mut TcpStream, state: &Mutex<State>,
) -> io::Result<Option<ImportLease>> {
    let op = read::<8>(socket).await?;
    if op.get(..2) != Some(&[1, 0x11]) || wire::word(&op, 4)? != 0 {
        return Err(wire::invalid());
    }
    let command = u16::from_be_bytes(
        op.get(2..4)
            .ok_or_else(wire::invalid)?
            .try_into()
            .map_err(|_| wire::invalid())?,
    );
    let product = state.lock().await.product;
    match command {
        0x8005 => {
            let mut reply = wire::operation(5, 0);
            reply.extend_from_slice(&1_u32.to_be_bytes());
            reply.extend_from_slice(&wire::exported(product));
            reply.extend_from_slice(&[6, 1, 1, 0]);
            write(socket, &reply).await?;
            Ok(None)
        }
        0x8003 => {
            let bus = read::<32>(socket).await?;
            if bus.get(..4) != Some(b"1-1\0") || bus.iter().skip(4).any(|b| *b != 0) {
                write(socket, &wire::operation(3, 1)).await?;
                return Ok(None);
            }
            let imported = Arc::clone(&state.lock().await.imported);
            if imported
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                write(socket, &wire::operation(3, 1)).await?;
                return Ok(None);
            }
            let lease = ImportLease(imported);
            let mut reply = wire::operation(3, 0);
            reply.extend_from_slice(&wire::exported(product));
            write(socket, &reply).await?;
            Ok(Some(lease))
        }
        _ => Err(wire::invalid()),
    }
}

async fn connection(
    mut socket: TcpStream, state: &Mutex<State>, changed: &Notify,
) -> io::Result<()> {
    socket.set_nodelay(true)?;
    let Some(_lease) = negotiate(&mut socket, state).await? else {
        return Ok(());
    };
    let pending_count = PendingCount(Arc::clone(&state.lock().await.pending_bulk_in));
    // One reader owns framing, including fragmented TCP headers. Cancellation of read_exact
    // is avoided: select only on the channel, never on a partially consumed network header.
    let (mut reader, mut writer) = socket.into_split();
    let (send, mut receive) = tokio::sync::mpsc::channel::<io::Result<(Urb, Vec<u8>)>>(8);
    let reader_task = tokio::spawn(async move {
        loop {
            let request = async {
                let mut header = [0; 48];
                timeout(Duration::from_secs(30), reader.read_exact(&mut header)).await??;
                let urb = Urb::parse(&header)?;
                let mut payload = vec![
                    0;
                    if urb.command == 1 && urb.direction == 0 {
                        urb.length
                    } else {
                        0
                    }
                ];
                timeout(LIMIT, reader.read_exact(&mut payload)).await??;
                Ok((urb, payload))
            }
            .await;
            let failed = request.is_err();
            if send.send(request).await.is_err() || failed {
                break;
            }
        }
    });
    let _reader_guard = Abort(reader_task);
    let mut pending = Pending::new();
    loop {
        tokio::select! {
            request = receive.recv() => {
                let Some(request) = request else { return Ok(()); };
                let (urb, payload) = request?;
                if urb.command == 2 {
                    let before = pending.len();
                    pending.retain(|(p, _, _)| p.seq != urb.unlink);
                    state.lock().await.unlinked += usize::from(before != pending.len());
                    write(&mut writer, &urb.reply(if before == pending.len() { 0 } else { -104 }, &[], 0)?).await?;
                } else {
                    state.lock().await.submitted += 1;
                    if let Some(reply) = execute(&urb, &payload, &mut *state.lock().await).await? {
                        write(&mut writer, &reply).await?;
                    } else {
                        let memory: usize = pending.iter().map(|(p, _, _)| p.length).sum();
                        if pending.iter().any(|(p, _, _)| p.seq == urb.seq) || pending.len() >= MAX_PENDING || memory.saturating_add(urb.length) > MAX_MEMORY {
                            return Err(wire::invalid());
                        }
                        pending.push_back((urb, payload, Instant::now() + LIMIT));
                    }
                }
            }
            () = changed.notified() => {}
            () = tokio::time::sleep_until(pending.front().map_or(Instant::now() + Duration::from_secs(30), |(_, _, deadline)| *deadline)) => {}
        }
        if std::mem::take(&mut state.lock().await.cancellation.flush_bulk) {
            cancel_bulk(&mut pending, &mut writer).await?;
        }
        if state.lock().await.disconnect {
            return Ok(());
        }
        let count = pending.len();
        for _ in 0..count {
            let Some((urb, payload, deadline)) = pending.pop_front() else {
                break;
            };
            let response = if Instant::now() >= deadline {
                Some(urb.reply(-110, &[], 0)?)
            } else if urb.direction == 1 {
                execute(&urb, &payload, &mut *state.lock().await).await?
            } else {
                None
            };
            if let Some(reply) = response {
                write(&mut writer, &reply).await?;
            } else {
                pending.push_back((urb, payload, deadline));
            }
        }
        pending_count.0.store(
            pending
                .iter()
                .filter(|(urb, _, _)| urb.endpoint == 1)
                .count(),
            Ordering::Release,
        );
    }
}

/// Clear the observation before releasing an import, including every error/abort path.
struct PendingCount(Arc<AtomicUsize>);
impl Drop for PendingCount {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Release);
    }
}

/// Complete pending bulk URBs when a class cancellation flushes the active transaction.
async fn cancel_bulk(
    pending: &mut Pending, writer: &mut tokio::net::tcp::OwnedWriteHalf,
) -> io::Result<()> {
    let mut interrupts = Pending::new();
    for (urb, payload, deadline) in std::mem::take(pending) {
        if matches!(urb.endpoint, 1 | 2) {
            write(writer, &urb.reply(-104, &[], 0)?).await?;
        } else {
            interrupts.push_back((urb, payload, deadline));
        }
    }
    *pending = interrupts;
    Ok(())
}

struct Abort(JoinHandle<()>);
impl Drop for Abort {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn execute(urb: &Urb, payload: &[u8], state: &mut State) -> io::Result<Option<Vec<u8>>> {
    if urb.endpoint == 0 {
        return control(urb, payload, state).await.map(Some);
    }
    if urb.endpoint == 2 && urb.direction == 0 {
        let cost = urb
            .length
            .saturating_mul(4)
            .saturating_add(2 * wire::MAX_TRANSFER + 512);
        state.retained_work = state.retained_work.saturating_add(cost);
        if state.retained_work > MAX_RETAINED_WORK {
            return Err(wire::invalid());
        }
    }
    if state.hold_out && urb.direction == 0 {
        return Ok(None);
    }
    let mut bytes = vec![0; urb.length];
    let result = match (urb.endpoint, urb.direction) {
        (2, 0) => state
            .camera
            .bulk_out(payload, LIMIT)
            .await
            .map(|()| payload.len()),
        (1, 1) => state.camera.bulk_in(&mut bytes, LIMIT).await,
        (3, 1) => state.camera.interrupt_in(&mut bytes, LIMIT).await,
        _ => return urb.reply(-32, &[], 0).map(Some),
    };
    match result {
        Ok(length) => urb
            .reply(0, bytes.get(..length).ok_or_else(wire::invalid)?, length)
            .map(Some),
        Err(TransportError::Timeout) => Ok(None),
        Err(TransportError::Stall) => urb.reply(-32, &[], 0).map(Some),
        Err(TransportError::Disconnected) => {
            state.disconnect = true;
            Ok(None)
        }
        Err(_) => urb.reply(-71, &[], 0).map(Some),
    }
}

async fn control(urb: &Urb, payload: &[u8], state: &mut State) -> io::Result<Vec<u8>> {
    let [
        kind,
        request,
        value_lo,
        value_hi,
        index_lo,
        index_hi,
        length_lo,
        length_hi,
    ] = urb.setup;
    let length = usize::from(u16::from_le_bytes([length_lo, length_hi]));
    if length != urb.length || urb.direction != u32::from(kind >> 7) {
        return urb.reply(-32, &[], 0);
    }
    let index = u16::from_le_bytes([index_lo, index_hi]);
    if state.controls.len() >= 4096 {
        return Err(wire::invalid());
    }
    state.controls.push(urb.setup);
    let data = match (kind, request) {
        (0x80, 6) => wire::descriptor(value_hi, value_lo, state.product),
        (0x00, 9) if value_hi == 0 && value_lo <= 1 && index == 0 && length == 0 => {
            state.configuration = value_lo;
            state.alternate = 0;
            Some(Vec::new())
        }
        (0x01, 11)
            if state.configuration == 1
                && value_hi == 0
                && value_lo <= 1
                && index == 0
                && length == 0 =>
        {
            state.alternate = value_lo;
            Some(Vec::new())
        }
        (0x21, 0x64) if index == 0 && value_lo == 0 && value_hi == 0 => match payload {
            [1, 0x40, a, b, c, d] => {
                if state
                    .camera
                    .cancel_transaction(u32::from_le_bytes([*a, *b, *c, *d]))
                {
                    state.cancellation.flush_bulk = true;
                    state.cancellation.busy_status = true;
                    state.status = 0x2019;
                }
                Some(Vec::new())
            }
            _ => None,
        },
        (0x80, 8) => Some(vec![state.configuration]),
        (0x81, 10) if index == 0 && state.configuration == 1 => Some(vec![state.alternate]),
        (0x80..=0x82, 0) => Some(vec![0, 0]),
        (0x02, 1) if value_lo == 0 && value_hi == 0 && index_hi == 0 && length == 0 => {
            let endpoint = match index_lo {
                0x81 => Endpoint::BulkIn,
                2 => Endpoint::BulkOut,
                0x83 => Endpoint::InterruptIn,
                _ => return urb.reply(-32, &[], 0),
            };
            state
                .camera
                .clear_halt(endpoint, LIMIT)
                .await
                .map_err(io::Error::other)?;
            Some(Vec::new())
        }
        (0x21, 0x66) if !state.class_stall && index == 0 && length == 0 => {
            state.camera.reset(LIMIT).await.map_err(io::Error::other)?;
            state.status = 0x2001;
            state.cancellation.busy_status = false;
            state.cancellation.flush_bulk = true;
            Some(Vec::new())
        }
        (0xa1, 0x67) if index == 0 => {
            let [low, high] = state.status.to_le_bytes();
            if std::mem::take(&mut state.cancellation.busy_status) {
                state.status = 0x2001;
            }
            Some(vec![4, 0, low, high])
        }
        _ => None,
    };
    match data {
        Some(mut bytes) => {
            bytes.truncate(length);
            urb.reply(
                0,
                &bytes,
                if urb.direction == 0 {
                    payload.len()
                } else {
                    bytes.len()
                },
            )
        }
        None => urb.reply(-32, &[], 0),
    }
}
