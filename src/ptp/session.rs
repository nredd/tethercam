//! PTP session and transaction engine over any [`Transport`].
//!
//! A transaction is a command, an optional data phase in either direction, and a response.
//! [`Session`] sequences transaction ids, splits data phases across bulk transfers, terminates
//! them with a zero-length packet when the container is a multiple of the packet size, polls
//! events and turns non-OK response codes into typed errors.
//!
//! Session hygiene is part of the contract, because a camera whose session was abandoned
//! without `CloseSession` stalls the next `OpenSession` until it is reset:
//! - [`Session::scoped`] opens a session, runs a closure and closes the session on every exit
//!   path, including errors from the closure. Prefer it to calling `open` and `close` by hand
//! - [`Session::open`] recovers from a stalled or wedged camera: clear the halted endpoints,
//!   retry, then reset the device and retry once more, and only then give up
//! - [`Session::close`] sends `CloseSession`. If `CloseSession` fails, or the pipe state is
//!   unknown, it clears halts and resets the device instead, which also ends the session
//! - Any transport or framing failure during a transaction clears the halted endpoints, then
//!   resets the device, and returns the original error. The reset ends the session, so later
//!   transactions fail with [`SessionError::NotOpen`] until it is reopened. The transaction is
//!   not retried, since it may not be idempotent. A non-OK response is not a failure of the
//!   pipe and needs no recovery
//!
//! Timeouts: every transfer gets [`SessionConfig::timeout`], and the whole exchange (command,
//! data phases and response) must also finish within [`SessionConfig::transaction_timeout`],
//! measured on a monotonic clock ([`Instant`], no async runtime needed). Each transfer is
//! given `min(timeout, time left)`, so a camera that trickles bytes just inside the
//! per-transfer timeout cannot hold a transaction open forever. At the deadline the call fails
//! with [`SessionError::DeadlineExceeded`] and takes the normal failure path: clear halts, then
//! reset. [`Session::transaction_within`] overrides the deadline per call for large objects.
//! [`Session::with_clock`] injects a fake clock for deterministic tests. The deadline covers
//! only the exchange, and it is checked before each transfer: an exchange whose final transfer
//! completes is a success even if it ran over, because the camera has already acted on it. A
//! missed deadline is never retried, including in `open`. A cancelled or abandoned future is not bounded by it.
//!
//! Dropping a [`Session`] cannot send `CloseSession` (drop is not async), and cancelling a
//! [`Session::scoped`] future skips its close. Always run the future to completion. If a
//! transaction future is cancelled after its command was sent, the handle refuses further
//! transactions and `close` resets the device.
//!
//! This layer takes raw `u16` codes and `&[u32]` parameters and returns raw payload bytes.
//!
//! References:
//! - USB Still Image Capture Device Definition 1.0, sections 5-7
//! - PTP, ISO 15740, transaction and session semantics

use std::time::{Duration, Instant};

use thiserror::Error;

use super::{Container, ContainerError, ContainerHeader, ContainerKind, HEADER_LEN};
use crate::transport::{Endpoint, Transport, TransportError};

/// Operation and response codes this layer needs.
pub mod code {
    /// `GetDeviceInfo` operation.
    pub const GET_DEVICE_INFO: u16 = 0x1001;
    /// `OpenSession` operation.
    pub const OPEN_SESSION: u16 = 0x1002;
    /// `CloseSession` operation.
    pub const CLOSE_SESSION: u16 = 0x1003;
    /// Response: operation succeeded.
    pub const RESPONSE_OK: u16 = 0x2001;
    /// Response: unspecified failure.
    pub const GENERAL_ERROR: u16 = 0x2002;
    /// Response: the operation needs an open session.
    pub const SESSION_NOT_OPEN: u16 = 0x2003;
    /// Response: the transaction id is not the one the camera expects.
    pub const INVALID_TRANSACTION_ID: u16 = 0x2004;
    /// Response: the camera does not implement the operation.
    pub const OPERATION_NOT_SUPPORTED: u16 = 0x2005;
    /// Response: the camera is busy; try again later.
    pub const DEVICE_BUSY: u16 = 0x2019;
    /// Response: a parameter is invalid.
    pub const INVALID_PARAMETER: u16 = 0x201D;
    /// Response: a session is already open.
    pub const SESSION_ALREADY_OPEN: u16 = 0x201E;
}

/// Zero-length reads tolerated while waiting for a container header.
const MAX_EMPTY_READS: usize = 8;

/// Largest event container: header plus five parameters, with slack.
const EVENT_BUFFER_LEN: usize = 64;

/// Upfront capacity cap for a data phase, so a hostile length cannot force a large allocation.
const MAX_PREALLOC: usize = 1 << 20;

/// Why a session operation failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SessionError {
    /// The transport failed.
    #[error("transport failure: {0}")]
    Transport(#[from] TransportError),
    /// A container could not be encoded or decoded.
    #[error("malformed container: {0}")]
    Container(#[from] ContainerError),
    /// The camera answered with a response code other than `OK`.
    #[error("camera responded '{code:#06x}'")]
    Response {
        /// The response code.
        code: u16,
        /// The response parameters.
        params: Vec<u32>,
    },
    /// A container of the wrong kind arrived.
    #[error("expected {expected}, got a '{actual:?}' container")]
    UnexpectedContainer {
        /// What the protocol allows at this point.
        expected: &'static str,
        /// The kind that arrived.
        actual: ContainerKind,
    },
    /// A container belongs to a different transaction.
    #[error("expected transaction '{expected}', got '{actual}'")]
    TransactionMismatch {
        /// The transaction in flight.
        expected: u32,
        /// The transaction id the camera sent.
        actual: u32,
    },
    /// A container announced an unknown length (`0xFFFF_FFFF`), which this engine does not
    /// support. The announced length is the only way to find the end of a container, because
    /// the camera's transfer boundaries are arbitrary, so it fails rather than guess.
    #[error("container announces an unknown length")]
    UnsupportedLength,
    /// The transaction did not finish within its deadline, even though each transfer may
    /// have made progress within its own timeout.
    #[error("transaction exceeded its deadline of '{0:?}'")]
    DeadlineExceeded(Duration),
    /// A data phase announced more bytes than the configured limit.
    #[error("data phase of '{announced}' bytes exceeds the limit of '{limit}'")]
    DataTooLarge {
        /// Announced container length.
        announced: usize,
        /// Configured limit.
        limit: usize,
    },
    /// The camera kept sending zero-length packets instead of a container.
    #[error("camera sent only zero-length packets")]
    TooManyEmptyReads,
    /// The operation needs an open session.
    #[error("no session is open")]
    NotOpen,
    /// A session is already open on this handle.
    #[error("a session is already open")]
    AlreadyOpen,
    /// Clearing halts and resetting the device did not revive the camera.
    #[error("camera did not recover after clear-halt and reset: {0}")]
    Unrecoverable(TransportError),
}

/// Tunables for a [`Session`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    /// Timeout applied to every transfer.
    pub timeout: Duration,
    /// Bulk endpoint packet size: 512 on USB 2, 1024 on USB 3.
    pub max_packet_size: usize,
    /// Size of one bulk transfer. Rounded down to a multiple of `max_packet_size`.
    pub transfer_size: usize,
    /// Largest data phase accepted from the camera, in bytes.
    pub max_data_len: usize,
    /// Session id sent in `OpenSession`.
    pub session_id: u32,
    /// Deadline for a whole transaction (or `OpenSession` / `CloseSession`), from the first
    /// byte sent to the last byte of the response. See the module docs.
    pub transaction_timeout: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            max_packet_size: 512,
            transfer_size: 16 * 1024,
            max_data_len: 64 * 1024 * 1024,
            session_id: 1,
            transaction_timeout: Duration::from_secs(60),
        }
    }
}

