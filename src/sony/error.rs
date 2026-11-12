//! The Sony driver's error type.
//!
//! References:
//! - libgphoto2 `camlibs/ptp2/library.c`, `ptp.c` (Sony sections)
//! - `watsonmw/alphawire` `src/aw/aw-control.c`
//! - `docs/architecture.md`

use std::time::Duration;

use thiserror::Error;

use super::protocol::Protocol;
use crate::ptp::session::SessionError;
use crate::ptp::{CodecError, DevicePropCode};

/// Why a live-view object could not be turned into a JPEG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum LiveViewError {
    /// The object is shorter than the Sony header.
    #[error("live-view object of '{0}' bytes is shorter than its header")]
    Truncated(usize),
    /// The header places the JPEG inside the header itself.
    #[error("live-view JPEG offset '{0}' lies inside the header")]
    OffsetInsideHeader(u32),
    /// The header places the JPEG partly or wholly outside the object.
    #[error(
        "live-view JPEG at offset '{offset}' with size '{size}' exceeds the object of '{len}' bytes"
    )]
    OutOfBounds {
        /// Announced JPEG offset.
        offset: u32,
        /// Announced JPEG size.
        size: u32,
        /// Size of the whole object.
        len: usize,
    },
    /// The payload does not start with the JPEG start-of-image marker.
    #[error("live-view payload is not a JPEG")]
    NotJpeg,
}

/// Why a Sony operation failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SonyError {
    /// The PTP session or transport failed.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// A payload from the camera could not be decoded, or a value could not be encoded.
    #[error("malformed payload: {0}")]
    Codec(#[from] CodecError),
    /// The camera does not advertise the Sony SDIO extension. It is probably not in PC Remote
    /// mode.
    #[error("camera does not advertise the Sony SDIO extension; is it in PC Remote mode?")]
    SdioUnsupported,
    /// The camera answered `SDIOGetExtDeviceInfo` with a protocol generation this driver
    /// cannot speak.
    #[error("unsupported Sony protocol version '{0}'")]
    UnsupportedProtocol(u16),
    /// The camera refused the requested protocol generation, and the older one too.
    #[error(
        "camera refused protocol '{requested:?}' ({requested_error}) and then '{fallback:?}' ({fallback_error})"
    )]
    ProtocolRefused {
        /// The generation requested first.
        requested: Protocol,
        /// Why it failed.
        requested_error: Box<SonyError>,
        /// The older generation requested second.
        fallback: Protocol,
        /// Why that failed.
        fallback_error: Box<SonyError>,
    },
    /// `SDIOGetExtDeviceInfo` kept answering empty or busy.
    #[error("extended device info unavailable after '{attempts}' attempts")]
    ExtInfoUnavailable {
        /// Attempts made.
        attempts: u32,
    },
    /// The property is not in the camera's property table.
    #[error("unknown property '{0}'")]
    UnknownProperty(DevicePropCode),
    /// The property is read-only.
    #[error("property '{0}' is read-only")]
    NotWritable(DevicePropCode),
    /// The value's datatype is not the property's.
    #[error("property '{code}' has datatype '{expected:#06x}', not '{actual:#06x}'")]
    TypeMismatch {
        /// The property.
        code: DevicePropCode,
        /// Datatype code the property has.
        expected: u16,
        /// Datatype code of the supplied value.
        actual: u16,
    },
    /// The camera does not list the control.
    #[error("unsupported control '{0}'")]
    UnsupportedControl(DevicePropCode),
    /// The camera cannot provide live view.
    #[error("camera does not support live view")]
    LiveViewUnsupported,
    /// The camera cannot capture stills remotely.
    #[error("camera does not support remote still capture")]
    CaptureUnsupported,
    /// Live view did not become ready in time.
    #[error("live view is not ready")]
    LiveViewNotReady,
    /// The live-view object was malformed.
    #[error("malformed live-view frame: {0}")]
    LiveView(#[from] LiveViewError),
    /// No captured image arrived in time.
    #[error("no captured image arrived within '{timeout:?}'")]
    CaptureTimeout {
        /// How long the wait lasted.
        timeout: Duration,
    },
}
