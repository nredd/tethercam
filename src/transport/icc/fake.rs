//! Deterministic test boundaries: a scripted one for race and failure paths, and one backed by
//! the simulated Sony body for end-to-end driver tests.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use super::{Boundary, IccDeviceId, IccError, IccReply, IccTimeout, Lease};
use crate::ptp::session::{Session, SessionConfig, SessionError};
use crate::ptp::{Container, ContainerKind};
use crate::transport::sim::SimCamera;

/// Longest a held native call waits for its test, so a regression fails instead of hanging.
pub(super) const HOLD_LIMIT: Duration = Duration::from_secs(10);

/// One recorded boundary call. Payload bytes and identifiers are not recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Call {
    Discover,
    Open,
    Transact { operation: u16, params: Vec<u32> },
    Close,
    Event,
    LeaseDropped,
}

/// A scripted transaction outcome.
pub(super) enum Step {
    /// Return at once.
    Done(Result<IccReply, IccError>),
    /// Signal `entered`, block until `release` fires, then return. Models a native operation
    /// that keeps running after its caller stopped waiting.
    Hold {
        entered: SyncSender<()>,
        release: Receiver<()>,
        result: Result<IccReply, IccError>,
    },
    /// As `Hold`, but the native owner dies once released.
    Die {
        entered: SyncSender<()>,
        release: Receiver<()>,
    },
}

/// What the fake answers, in order. An exhausted queue answers success.
#[derive(Default)]
pub(super) struct Script {
    pub discover: VecDeque<Result<Vec<IccDeviceId>, IccError>>,
    pub open: VecDeque<Result<(), IccError>>,
    pub transact: VecDeque<Step>,
    pub close: VecDeque<Result<(), IccError>>,
    /// The ONE process-wide event queue the boundary and every lease read, like the native one.
    /// Whatever is queued when a session opens is stale.
    pub events: VecDeque<Result<Option<Vec<u8>>, IccError>>,
    /// Events that arrive once the session is open: released when a lease first polls an empty
    /// queue, which is where the bridge's stale drain ends.
    pub fresh: VecDeque<Result<Option<Vec<u8>>, IccError>>,
    pub calls: Vec<Call>,
    pub live_leases: usize,
    pub panic_on_discover: bool,
    /// Block the next `open` the way a slow native open does: signal, then wait for release.
    pub hold_open: Option<(SyncSender<()>, Receiver<()>)>,
}

pub(super) type Shared = Arc<Mutex<Script>>;

fn lock(shared: &Shared) -> MutexGuard<'_, Script> {
    shared.lock().expect("script lock")
}

/// The OK reply an exhausted script answers.
pub(super) fn ok_reply() -> IccReply {
    IccReply {
        code: 0x2001,
        params: Vec::new(),
        data: Vec::new(),
    }
}

/// A scripted boundary.
pub(super) struct Fake {
    shared: Shared,
}

impl Fake {
    pub(super) fn new() -> (Self, Shared) {
        let shared = Shared::default();
        (
            Self {
                shared: Arc::clone(&shared),
            },
            shared,
        )
    }
}

pub(super) struct FakeLease<'a> {
    shared: Shared,
    closed: bool,
    _borrow: &'a mut Fake,
}

impl Drop for FakeLease<'_> {
    fn drop(&mut self) {
        let mut script = lock(&self.shared);
        script.live_leases -= 1;
        if !self.closed {
            script.calls.push(Call::LeaseDropped);
        }
    }
}

impl Lease for FakeLease<'_> {
    fn transact(
        &mut self, operation: u16, params: &[u32], _outgoing: Vec<u8>, _timeout: IccTimeout,
    ) -> Result<IccReply, IccError> {
        let step = {
            let mut script = lock(&self.shared);
            script.calls.push(Call::Transact {
                operation,
                params: params.to_vec(),
            });
            script.transact.pop_front()
        };
        match step {
            None => Ok(ok_reply()),
            Some(Step::Done(result)) => result,
            Some(Step::Hold {
                entered,
                release,
                result,
            }) => {
                entered.send(()).expect("test is listening");
                release.recv_timeout(HOLD_LIMIT).expect("test releases");
                result
            }
            Some(Step::Die { entered, release }) => {
                entered.send(()).expect("test is listening");
                release.recv_timeout(HOLD_LIMIT).expect("test releases");
                std::panic::resume_unwind(Box::new("synthetic native owner failure"))
            }
        }
    }

    fn close(mut self, _timeout: IccTimeout) -> Result<(), IccError> {
        self.closed = true;
        let mut script = lock(&self.shared);
        script.calls.push(Call::Close);
        script.close.pop_front().unwrap_or(Ok(()))
    }

    fn poll_event(&mut self) -> Result<Option<Vec<u8>>, IccError> {
        let mut script = lock(&self.shared);
        script.calls.push(Call::Event);
        script.events.pop_front().unwrap_or_else(|| {
            let arrived = std::mem::take(&mut script.fresh);
            script.events.extend(arrived);
            Ok(None)
        })
    }
}

impl Boundary for Fake {
    type Lease<'a> = FakeLease<'a>;