impl SessionConfig {
    fn packet_size(&self) -> usize {
        self.max_packet_size.max(1)
    }

    /// Transfer size as a whole number of packets, at least one.
    fn chunk_size(&self) -> usize {
        let packet = self.packet_size();
        (self.transfer_size / packet).max(1).saturating_mul(packet)
    }
}

/// A successfully completed transaction.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outcome {
    /// Payload of the device-to-host data phase. Empty when there was none.
    pub data: Vec<u8>,
    /// Parameters of the `OK` response.
    pub params: Vec<u32>,
}

/// An asynchronous event from the interrupt pipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Event code.
    pub code: u16,
    /// Transaction the event relates to, or `0xFFFF_FFFF` for none.
    pub transaction_id: u32,
    /// Event parameters.
    pub params: Vec<u32>,
}

/// A PTP session handle. See the module docs for the hygiene contract.
#[derive(Debug)]
pub struct Session<T> {
    transport: T,
    config: SessionConfig,
    next_transaction: u32,
    open: bool,
    /// The pipe may hold stale data or a halt after an error, so `close` must reset.
    dirty: bool,
    /// A command was sent and its response not yet fully read, so a retry could read it.
    in_flight: bool,
    buffer: Vec<u8>,
    /// Bytes read past the end of the container being assembled (e.g. a coalesced response).
    leftover: Vec<u8>,
    clock: fn() -> Instant,
    deadline: Option<Deadline>,
}

/// The deadline of the exchange in progress.
#[derive(Debug, Clone, Copy)]
struct Deadline {
    at: Option<Instant>,
    limit: Duration,
}

impl<T: Transport> Session<T> {
    /// Wraps `transport`. No I/O happens until [`Session::open`].
    #[must_use]
    pub fn new(transport: T, config: SessionConfig) -> Self {
        let buffer = vec![0; config.chunk_size()];
        Self {
            transport,
            config,
            next_transaction: 0,
            open: false,
            dirty: false,
            in_flight: false,
            buffer,
            leftover: Vec::new(),
            clock: Instant::now,
            deadline: None,
        }
    }

    /// Replaces the monotonic clock used for transaction deadlines. Meant for deterministic
    /// tests; the default is [`Instant::now`].
    #[must_use]
    pub fn with_clock(mut self, clock: fn() -> Instant) -> Self {
        self.clock = clock;
        self
    }

    /// Borrows the transport, e.g. to inspect a simulated camera.
    #[must_use]
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Gives the transport back. Abandons any open session on the camera: close it first.
    #[must_use]
    pub fn into_transport(self) -> T {
        self.transport
    }

    /// Whether a session is open on this handle.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Opens the session, recovering a stalled or wedged camera.
    ///
    /// Recovery order: clear the halted bulk endpoints and retry, reset the device and retry,
    /// then fail. A camera that still holds a session answers `SessionAlreadyOpen`; the device is
    /// reset to end that stale session and the open retried.
    ///
    /// # Errors
    ///
    /// [`SessionError::AlreadyOpen`] when this handle is open, [`SessionError::Unrecoverable`]
    /// when the camera stays stalled after a reset, and any other [`SessionError`] from the
    /// open transaction. A disconnect is returned at once.
    pub async fn open(&mut self) -> Result<(), SessionError> {
        if self.open {
            return Err(SessionError::AlreadyOpen);
        }
        let result = self.open_with_recovery().await;
        if let Err(err) = &result
            && !matches!(err, SessionError::Transport(TransportError::Disconnected))
        {
            // The camera may have accepted `OpenSession` before the failure (e.g. a malformed
            // response), and nothing else would close that session. Recovery is best effort.
            let _ = self.clear_halts_then_reset().await;
        }
        result
    }

    async fn open_with_recovery(&mut self) -> Result<(), SessionError> {
        if let Err(err) = self.try_open().await {
            recoverable(err)?;
        } else {
            return Ok(());
        }
        // A response may still be queued from the failed attempt: a retry would mistake it
        // for its own, so only a reset is safe then.
        if self.in_flight {
            self.clear_halts_then_reset().await?;
        } else if self.clear_bulk_halts().await.is_err() {
            self.reset_device().await?;
        }
        if let Err(err) = self.try_open().await {
            recoverable(err)?;
        } else {
            return Ok(());
        }
        self.clear_halts_then_reset().await?;
        self.try_open().await.map_err(|err| match recoverable(err) {
            Ok(cause) => SessionError::Unrecoverable(cause),
            Err(other) => other,
        })
    }

    /// Closes the session. Does nothing when none is open.
    ///
    /// Falls back to a device reset when the pipe state is unknown or `CloseSession` itself
    /// stalls; a reset ends the session as well.
    ///
    /// # Errors
    ///
    /// Returns the transport error when the camera is gone or the reset fails. The handle is
    /// marked closed either way.
    pub async fn close(&mut self) -> Result<(), SessionError> {
        if !self.open {
            return Ok(());
        }
        let result = self.close_inner().await;
        self.open = false;
        result
    }

    /// Opens a session, runs `body`, and closes the session on every exit path.
    ///
    /// The body's error wins over a close error; a close error after a successful body is
    /// returned.
    ///
    /// # Errors
    ///
    /// Any error from [`Session::open`], `body` or [`Session::close`].
    pub async fn scoped<R>(
        &mut self, body: impl AsyncFnOnce(&mut Self) -> Result<R, SessionError>,
    ) -> Result<R, SessionError> {
        self.open().await?;
        let result = body(self).await;
        let closed = self.close().await;
        match result {
            Ok(value) => closed.map(|()| value),
            Err(err) => Err(err),
        }
    }

    /// Runs one transaction: `operation` with `params`, an optional host-to-device data phase
    /// `data_out`, then any device-to-host data phase and the response.
    ///
    /// # Errors
    ///
    /// [`SessionError::NotOpen`] without an open session, [`SessionError::Response`] for a
    /// non-OK response code, and transport or framing errors otherwise. After a stall or
    /// timeout the halted endpoints are cleared before the error is returned.
    pub async fn transaction(
        &mut self, operation: u16, params: &[u32], data_out: Option<&[u8]>,
    ) -> Result<Outcome, SessionError> {
        let limit = self.config.transaction_timeout;
        self.transaction_within(limit, operation, params, data_out)
            .await
    }

    /// As [`Session::transaction`], with `limit` as the whole-transaction deadline instead of
    /// [`SessionConfig::transaction_timeout`]. Use a longer limit for operations that
    /// legitimately move large objects.
    ///
    /// # Errors
    ///
    /// As [`Session::transaction`], plus [`SessionError::DeadlineExceeded`].
    pub async fn transaction_within(
        &mut self, limit: Duration, operation: u16, params: &[u32], data_out: Option<&[u8]>,
    ) -> Result<Outcome, SessionError> {
        // A dirty handle failed recovery: the pipe state is unknown until `close` resets it.
        if !self.open || self.dirty || self.in_flight {
            return Err(SessionError::NotOpen);
        }
        let transaction_id = self.take_transaction_id();
        let result = self
            .exchange(operation, params, data_out, (transaction_id, limit))
            .await;
        self.settle(result).await
    }

    /// Fetches the raw `DeviceInfo` dataset.
    ///
    /// # Errors
    ///
    /// As [`Session::transaction`].
    pub async fn get_device_info(&mut self) -> Result<Vec<u8>, SessionError> {
        Ok(self
            .transaction(code::GET_DEVICE_INFO, &[], None)
            .await?
            .data)
    }

