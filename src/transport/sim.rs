//! An in-memory, scriptable PTP camera implementing [`Transport`].
//!
//! Every layer above the transport is tested against [`SimCamera`], so CI never needs USB.
//!
//! What it models:
//! - Transfers, not a byte stream. Each `bulk_in` returns at most one queued device transfer
//!   and at most `bytes.len()` of it. When the host buffer is smaller than the transfer, the
//!   rest stays queued for the next read, as on a real bulk pipe. A data container whose
//!   length is a multiple of the packet size is followed by a zero-length packet
//! - A PTP responder: `OpenSession`, `CloseSession`, `GetDeviceInfo` are built in; any other
//!   operation is scripted with [`SimCamera::with_operation`] and unknown operations answer
//!   `OperationNotSupported`
//! - Injected faults ([`Fault`]): stall, short read, timeout, disconnect
//! - The wedge seen on real hardware: opening a session while a previous one was abandoned
//!   without `CloseSession` stalls the bulk pipes, and `clear_halt` does not cure it. Only
//!   `reset` does. Turn the model off with [`SimCamera::with_wedge_model`] to get the standard
//!   `SessionAlreadyOpen` response instead
//!
//! Timeouts are deterministic: reading when the camera has nothing queued returns
//! [`TransportError::Timeout`] immediately, without sleeping.
//!
//! References:
//! - USB Still Image Capture Device Definition 1.0, sections 5-7

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use super::{Endpoint, Transport, TransportError};
use crate::ptp::session::code;
use crate::ptp::{Container, ContainerHeader, ContainerKind, HEADER_LEN, PayloadLen};

pub mod sony;

/// A fault injected into an endpoint with [`SimCamera::inject`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The endpoint halts and reports [`TransportError::Stall`] until `clear_halt`.
    Stall,
    /// The next receive delivers only this many bytes and drops the rest of the transfer.
    ShortRead(usize),
    /// The operation fails once with [`TransportError::Timeout`], transferring nothing.
    Timeout,
    /// The camera disappears. Every later operation fails with
    /// [`TransportError::Disconnected`].
    Disconnect,
}

#[derive(Debug, Clone, Copy)]
struct Injection {
    endpoint: Endpoint,
    remaining: usize,
    fault: Fault,
}

/// What the simulated camera does when it receives an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// Response code sent last.
    pub response: u16,
    /// Response parameters.
    pub params: Vec<u32>,
    /// Payload of a device-to-host data phase, when the operation has one.
    pub data_in: Option<Vec<u8>>,
    /// Whether the operation expects a host-to-device data phase before it responds.
    pub expects_data_out: bool,
    /// Size of each transfer the data container is split into. `None` sends it whole.
    pub chunk_size: Option<usize>,
    /// Explicit transfer boundaries over the data container and response as one byte stream:
    /// each entry is a transfer length, and whatever remains is the last transfer. Overrides
    /// `chunk_size` and the automatic zero-length packet. `Some(vec![])` coalesces everything
    /// into one transfer.
    pub cuts: Option<Vec<usize>>,
}

impl Reply {
    /// A bare `OK` response.
    #[must_use]
    pub fn ok() -> Self {
        Self::with_code(code::RESPONSE_OK)
    }

    /// A bare response with the given code.
    #[must_use]
    pub fn with_code(response: u16) -> Self {
        Self {
            response,
            params: Vec::new(),
            data_in: None,
            expects_data_out: false,
            chunk_size: None,
            cuts: None,
        }
    }

    /// An `OK` response preceded by a device-to-host data phase.
    #[must_use]
    pub fn data(payload: Vec<u8>) -> Self {
        Self {
            data_in: Some(payload),
            ..Self::ok()
        }
    }

    /// An `OK` response that first requires a host-to-device data phase.
    #[must_use]
    pub fn accepting_data() -> Self {
        Self {
            expects_data_out: true,
            ..Self::ok()
        }
    }

    /// Splits the data container into transfers of `size` bytes (`size` of 0 is treated as 1).
    #[must_use]
    pub fn chunked(mut self, size: usize) -> Self {
        self.chunk_size = Some(size.max(1));
        self
    }

    /// Sets explicit transfer boundaries over the data container and response. See
    /// [`Reply::cuts`].
    #[must_use]
    pub fn cut(mut self, lengths: &[usize]) -> Self {
        self.cuts = Some(lengths.to_vec());
        self
    }

    /// Sets the response parameters.
    #[must_use]
    pub fn with_params(mut self, params: &[u32]) -> Self {
        self.params = params.to_vec();
        self
    }
}

/// A vendor extension plugged into a [`SimCamera`] with [`SimCamera::with_responder`].
///
/// Scripted operations answer with a fixed [`Reply`]; an extension keeps state, so it can answer
/// the same operation differently over time and react to the data the host sends. It is
/// consulted after the built-in session operations and before the scripted ones.
pub trait Responder: std::fmt::Debug + Send {
    /// Handles `operation`, or returns `None` to leave it to the scripted operations. A reply
    /// with `expects_data_out` is completed by [`Responder::data_out`].
    fn command(&mut self, operation: u16, params: &[u32]) -> Option<Reply>;

