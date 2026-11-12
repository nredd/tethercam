//! Loopback-only, bounded USB/IP engine shared by arbitrary verification personalities.
//!
//! References:
//! - <https://docs.kernel.org/usb/usbip_protocol.html>
//! - <https://www.usb.org/document-library/still-image-capture-device-definition-10-and-errata-16-mar-2007>

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use super::DeviceModel;
use super::model_io::{self, ModelIo};
use crate::transport::TransportError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Notify};
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

/// Export-lifetime state for explicit synthetic fault injection.
pub struct State {
    retained_work: usize,
    flush_bulk: bool,
    configuration: u8,
    alternate: u8,
    /// Hold OUT requests until deadline or unlink.
    pub hold_out: bool,
    /// Disconnect the imported peer on the next wakeup.
    pub disconnect: bool,
    /// Bounded control setup history.
    pub controls: Vec<[u8; 8]>,
    /// Count of submitted requests.
    pub submitted: usize,
    /// Pending bulk IN count, cleared before releasing an import.
    pub pending_bulk_in: Arc<AtomicUsize>,
    /// Count of successful unlinks.
    pub unlinked: usize,
    /// Bounded error categories, never USB payloads.
    pub errors: Vec<String>,
    imported: Arc<AtomicBool>,
}

impl State {
    fn new() -> Self {
        Self {
            retained_work: 0,
            flush_bulk: false,
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
        }
    }
}

/// Owned loopback export. Dropping aborts all owned connection tasks.
pub struct Server<M> {
    /// Shared device state and explicit fault injection.
    pub state: Arc<Mutex<State>>,
    /// Typed personality, serialized independently from export lifecycle/fault state.
    pub model: Arc<Mutex<M>>,
    /// Notify pending transfers after changing device state.
    pub changed: Arc<Notify>,
    /// Bound loopback address; port zero selects an unused port.
    pub address: std::net::SocketAddr,
    task: JoinHandle<io::Result<()>>,
    stop: Arc<Notify>,
}

type AcceptFuture<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = io::Result<(TcpStream, std::net::SocketAddr)>> + Send + 'a,
    >,
>;

type Writer = dyn tokio::io::AsyncWrite + Unpin + Send;
type WriteFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<()>> + Send + 'a>>;
type WriteOp = for<'a> fn(&'a mut Writer, &'a [u8]) -> WriteFuture<'a>;

/// Narrow native syscall seam for failures that cannot be induced on a valid owned socket.
#[derive(Clone, Copy)]
struct SocketOps {
    address: fn(&TcpListener) -> io::Result<std::net::SocketAddr>,
    accept: for<'a> fn(&'a TcpListener) -> AcceptFuture<'a>,
    nodelay: fn(&TcpStream) -> io::Result<()>,
    write: WriteOp,
}
impl Default for SocketOps {
    fn default() -> Self {
        Self {
            address: TcpListener::local_addr,
            accept: |listener| Box::pin(listener.accept()),
            nodelay: |socket| socket.set_nodelay(true),
            write: |writer, bytes| Box::pin(write(writer, bytes)),
        }
    }
}

impl<M: DeviceModel> Server<M> {
    /// Validate a personality and bind an unprivileged loopback export.
    ///
    /// # Errors
    /// Returns descriptor validation, bind or socket address errors.
    pub async fn start(port: u16, model: M) -> io::Result<Self> {
        Self::start_with(port, model, SocketOps::default()).await
    }

    async fn start_with(port: u16, model: M, ops: SocketOps) -> io::Result<Self> {
        let model = Arc::new(Mutex::new(model));
        build(port, model_io::shared(&model), ops)
            .await
            .map(|parts| Self {
                model,
                state: parts.state,
                changed: parts.changed,
                address: parts.address,
                task: parts.task,
                stop: parts.stop,
            })
    }

    /// Stop accepting connections and join cancellation of all owned tasks.
    ///
    /// # Errors
    /// Returns a task failure if the server panicked, not for requested cancellation.
    pub async fn shutdown(mut self) -> io::Result<()> {
        finish(&mut self.task, &self.stop).await
    }
}

