//! [`IccSession`]: a [`PtpSession`] over the single-flight `ImageCaptureCore` bridge.
//!
//! Differences from the raw USB `Session`, all because ICC owns the pipes:
//! - no raw `OpenSession` or `CloseSession`: ICC negotiates the session, so those operations
//!   are refused as [`IccError::RawSessionOperation`]
//! - a deadline ends only the caller's wait. The native operation completes and is never
//!   retried, cancelled or overlapped. After a deadline the session is retired: it refuses
//!   work, and a replacement `open` reports [`IccError::Busy`] until native cleanup settles
//! - at most one native operation at a time, no backlog ([`IccError::Busy`])
//! - events are advisory and bounded. Overflow, removal and any malformed event fail closed
//!   and fault the session, so the Sony layer must close and reconnect to resynchronize
//!
//! Lifecycle: `Closed -> Open -> Closed`, with the failure states
//! - `Faulted`: no more transactions, but the lease remains and `close` confirms closure
//! - `Retired`: the lease was given up; native cleanup settles it. Reuse is only probed
//!   through `open`, which the native owner refuses while cleanup is unresolved
//! - `Quarantined`: terminal. No new work and no replacement session
//!
//! Caller timeouts: `transaction_within` is the whole-command wait. Admission, the latest the
//! command may still enter native code, is `min(5 s, limit)`, so an expired admission reports
//! [`IccError::AdmissionExpired`] and submitted nothing.
//!
//! References:
//! - `docs/macos-camera.md`, Ownership and deadlines

use std::time::{Duration, Instant};

use super::event::{Notice, classify};
use super::{
    Effect, IccDeviceId, IccError, IccHandle, IccReply, IccTimeout, MAX_DATA_BYTES, MAX_WAIT,
};
use crate::ptp::session::{Event, Outcome, PtpSession, SessionConfig, SessionError, code};
use crate::ptp::{ContainerError, MAX_PARAMS};

/// Latest a command may still enter native code.
const ADMISSION: Duration = Duration::from_secs(5);
/// Pause between polls of an empty event queue.
const POLL_STEP: Duration = Duration::from_millis(10);
/// Advisory events skipped per poll, the size of the native queue.
const MAX_ADVISORY: usize = 64;

/// Where the session is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Closed,
    Open,
    Faulted,
    Retired,
    Quarantined,
}

/// A single-flight PTP session over `ImageCaptureCore`.
#[derive(Debug)]
pub struct IccSession {
    handle: IccHandle,
    device: IccDeviceId,
    pub(super) config: SessionConfig,
    clock: fn() -> Instant,
    state: State,
}

impl IccSession {
    /// An unopened session on the exact `device`, using `config`'s caps and deadlines.
    #[must_use]
    pub fn new(
        handle: IccHandle, device: IccDeviceId, config: SessionConfig, clock: fn() -> Instant,
    ) -> Self {
        Self {
            handle,
            device,
            config,
            clock,
            state: State::Closed,
        }
    }

    /// Give the bridge handle back, for reuse after `close` or by a factory's `recover`.
    #[must_use]
    pub fn into_handle(self) -> IccHandle {
        self.handle
    }

    fn timeout(wait: Duration) -> Result<IccTimeout, IccError> {
        IccTimeout::new(ADMISSION.min(wait), wait)
    }

    /// Apply the consequence of a failed transaction or event poll.
    fn settle(&mut self, error: IccError) -> SessionError {
        match error.effect() {
            Effect::Keep => {}
            // Only an open session reaches a fault: both callers check `require_open` first.
            Effect::Fault => self.state = State::Faulted,
            Effect::Retire => self.state = State::Retired,
            Effect::Quarantine => self.state = State::Quarantined,
        }
        error.into()
    }

    fn require_open(&self) -> Result<(), SessionError> {
        match self.state {
            State::Open => Ok(()),
            State::Quarantined => Err(IccError::Quarantined.into()),
            State::Closed | State::Faulted | State::Retired => Err(SessionError::NotOpen),
        }
    }

    fn finish(&mut self, reply: IccReply) -> Result<Outcome, SessionError> {
        if reply.params.len() > MAX_PARAMS {
            return Err(self.settle(IccError::MalformedCompletion));
        }
        if reply.code != code::RESPONSE_OK {
            return Err(SessionError::Response {
                code: reply.code,
                params: reply.params,
            });
        }
        let limit = self.config.max_data_len;
        if reply.data.len() > limit {
            return Err(SessionError::DataTooLarge {
                announced: reply.data.len(),
                limit,
            });
        }
        Ok(Outcome {
            data: reply.data,
            params: reply.params,
        })
    }