    /// Completes an operation whose reply expected a host-to-device data phase, or returns
    /// `None` to answer with the reply [`Responder::command`] produced.
    fn data_out(&mut self, operation: u16, params: &[u32], payload: &[u8]) -> Option<Reply>;

    /// The session opened (`true`) or ended by `CloseSession` or a device reset (`false`).
    fn session_changed(&mut self, open: bool);

    /// Whether the camera stalls the bulk pipe instead of answering `operation`.
    fn stall(&mut self, operation: u16, params: &[u32]) -> bool;

    /// Events to deliver on the interrupt pipe now, as `(code, params)`. Called after every
    /// command, data phase and reset; the extension hands each event out once.
    fn take_events(&mut self) -> Vec<(u16, Vec<u32>)>;
}

/// A command the camera received, for assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRecord {
    /// Operation code.
    pub code: u16,
    /// Transaction id the host used.
    pub transaction_id: u32,
    /// Command parameters.
    pub params: Vec<u32>,
}

/// Device-side state of a host-to-device data phase in progress.
#[derive(Debug)]
struct PendingData {
    code: u16,
    transaction_id: u32,
    params: Vec<u32>,
    reply: Reply,
}

/// Health of the camera's USB link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    Up,
    /// Bulk pipes stall until a reset.
    Wedged,
    /// Unplugged for good.
    Gone,
}

/// The simulated camera. See the module docs.
#[derive(Debug)]
pub struct SimCamera {
    device_info: Vec<u8>,
    operations: HashMap<u16, Reply>,
    responder: Option<Box<dyn Responder>>,
    idle_sink: Option<fn(Duration)>,
    wedge_model: bool,
    max_packet_size: usize,
    max_payload: usize,
    session_open: bool,
    expected_transaction: u32,
    link: Link,
    halted: [bool; 3],
    host_buffer: Vec<u8>,
    pending: Option<PendingData>,
    bulk_in: VecDeque<Vec<u8>>,
    interrupt_in: VecDeque<Vec<u8>>,
    injections: Vec<Injection>,
    commands: Vec<CommandRecord>,
    received_data: Vec<Vec<u8>>,
    bulk_out_transfers: Vec<usize>,
    clear_halts: usize,
    resets: usize,
    #[cfg(test)]
    pub(crate) fail_clear: usize,
    #[cfg(test)]
    pub(crate) fail_reset: bool,
}

impl Default for SimCamera {
    fn default() -> Self {
        Self::new()
    }
}

impl SimCamera {
    /// A camera with an empty `DeviceInfo` dataset, the wedge model on and 512-byte packets.
    #[must_use]
    pub fn new() -> Self {
        Self {
            device_info: Vec::new(),
            operations: HashMap::new(),
            responder: None,
            idle_sink: None,
            wedge_model: true,
            max_packet_size: 512,
            max_payload: 256 * 1024 * 1024,
            session_open: false,
            expected_transaction: 1,
            link: Link::Up,
            halted: [false; 3],
            host_buffer: Vec::new(),
            pending: None,
            bulk_in: VecDeque::new(),
            interrupt_in: VecDeque::new(),
            injections: Vec::new(),
            commands: Vec::new(),
            received_data: Vec::new(),
            bulk_out_transfers: Vec::new(),
            clear_halts: 0,
            resets: 0,
            #[cfg(test)]
            fail_clear: 0,
            #[cfg(test)]
            fail_reset: false,
        }
    }

    /// Sets the raw `DeviceInfo` dataset returned by `GetDeviceInfo`.
    #[must_use]
    pub fn with_device_info(mut self, dataset: Vec<u8>) -> Self {
        self.device_info = dataset;
        self
    }

    /// Scripts an operation. Replaces any earlier script for `operation`.
    #[must_use]
    pub fn with_operation(mut self, operation: u16, reply: Reply) -> Self {
        self.operations.insert(operation, reply);
        self
    }

    /// Plugs in a vendor extension. Replaces any earlier one.
    #[must_use]
    pub fn with_responder(mut self, responder: impl Responder + 'static) -> Self {
        self.responder = Some(Box::new(responder));
        self
    }

    /// Calls `sink` with the requested timeout whenever a receive finds nothing queued, just
    /// before it times out. Real hardware spends that time waiting; a test passes a sink that
    /// advances its injected clock by it.
    #[must_use]
    pub fn with_idle_sink(mut self, sink: fn(Duration)) -> Self {
        self.idle_sink = Some(sink);
        self
    }

    /// Turns the abandoned-session wedge on or off.
    #[must_use]
    pub fn with_wedge_model(mut self, enabled: bool) -> Self {
        self.wedge_model = enabled;
        self
    }

