//! The macOS-only delegation to the `image-capture` crate.
//!
//! All native interop, and every `unsafe` block, lives in that crate. This file only maps its
//! owned, safe API onto [`Boundary`] and [`Lease`]. Each impl method is a single delegation
//! plus a total, tested mapping, so the decisions around them stay in the portable, fully
//! covered layers. The delegations themselves need a real `Client`, which exists only inside
//! `native::run` on the main thread with a camera present, so they are exercised by the gated
//! `icc_sample` example and not by hosted tests.
//!
//! References:
//! - <https://github.com/nredd/image-capture/blob/main/docs/native-contract.md>

use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use image_capture::client::{Client, CommandResult, DeviceId, Error, Session, Timeout};
use image_capture::events::EventReceiver;
use image_capture::native;

use super::{Boundary, IccDeviceId, IccError, IccHandle, IccReply, IccTimeout, Lease, run_with};

/// Map the crate's error onto sanitized categories. Total over every variant.
fn map_error(error: Error) -> IccError {
    match error {
        Error::InvalidInput => IccError::InvalidInput,
        Error::Busy => IccError::Busy,
        Error::AdmissionExpired => IccError::AdmissionExpired,
        Error::WaitExpired => IccError::DeadlineExceeded,
        Error::Disconnected => IccError::Disconnected,
        Error::NotFound => IccError::NotFound,
        Error::Ambiguous => IccError::Ambiguous,
        Error::LimitExceeded => IccError::LimitExceeded,
        Error::NativeFailure => IccError::NativeFailure,
        Error::InvalidResponse => IccError::MalformedCompletion,
        Error::CallbackFailure => IccError::CallbackFailure,
        Error::Quarantined => IccError::Quarantined,
        Error::NotMainThread
        | Error::AlreadyCreated
        | Error::IdentityExhausted
        | Error::WorkerPanicked
        | Error::WorkerStartFailed => IccError::Runtime,
    }
}

fn native_timeout(limit: IccTimeout) -> Timeout {
    Timeout {
        admission: limit.admission(),
        wait: limit.wait(),
    }
}

fn reply(result: CommandResult) -> IccReply {
    IccReply {
        code: result.response.code,
        params: result.response.parameters,
        data: result.data,
    }
}

fn identifiers(devices: &[DeviceId]) -> Result<Vec<IccDeviceId>, IccError> {
    devices
        .iter()
        .map(|id| IccDeviceId::new(id.as_str().to_owned()))
        .collect()
}

/// One queued advisory event. Loss is reported before anything else, because events queued
/// around a loss cannot be trusted to be complete.
fn next_event(events: &EventReceiver) -> Result<Option<Vec<u8>>, IccError> {
    if events.take_overflow() {
        return Err(IccError::EventOverflow);
    }
    match events.try_recv() {
        Ok(event) => Ok(Some(event.0)),
        Err(TryRecvError::Empty) => Ok(None),
        Err(TryRecvError::Disconnected) => Err(IccError::Disconnected),
    }
}

/// A native session lease.
pub struct NativeLease<'a>(Session<'a>);

impl Lease for NativeLease<'_> {
    fn transact(
        &mut self, operation: u16, params: &[u32], outgoing: Vec<u8>, wait: IccTimeout,
    ) -> Result<IccReply, IccError> {
        let result = self
            .0
            .transact(operation, params, outgoing, native_timeout(wait));
        result.map(reply).map_err(map_error)
    }

    fn close(self, wait: IccTimeout) -> Result<(), IccError> {
        self.0.close(native_timeout(wait)).map_err(map_error)
    }

    fn poll_event(&mut self) -> Result<Option<Vec<u8>>, IccError> {
        next_event(self.0.events())
    }
}

impl Boundary for Client {
    type Lease<'a> = NativeLease<'a>;

    fn discover(
        &mut self, window: Duration, wait: IccTimeout,
    ) -> Result<Vec<IccDeviceId>, IccError> {
        let found = Client::discover(self, window, native_timeout(wait)).map_err(map_error)?;
        identifiers(&found)
    }

