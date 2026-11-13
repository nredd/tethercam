//! The seam between this crate and the native `ImageCaptureCore` boundary.
//!
//! [`Boundary`] and [`Lease`] mirror the blocking, owned-session API of the `image-capture`
//! crate without naming any of its types, so the bridge, the PTP session and every failure
//! path are exercised on any platform against a deterministic implementation. Only the thin
//! macOS adapter in `native` implements them over the real crate.
//!
//! Contract implementations must keep:
//! - one native session lease at a time, borrowed from the boundary
//! - a timeout is never cancellation: native work completes, and [`IccError::DeadlineExceeded`]
//!   only ends the caller's wait
//! - raw `OpenSession` and `CloseSession` are never forwarded
//!
//! References:
//! - `docs/macos-camera.md`
//! - <https://github.com/nredd/image-capture/blob/main/docs/native-contract.md>

use std::fmt;
use std::time::Duration;

use super::IccError;

/// Largest identifier, in UTF-8 bytes.
pub const MAX_ID_BYTES: usize = 1024;
/// Largest data phase in either direction, matching the native bound.
pub const MAX_DATA_BYTES: usize = 16 * 1024 * 1024;
/// Longest admission or wait.
pub const MAX_WAIT: Duration = Duration::from_secs(3600);

/// An exact framework device identifier. It identifies a private device, so it is never
/// printed: `Debug` is redacted and nothing here formats the value.
#[derive(Clone, PartialEq, Eq)]
pub struct IccDeviceId(String);

impl IccDeviceId {
    /// Validate an exact identifier.
    ///
    /// # Errors
    ///
    /// [`IccError::InvalidInput`] when it is empty, over [`MAX_ID_BYTES`] or contains NUL.
    pub fn new(value: String) -> Result<Self, IccError> {
        if value.is_empty() || value.len() > MAX_ID_BYTES || value.contains('\0') {
            return Err(IccError::InvalidInput);
        }
        Ok(Self(value))
    }

    /// The exact identifier. Do not log it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for IccDeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IccDeviceId(..)")
    }
}

/// Separate admission and caller-wait limits, relative to enqueueing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IccTimeout {
    admission: Duration,
    wait: Duration,
}

impl Default for IccTimeout {
    /// Five seconds to admission and thirty to the caller's wait.
    fn default() -> Self {
        Self {
            admission: Duration::from_secs(5),
            wait: Duration::from_secs(30),
        }
    }
}

impl IccTimeout {
    /// Validate: both nonzero, `admission <= wait <= MAX_WAIT`.
    ///
    /// # Errors
    ///
    /// [`IccError::InvalidInput`] otherwise.
    pub fn new(admission: Duration, wait: Duration) -> Result<Self, IccError> {
        if admission.is_zero() || wait.is_zero() || admission > wait || wait > MAX_WAIT {
            return Err(IccError::InvalidInput);
        }
        Ok(Self { admission, wait })
    }

    /// Latest permitted admission.
    #[must_use]
    pub const fn admission(self) -> Duration {
        self.admission
    }

    /// Latest caller wait. Expiry never cancels native work.
    #[must_use]
    pub const fn wait(self) -> Duration {
        self.wait
    }
}

/// A completed PTP command: the camera's response and its separate data phase.
///
/// ICC transaction ids are opaque and deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IccReply {
    /// Response code. Non-`OK` codes are protocol results, not failures of this layer.
    pub code: u16,
    /// Response parameters, at most five.
    pub params: Vec<u32>,
    /// Device-to-host data, without a container header. Empty when there was none.
    pub data: Vec<u8>,
}

/// An exclusive session lease on one camera.
pub trait Lease {
    /// Run one serialized command.
    ///
    /// # Errors
    ///
    /// An [`IccError`] for bounds, framing, deadline, lifecycle or native failures.
    fn transact(
        &mut self, operation: u16, params: &[u32], outgoing: Vec<u8>, timeout: IccTimeout,
    ) -> Result<IccReply, IccError>;

    /// Close with confirmation, consuming the lease.
    ///
    /// # Errors
    ///
    /// An [`IccError`]; a failed close may quarantine the native graph forever.
    fn close(self, timeout: IccTimeout) -> Result<(), IccError>
    where
        Self: Sized;

    /// Take one queued advisory event, or `None`.
    ///
    /// # Errors
    ///
    /// [`IccError::EventOverflow`] once after loss, or [`IccError::Disconnected`].
    fn poll_event(&mut self) -> Result<Option<Vec<u8>>, IccError>;
}

/// The native owner's worker endpoint. Not cloneable; one lease at a time.
pub trait Boundary {
    /// The session lease, borrowing the boundary.
    type Lease<'a>: Lease
    where
        Self: 'a;

    /// Observe a bounded discovery window and return exact identifiers.
    ///
    /// # Errors
    ///
    /// An [`IccError`] for deadlines, limits or a disconnected owner.
    fn discover(
        &mut self, window: Duration, timeout: IccTimeout,
    ) -> Result<Vec<IccDeviceId>, IccError>;

    /// Acquire the only session lease by exact identifier.
    ///
    /// # Errors
    ///
    /// An [`IccError`]; after [`IccError::DeadlineExceeded`] an opening session may still
    /// complete and is closed natively afterwards.
    fn open(&mut self, id: &IccDeviceId, timeout: IccTimeout) -> Result<Self::Lease<'_>, IccError>;

    /// Take one queued advisory event while no lease is held, or `None`.
    ///
    /// # Errors
    ///
    /// As [`Lease::poll_event`].
    fn poll_event(&mut self) -> Result<Option<Vec<u8>>, IccError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_exact_bounded_and_never_printed() {
        let id = IccDeviceId::new("synthetic-camera-0".into()).expect("valid");
        assert_eq!(id.as_str(), "synthetic-camera-0");
        assert_eq!(format!("{id:?}"), "IccDeviceId(..)");
        for bad in [String::new(), "x".repeat(MAX_ID_BYTES + 1), "a\0b".into()] {
            assert_eq!(IccDeviceId::new(bad), Err(IccError::InvalidInput));
        }
        assert!(IccDeviceId::new("x".repeat(MAX_ID_BYTES)).is_ok());
    }

    #[test]
    fn timeouts_are_validated_like_the_native_boundary() {
        let s = Duration::from_secs;
        let ok = IccTimeout::new(s(1), s(2)).expect("valid");
        assert_eq!((ok.admission(), ok.wait()), (s(1), s(2)));
        assert!(IccTimeout::new(s(2), s(2)).is_ok());
        assert!(IccTimeout::new(s(1), MAX_WAIT).is_ok());
        assert_eq!(IccTimeout::default(), IccTimeout::new(s(5), s(30)).unwrap());
        for (admission, wait) in [
            (Duration::ZERO, s(1)),
            (s(1), Duration::ZERO),
            (s(3), s(2)),
            (s(1), MAX_WAIT + s(1)),
        ] {
            assert_eq!(
                IccTimeout::new(admission, wait),
                Err(IccError::InvalidInput)
            );
        }
    }
}