    /// Sets the bulk endpoint packet size (512 for USB 2, 1024 for USB 3). Clamped to 1.
    #[must_use]
    pub fn with_max_packet_size(mut self, size: usize) -> Self {
        self.max_packet_size = size.max(1);
        self
    }

    /// Sets the maximum payload size of generated data, response, and event containers.
    #[must_use]
    pub fn with_max_payload(mut self, max_payload: usize) -> Self {
        self.max_payload = max_payload;
        self
    }

    /// Injects `fault` into `endpoint` after `after` more operations on it succeed.
    pub fn inject(&mut self, endpoint: Endpoint, after: usize, fault: Fault) {
        self.injections.push(Injection {
            endpoint,
            remaining: after,
            fault,
        });
    }

    /// Queues an event container on the interrupt pipe.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::Other`] if the container cannot be encoded.
    pub fn push_event(
        &mut self, event: u16, transaction_id: u32, params: &[u32],
    ) -> Result<(), TransportError> {
        let len = validate_payload(params.len().saturating_mul(4), self.max_payload)?;
        let payload: Vec<u8> = params.iter().flat_map(|p| p.to_le_bytes()).collect();
        let bytes = encode(ContainerKind::Event, event, transaction_id, &payload, len);
        self.interrupt_in.push_back(bytes);
        Ok(())
    }

    /// Queues raw bytes as one device-to-host bulk transfer, ahead of nothing: it is read after
    /// anything already queued. Used to simulate a misbehaving device.
    pub fn push_raw_bulk_in(&mut self, transfer: Vec<u8>) {
        self.bulk_in.push_back(transfer);
    }

    /// Queues raw bytes as one interrupt transfer.
    pub fn push_raw_interrupt(&mut self, transfer: Vec<u8>) {
        self.interrupt_in.push_back(transfer);
    }

    /// Sets the transaction id the camera expects next, e.g. to test wraparound.
    pub fn expect_transaction(&mut self, transaction_id: u32) {
        self.expected_transaction = transaction_id;
    }

    /// Whether the camera currently holds an open session.
    #[must_use]
    pub fn session_open(&self) -> bool {
        self.session_open
    }

    /// Whether the camera is wedged and needs a reset.
    #[must_use]
    pub fn is_wedged(&self) -> bool {
        self.link == Link::Wedged
    }

    /// Every command received so far, in order.
    #[must_use]
    pub fn commands(&self) -> &[CommandRecord] {
        &self.commands
    }

    /// Payloads of every completed host-to-device data phase.
    #[must_use]
    pub fn received_data(&self) -> &[Vec<u8>] {
        &self.received_data
    }

    /// Size of every bulk-out transfer accepted, in order. Zero marks a zero-length packet.
    #[must_use]
    pub fn bulk_out_transfers(&self) -> &[usize] {
        &self.bulk_out_transfers
    }

    /// Number of `clear_halt` calls.
    #[must_use]
    pub fn clear_halt_count(&self) -> usize {
        self.clear_halts
    }

    /// Number of `reset` calls.
    #[must_use]
    pub fn reset_count(&self) -> usize {
        self.resets
    }

    /// Entry check shared by every transfer: disconnect, halt, wedge, then injected faults.
    /// Returns the byte limit of an injected short read, if any.
    fn begin(&mut self, endpoint: Endpoint) -> Result<Option<usize>, TransportError> {
        if self.link == Link::Gone {
            return Err(TransportError::Disconnected);
        }
        let halted = *halt_flag(&mut self.halted, endpoint);
        if halted || (self.link == Link::Wedged && endpoint != Endpoint::InterruptIn) {
            return Err(TransportError::Stall);
        }
        let Some((index, injection)) = self
            .injections
            .iter_mut()
            .enumerate()
            .find(|(_, inj)| inj.endpoint == endpoint)
        else {
            return Ok(None);
        };
        if injection.remaining > 0 {
            injection.remaining -= 1;
            return Ok(None);
        }
        let fault = injection.fault;
        self.injections.remove(index);
        match fault {
            Fault::Stall => {
                self.halt(endpoint);
                Err(TransportError::Stall)
            }
            Fault::Timeout => Err(TransportError::Timeout),
            Fault::Disconnect => {
                self.link = Link::Gone;
                Err(TransportError::Disconnected)
            }
            Fault::ShortRead(len) => Ok(Some(len)),
        }
    }

    fn halt(&mut self, endpoint: Endpoint) {
        *halt_flag(&mut self.halted, endpoint) = true;
    }

    /// Delivers up to `buffer.len()` bytes of the next queued transfer.
    fn receive(
        queue_of: fn(&mut Self) -> &mut VecDeque<Vec<u8>>, camera: &mut Self, endpoint: Endpoint,
        buffer: &mut [u8], timeout: Duration,
    ) -> Result<usize, TransportError> {
        let short = camera.begin(endpoint)?;
        let sink = camera.idle_sink;
        let queue = queue_of(camera);
        let Some(transfer) = queue.pop_front() else {
            if let Some(sink) = sink {
                sink(timeout);
            }
            return Err(TransportError::Timeout);
        };
        let mut deliver = transfer.len().min(buffer.len());
        if let Some(limit) = short {
            deliver = deliver.min(limit);
        }
        let (head, tail) = transfer.split_at(deliver);
        for (out, byte) in buffer.iter_mut().zip(head) {
            *out = *byte;
        }
        if short.is_none() && !tail.is_empty() {
            queue.push_front(tail.to_vec());
        }
        Ok(deliver)
    }