    fn open(&mut self, id: &IccDeviceId, wait: IccTimeout) -> Result<NativeLease<'_>, IccError> {
        let id = DeviceId::new(id.as_str().to_owned()).map_err(map_error)?;
        Client::open(self, id, native_timeout(wait))
            .map(NativeLease)
            .map_err(map_error)
    }

    fn poll_event(&mut self) -> Result<Option<Vec<u8>>, IccError> {
        next_event(self.events())
    }
}

/// Run `user` against the real boundary.
///
/// This is the main-thread entry: it must be called on the process main thread, it drives the
/// default run loop and blocks until `user` returns and native cleanup settles, possibly
/// forever if the framework never calls back. Only one native runtime is ever permitted per
/// process, including after failure. `user` receives the [`IccHandle`] on a joined Rust worker
/// and typically builds its own async runtime there.
///
/// # Errors
///
/// [`IccError::Runtime`] when called off the main thread or a second time, when the worker
/// could not start, or when `user` panicked. Failures inside `user` are its own `T`.
pub fn run<F, T>(user: F) -> Result<T, IccError>
where
    F: FnOnce(IccHandle) -> T + Send + 'static,
    T: Send + 'static,
{
    native::run(move |client| run_with(client, user))
        .map_err(map_error)
        .and_then(|result| result)
}

#[cfg(test)]
mod tests {
    use image_capture::events::{Event, event_channel};
    use image_capture::framing::Response;

    use super::*;

    #[test]
    fn every_native_error_has_a_sanitized_category() {
        let table = [
            (Error::NotMainThread, IccError::Runtime),
            (Error::AlreadyCreated, IccError::Runtime),
            (Error::InvalidInput, IccError::InvalidInput),
            (Error::Busy, IccError::Busy),
            (Error::AdmissionExpired, IccError::AdmissionExpired),
            (Error::WaitExpired, IccError::DeadlineExceeded),
            (Error::Disconnected, IccError::Disconnected),
            (Error::NotFound, IccError::NotFound),
            (Error::Ambiguous, IccError::Ambiguous),
            (Error::LimitExceeded, IccError::LimitExceeded),
            (Error::NativeFailure, IccError::NativeFailure),
            (Error::InvalidResponse, IccError::MalformedCompletion),
            (Error::CallbackFailure, IccError::CallbackFailure),
            (Error::Quarantined, IccError::Quarantined),
            (Error::IdentityExhausted, IccError::Runtime),
            (Error::WorkerPanicked, IccError::Runtime),
            (Error::WorkerStartFailed, IccError::Runtime),
        ];
        for (native, expected) in table {
            assert_eq!(map_error(native), expected, "{native:?}");
        }
    }

    #[test]
    fn timeouts_replies_and_identifiers_convert_without_loss() {
        let wait = IccTimeout::new(Duration::from_secs(2), Duration::from_secs(9)).unwrap();
        let native = native_timeout(wait);
        assert_eq!(
            (native.admission, native.wait),
            (Duration::from_secs(2), Duration::from_secs(9))
        );
        let converted = reply(CommandResult {
            response: Response {
                code: 0x2001,
                transaction_id: 77,
                parameters: vec![1, 2],
            },
            data: vec![9, 8],
        });
        assert_eq!(
            converted,
            IccReply {
                code: 0x2001,
                params: vec![1, 2],
                data: vec![9, 8],
            }
        );
        let ids = [DeviceId::new("synthetic-a".into()).unwrap()];
        assert_eq!(
            identifiers(&ids).unwrap(),
            [IccDeviceId::new("synthetic-a".into()).unwrap()]
        );
        assert_eq!(identifiers(&[]), Ok(Vec::new()));
    }

    #[test]
    fn events_report_loss_first_then_drain_then_disconnect() {
        let (sender, receiver) = event_channel(2, 4);
        assert_eq!(next_event(&receiver), Ok(None));
        for bytes in [vec![1], vec![2], vec![3]] {
            let _ = sender.publish(Event(bytes));
        }
        assert_eq!(next_event(&receiver), Err(IccError::EventOverflow));
        assert_eq!(next_event(&receiver), Ok(Some(vec![1])));
        assert_eq!(next_event(&receiver), Ok(Some(vec![2])));
        assert_eq!(next_event(&receiver), Ok(None));
        drop(sender);
        assert_eq!(next_event(&receiver), Err(IccError::Disconnected));
    }
}