impl<M> Drop for Server<M> {
    fn drop(&mut self) {
        self.stop.notify_one();
        self.task.abort();
    }
}

/// Task completion has no device-type-dependent decisions.
async fn finish(task: &mut JoinHandle<io::Result<()>>, stop: &Notify) -> io::Result<()> {
    stop.notify_one();
    match task.await {
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) => Err(io::Error::other(error)),
        Ok(result) => result,
    }
}

struct Parts {
    state: Arc<Mutex<State>>,
    changed: Arc<Notify>,
    address: std::net::SocketAddr,
    task: JoinHandle<io::Result<()>>,
    stop: Arc<Notify>,
}

async fn build(port: u16, personality: Arc<dyn ModelIo>, ops: SocketOps) -> io::Result<Parts> {
    personality.identity().await.validate()?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?;
    let address = (ops.address)(&listener)?;
    let state = Arc::new(Mutex::new(State::new()));
    let changed = Arc::new(Notify::new());
    let shared = Arc::clone(&state);
    let notification = Arc::clone(&changed);
    let stop = Arc::new(Notify::new());
    let stopping = Arc::clone(&stop);
    let task = tokio::spawn(serve(
        listener,
        shared,
        notification,
        stopping,
        personality,
        ops,
    ));
    Ok(Parts {
        state,
        changed,
        address,
        task,
        stop,
    })
}

async fn serve(
    listener: TcpListener, shared: Arc<Mutex<State>>, notification: Arc<Notify>,
    stopping: Arc<Notify>, personality: Arc<dyn ModelIo>, ops: SocketOps,
) -> io::Result<()> {
    let mut tasks = tokio::task::JoinSet::new();
    let result = async { loop {
                tokio::select! {
                    () = stopping.notified() => break Ok(()),
                    accepted = (ops.accept)(&listener) => {
                        // The listener itself is IPv4-loopback-only; no non-loopback peer
                        // can reach this socket. Preserve native accept failures for shutdown.
                        let (socket, _) = accepted?;
                        if tasks.len() >= 4 { continue; }
                        let state = Arc::clone(&shared);
                        let changed = Arc::clone(&notification);
                        let model = Arc::clone(&personality);
                        tasks.spawn(async move {
                            if let Err(error) = connection(socket, &state, &changed, ops, model.as_ref()).await {
                                record_error(&state, error).await;
                            }
                        });
                    }
                    Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                }
            } }.await;
    tasks.shutdown().await;
    result
}

/// Retain only bounded error categories, ignoring ordinary peer disconnects.
async fn record_error(state: &Mutex<State>, error: io::Error) {
    if matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
    ) {
        return;
    }
    let mut state = state.lock().await;
    if state.errors.len() < 64 {
        state.errors.push(format!("{:?}", error.kind()));
    }
}

/// Bound a reply even when the loopback peer stops consuming its socket.
async fn write(writer: &mut Writer, bytes: &[u8]) -> io::Result<()> {
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
    socket: &mut TcpStream, state: &Mutex<State>, write_op: WriteOp, model: &dyn ModelIo,
) -> io::Result<Option<ImportLease>> {
    let op = read::<8>(socket).await?;
    let [
        version_high,
        version_low,
        high,
        low,
        status0,
        status1,
        status2,
        status3,
    ] = op;
    if [version_high, version_low] != [1, 0x11] || [status0, status1, status2, status3] != [0; 4] {
        return Err(wire::invalid());
    }
    let command = u16::from_be_bytes([high, low]);
    let identity = model.identity().await;
    match command {
        0x8005 => {
            let mut reply = wire::operation(5, 0);
            reply.extend_from_slice(&1_u32.to_be_bytes());
            reply.extend_from_slice(&wire::exported(&identity));
            reply.extend_from_slice(&identity.interface);
            write_op(socket, &reply).await?;
            Ok(None)
        }
        0x8003 => {
            let bus = read::<32>(socket).await?;
            if bus.get(..4) != Some(b"1-1\0") || bus.iter().skip(4).any(|b| *b != 0) {
                write_op(socket, &wire::operation(3, 1)).await?;
                return Ok(None);
            }
            let imported = Arc::clone(&state.lock().await.imported);
            if imported
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                write_op(socket, &wire::operation(3, 1)).await?;
                return Ok(None);
            }
            let lease = ImportLease(imported);
            let mut reply = wire::operation(3, 0);
            reply.extend_from_slice(&wire::exported(&identity));
            write_op(socket, &reply).await?;
            Ok(Some(lease))
        }
        _ => Err(wire::invalid()),
    }
}