    fn discover(
        &mut self, _window: Duration, _timeout: IccTimeout,
    ) -> Result<Vec<IccDeviceId>, IccError> {
        let mut script = lock(&self.shared);
        script.calls.push(Call::Discover);
        let boom = script.panic_on_discover;
        let answer = script.discover.pop_front().unwrap_or(Ok(Vec::new()));
        drop(script);
        // Models a native owner that dies mid-request, without poisoning the script lock.
        assert!(!boom, "synthetic native owner failure");
        answer
    }

    fn open(
        &mut self, _id: &IccDeviceId, _timeout: IccTimeout,
    ) -> Result<Self::Lease<'_>, IccError> {
        let shared = Arc::clone(&self.shared);
        let (held, outcome) = {
            let mut script = lock(&shared);
            script.calls.push(Call::Open);
            (script.hold_open.take(), script.open.pop_front())
        };
        if let Some((entered, release)) = held {
            entered.send(()).expect("test is listening");
            release.recv_timeout(HOLD_LIMIT).expect("test releases");
        }
        outcome.unwrap_or(Ok(()))?;
        lock(&shared).live_leases += 1;
        Ok(FakeLease {
            shared,
            closed: false,
            _borrow: self,
        })
    }

    fn poll_event(&mut self) -> Result<Option<Vec<u8>>, IccError> {
        let mut script = lock(&self.shared);
        script.calls.push(Call::Event);
        script.events.pop_front().unwrap_or(Ok(None))
    }
}

/// A boundary that carries the simulated Sony body behind the ICC seam: it runs the real raw
/// PTP engine against `SimCamera`, but only ever sees what ICC would forward.
pub(super) struct SimBoundary {
    runtime: tokio::runtime::Runtime,
    camera: Option<SimCamera>,
}

impl SimBoundary {
    pub(super) fn new(camera: SimCamera) -> Self {
        Self {
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("runtime"),
            camera: Some(camera),
        }
    }
}

pub(super) struct SimLease<'a> {
    runtime: &'a tokio::runtime::Runtime,
    session: Session<SimCamera>,
    home: &'a mut Option<SimCamera>,
}

impl Drop for SimLease<'_> {
    fn drop(&mut self) {
        // Hand the camera back so the next lease can reopen it, leaving an inert placeholder.
        let spent = Session::new(SimCamera::new(), SessionConfig::default());
        let session = std::mem::replace(&mut self.session, spent);
        *self.home = Some(session.into_transport());
    }
}

impl Lease for SimLease<'_> {
    fn transact(
        &mut self, operation: u16, params: &[u32], outgoing: Vec<u8>, _timeout: IccTimeout,
    ) -> Result<IccReply, IccError> {
        let data = (!outgoing.is_empty()).then_some(outgoing.as_slice());
        let result = self
            .runtime
            .block_on(self.session.transaction(operation, params, data));
        match result {
            Ok(outcome) => Ok(IccReply {
                code: 0x2001,
                params: outcome.params,
                data: outcome.data,
            }),
            Err(SessionError::Response { code, params }) => Ok(IccReply {
                code,
                params,
                data: Vec::new(),
            }),
            Err(_) => Err(IccError::NativeFailure),
        }
    }

    fn close(mut self, _timeout: IccTimeout) -> Result<(), IccError> {
        self.runtime
            .block_on(self.session.close())
            .map_err(|_| IccError::NativeFailure)
    }

    fn poll_event(&mut self) -> Result<Option<Vec<u8>>, IccError> {
        let polled = self
            .runtime
            .block_on(self.session.poll_event_within(Duration::from_millis(1)));
        let Some(event) = polled.map_err(|_| IccError::NativeFailure)? else {
            return Ok(None);
        };
        let payload: Vec<u8> = event.params.iter().flat_map(|p| p.to_le_bytes()).collect();
        let mut wire = vec![11];
        // Cannot fail: five parameters at most.
        let _ = Container {
            kind: ContainerKind::Event,
            code: event.code,
            transaction_id: event.transaction_id,
            payload: &payload,
        }
        .encode_into(&mut wire);
        Ok(Some(wire))
    }
}

impl Boundary for SimBoundary {
    type Lease<'a> = SimLease<'a>;

    fn discover(
        &mut self, _window: Duration, _timeout: IccTimeout,
    ) -> Result<Vec<IccDeviceId>, IccError> {
        Ok(vec![
            IccDeviceId::new("synthetic-sony".into()).expect("valid"),
        ])
    }

    fn open(&mut self, _id: &IccDeviceId, _timeout: IccTimeout) -> Result<SimLease<'_>, IccError> {
        let Self { runtime, camera } = self;
        // The bridge holds one lease at a time, and a lease returns the camera when dropped.
        let taken = camera.take().expect("camera is home between leases");
        // ICC negotiates the session itself, so the simulated raw handshake lives here.
        let mut session = Session::new(taken, SessionConfig::default());
        runtime
            .block_on(session.open())
            .map_err(|_| IccError::NativeFailure)?;
        Ok(SimLease {
            runtime,
            session,
            home: camera,
        })
    }

    fn poll_event(&mut self) -> Result<Option<Vec<u8>>, IccError> {
        Ok(None)
    }
}