    /// Consumes whatever the host has written so far.
    fn process(&mut self) -> Result<(), TransportError> {
        if let Some(pending) = self.pending.take() {
            return self.process_data(pending);
        }
        let (code, transaction_id, params, well_formed, consumed) = {
            let Ok((container, rest)) = Container::decode(&self.host_buffer) else {
                return Ok(());
            };
            if container.kind != ContainerKind::Command {
                self.halt(Endpoint::BulkOut);
                self.host_buffer.clear();
                return Err(TransportError::Stall);
            }
            (
                container.code,
                container.transaction_id,
                container.params().collect::<Vec<_>>(),
                container.payload.len() % 4 == 0,
                self.host_buffer.len() - rest.len(),
            )
        };
        self.host_buffer.drain(..consumed);
        self.commands.push(CommandRecord {
            code,
            transaction_id,
            params: params.clone(),
        });
        self.handle_command(code, transaction_id, &params, well_formed)
    }

    fn process_data(&mut self, pending: PendingData) -> Result<(), TransportError> {
        let payload = match Container::decode(&self.host_buffer) {
            Ok((container, _))
                if container.kind == ContainerKind::Data
                    && container.code == pending.code
                    && container.transaction_id == pending.transaction_id =>
            {
                container.payload.to_vec()
            }
            Ok(_) => {
                self.halt(Endpoint::BulkOut);
                self.host_buffer.clear();
                return Err(TransportError::Stall);
            }
            Err(_) => {
                self.pending = Some(pending);
                return Ok(());
            }
        };
        self.host_buffer.clear();
        let reply = self
            .responder
            .as_mut()
            .and_then(|r| r.data_out(pending.code, &pending.params, &payload))
            .unwrap_or(pending.reply);
        self.received_data.push(payload);
        self.respond(pending.code, pending.transaction_id, &reply)
    }

    /// Strict device-side checks: `OpenSession` carries transaction id 0 and one non-zero
    /// session id; inside a session ids run 1, 2, ... skipping `0xFFFF_FFFF` and wrapping to 1.
    fn validate(
        &self, op: u16, transaction_id: u32, params: &[u32], well_formed: bool,
    ) -> Option<u16> {
        if op == code::OPEN_SESSION {
            if transaction_id != 0 {
                return Some(code::INVALID_TRANSACTION_ID);
            }
            return match params {
                [id] if *id != 0 && well_formed => None,
                _ => Some(code::INVALID_PARAMETER),
            };
        }
        let sequenced = self.session_open;
        if sequenced && transaction_id != self.expected_transaction {
            return Some(code::INVALID_TRANSACTION_ID);
        }
        (!well_formed).then_some(code::INVALID_PARAMETER)
    }

    fn handle_command(
        &mut self, op: u16, transaction_id: u32, params: &[u32], well_formed: bool,
    ) -> Result<(), TransportError> {
        let needs_session = op != code::OPEN_SESSION && op != code::GET_DEVICE_INFO;
        let rejection = if needs_session && !self.session_open {
            Some(code::SESSION_NOT_OPEN)
        } else {
            self.validate(op, transaction_id, params, well_formed)
        };
        if rejection.is_none() && self.session_open && op != code::OPEN_SESSION {
            // An accepted command consumes its id whatever the response code is.
            self.expected_transaction = match transaction_id.checked_add(1) {
                Some(next) if next != u32::MAX => next,
                _ => 1,
            };
        }
        let reply = match rejection {
            Some(response) => Reply::with_code(response),
            None => self.execute(op, params)?,
        };
        if reply.expects_data_out && reply.response == code::RESPONSE_OK {
            self.pending = Some(PendingData {
                code: op,
                transaction_id,
                params: params.to_vec(),
                reply,
            });
            return Ok(());
        }
        self.respond(op, transaction_id, &reply)
    }

    /// Runs a validated command and returns its reply.
    fn execute(&mut self, op: u16, params: &[u32]) -> Result<Reply, TransportError> {
        Ok(match op {
            code::OPEN_SESSION if self.session_open => {
                if self.wedge_model {
                    self.link = Link::Wedged;
                    return Err(TransportError::Stall);
                }
                Reply::with_code(code::SESSION_ALREADY_OPEN)
            }
            code::OPEN_SESSION => {
                self.session_open = true;
                self.expected_transaction = 1;
                self.notify_session(true);
                Reply::ok()
            }
            code::CLOSE_SESSION => {
                self.session_open = false;
                self.notify_session(false);
                Reply::ok()
            }
            code::GET_DEVICE_INFO => Reply::data(self.device_info.clone()),
            _ if self.responder.as_mut().is_some_and(|r| r.stall(op, params)) => {
                self.halt(Endpoint::BulkOut);
                return Err(TransportError::Stall);
            }
            _ => match self.responder.as_mut().and_then(|r| r.command(op, params)) {
                Some(reply) => reply,
                None => match self.operations.get(&op) {
                    Some(reply) => reply.clone(),
                    None => Reply::with_code(code::OPERATION_NOT_SUPPORTED),
                },
            },
        })
    }