async fn connection(
    mut socket: TcpStream, state: &Mutex<State>, changed: &Notify, ops: SocketOps,
    model: &dyn ModelIo,
) -> io::Result<()> {
    (ops.nodelay)(&socket)?;
    let Some(_lease) = negotiate(&mut socket, state, ops.write, model).await? else {
        return Ok(());
    };
    let pending_count = PendingCount(Arc::clone(&state.lock().await.pending_bulk_in));
    // The persistent frame buffer and deadline survive cancellation of next() when a
    // pending transfer or a model notification wins select. No detached reader task exists.
    let (reader, mut writer) = socket.into_split();
    let mut reader = Framed::new(reader);
    let mut pending = Pending::new();
    loop {
        tokio::select! {
            request = reader.next() => {
                let (urb, payload) = request?;
                if urb.command == 2 {
                    let before = pending.len();
                    pending.retain(|(p, _, _)| p.seq != urb.unlink);
                    state.lock().await.unlinked += usize::from(before != pending.len());
                    (ops.write)(&mut writer, &urb.status(if before == pending.len() { 0 } else { -104 })).await?;
                } else {
                    state.lock().await.submitted += 1;
                    if let Some(reply) = execute(&urb, &payload, &mut *state.lock().await, model).await? {
                        (ops.write)(&mut writer, &reply).await?;
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
        if std::mem::take(&mut state.lock().await.flush_bulk) {
            let endpoints = model.identity().await.bulk_endpoints;
            cancel_bulk(&mut pending, &mut writer, &endpoints, ops.write).await?;
        }
        if state.lock().await.disconnect {
            return Ok(());
        }
        for (urb, payload, deadline) in std::mem::take(&mut pending) {
            let response = if Instant::now() >= deadline {
                Some(urb.status(-110))
            } else if urb.direction == 1 {
                execute(&urb, &payload, &mut *state.lock().await, model).await?
            } else {
                None
            };
            if let Some(reply) = response {
                (ops.write)(&mut writer, &reply).await?;
            } else {
                pending.push_back((urb, payload, deadline));
            }
        }
        let endpoints = model.identity().await.bulk_endpoints;
        pending_count.0.store(
            pending
                .iter()
                .filter(|(urb, _, _)| urb.direction == 1 && endpoints.contains(&urb.endpoint))
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
    pending: &mut Pending, writer: &mut Writer, endpoints: &[u32], write_op: WriteOp,
) -> io::Result<()> {
    let mut interrupts = Pending::new();
    for (urb, payload, deadline) in std::mem::take(pending) {
        if endpoints.contains(&urb.endpoint) {
            write_op(writer, &urb.status(-104)).await?;
        } else {
            interrupts.push_back((urb, payload, deadline));
        }
    }
    *pending = interrupts;
    Ok(())
}

/// Persistent framing state makes each read cancel-safe under pending-URB notifications.
enum FrameState {
    Header(Vec<u8>),
    Payload(Urb, Vec<u8>),
}

type Reader<'a> = dyn tokio::io::AsyncRead + Unpin + Send + 'a;

struct Framed<'a> {
    reader: Box<Reader<'a>>,
    state: FrameState,
    deadline: Instant,
}
impl<'a> Framed<'a> {
    fn new(reader: impl tokio::io::AsyncRead + Unpin + Send + 'a) -> Self {
        Self {
            reader: Box::new(reader),
            state: FrameState::Header(Vec::with_capacity(48)),
            deadline: Instant::now() + Duration::from_secs(30),
        }
    }
    async fn next(&mut self) -> io::Result<(Urb, Vec<u8>)> {
        loop {
            match &mut self.state {
                FrameState::Header(bytes) => {
                    fill(self.reader.as_mut(), bytes, 48, self.deadline).await?;
                    let urb = Urb::parse(bytes)?;
                    self.state = FrameState::Payload(urb, Vec::new());
                    self.deadline = Instant::now() + LIMIT;
                }
                FrameState::Payload(urb, bytes) => {
                    let length = if urb.command == 1 && urb.direction == 0 {
                        urb.length
                    } else {
                        0
                    };
                    fill(self.reader.as_mut(), bytes, length, self.deadline).await?;
                    let result = (urb.clone(), std::mem::take(bytes));
                    self.state = FrameState::Header(Vec::with_capacity(48));
                    self.deadline = Instant::now() + Duration::from_secs(30);
                    return Ok(result);
                }
            }
        }
    }
}

/// Limit reads to this frame, retaining successful partial reads across select cancellation.
async fn fill(
    reader: &mut Reader<'_>, bytes: &mut Vec<u8>, length: usize, deadline: Instant,
) -> io::Result<()> {
    while bytes.len() < length {
        let mut limited = (&mut *reader).take((length - bytes.len()) as u64);
        let count = tokio::time::timeout_at(deadline, limited.read_buf(bytes)).await??;
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
    }
    Ok(())
}

async fn execute(
    urb: &Urb, payload: &[u8], state: &mut State, model: &dyn ModelIo,
) -> io::Result<Option<Vec<u8>>> {
    if urb.endpoint == 0 {
        return control(urb, payload, state, model).await.map(Some);
    }
    if urb.direction == 0 {
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
    let result = timeout(
        LIMIT,
        model.transfer(urb.endpoint, urb.direction, payload, &mut bytes),
    )
    .await
    .unwrap_or(Err(TransportError::Timeout));
    match result {
        Ok(length) => urb
            .reply(0, bytes.get(..length).ok_or_else(wire::invalid)?, length)
            .map(Some),
        Err(TransportError::Timeout) => Ok(None),
        Err(TransportError::Stall) => Ok(Some(urb.status(-32))),
        Err(TransportError::Disconnected) => {
            state.disconnect = true;
            Ok(None)
        }
        Err(_) => Ok(Some(urb.status(-71))),
    }
}

async fn control(
    urb: &Urb, payload: &[u8], state: &mut State, model: &dyn ModelIo,
) -> io::Result<Vec<u8>> {
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
        return Ok(urb.status(-32));
    }
    let index = u16::from_le_bytes([index_lo, index_hi]);
    if state.controls.len() >= 4096 {
        return Err(wire::invalid());
    }
    state.controls.push(urb.setup);
    let data = match (kind, request) {
        (0x80, 6) => model.identity().await.descriptor(value_hi, value_lo),
        (0x00, 9) if value_hi == 0 && value_lo <= 1 && index == 0 && length == 0 => {
            state.configuration = value_lo;
            state.alternate = 0;
            Some(Vec::new())
        }
        (0x01, 11)
            if state.configuration == 1
                && value_hi == 0
                && model.identity().await.alternates.contains(&value_lo)
                && index == 0
                && length == 0 =>
        {
            state.alternate = value_lo;
            Some(Vec::new())
        }
        (0x80, 8) => Some(vec![state.configuration]),
        (0x81, 10) if index == 0 && state.configuration == 1 => Some(vec![state.alternate]),
        (0x80..=0x82, 0) => Some(vec![0, 0]),
        _ => {
            let result = timeout(LIMIT, model.control(urb.setup, payload)).await??;
            state.flush_bulk |= result.flush_bulk;
            result.data
        }
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
        None => Ok(urb.status(-32)),
    }
}

#[cfg(test)]
mod network_tests;
#[cfg(test)]
mod tests;