    /// Polls the interrupt pipe once. `Ok(None)` means no event was pending.
    ///
    /// # Errors
    ///
    /// Transport errors other than a timeout, or a malformed or non-event container. A stall
    /// clears the interrupt endpoint's halt before returning.
    pub async fn poll_event(&mut self) -> Result<Option<Event>, SessionError> {
        let mut bytes = [0_u8; EVENT_BUFFER_LEN];
        let read = self
            .transport
            .interrupt_in(&mut bytes, self.config.timeout)
            .await;
        let len = match read {
            Ok(0) | Err(TransportError::Timeout) => return Ok(None),
            Ok(len) => len,
            Err(TransportError::Stall) => {
                self.transport
                    .clear_halt(Endpoint::InterruptIn, self.config.timeout)
                    .await?;
                return Err(TransportError::Stall.into());
            }
            Err(err) => return Err(err.into()),
        };
        let (container, _) = Container::decode(bytes.get(..len).unwrap_or_default())?;
        if container.kind != ContainerKind::Event {
            return Err(SessionError::UnexpectedContainer {
                expected: "an event",
                actual: container.kind,
            });
        }
        Ok(Some(Event {
            code: container.code,
            transaction_id: container.transaction_id,
            params: container.params().collect(),
        }))
    }

    fn take_transaction_id(&mut self) -> u32 {
        let id = self.next_transaction;
        // 0 is reserved for `OpenSession` and 0xFFFF_FFFF is reserved, so wrap to 1.
        self.next_transaction = match id.checked_add(1) {
            Some(next) if next != u32::MAX => next,
            _ => 1,
        };
        id
    }

    /// One `OpenSession` attempt, resetting away a stale session the camera reports.
    async fn try_open(&mut self) -> Result<(), SessionError> {
        let params = [self.config.session_id];
        let limit = self.config.transaction_timeout;
        let mut result = self
            .exchange(code::OPEN_SESSION, &params, None, (0, limit))
            .await
            .map(|_| ());
        if matches!(
            result,
            Err(SessionError::Response {
                code: code::SESSION_ALREADY_OPEN,
                ..
            })
        ) {
            // The stale session's transaction counter is unknown, so no `CloseSession` id can
            // be right. A reset ends it.
            self.reset_device().await?;
            result = self
                .exchange(code::OPEN_SESSION, &params, None, (0, limit))
                .await
                .map(|_| ());
        }
        if result.is_ok() {
            self.open = true;
            self.dirty = false;
            self.next_transaction = 1;
        }
        result
    }

    async fn close_inner(&mut self) -> Result<(), SessionError> {
        if self.dirty || self.in_flight {
            return self.clear_halts_then_reset().await;
        }
        let transaction_id = self.take_transaction_id();
        match self
            .exchange(
                code::CLOSE_SESSION,
                &[],
                None,
                (transaction_id, self.config.transaction_timeout),
            )
            .await
        {
            Ok(_)
            | Err(SessionError::Response {
                code: code::SESSION_NOT_OPEN,
                ..
            }) => Ok(()),
            Err(SessionError::Transport(TransportError::Disconnected)) => {
                Err(TransportError::Disconnected.into())
            }
            Err(err @ SessionError::DeadlineExceeded(_)) => {
                // Best effort: the deadline is the error the caller needs to see.
                let _ = self.clear_halts_then_reset().await;
                Err(err)
            }
            Err(_) => self.clear_halts_then_reset().await,
        }
    }

    /// Clears the bulk halts (best effort), then resets the device.
    async fn clear_halts_then_reset(&mut self) -> Result<(), SessionError> {
        let _ = self.clear_bulk_halts().await;
        self.reset_device().await
    }

    /// Records the outcome of an exchange and clears halts after a stall or timeout.
    async fn settle(
        &mut self, result: Result<Outcome, SessionError>,
    ) -> Result<Outcome, SessionError> {
        let Err(err) = &result else {
            return result;
        };
        if !matches!(err, SessionError::Response { .. }) {
            // The pipe state is unknown. If the reset fails (e.g. a disconnect) the handle
            // stays dirty and open so `close` retries and reports it.
            self.dirty = true;
            let _ = self.clear_halts_then_reset().await;
        }
        result
    }

    async fn clear_bulk_halts(&mut self) -> Result<(), TransportError> {
        let timeout = self.config.timeout;
        self.transport
            .clear_halt(Endpoint::BulkOut, timeout)
            .await?;
        self.transport.clear_halt(Endpoint::BulkIn, timeout).await
    }

    /// Resets the device. Success ends any session, so the handle is left closed and clean.
    async fn reset_device(&mut self) -> Result<(), SessionError> {
        self.transport.reset(self.config.timeout).await?;
        self.open = false;
        self.dirty = false;
        self.in_flight = false;
        self.leftover.clear();
        Ok(())
    }

    async fn send(&mut self, bytes: &[u8]) -> Result<(), SessionError> {
        let timeout = self.transfer_timeout()?;
        self.transport
            .bulk_out(bytes, timeout)
            .await
            .map_err(|err| self.classify(err))
    }

    async fn exchange(
        &mut self, operation: u16, params: &[u32], data_out: Option<&[u8]>,
        (transaction_id, limit): (u32, Duration),
    ) -> Result<Outcome, SessionError> {
        self.deadline = Some(Deadline {
            at: (self.clock)().checked_add(limit),
            limit,
        });
        let result = self
            .exchange_phases(operation, params, data_out, transaction_id)
            .await;
        self.deadline = None;
        result
    }

    /// Timeout for the next transfer: the per-transfer timeout, capped by what is left of the
    /// exchange's deadline.
    fn transfer_timeout(&self) -> Result<Duration, SessionError> {
        let Some(Deadline {
            at: Some(at),
            limit,
        }) = self.deadline
        else {
            return Ok(self.config.timeout);
        };
        let remaining = at.saturating_duration_since((self.clock)());
        if remaining.is_zero() {
            return Err(SessionError::DeadlineExceeded(limit));
        }
        Ok(self.config.timeout.min(remaining))
    }

    /// A timeout that used up the exchange's remaining budget is a missed deadline.
    fn classify(&self, err: TransportError) -> SessionError {
        if err == TransportError::Timeout
            && let Some(Deadline {
                at: Some(at),
                limit,
            }) = self.deadline
            && (self.clock)() >= at
        {
            return SessionError::DeadlineExceeded(limit);
        }
        err.into()
    }

    async fn exchange_phases(
        &mut self, operation: u16, params: &[u32], data_out: Option<&[u8]>, transaction_id: u32,
    ) -> Result<Outcome, SessionError> {
        let command = Container::command(operation, transaction_id, params)?;
        self.leftover.clear();
        // A timeout or disconnect does not prove the camera saw nothing, so assume it did.
        // Only a stall guarantees the command was not accepted.
        self.in_flight = true;
        if let Err(err) = self.send(&command).await {
            self.in_flight = !matches!(err, SessionError::Transport(TransportError::Stall));
            return Err(err);
        }
        if let Some(data) = data_out
            && let Err(err) = self.send_data(operation, transaction_id, data).await
        {
            return Err(self.early_response(transaction_id).await.unwrap_or(err));
        }
        let mut data: Option<Vec<u8>> = None;
        loop {
            let (header, payload) = self.read_container().await?;
            if header.transaction_id != transaction_id {
                return Err(SessionError::TransactionMismatch {
                    expected: transaction_id,
                    actual: header.transaction_id,
                });
            }
            match header.kind {
                ContainerKind::Data
                    if data.is_none() && data_out.is_none() && header.code == operation =>
                {
                    data = Some(payload);
                }
                ContainerKind::Response => {
                    self.in_flight = false;
                    let params = Container {
                        kind: header.kind,
                        code: header.code,
                        transaction_id,
                        payload: &payload,
                    }
                    .params()
                    .collect();
                    return if header.code == code::RESPONSE_OK {
                        Ok(Outcome {
                            data: data.unwrap_or_default(),
                            params,
                        })
                    } else {
                        Err(SessionError::Response {
                            code: header.code,
                            params,
                        })
                    };
                }
                actual => {
                    return Err(SessionError::UnexpectedContainer {
                        expected: "a data or response container",
                        actual,
                    });
                }
            }
        }
    }