    /// Queues the data phase (with its zero-length packet when needed) and the response.
    fn respond(
        &mut self, op: u16, transaction_id: u32, reply: &Reply,
    ) -> Result<(), TransportError> {
        let data = match &reply.data_in {
            Some(payload) => {
                let len = validate_payload(payload.len(), self.max_payload)?;
                Some(encode(
                    ContainerKind::Data,
                    op,
                    transaction_id,
                    payload,
                    len,
                ))
            }
            None => None,
        };
        let len = validate_payload(reply.params.len().saturating_mul(4), self.max_payload)?;
        let params: Vec<u8> = reply.params.iter().flat_map(|p| p.to_le_bytes()).collect();
        let response = encode(
            ContainerKind::Response,
            reply.response,
            transaction_id,
            &params,
            len,
        );
        if let Some(cuts) = &reply.cuts {
            let mut stream = data.unwrap_or_default();
            stream.extend_from_slice(&response);
            let mut rest = stream.as_slice();
            for &len in cuts {
                let (now, later) = rest.split_at(len.min(rest.len()));
                self.bulk_in.push_back(now.to_vec());
                rest = later;
            }
            if !rest.is_empty() {
                self.bulk_in.push_back(rest.to_vec());
            }
            return Ok(());
        }
        if let Some(data) = data {
            let needs_zlp = data.len() % self.max_packet_size == 0;
            let size = reply.chunk_size.unwrap_or(data.len()).max(1);
            self.bulk_in.extend(data.chunks(size).map(<[u8]>::to_vec));
            if needs_zlp {
                self.bulk_in.push_back(Vec::new());
            }
        }
        self.bulk_in.push_back(response);
        Ok(())
    }
}

fn halt_flag(halted: &mut [bool; 3], endpoint: Endpoint) -> &mut bool {
    let [bulk_out, bulk_in, interrupt_in] = halted;
    match endpoint {
        Endpoint::BulkOut => bulk_out,
        Endpoint::BulkIn => bulk_in,
        Endpoint::InterruptIn => interrupt_in,
    }
}

fn validate_payload(len: usize, max: usize) -> Result<PayloadLen, TransportError> {
    PayloadLen::new(len, max).map_err(|err| TransportError::Other(err.to_string()))
}

fn encode(
    kind: ContainerKind, code: u16, transaction_id: u32, payload: &[u8], len: PayloadLen,
) -> Vec<u8> {
    let header = ContainerHeader::for_payload(kind, code, transaction_id, len);
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(payload);
    out
}

impl SimCamera {
    fn write_bulk(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        self.begin(Endpoint::BulkOut)?;
        self.bulk_out_transfers.push(bytes.len());
        self.host_buffer.extend_from_slice(bytes);
        let result = self.process();
        self.flush_responder_events();
        result
    }

    fn notify_session(&mut self, open: bool) {
        if let Some(responder) = self.responder.as_mut() {
            responder.session_changed(open);
        }
    }

    /// Moves the extension's pending events onto the interrupt pipe. An event that cannot be
    /// encoded (more parameters than a container holds) is dropped.
    fn flush_responder_events(&mut self) {
        let events = self
            .responder
            .as_mut()
            .map(|r| r.take_events())
            .unwrap_or_default();
        for (event, params) in events {
            // Only too many parameters can fail the push; such an event is dropped.
            let _ = self.push_event(event, 0xFFFF_FFFF, &params);
        }
    }

    fn halt_cleared(&mut self, endpoint: Endpoint) -> Result<(), TransportError> {
        #[cfg(test)]
        if self.fail_clear > 0 {
            self.fail_clear -= 1;
            return Err(TransportError::Other("injected".into()));
        }
        if self.link == Link::Gone {
            return Err(TransportError::Disconnected);
        }
        self.clear_halts += 1;
        *halt_flag(&mut self.halted, endpoint) = false;
        Ok(())
    }

    fn reset_state(&mut self) -> Result<(), TransportError> {
        #[cfg(test)]
        if self.fail_reset {
            return Err(TransportError::Other("reset".into()));
        }
        if self.link == Link::Gone {
            return Err(TransportError::Disconnected);
        }
        self.resets += 1;
        self.halted = [false; 3];
        self.link = Link::Up;
        self.session_open = false;
        self.notify_session(false);
        self.pending = None;
        self.host_buffer.clear();
        self.bulk_in.clear();
        self.interrupt_in.clear();
        Ok(())
    }
}

