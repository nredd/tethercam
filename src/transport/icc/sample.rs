//! A read-only acceptance sample for the real `ImageCaptureCore` path.
//!
//! It performs exactly: discovery, then per cycle `open`, `GetDeviceInfo` (`0x1001`) and a
//! confirmed `close`. No settings write, shutter action, vendor handshake or streaming. The
//! [`Receipt`] holds counts and response codes only: never a device identity, name, serial or
//! payload byte, so it is safe to paste into shared logs.
//!
//! Running it against a real camera is a hardware action reserved to the hardware owner; see
//! `examples/icc_sample.rs`, which refuses to start native code without an explicit flag. The
//! camera is picked by exact identifier or as the sole discovery candidate ([`Selector`]).
//!
//! References:
//! - `docs/macos-camera.md`, Evidence and validation

use std::io::{self, Write};
use std::time::{Duration, Instant};

use thiserror::Error;

use super::{IccDeviceId, IccError, IccHandle, IccSession, IccTimeout};
use crate::ptp::session::{PtpSession, SessionConfig, SessionError, code};

/// How long discovery observes the framework.
pub const DISCOVERY_WINDOW: Duration = Duration::from_secs(2);

/// Why a sample cycle failed. Every variant is a sanitized category.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SampleError {
    /// Discovery or exact selection failed.
    #[error("discovery or selection failed: {0}")]
    Discovery(#[from] IccError),
    /// A session operation failed or the camera refused `GetDeviceInfo`.
    #[error("session failed: {0}")]
    Session(#[from] SessionError),
    /// `GetDeviceInfo` answered `OK` with no data.
    #[error("GetDeviceInfo returned no data")]
    EmptyDeviceInfo,
}

impl SampleError {
    /// A short static category for the receipt.
    #[must_use]
    pub const fn category(&self) -> &'static str {
        match self {
            Self::Discovery(_) => "discovery",
            Self::Session(_) => "session",
            Self::EmptyDeviceInfo => "empty_device_info",
        }
    }
}

/// What a sample observed. Counts and codes only.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Receipt {
    /// Cameras discovery found.
    pub candidates: usize,
    /// Cycles attempted.
    pub cycles: usize,
    /// `GetDeviceInfo` answers that were `OK` with data.
    pub device_info_ok: usize,
    /// Closes confirmed by the native owner.
    pub confirmed_closes: usize,
    /// The response code of each `GetDeviceInfo`.
    pub response_codes: Vec<u16>,
    /// Why the sample stopped early, if it did.
    pub failure: Option<SampleError>,
}

impl Receipt {
    /// Whether every requested cycle opened, answered and confirmed its close.
    #[must_use]
    pub fn passed(&self, cycles: usize) -> bool {
        self.failure.is_none()
            && self.cycles == cycles
            && self.device_info_ok == cycles
            && self.confirmed_closes == cycles
    }

    /// Write one `key=value` line. Nothing identifying or payload-derived is printed.
    ///
    /// # Errors
    ///
    /// Any I/O error from `out`.
    pub fn write_to(&self, out: &mut dyn Write) -> io::Result<()> {
        let codes: Vec<String> = self
            .response_codes
            .iter()
            .map(|code| format!("{code:#06x}"))
            .collect();
        write!(
            out,
            "candidates={} cycles={} device_info_ok={} confirmed_closes={} response_codes={}",
            self.candidates,
            self.cycles,
            self.device_info_ok,
            self.confirmed_closes,
            codes.join(","),
        )?;
        match &self.failure {
            None => writeln!(out, " result=ok"),
            Some(failure) => writeln!(out, " result=failed category={}", failure.category()),
        }
    }
}

/// How the sample picks its one camera. It never picks "the first of several".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// Exactly this identifier, which discovery must return (a repeat of it is the same device).
    Exact(IccDeviceId),
    /// Whatever camera discovery returns, but only if it returns exactly one distinct device. Lets
    /// the hardware owner run without ever handling the identifier.
    SoleCandidate,
}

/// The distinct identifiers in `found`, in first-seen order, each with how often it was reported.
/// The framework can list one device more than once; that is one device, not several.
pub(crate) fn distinct(found: &[IccDeviceId]) -> Vec<(&IccDeviceId, usize)> {
    let mut seen: Vec<(&IccDeviceId, usize)> = Vec::new();
    for id in found {
        match seen.iter_mut().find(|(known, _)| *known == id) {
            Some((_, count)) => *count = count.saturating_add(1),
            None => seen.push((id, 1)),
        }
    }
    seen
}

impl Selector {
    /// Choose from `found`, de-duplicated first: [`IccError::NotFound`] for none,
    /// [`IccError::Ambiguous`] for several different devices.
    pub(crate) fn choose(&self, found: &[IccDeviceId]) -> Result<IccDeviceId, IccError> {
        let matching: Vec<&IccDeviceId> = distinct(found)
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| match self {
                Self::Exact(wanted) => *id == wanted,
                Self::SoleCandidate => true,
            })
            .collect();
        match matching.as_slice() {
            [] => Err(IccError::NotFound),
            [only] => Ok((*only).clone()),
            _ => Err(IccError::Ambiguous),
        }
    }
}

/// One open, `GetDeviceInfo`, confirmed close cycle. Returns the handle for the next one.
async fn cycle(
    handle: IccHandle, selector: &IccDeviceId, receipt: &mut Receipt,
) -> Result<IccHandle, SampleError> {
    let mut session = IccSession::new(
        handle,
        selector.clone(),
        SessionConfig::default(),
        Instant::now,
    );
    receipt.cycles += 1;
    session.open().await?;
    let info = session.transaction(code::GET_DEVICE_INFO, &[], None).await;
    // Always close an opened session, even after a refused or failed command.
    let closed = session.close().await;
    match &info {
        Ok(_) => receipt.response_codes.push(code::RESPONSE_OK),
        Err(SessionError::Response { code, .. }) => receipt.response_codes.push(*code),
        Err(_) => {}
    }
    if closed.is_ok() {
        receipt.confirmed_closes += 1;
    }
    let outcome = info?;
    closed?;
    if outcome.data.is_empty() {
        return Err(SampleError::EmptyDeviceInfo);
    }
    receipt.device_info_ok += 1;
    Ok(session.into_handle())
}

/// Run `cycles` read-only cycles on the one camera `selector` picks from discovery.
///
/// The first failure ends the sample, and the receipt records it, including the candidate count
/// when selection fails.
pub async fn read_only_sample(
    mut handle: IccHandle, selector: &Selector, cycles: usize,
) -> Receipt {
    let mut receipt = Receipt::default();
    let found = match handle
        .discover(DISCOVERY_WINDOW, IccTimeout::default())
        .await
    {
        Ok(found) => found,
        Err(error) => {
            receipt.failure = Some(error.into());
            return receipt;
        }
    };
    receipt.candidates = found.len();
    let device = match selector.choose(&found) {
        Ok(device) => device,
        Err(error) => {
            receipt.failure = Some(error.into());
            return receipt;
        }
    };
    for _ in 0..cycles {
        match cycle(handle, &device, &mut receipt).await {
            Ok(next) => handle = next,
            Err(error) => {
                receipt.failure = Some(error);
                break;
            }
        }
    }
    receipt
}