    /// After a rejected data-out phase, recovers the camera's non-OK response, if it sent one,
    /// so the caller sees the typed error instead of the stall it caused.
    async fn early_response(&mut self, transaction_id: u32) -> Option<SessionError> {
        let (header, payload) = self.read_container().await.ok()?;
        if header.kind != ContainerKind::Response
            || header.transaction_id != transaction_id
            || header.code == code::RESPONSE_OK
        {
            return None;
        }
        let params = Container {
            kind: header.kind,
            code: header.code,
            transaction_id,
            payload: &payload,
        }
        .params()
        .collect();
        self.in_flight = false;
        // The rejected data container left the bulk-out endpoint halted. If it cannot be
        // cleared, fall back to a reset, which ends the session.
        if self.clear_bulk_halts().await.is_err() {
            self.dirty = true;
            let _ = self.reset_device().await;
        }
        Some(SessionError::Response {
            code: header.code,
            params,
        })
    }

    /// Sends a data container in whole-packet transfers, then a zero-length packet when the
    /// container ends exactly on a packet boundary.
    async fn send_data(
        &mut self, operation: u16, transaction_id: u32, data: &[u8],
    ) -> Result<(), SessionError> {
        let mut wire = Vec::new();
        Container {
            kind: ContainerKind::Data,
            code: operation,
            transaction_id,
            payload: data,
        }
        .encode_into(&mut wire)?;
        for chunk in wire.chunks(self.config.chunk_size()) {
            self.send(chunk).await?;
        }
        if wire.len() % self.config.packet_size() == 0 {
            self.send(&[]).await?;
        }
        Ok(())
    }

    /// Reads one non-empty bulk transfer into the scratch buffer, skipping zero-length packets.
    async fn read_transfer(&mut self) -> Result<usize, SessionError> {
        for _ in 0..MAX_EMPTY_READS {
            let timeout = self.transfer_timeout()?;
            let len = match self.transport.bulk_in(&mut self.buffer, timeout).await {
                Ok(len) => len,
                Err(err) => return Err(self.classify(err)),
            };
            if len > 0 {
                return Ok(len);
            }
        }
        Err(SessionError::TooManyEmptyReads)
    }

    /// Appends exactly `need` more bytes to `out`, from `leftover` first, then from new
    /// transfers. Surplus bytes of the last transfer go back to `leftover`.
    async fn pull(&mut self, out: &mut Vec<u8>, need: usize) -> Result<(), SessionError> {
        let target = out.len().saturating_add(need);
        let reuse = self.leftover.len().min(need);
        out.extend(self.leftover.drain(..reuse));
        while out.len() < target {
            let len = self.read_transfer().await?;
            let chunk = self.buffer.get(..len).unwrap_or_default();
            let (now, later) = chunk.split_at(chunk.len().min(target - out.len()));
            out.extend_from_slice(now);
            self.leftover.extend_from_slice(later);
        }
        Ok(())
    }

    /// Reads one whole container.
    ///
    /// The header's `length` field is the only authority on where a container ends. Transfer
    /// boundaries are arbitrary: the header may arrive alone, the payload may span several
    /// transfers, and the next container may share a transfer with the tail of this one. A
    /// short packet therefore proves nothing; a camera that stops delivering surfaces as a
    /// transport timeout. A container announcing the unknown length `0xFFFF_FFFF` is rejected
    /// with [`SessionError::UnsupportedLength`].
    async fn read_container(&mut self) -> Result<(ContainerHeader, Vec<u8>), SessionError> {
        let mut head = Vec::with_capacity(HEADER_LEN);
        self.pull(&mut head, HEADER_LEN).await?;
        let header = ContainerHeader::decode(&head)?;
        if header.length == u32::MAX {
            return Err(SessionError::UnsupportedLength);
        }
        let wanted = header.payload_len();
        let limit = self.config.max_data_len;
        if wanted > limit {
            return Err(SessionError::DataTooLarge {
                announced: wanted.saturating_add(HEADER_LEN),
                limit,
            });
        }
        let mut payload = Vec::with_capacity(wanted.min(MAX_PREALLOC));
        self.pull(&mut payload, wanted).await?;
        Ok((header, payload))
    }
}