    /// Take one raw event from the bridge, applying failure effects.
    async fn raw_event(&mut self) -> Result<Option<Vec<u8>>, SessionError> {
        match self.handle.poll_event().await {
            Ok(event) => Ok(event),
            Err(error) => Err(self.settle(error)),
        }
    }

    /// Why a close attempt failed, and the state it leaves behind.
    fn close_failed(&mut self, error: IccError) -> SessionError {
        self.state = match error.effect() {
            // Nothing was submitted and the lease remains: the caller may retry.
            Effect::Keep if error == IccError::Busy => return error.into(),
            Effect::Keep | Effect::Retire => State::Retired,
            Effect::Quarantine | Effect::Fault => State::Quarantined,
        };
        if error.effect() == Effect::Fault {
            return IccError::CleanupFailure.into();
        }
        error.into()
    }
}

impl PtpSession for IccSession {
    fn now(&self) -> Instant {
        (self.clock)()
    }

    fn is_open(&self) -> bool {
        self.state == State::Open
    }

    async fn open(&mut self) -> Result<(), SessionError> {
        match self.state {
            State::Quarantined => return Err(IccError::Quarantined.into()),
            State::Open | State::Faulted => return Err(SessionError::AlreadyOpen),
            State::Closed | State::Retired => {}
        }
        let timeout = Self::timeout(self.config.transaction_timeout)?;
        match self.handle.open(self.device.clone(), timeout).await {
            Ok(()) => {
                self.state = State::Open;
                Ok(())
            }
            // After a deadline the open is retained natively; a late success is closed there
            // once it completes. A failed open leaves nothing open, so it changes no state.
            Err(error) if matches!(error.effect(), Effect::Retire | Effect::Quarantine) => {
                Err(self.settle(error))
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn close(&mut self) -> Result<(), SessionError> {
        match self.state {
            State::Closed => return Ok(()),
            State::Quarantined => return Err(IccError::Quarantined.into()),
            State::Retired => return Err(IccError::Busy.into()),
            State::Open | State::Faulted => {}
        }
        let timeout = Self::timeout(self.config.transaction_timeout)?;
        match self.handle.close(timeout).await {
            Ok(()) => {
                self.state = State::Closed;
                Ok(())
            }
            Err(error) => Err(self.close_failed(error)),
        }
    }

    async fn transaction(
        &mut self, operation: u16, params: &[u32], data_out: Option<&[u8]>,
    ) -> Result<Outcome, SessionError> {
        let limit = self.config.transaction_timeout;
        self.transaction_within(limit, operation, params, data_out)
            .await
    }

    async fn transaction_within(
        &mut self, limit: Duration, operation: u16, params: &[u32], data_out: Option<&[u8]>,
    ) -> Result<Outcome, SessionError> {
        self.require_open()?;
        if matches!(operation, code::OPEN_SESSION | code::CLOSE_SESSION) {
            return Err(IccError::RawSessionOperation.into());
        }
        if params.len() > MAX_PARAMS {
            return Err(ContainerError::TooManyParams(params.len()).into());
        }
        let outgoing = data_out.unwrap_or_default();
        let cap = self.config.max_data_out.min(MAX_DATA_BYTES);
        if outgoing.len() > cap {
            return Err(SessionError::DataTooLarge {
                announced: outgoing.len(),
                limit: cap,
            });
        }
        if limit.is_zero() {
            return Err(IccError::AdmissionExpired.into());
        }
        let timeout = Self::timeout(limit)?;
        let reply = self
            .handle
            .transact(operation, params.to_vec(), outgoing.to_vec(), timeout)
            .await;
        match reply {
            Ok(reply) => self.finish(reply),
            Err(error) => Err(self.settle(error)),
        }
    }

    async fn get_device_info(&mut self) -> Result<Vec<u8>, SessionError> {
        Ok(self
            .transaction(code::GET_DEVICE_INFO, &[], None)
            .await?
            .data)
    }

    async fn poll_event(&mut self) -> Result<Option<Event>, SessionError> {
        let timeout = self.config.timeout;
        self.poll_event_within(timeout).await
    }

    async fn poll_event_within(
        &mut self, timeout: Duration,
    ) -> Result<Option<Event>, SessionError> {
        self.require_open()?;
        let deadline = tokio::time::Instant::now() + timeout.min(MAX_WAIT);
        let mut skipped = 0;
        loop {
            let Some(bytes) = self.raw_event().await? else {
                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                if left.is_zero() {
                    return Ok(None);
                }
                tokio::time::sleep(left.min(POLL_STEP)).await;
                continue;
            };
            match classify(&bytes) {
                Ok(Notice::Ptp(event)) => return Ok(Some(event)),
                Ok(Notice::Advisory) => {
                    skipped += 1;
                    if skipped >= MAX_ADVISORY {
                        return Ok(None);
                    }
                }
                Err(error) => return Err(self.settle(error)),
            }
        }
    }
}