/// Every operation completes immediately, so each future is already resolved.
impl Transport for SimCamera {
    fn bulk_out(
        &mut self, bytes: &[u8], _timeout: Duration,
    ) -> impl Future<Output = Result<(), TransportError>> + Send {
        std::future::ready(self.write_bulk(bytes))
    }

    fn bulk_in(
        &mut self, bytes: &mut [u8], timeout: Duration,
    ) -> impl Future<Output = Result<usize, TransportError>> + Send {
        std::future::ready(Self::receive(
            |c| &mut c.bulk_in,
            self,
            Endpoint::BulkIn,
            bytes,
            timeout,
        ))
    }

    fn interrupt_in(
        &mut self, bytes: &mut [u8], timeout: Duration,
    ) -> impl Future<Output = Result<usize, TransportError>> + Send {
        std::future::ready(Self::receive(
            |c| &mut c.interrupt_in,
            self,
            Endpoint::InterruptIn,
            bytes,
            timeout,
        ))
    }

    fn clear_halt(
        &mut self, endpoint: Endpoint, _timeout: Duration,
    ) -> impl Future<Output = Result<(), TransportError>> + Send {
        std::future::ready(self.halt_cleared(endpoint))
    }

    fn reset(
        &mut self, _timeout: Duration,
    ) -> impl Future<Output = Result<(), TransportError>> + Send {
        std::future::ready(self.reset_state())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(1);

    async fn send(camera: &mut SimCamera, op: u16, tx: u32, params: &[u32]) {
        let bytes = Container::command(op, tx, params).unwrap();
        camera.bulk_out(&bytes, T).await.unwrap();
    }

    async fn read(camera: &mut SimCamera, size: usize) -> Result<Vec<u8>, TransportError> {
        let mut buf = vec![0; size];
        let n = camera.bulk_in(&mut buf, T).await?;
        buf.truncate(n);
        Ok(buf)
    }

    #[tokio::test]
    async fn open_session_answers_ok() {
        let mut camera = SimCamera::new();
        send(&mut camera, code::OPEN_SESSION, 0, &[1]).await;
        let bytes = read(&mut camera, 64).await.unwrap();
        let (container, _) = Container::decode(&bytes).unwrap();
        assert_eq!(container.kind, ContainerKind::Response);
        assert_eq!(container.code, code::RESPONSE_OK);
        assert!(camera.session_open());
    }

    #[tokio::test]
    async fn unknown_operation_is_not_supported() {
        let mut camera = SimCamera::new();
        send(&mut camera, code::OPEN_SESSION, 0, &[1]).await;
        read(&mut camera, 64).await.unwrap();
        send(&mut camera, 0x9999, 1, &[]).await;
        let bytes = read(&mut camera, 64).await.unwrap();
        let (container, _) = Container::decode(&bytes).unwrap();
        assert_eq!(container.code, code::OPERATION_NOT_SUPPORTED);
    }

    #[tokio::test]
    async fn small_host_buffer_splits_a_transfer() {
        let mut camera = SimCamera::new().with_device_info(vec![7; 100]);
        send(&mut camera, code::GET_DEVICE_INFO, 0, &[]).await;
        assert_eq!(read(&mut camera, 64).await.unwrap().len(), 64);
        assert_eq!(read(&mut camera, 64).await.unwrap().len(), 48);
    }

    #[tokio::test]
    async fn aligned_data_container_is_followed_by_zlp() {
        let mut camera = SimCamera::new().with_device_info(vec![7; 512 - HEADER_LEN]);
        send(&mut camera, code::GET_DEVICE_INFO, 0, &[]).await;
        assert_eq!(read(&mut camera, 1024).await.unwrap().len(), 512);
        assert_eq!(read(&mut camera, 1024).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn empty_pipe_times_out() {
        let mut camera = SimCamera::new();
        assert_eq!(read(&mut camera, 8).await, Err(TransportError::Timeout));
    }

    #[tokio::test]
    async fn stall_persists_until_clear_halt() {
        let mut camera = SimCamera::new();
        camera.inject(Endpoint::BulkIn, 0, Fault::Stall);
        assert_eq!(read(&mut camera, 8).await, Err(TransportError::Stall));
        assert_eq!(read(&mut camera, 8).await, Err(TransportError::Stall));
        camera.clear_halt(Endpoint::BulkIn, T).await.unwrap();
        assert_eq!(read(&mut camera, 8).await, Err(TransportError::Timeout));
    }

    #[tokio::test]
    async fn fault_fires_after_requested_operations() {
        let mut camera = SimCamera::new();
        camera.inject(Endpoint::BulkIn, 1, Fault::Timeout);
        camera.push_raw_bulk_in(vec![1, 2, 3]);
        assert_eq!(read(&mut camera, 8).await.unwrap(), [1, 2, 3]);
        assert_eq!(read(&mut camera, 8).await, Err(TransportError::Timeout));
    }

    #[tokio::test]
    async fn short_read_drops_the_tail() {
        let mut camera = SimCamera::new();
        camera.inject(Endpoint::BulkIn, 0, Fault::ShortRead(2));
        camera.push_raw_bulk_in(vec![1, 2, 3, 4]);
        assert_eq!(read(&mut camera, 8).await.unwrap(), [1, 2]);
        assert_eq!(read(&mut camera, 8).await, Err(TransportError::Timeout));
    }

    #[tokio::test]
    async fn disconnect_is_permanent() {
        let mut camera = SimCamera::new();
        camera.inject(Endpoint::BulkIn, 0, Fault::Disconnect);
        assert_eq!(
            read(&mut camera, 8).await,
            Err(TransportError::Disconnected)
        );
        assert_eq!(camera.reset(T).await, Err(TransportError::Disconnected));
    }

    #[tokio::test]
    async fn second_open_wedges_until_reset() {
        let mut camera = SimCamera::new();
        send(&mut camera, code::OPEN_SESSION, 0, &[1]).await;
        read(&mut camera, 64).await.unwrap();

        let open = Container::command(code::OPEN_SESSION, 0, &[1]).unwrap();
        assert_eq!(camera.bulk_out(&open, T).await, Err(TransportError::Stall));
        assert!(camera.is_wedged());

        camera.clear_halt(Endpoint::BulkOut, T).await.unwrap();
        assert_eq!(camera.bulk_out(&open, T).await, Err(TransportError::Stall));

        camera.reset(T).await.unwrap();
        assert!(!camera.is_wedged());
        camera.bulk_out(&open, T).await.unwrap();
    }

    #[tokio::test]
    async fn wedge_model_off_reports_already_open() {
        let mut camera = SimCamera::new().with_wedge_model(false);
        send(&mut camera, code::OPEN_SESSION, 0, &[1]).await;
        read(&mut camera, 64).await.unwrap();
        send(&mut camera, code::OPEN_SESSION, 0, &[1]).await;
        let bytes = read(&mut camera, 64).await.unwrap();
        let (container, _) = Container::decode(&bytes).unwrap();
        assert_eq!(container.code, code::SESSION_ALREADY_OPEN);
    }

    #[tokio::test]
    async fn data_out_is_collected_across_transfers() {
        let mut camera = SimCamera::new().with_operation(0x100D, Reply::accepting_data());
        send(&mut camera, code::OPEN_SESSION, 0, &[1]).await;
        read(&mut camera, 64).await.unwrap();
        send(&mut camera, 0x100D, 1, &[]).await;
        assert!(read(&mut camera, 64).await.is_err());

        let mut data = Vec::new();
        Container {
            kind: ContainerKind::Data,
            code: 0x100D,
            transaction_id: 1,
            payload: &[9; 40],
        }
        .encode_into(&mut data)
        .unwrap();
        let (a, b) = data.split_at(30);
        camera.bulk_out(a, T).await.unwrap();
        assert_eq!(camera.received_data().len(), 0);
        camera.bulk_out(b, T).await.unwrap();
        assert_eq!(camera.received_data(), [vec![9; 40]]);
        assert_eq!(read(&mut camera, 64).await.unwrap().len(), HEADER_LEN);
    }

    #[tokio::test]
    async fn mismatched_data_container_stalls() {
        let mut camera = SimCamera::new().with_operation(0x100D, Reply::accepting_data());
        send(&mut camera, code::OPEN_SESSION, 0, &[1]).await;
        read(&mut camera, 64).await.unwrap();
        send(&mut camera, 0x100D, 1, &[]).await;
        let mut data = Vec::new();
        Container {
            kind: ContainerKind::Data,
            code: 0x100D,
            transaction_id: 7,
            payload: &[1],
        }
        .encode_into(&mut data)
        .unwrap();
        assert_eq!(camera.bulk_out(&data, T).await, Err(TransportError::Stall));
        assert_eq!(camera.received_data().len(), 0);
    }

    proptest::proptest! {
        #[test]
        fn arbitrary_host_bytes_never_panic(
            transfers in proptest::collection::vec(
                proptest::collection::vec(proptest::prelude::any::<u8>(), 0..256), 0..8),
        ) {
            let mut camera = SimCamera::new().with_operation(0x100D, Reply::accepting_data());
            let mut open = Container::command(code::OPEN_SESSION, 0, &[1]).unwrap();
            open.extend_from_slice(&Container::command(0x100D, 1, &[]).unwrap());
            let _ = camera.write_bulk(&open);
            for transfer in transfers {
                let _ = camera.write_bulk(&transfer);
            }
        }
    }

    async fn response_code(camera: &mut SimCamera) -> u16 {
        let bytes = read(camera, 64).await.unwrap();
        Container::decode(&bytes).unwrap().0.code
    }

    #[tokio::test]
    async fn malformed_opens_are_rejected() {
        for (tx, params, expected) in [
            (5, vec![1], code::INVALID_TRANSACTION_ID),
            (0, vec![], code::INVALID_PARAMETER),
            (0, vec![0], code::INVALID_PARAMETER),
            (0, vec![1, 2], code::INVALID_PARAMETER),
        ] {
            let mut camera = SimCamera::new();
            send(&mut camera, code::OPEN_SESSION, tx, &params).await;
            assert_eq!(response_code(&mut camera).await, expected);
            assert!(!camera.session_open());
        }
    }

    #[tokio::test]
    async fn open_with_a_partial_parameter_word_is_rejected() {
        let mut camera = SimCamera::new();
        let mut command = Vec::new();
        Container {
            kind: ContainerKind::Command,
            code: code::OPEN_SESSION,
            transaction_id: 0,
            payload: &[1, 0, 0, 0, 9],
        }
        .encode_into(&mut command)
        .unwrap();
        camera.bulk_out(&command, T).await.unwrap();
        assert_eq!(response_code(&mut camera).await, code::INVALID_PARAMETER);
        assert!(!camera.session_open());
    }

    #[tokio::test]
    async fn zero_chunk_size_does_not_panic() {
        let mut reply = Reply::data(vec![1; 10]);
        reply.chunk_size = Some(0);
        let mut camera = SimCamera::new().with_operation(0x9001, reply);
        send(&mut camera, code::OPEN_SESSION, 0, &[1]).await;
        response_code(&mut camera).await;
        send(&mut camera, 0x9001, 1, &[]).await;
        assert_eq!(read(&mut camera, 64).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn commands_need_sequential_transaction_ids() {
        let mut camera = SimCamera::new().with_operation(0x9001, Reply::ok());
        send(&mut camera, code::OPEN_SESSION, 0, &[1]).await;
        assert_eq!(response_code(&mut camera).await, code::RESPONSE_OK);
        for (tx, expected) in [
            (0, code::INVALID_TRANSACTION_ID),
            (2, code::INVALID_TRANSACTION_ID),
            (1, code::RESPONSE_OK),
            (1, code::INVALID_TRANSACTION_ID),
            (2, code::RESPONSE_OK),
        ] {
            send(&mut camera, 0x9001, tx, &[]).await;
            assert_eq!(response_code(&mut camera).await, expected, "tx {tx}");
        }
    }

    #[tokio::test]
    async fn transaction_ids_wrap_to_one_skipping_the_reserved_value() {
        let mut camera = SimCamera::new().with_operation(0x9001, Reply::ok());
        send(&mut camera, code::OPEN_SESSION, 0, &[1]).await;
        response_code(&mut camera).await;
        camera.expect_transaction(u32::MAX - 1);
        for tx in [u32::MAX - 1, 1, 2] {
            send(&mut camera, 0x9001, tx, &[]).await;
            assert_eq!(response_code(&mut camera).await, code::RESPONSE_OK);
        }
    }

    #[tokio::test]
    async fn commands_outside_a_session_report_session_not_open() {
        let mut camera = SimCamera::new().with_operation(0x9001, Reply::ok());
        send(&mut camera, 0x9001, 1, &[]).await;
        assert_eq!(response_code(&mut camera).await, code::SESSION_NOT_OPEN);
    }

    #[test]
    fn generated_containers_reject_oversized_payloads() {
        let mut camera = SimCamera::new().with_max_payload(3);
        assert!(camera.push_event(0x4002, 0, &[1]).is_err());
        assert!(camera.respond(0x9001, 1, &Reply::data(vec![0; 4])).is_err());
        assert!(
            camera
                .respond(0x9001, 1, &Reply::ok().with_params(&[1]))
                .is_err()
        );
        assert!(camera.bulk_in.is_empty());
        assert!(camera.interrupt_in.is_empty());
    }

    #[tokio::test]
    async fn default_disconnected_and_exact_cut() {
        let mut camera = SimCamera {
            link: Link::Gone,
            ..SimCamera::default()
        };
        assert_eq!(
            camera.begin(Endpoint::BulkIn),
            Err(TransportError::Disconnected)
        );

        let mut reply = Reply::ok();
        reply.cuts = Some(vec![HEADER_LEN]);
        let mut camera = SimCamera::new();
        camera.respond(0x9001, 1, &reply).unwrap();
        assert_eq!(read(&mut camera, 64).await.unwrap().len(), HEADER_LEN);
        assert_eq!(read(&mut camera, 64).await, Err(TransportError::Timeout));
    }

    #[tokio::test]
    async fn events_arrive_on_the_interrupt_pipe() {
        let mut camera = SimCamera::new();
        camera.push_event(0x4002, 0, &[5]).unwrap();
        let mut buf = [0_u8; 32];
        let n = camera.interrupt_in(&mut buf, T).await.unwrap();
        let (container, _) = Container::decode(&buf[..n]).unwrap();
        assert_eq!(container.kind, ContainerKind::Event);
        assert_eq!(container.params().collect::<Vec<_>>(), [5]);
    }
}