/// Returns the cause of a recoverable failure (stall or timeout), and anything else (a
/// disconnect, a malformed container, a missed deadline, a non-OK response) as the fatal error
/// it is.
fn recoverable(err: SessionError) -> Result<TransportError, SessionError> {
    match err {
        SessionError::Transport(cause @ (TransportError::Stall | TransportError::Timeout)) => {
            Ok(cause)
        }
        other => Err(other),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::transport::sim::{Fault, Reply, SimCamera};

    const GET_OBJECT: u16 = 0x1009;
    const SEND_OBJECT: u16 = 0x100D;

    fn session(camera: SimCamera) -> Session<SimCamera> {
        Session::new(camera, SessionConfig::default())
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect()
    }

    fn ids(session: &Session<SimCamera>) -> Vec<(u16, u32)> {
        session
            .transport()
            .commands()
            .iter()
            .map(|c| (c.code, c.transaction_id))
            .collect()
    }

    #[tokio::test]
    async fn open_close_sequences_transaction_ids() {
        let camera = SimCamera::new().with_operation(0x9001, Reply::ok().with_params(&[7]));
        let mut s = session(camera);
        s.open().await.unwrap();
        assert!(s.is_open());
        assert_eq!(s.transaction(0x9001, &[], None).await.unwrap().params, [7]);
        s.transaction(0x9001, &[], None).await.unwrap();
        s.close().await.unwrap();
        assert!(!s.is_open());
        assert!(!s.transport().session_open());
        assert_eq!(
            ids(&s),
            [
                (code::OPEN_SESSION, 0),
                (0x9001, 1),
                (0x9001, 2),
                (code::CLOSE_SESSION, 3)
            ]
        );
        assert_eq!(s.transport().commands()[0].params, [1]);
    }

    #[tokio::test]
    async fn transaction_ids_wrap_past_the_reserved_value() {
        let mut s = session(SimCamera::new());
        s.next_transaction = u32::MAX - 1;
        assert_eq!(s.take_transaction_id(), u32::MAX - 1);
        assert_eq!(s.take_transaction_id(), 1);
    }

    #[tokio::test]
    async fn transaction_requires_an_open_session() {
        let mut s = session(SimCamera::new());
        assert_eq!(
            s.transaction(0x9001, &[], None).await,
            Err(SessionError::NotOpen)
        );
        s.open().await.unwrap();
        assert_eq!(s.open().await, Err(SessionError::AlreadyOpen));
    }

    #[tokio::test]
    async fn get_device_info_returns_the_dataset() {
        let mut s = session(SimCamera::new().with_device_info(payload(300)));
        s.open().await.unwrap();
        assert_eq!(s.get_device_info().await.unwrap(), payload(300));
    }

    #[tokio::test]
    async fn non_ok_response_is_a_typed_error() {
        let mut s = session(SimCamera::new());
        s.open().await.unwrap();
        assert_eq!(
            s.transaction(0x9999, &[], None).await,
            Err(SessionError::Response {
                code: code::OPERATION_NOT_SUPPORTED,
                params: vec![]
            })
        );
        s.close().await.unwrap();
        assert_eq!(
            s.transport().reset_count(),
            0,
            "a clean error needs no reset"
        );
    }

    #[tokio::test]
    async fn data_in_larger_than_one_transfer() {
        let data = payload(48 * 1024);
        let camera = SimCamera::new().with_operation(GET_OBJECT, Reply::data(data.clone()));
        let mut s = session(camera);
        s.open().await.unwrap();
        let outcome = s.transaction(GET_OBJECT, &[1], None).await.unwrap();
        assert_eq!(outcome.data, data);
    }

    #[tokio::test]
    async fn data_in_ending_on_a_packet_boundary_consumes_the_zlp() {
        for packet in [512, 1024] {
            let data = payload(packet * 4 - HEADER_LEN);
            let camera = SimCamera::new()
                .with_max_packet_size(packet)
                .with_operation(GET_OBJECT, Reply::data(data.clone()));
            let config = SessionConfig {
                max_packet_size: packet,
                transfer_size: packet * 2,
                ..SessionConfig::default()
            };
            let mut s = Session::new(camera, config);
            s.open().await.unwrap();
            assert_eq!(
                s.transaction(GET_OBJECT, &[1], None).await.unwrap().data,
                data
            );
            s.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn data_out_is_chunked_and_zlp_terminated() {
        let data = payload(1024 * 2 - HEADER_LEN);
        let camera = SimCamera::new().with_operation(SEND_OBJECT, Reply::accepting_data());
        let config = SessionConfig {
            transfer_size: 1024,
            ..SessionConfig::default()
        };
        let mut s = Session::new(camera, config);
        s.open().await.unwrap();
        s.transaction(SEND_OBJECT, &[], Some(&data)).await.unwrap();
        assert_eq!(s.transport().received_data(), [data]);
        let sizes = s.transport().bulk_out_transfers();
        assert_eq!(&sizes[sizes.len() - 3..], [1024, 1024, 0]);
    }

    #[tokio::test]
    async fn data_out_off_boundary_sends_no_zlp() {
        let camera = SimCamera::new().with_operation(SEND_OBJECT, Reply::accepting_data());
        let mut s = session(camera);
        s.open().await.unwrap();
        s.transaction(SEND_OBJECT, &[], Some(&payload(100)))
            .await
            .unwrap();
        assert_eq!(s.transport().bulk_out_transfers().last(), Some(&112));
    }

    #[tokio::test]
    async fn rejected_data_out_keeps_the_typed_response() {
        let mut s = session(SimCamera::new());
        s.open().await.unwrap();
        assert_eq!(
            s.transaction(0x9999, &[], Some(&[1, 2, 3])).await,
            Err(SessionError::Response {
                code: code::OPERATION_NOT_SUPPORTED,
                params: vec![]
            })
        );
        assert!(s.is_open());
        s.transaction(code::GET_DEVICE_INFO, &[], None)
            .await
            .unwrap();
    }

    /// Delegates to a `SimCamera` but fails `clear_halt` once.
    struct FlakyClear {
        inner: SimCamera,
        failures: usize,
        reset_fails: bool,
    }

    impl Transport for FlakyClear {
        fn bulk_out(
            &mut self, bytes: &[u8], timeout: Duration,
        ) -> impl Future<Output = Result<(), TransportError>> + Send {
            self.inner.bulk_out(bytes, timeout)
        }

        fn bulk_in(
            &mut self, bytes: &mut [u8], timeout: Duration,
        ) -> impl Future<Output = Result<usize, TransportError>> + Send {
            self.inner.bulk_in(bytes, timeout)
        }

        fn interrupt_in(
            &mut self, bytes: &mut [u8], timeout: Duration,
        ) -> impl Future<Output = Result<usize, TransportError>> + Send {
            self.inner.interrupt_in(bytes, timeout)
        }

        async fn clear_halt(
            &mut self, endpoint: Endpoint, timeout: Duration,
        ) -> Result<(), TransportError> {
            if self.failures > 0 {
                self.failures -= 1;
                return Err(TransportError::Other("injected".into()));
            }
            self.inner.clear_halt(endpoint, timeout).await
        }

        async fn reset(&mut self, timeout: Duration) -> Result<(), TransportError> {
            if self.reset_fails {
                return Err(TransportError::Other("reset".into()));
            }
            self.inner.reset(timeout).await
        }
    }

    thread_local! {
        /// Fake time elapsed on this test thread.
        static ELAPSED: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
    }

    fn fake_now() -> Instant {
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *BASE.get_or_init(Instant::now) + ELAPSED.get()
    }

    /// Delivers one byte per read and advances the fake clock by `step` per read, so every
    /// read is within the per-transfer timeout while the transaction as a whole crawls.
    struct Trickle {
        inner: SimCamera,
        step: Duration,
        active: bool,
        timeouts: Vec<Duration>,
    }

    impl Transport for Trickle {
        fn bulk_out(
            &mut self, bytes: &[u8], timeout: Duration,
        ) -> impl Future<Output = Result<(), TransportError>> + Send {
            self.inner.bulk_out(bytes, timeout)
        }

        async fn bulk_in(
            &mut self, bytes: &mut [u8], timeout: Duration,
        ) -> Result<usize, TransportError> {
            if !self.active {
                return self.inner.bulk_in(bytes, timeout).await;
            }
            self.timeouts.push(timeout);
            ELAPSED.set(ELAPSED.get() + self.step);
            let one = bytes.get_mut(..1).unwrap_or_default();
            self.inner.bulk_in(one, timeout).await
        }

        fn interrupt_in(
            &mut self, bytes: &mut [u8], timeout: Duration,
        ) -> impl Future<Output = Result<usize, TransportError>> + Send {
            self.inner.interrupt_in(bytes, timeout)
        }

        fn clear_halt(
            &mut self, endpoint: Endpoint, timeout: Duration,
        ) -> impl Future<Output = Result<(), TransportError>> + Send {
            self.inner.clear_halt(endpoint, timeout)
        }

        fn reset(
            &mut self, timeout: Duration,
        ) -> impl Future<Output = Result<(), TransportError>> + Send {
            self.inner.reset(timeout)
        }
    }

    fn trickle_session() -> Session<Trickle> {
        ELAPSED.set(Duration::ZERO);
        let camera = SimCamera::new().with_operation(GET_OBJECT, Reply::data(payload(100)));
        let config = SessionConfig {
            timeout: Duration::from_secs(5),
            transaction_timeout: Duration::from_secs(60),
            ..SessionConfig::default()
        };
        Session::new(
            Trickle {
                inner: camera,
                step: Duration::from_secs(4),
                active: false,
                timeouts: Vec::new(),
            },
            config,
        )
        .with_clock(fake_now)
    }

    #[tokio::test]
    async fn trickling_device_hits_the_transaction_deadline_and_session_recovers() {
        let mut s = trickle_session();
        s.open().await.unwrap();
        s.transport.active = true;
        assert_eq!(
            s.transaction(GET_OBJECT, &[1], None).await,
            Err(SessionError::DeadlineExceeded(Duration::from_secs(60)))
        );
        let timeouts = &s.transport().timeouts;
        assert_eq!(
            timeouts.len(),
            15,
            "stops at the deadline, not at 112 reads"
        );
        assert!(timeouts.iter().all(|t| *t <= Duration::from_secs(5)));
        assert_eq!(
            timeouts.last(),
            Some(&Duration::from_secs(4)),
            "capped by time left"
        );
        // Normal failure path: reset ends the session; reopening works.
        assert!(!s.is_open());
        assert_eq!(s.transport().inner.reset_count(), 1);
        s.transport.active = false;
        s.open().await.unwrap();
        assert_eq!(
            s.transaction(GET_OBJECT, &[1], None).await.unwrap().data,
            payload(100)
        );
    }

    /// Spends its whole timeout on every read, then reports a timeout.
    struct Stalling {
        inner: SimCamera,
        step: Duration,
    }

    impl Transport for Stalling {
        fn bulk_out(
            &mut self, bytes: &[u8], timeout: Duration,
        ) -> impl Future<Output = Result<(), TransportError>> + Send {
            self.inner.bulk_out(bytes, timeout)
        }

        fn bulk_in(
            &mut self, _bytes: &mut [u8], timeout: Duration,
        ) -> impl Future<Output = Result<usize, TransportError>> + Send {
            ELAPSED.set(ELAPSED.get() + timeout.min(self.step));
            std::future::ready(Err(TransportError::Timeout))
        }

        fn interrupt_in(
            &mut self, bytes: &mut [u8], timeout: Duration,
        ) -> impl Future<Output = Result<usize, TransportError>> + Send {
            self.inner.interrupt_in(bytes, timeout)
        }

        fn clear_halt(
            &mut self, endpoint: Endpoint, timeout: Duration,
        ) -> impl Future<Output = Result<(), TransportError>> + Send {
            self.inner.clear_halt(endpoint, timeout)
        }

        fn reset(
            &mut self, timeout: Duration,
        ) -> impl Future<Output = Result<(), TransportError>> + Send {
            self.inner.reset(timeout)
        }
    }

    #[tokio::test]
    async fn a_read_timeout_that_spends_the_budget_is_a_missed_deadline() {
        ELAPSED.set(Duration::ZERO);
        let config = SessionConfig {
            timeout: Duration::from_secs(5),
            transaction_timeout: Duration::from_secs(3),
            ..SessionConfig::default()
        };
        let mut s = Session::new(
            Stalling {
                inner: SimCamera::new(),
                step: Duration::from_secs(100),
            },
            config,
        )
        .with_clock(fake_now);
        // The clamped timeout (3 s) is spent in full, so the timeout is the deadline.
        assert_eq!(
            s.open().await,
            Err(SessionError::DeadlineExceeded(Duration::from_secs(3)))
        );
    }

    #[tokio::test]
    async fn expired_deadline_is_kept_through_open_and_close_recovery() {
        let config = SessionConfig {
            transaction_timeout: Duration::ZERO,
            ..SessionConfig::default()
        };
        let mut s = Session::new(SimCamera::new(), config);
        assert_eq!(
            s.open().await,
            Err(SessionError::DeadlineExceeded(Duration::ZERO))
        );
        assert!(!s.is_open());

        let mut s = session(SimCamera::new());
        s.open().await.unwrap();
        s.config.transaction_timeout = Duration::ZERO;
        assert_eq!(
            s.close().await,
            Err(SessionError::DeadlineExceeded(Duration::ZERO))
        );
        assert!(!s.is_open());
        assert!(!s.transport().session_open(), "the reset ended the session");
    }

    #[tokio::test]
    async fn expired_open_is_not_retried() {
        let mut s = trickle_session();
        s.config.transaction_timeout = Duration::from_secs(3);
        s.transport.active = true;
        assert_eq!(
            s.open().await,
            Err(SessionError::DeadlineExceeded(Duration::from_secs(3)))
        );
        let opens = s
            .transport()
            .inner
            .commands()
            .iter()
            .filter(|c| c.code == code::OPEN_SESSION)
            .count();
        assert_eq!(opens, 1, "a fresh budget must not turn expiry into success");
        assert_eq!(s.transport().inner.reset_count(), 1);
    }

    #[tokio::test]
    async fn close_keeps_the_deadline_error_when_reset_fails() {
        let mut s = Session::new(
            FlakyClear {
                inner: SimCamera::new(),
                failures: 0,
                reset_fails: false,
            },
            SessionConfig::default(),
        );
        s.open().await.unwrap();
        s.transport.reset_fails = true;
        s.config.transaction_timeout = Duration::ZERO;
        assert_eq!(
            s.close().await,
            Err(SessionError::DeadlineExceeded(Duration::ZERO))
        );
        assert!(!s.is_open());
    }

    #[tokio::test]
    async fn per_call_override_allows_a_slow_large_transfer() {
        let mut s = trickle_session();
        s.open().await.unwrap();
        s.transport.active = true;
        let outcome = s
            .transaction_within(Duration::from_secs(1000), GET_OBJECT, &[1], None)
            .await
            .unwrap();
        assert_eq!(outcome.data, payload(100));
    }

    #[tokio::test]
    async fn a_zero_deadline_fails_before_any_transfer() {
        let mut s = session(SimCamera::new());
        s.open().await.unwrap();
        let sent = s.transport().bulk_out_transfers().len();
        assert_eq!(
            s.transaction_within(Duration::ZERO, code::GET_DEVICE_INFO, &[], None)
                .await,
            Err(SessionError::DeadlineExceeded(Duration::ZERO))
        );
        assert_eq!(s.transport().bulk_out_transfers().len(), sent);
    }

    #[tokio::test]
    async fn rejected_data_out_resets_when_the_halt_cannot_be_cleared() {
        let mut s = Session::new(
            FlakyClear {
                inner: SimCamera::new(),
                failures: 1,
                reset_fails: false,
            },
            SessionConfig::default(),
        );
        s.open().await.unwrap();
        assert!(matches!(
            s.transaction(0x9999, &[], Some(&[1])).await,
            Err(SessionError::Response { .. })
        ));
        assert!(!s.is_open());
        assert_eq!(s.transport().inner.reset_count(), 1);
    }

    #[tokio::test]
    async fn open_retry_after_a_lost_response_does_not_read_a_stale_ok() {
        let mut camera = SimCamera::new().with_wedge_model(false);
        camera.inject(Endpoint::BulkIn, 0, Fault::Timeout);
        let mut s = session(camera);
        s.open().await.unwrap();
        assert_eq!(s.transport().reset_count(), 1);
        s.transaction(code::GET_DEVICE_INFO, &[], None)
            .await
            .unwrap();
        s.close().await.unwrap();
    }

    #[tokio::test]
    async fn open_retry_after_a_timed_out_command_resets() {
        let mut camera = SimCamera::new().with_wedge_model(false);
        camera.inject(Endpoint::BulkOut, 0, Fault::Timeout);
        let mut s = session(camera);
        s.open().await.unwrap();
        assert_eq!(s.transport().reset_count(), 1);
    }

    #[tokio::test]
    async fn open_clears_a_second_stall_before_resetting() {
        let mut camera = SimCamera::new();
        camera.inject(Endpoint::BulkOut, 0, Fault::Stall);
        camera.inject(Endpoint::BulkIn, 0, Fault::Stall);
        let mut s = session(camera);
        s.open().await.unwrap();
        assert_eq!(s.transport().reset_count(), 1);
        assert!(s.transport().clear_halt_count() >= 4);
    }

    #[tokio::test]
    async fn abandoned_exchange_refuses_transactions_until_close_resets() {
        let mut s = session(SimCamera::new());
        s.open().await.unwrap();
        // As left by a transaction future dropped after its command was sent.
        s.in_flight = true;
        assert_eq!(
            s.transaction(code::GET_DEVICE_INFO, &[], None).await,
            Err(SessionError::NotOpen)
        );
        s.close().await.unwrap();
        assert_eq!(s.transport().reset_count(), 1);
        assert!(!s.transport().session_open());
    }

    #[tokio::test]
    async fn dirty_handle_refuses_transactions() {
        let mut s = session(SimCamera::new());
        s.open().await.unwrap();
        s.dirty = true;
        assert_eq!(
            s.transaction(code::GET_DEVICE_INFO, &[], None).await,
            Err(SessionError::NotOpen)
        );
        s.close().await.unwrap();
        assert!(!s.transport().session_open());
    }

    #[tokio::test]
    async fn events_are_polled() {
        let mut s = session(SimCamera::new());
        s.open().await.unwrap();
        assert_eq!(s.poll_event().await.unwrap(), None);
        s.transport.push_event(0x4006, 0, &[1, 2]).unwrap();
        assert_eq!(
            s.poll_event().await.unwrap(),
            Some(Event {
                code: 0x4006,
                transaction_id: 0,
                params: vec![1, 2]
            })
        );
    }

    #[tokio::test]
    async fn poll_event_rejects_non_event_containers() {
        let mut s = session(SimCamera::new());
        s.transport
            .push_raw_interrupt(Container::command(0x1001, 0, &[]).unwrap());
        assert!(matches!(
            s.poll_event().await,
            Err(SessionError::UnexpectedContainer { .. })
        ));
    }

    #[tokio::test]
    async fn interrupt_stall_clears_the_halt() {
        let mut s = session(SimCamera::new());
        s.transport.inject(Endpoint::InterruptIn, 0, Fault::Stall);
        assert_eq!(
            s.poll_event().await,
            Err(SessionError::Transport(TransportError::Stall))
        );
        assert_eq!(s.poll_event().await.unwrap(), None);
    }

    #[tokio::test]
    async fn open_recovers_from_a_stall_with_clear_halt() {
        let mut camera = SimCamera::new();
        camera.inject(Endpoint::BulkOut, 0, Fault::Stall);
        let mut s = session(camera);
        s.open().await.unwrap();
        assert_eq!(s.transport().clear_halt_count(), 2);
        assert_eq!(s.transport().reset_count(), 0);
    }

    #[tokio::test]
    async fn open_recovers_from_the_wedge_with_a_reset() {
        let mut first = session(SimCamera::new());
        first.open().await.unwrap();
        // Abandon the session without CloseSession.
        let mut s = session(first.into_transport());
        s.open().await.unwrap();
        assert!(s.transport().session_open());
        assert!(!s.transport().is_wedged());
        assert_eq!(s.transport().reset_count(), 1);
        assert!(
            s.transport().clear_halt_count() >= 2,
            "clear halt comes first"
        );
        s.close().await.unwrap();
    }

    #[tokio::test]
    async fn open_gives_up_when_the_camera_never_recovers() {
        let mut camera = SimCamera::new();
        for _ in 0..3 {
            camera.inject(Endpoint::BulkOut, 0, Fault::Timeout);
        }
        let mut s = session(camera);
        assert_eq!(
            s.open().await,
            Err(SessionError::Unrecoverable(TransportError::Timeout))
        );
        assert!(!s.is_open());
    }

    #[tokio::test]
    async fn open_resets_away_a_stale_session_when_wedge_model_is_off() {
        let mut first = session(SimCamera::new().with_wedge_model(false));
        first.open().await.unwrap();
        let mut s = session(first.into_transport());
        s.open().await.unwrap();
        assert_eq!(s.transport().reset_count(), 1);
        assert!(s.transport().session_open());
    }

    #[tokio::test]
    async fn failed_open_does_not_leave_the_camera_open() {
        let mut s = session(SimCamera::new());
        // The camera accepts OpenSession but the response read hits garbage first.
        s.transport.push_raw_bulk_in(vec![0xFF; 12]);
        let result = s.scoped(async |_| Ok(())).await;
        assert!(result.is_err());
        assert!(!s.transport().session_open());
    }

    #[tokio::test]
    async fn open_fails_fast_on_disconnect() {
        let mut camera = SimCamera::new();
        camera.inject(Endpoint::BulkOut, 0, Fault::Disconnect);
        let mut s = session(camera);
        assert_eq!(
            s.open().await,
            Err(SessionError::Transport(TransportError::Disconnected))
        );
    }

    #[tokio::test]
    async fn stall_mid_transaction_clears_halts_then_close_resets() {
        let camera = SimCamera::new().with_operation(GET_OBJECT, Reply::data(payload(40_000)));
        let mut s = session(camera);
        s.open().await.unwrap();
        s.transport.inject(Endpoint::BulkIn, 1, Fault::Stall);
        assert_eq!(
            s.transaction(GET_OBJECT, &[1], None).await,
            Err(SessionError::Transport(TransportError::Stall))
        );
        assert_eq!(s.transport().clear_halt_count(), 2);
        s.close().await.unwrap();
        assert!(!s.transport().session_open());
        assert_eq!(s.transport().reset_count(), 1);
    }

    #[tokio::test]
    async fn timeout_is_reported_and_recovered() {
        let mut s = session(SimCamera::new());
        s.open().await.unwrap();
        s.transport.inject(Endpoint::BulkIn, 0, Fault::Timeout);
        assert_eq!(
            s.transaction(code::GET_DEVICE_INFO, &[], None).await,
            Err(SessionError::Transport(TransportError::Timeout))
        );
        s.close().await.unwrap();
        assert!(!s.transport().session_open());
    }

    #[tokio::test]
    async fn truncated_delivery_times_out_and_recovers() {
        let camera = SimCamera::new().with_operation(GET_OBJECT, Reply::data(payload(2000)));
        let mut s = session(camera);
        s.open().await.unwrap();
        s.transport
            .inject(Endpoint::BulkIn, 0, Fault::ShortRead(100));
        assert_eq!(
            s.transaction(GET_OBJECT, &[1], None).await,
            Err(SessionError::Transport(TransportError::Timeout))
        );
        s.close().await.unwrap();
        assert!(!s.transport().session_open());
    }

    #[tokio::test]
    async fn garbage_header_is_a_container_error() {
        let mut s = session(SimCamera::new());
        s.open().await.unwrap();
        s.transport.push_raw_bulk_in(vec![0xFF; 12]);
        assert!(matches!(
            s.transaction(code::GET_DEVICE_INFO, &[], None).await,
            Err(SessionError::Container(_))
        ));
    }

    #[tokio::test]
    async fn unknown_length_container_is_rejected() {
        let mut s = session(SimCamera::new());
        s.open().await.unwrap();
        let mut header = Container::command(0x1001, 1, &[]).unwrap();
        header[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        header[4..6].copy_from_slice(&2_u16.to_le_bytes());
        s.transport.push_raw_bulk_in(header);
        assert_eq!(
            s.transaction(code::GET_DEVICE_INFO, &[], None).await,
            Err(SessionError::UnsupportedLength)
        );
    }

    /// A 700-byte data phase, a total that is not a multiple of any packet size.
    async fn read_with(reply: Reply, len: usize) -> (Session<SimCamera>, Outcome) {
        let camera = SimCamera::new().with_operation(GET_OBJECT, reply);
        let mut s = session(camera);
        s.open().await.unwrap();
        let outcome = s.transaction(GET_OBJECT, &[1], None).await.unwrap();
        assert_eq!(outcome.data, payload(len));
        (s, outcome)
    }

    #[tokio::test]
    async fn header_arriving_alone_is_not_a_short_transfer() {
        let (s, _) = read_with(Reply::data(payload(700)).cut(&[HEADER_LEN, 700]), 700).await;
        assert_eq!(s.transport().reset_count(), 0);
    }

    #[tokio::test]
    async fn payload_split_across_transfers_with_final_zlp() {
        for packet in [512, 1024] {
            let len = packet * 3 - HEADER_LEN;
            let camera = SimCamera::new()
                .with_max_packet_size(packet)
                .with_operation(GET_OBJECT, Reply::data(payload(len)).chunked(HEADER_LEN));
            let config = SessionConfig {
                max_packet_size: packet,
                ..SessionConfig::default()
            };
            let mut s = Session::new(camera, config);
            s.open().await.unwrap();
            assert_eq!(
                s.transaction(GET_OBJECT, &[1], None).await.unwrap().data,
                payload(len)
            );
            s.close().await.unwrap();
            assert_eq!(s.transport().reset_count(), 0);
        }
    }

    #[tokio::test]
    async fn data_and_response_in_one_read() {
        let (s, _) = read_with(Reply::data(payload(700)).cut(&[]), 700).await;
        assert_eq!(s.transport().reset_count(), 0);
    }

    #[tokio::test]
    async fn data_tail_and_response_in_one_read() {
        let (s, _) = read_with(Reply::data(payload(700)).cut(&[100]), 700).await;
        assert_eq!(s.transport().reset_count(), 0);
    }

    #[tokio::test]
    async fn response_after_data_in_its_own_read() {
        let (s, _) = read_with(Reply::data(payload(700)).cut(&[HEADER_LEN + 700]), 700).await;
        assert_eq!(s.transport().reset_count(), 0);
    }

    #[tokio::test]
    async fn leftover_holds_a_partial_response_header() {
        let (mut s, _) = read_with(
            Reply::data(payload(700))
                .with_params(&[9])
                .cut(&[HEADER_LEN + 700 + 5]),
            700,
        )
        .await;
        assert_eq!(s.transport().reset_count(), 0);
        // The session is still in step afterwards.
        s.transaction(GET_OBJECT, &[1], None).await.unwrap();
    }

    #[tokio::test]
    async fn response_params_survive_coalescing() {
        let camera =
            SimCamera::new().with_operation(0x9001, Reply::ok().with_params(&[3, 4]).cut(&[5]));
        let mut s = session(camera);
        s.open().await.unwrap();
        assert_eq!(
            s.transaction(0x9001, &[], None).await.unwrap().params,
            [3, 4]
        );
    }

    #[tokio::test]
    async fn device_busy_is_a_typed_error_without_reset() {
        let camera = SimCamera::new().with_operation(0x9002, Reply::with_code(code::DEVICE_BUSY));
        let mut s = session(camera);
        s.open().await.unwrap();
        assert_eq!(
            s.transaction(0x9002, &[], None).await,
            Err(SessionError::Response {
                code: code::DEVICE_BUSY,
                params: vec![]
            })
        );
        assert!(s.is_open());
        assert_eq!(s.transport().reset_count(), 0);
        s.transaction(0x9002, &[], None).await.unwrap_err();
        s.close().await.unwrap();
        assert_eq!(s.transport().reset_count(), 0);
        assert_eq!(
            s.transport().commands().last().map(|c| c.code),
            Some(code::CLOSE_SESSION)
        );
    }

    #[tokio::test]
    async fn transaction_ids_wrap_against_a_strict_camera() {
        let camera = SimCamera::new().with_operation(0x9001, Reply::ok());
        let mut s = session(camera);
        s.open().await.unwrap();
        s.next_transaction = u32::MAX - 2;
        s.transport.expect_transaction(u32::MAX - 2);
        for _ in 0..4 {
            s.transaction(0x9001, &[], None).await.unwrap();
        }
        let sent: Vec<u32> = s
            .transport()
            .commands()
            .iter()
            .skip(1)
            .map(|c| c.transaction_id)
            .collect();
        assert_eq!(sent, [u32::MAX - 2, u32::MAX - 1, 1, 2]);
        s.close().await.unwrap();
    }

    #[tokio::test]
    async fn disconnect_mid_transaction_and_close_reports_it() {
        let mut s = session(SimCamera::new());
        s.open().await.unwrap();
        s.transport.inject(Endpoint::BulkIn, 0, Fault::Disconnect);
        assert_eq!(
            s.transaction(code::GET_DEVICE_INFO, &[], None).await,
            Err(SessionError::Transport(TransportError::Disconnected))
        );
        assert_eq!(
            s.close().await,
            Err(SessionError::Transport(TransportError::Disconnected))
        );
        assert!(!s.is_open());
    }

    #[tokio::test]
    async fn oversized_data_phase_is_rejected() {
        let camera = SimCamera::new().with_operation(GET_OBJECT, Reply::data(payload(5000)));
        let config = SessionConfig {
            max_data_len: 1000,
            ..SessionConfig::default()
        };
        let mut s = Session::new(camera, config);
        s.open().await.unwrap();
        assert!(matches!(
            s.transaction(GET_OBJECT, &[], None).await,
            Err(SessionError::DataTooLarge { limit: 1000, .. })
        ));
    }

    #[tokio::test]
    async fn mismatched_transaction_id_is_rejected() {
        let mut s = session(SimCamera::new());
        s.open().await.unwrap();
        let stale = Container {
            kind: ContainerKind::Response,
            code: code::RESPONSE_OK,
            transaction_id: 99,
            payload: &[],
        };
        let mut bytes = Vec::new();
        stale.encode_into(&mut bytes).unwrap();
        s.transport.push_raw_bulk_in(bytes);
        assert_eq!(
            s.transaction(code::GET_DEVICE_INFO, &[], None).await,
            Err(SessionError::TransactionMismatch {
                expected: 1,
                actual: 99
            })
        );
    }

    #[tokio::test]
    async fn endless_zero_length_packets_are_bounded() {
        let mut s = session(SimCamera::new());
        s.open().await.unwrap();
        for _ in 0..=MAX_EMPTY_READS {
            s.transport.push_raw_bulk_in(Vec::new());
        }
        assert_eq!(
            s.transaction(code::GET_DEVICE_INFO, &[], None).await,
            Err(SessionError::TooManyEmptyReads)
        );
    }

    #[tokio::test]
    async fn scoped_closes_after_success_and_after_error() {
        let camera = SimCamera::new();
        let mut s = session(camera);
        let ok = s
            .scoped(async |s| s.transaction(code::GET_DEVICE_INFO, &[], None).await)
            .await;
        assert!(ok.is_ok());
        assert!(!s.transport().session_open());
        assert_eq!(
            s.transport().commands().last().map(|c| c.code),
            Some(code::CLOSE_SESSION)
        );

        let err = s
            .scoped(async |s| s.transaction(0x9999, &[], None).await)
            .await;
        assert!(matches!(err, Err(SessionError::Response { .. })));
        assert!(!s.transport().session_open());
        assert_eq!(
            s.transport().commands().last().map(|c| c.code),
            Some(code::CLOSE_SESSION)
        );
    }

    #[tokio::test]
    async fn scoped_closes_after_a_fault_so_the_next_open_is_not_wedged() {
        let camera = SimCamera::new().with_operation(GET_OBJECT, Reply::data(payload(3000)));
        let mut s = session(camera);
        s.transport
            .inject(Endpoint::BulkIn, 1, Fault::ShortRead(20));
        let result = s
            .scoped(async |s| s.transaction(GET_OBJECT, &[1], None).await)
            .await;
        assert!(result.is_err());
        assert!(!s.transport().session_open());
        let mut again = session(s.into_transport());
        again.open().await.unwrap();
        assert_eq!(
            again.transport().reset_count(),
            1,
            "only the dirty close reset"
        );
    }

    proptest! {
        #[test]
        fn arbitrary_device_bytes_never_panic(
            transfers in proptest::collection::vec(
                proptest::collection::vec(any::<u8>(), 0..2048), 0..6),
            events in proptest::collection::vec(any::<u8>(), 0..128),
        ) {
            let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
            runtime.block_on(async {
                let mut camera = SimCamera::new();
                for transfer in transfers {
                    camera.push_raw_bulk_in(transfer);
                }
                camera.push_raw_interrupt(events);
                let mut s = session(camera);
                let _ = s.open().await;
                let _ = s.transaction(GET_OBJECT, &[1], None).await;
                let _ = s.poll_event().await;
                let _ = s.close().await;
            });
        }
    }
}
