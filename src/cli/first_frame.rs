//! Sanitized camera session and first-frame failure categories for `probe` and `doctor`.
//!
//! Each cause is a fixed category chosen from the typed driver error and the stage it happened
//! in. Nothing the camera or the OS said is interpolated: no response codes, device strings,
//! identifiers or OS error text. Advice names only settings the docs already state, and where
//! the cause is not established it says so.
//!
//! References:
//! - `docs/macos-quickstart.md`, camera checklist and troubleshooting
//! - `docs/usb.md`, USB modes

use thiserror::Error;

use crate::ptp::session::SessionError;
use crate::sony::SonyError;
use crate::supervisor::PipelineError;
use crate::transport::TransportError;

#[cfg(test)]
#[path = "first_frame_tests.rs"]
mod tests;

/// A sanitized session or first-frame failure category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FirstFrameCause {
    /// The body did not advertise the Sony remote-control extension.
    #[error(
        "no first frame: the camera did not offer Sony remote control, probably not in PC Remote; check USB Connection is PC Remote"
    )]
    NotRemote,
    /// The handshake was refused.
    #[error(
        "no first frame: the camera refused the Sony remote-control handshake; check USB Connection is PC Remote and that the device is a supported Sony body"
    )]
    Refused,
    /// The camera says it cannot provide live view.
    #[error(
        "no first frame: the camera reports live view unavailable; the cause is not established, see the camera checklist in docs/macos-quickstart.md"
    )]
    LiveViewUnavailable,
    /// Live view never reported ready.
    #[error(
        "no first frame: live view never reported ready; the camera may be asleep or have left PC Remote (cause unestablished), check its screen and cable and the camera checklist in docs/macos-quickstart.md"
    )]
    NotReady,
    /// Live view reported ready but no frame came.
    #[error(
        "no first frame: live view reported ready but no frame arrived in time; retry, and report it if it repeats"
    )]
    NoFrame,
    /// The camera stopped answering.
    #[error("no first frame: the camera stopped answering; check the cable and that it is awake")]
    Timeout,
    /// The camera went away mid-check.
    #[error("no first frame: the camera disconnected during the check; check the cable and power")]
    Disconnected,
    /// A USB transfer failed.
    #[error("no first frame: a USB transfer failed; reseat the cable or power-cycle the camera")]
    Transport,
    /// The camera is busy with something else, or kept answering busy or empty.
    #[error(
        "no first frame: the camera is busy or not answering the handshake; retry shortly, and if it persists check whether another program holds it"
    )]
    Busy,
    /// The OS restricts camera access for this process.
    #[error(
        "no first frame: camera access is restricted for this process; see docs/macos-camera.md"
    )]
    Permission,
    /// The camera answered a command with an error status.
    #[error(
        "no first frame: the camera rejected a command, cause unestablished; if it repeats, confirm PC Remote mode and report it"
    )]
    Rejected,
    /// A payload could not be decoded.
    #[error(
        "no first frame: the camera sent data this version cannot decode, likely an unsupported body or firmware; report it"
    )]
    Protocol,
    /// A frame arrived but `run` would reject it: not decodable, or not carriable as MJPEG.
    #[error(
        "no first frame: a frame arrived but run would reject it as undecodable or not valid for MJPEG; report it"
    )]
    Decode,
    /// A decodable frame arrived, but the H.264 encoder refuses its size or the stream settings.
    #[error(
        "no first frame: a frame arrived but the H.264 encoder rejects its size or the `streams` settings; check `streams` in the configuration"
    )]
    Encoder,
    /// A frame `run` accepts arrived, but closing the session failed.
    #[error(
        "frame received and valid, but closing the camera session failed; cause unestablished, retry and reseat the cable or power-cycle the camera if it repeats"
    )]
    CloseFailed,
    /// Native work exceeded its deadline and remains in progress.
    #[cfg(feature = "macos-icc")]
    #[error(
        "no first frame: native camera work exceeded its deadline and is still finishing; wait for cleanup, check the cable and camera power, then retry"
    )]
    NativeDeadline,
    /// A sanitized native camera operation failure.
    #[cfg(feature = "macos-icc")]
    #[error("no first frame: native camera operation failed: {0}")]
    Native(crate::transport::icc::IccError),
    /// The native runtime cannot be reused.
    #[error("no first frame: native camera runtime quarantined; restart the process")]
    Quarantined,
    /// Native session cleanup failed.
    #[error("no first frame: camera session cleanup failed; restart the process and retry")]
    CleanupFailed,
    /// The USB data was malformed or out of sequence.
    #[error(
        "no first frame: the camera's USB data was malformed or out of sequence, cause unestablished (cable, port or an unsupported body); retry with another cable or port and report it if it repeats"
    )]
    Framing,
    /// None of the above.
    #[error(
        "no first frame: the check failed for a reason this version cannot classify; report it"
    )]
    Unclassified,
}

impl FirstFrameCause {
    /// Classify `error`. `waiting_for_frame` is true once live view reported ready and the frame
    /// fetch began, which is what tells "never ready" from "ready, no frame".
    pub(super) fn of(error: &SonyError, waiting_for_frame: bool) -> Self {
        match error {
            SonyError::SdioUnsupported => Self::NotRemote,
            // A refusal that is really a stall or timeout is named for what it was.
            SonyError::ProtocolRefused { fallback_error, .. } => match fallback_error.as_ref() {
                SonyError::Session(SessionError::Response { .. }) => Self::Refused,
                other => Self::of(other, waiting_for_frame),
            },
            SonyError::ExtInfoUnavailable { .. } => Self::Busy,
            SonyError::LiveViewUnsupported => Self::LiveViewUnavailable,
            SonyError::LiveViewNotReady if waiting_for_frame => Self::NoFrame,
            SonyError::LiveViewNotReady => Self::NotReady,
            SonyError::LiveView(_) => Self::Decode,
            SonyError::UnsupportedProtocol(_) | SonyError::Codec(_) => Self::Protocol,
            SonyError::Session(error) => Self::session(error),
            _ => Self::Unclassified,
        }
    }

    /// Classify what [`crate::supervisor::PipelineError`] says about the first frame.
    pub(super) fn of_pipeline(error: &PipelineError) -> Self {
        match error {
            PipelineError::Jpeg(_) | PipelineError::RtpJpeg(_) => Self::Decode,
            PipelineError::Encode(_) => Self::Encoder,
            _ => Self::Unclassified,
        }
    }

    fn session(error: &SessionError) -> Self {
        match error {
            SessionError::Transport(error) | SessionError::Unrecoverable(error) => {
                Self::transport(error)
            }
            SessionError::DeadlineExceeded(_) | SessionError::TooManyEmptyReads => Self::Timeout,
            SessionError::Response { code, .. }
                if *code == crate::ptp::session::code::DEVICE_BUSY =>
            {
                Self::Busy
            }
            SessionError::Response { .. } => Self::Rejected,
            SessionError::Container(_)
            | SessionError::UnexpectedContainer { .. }
            | SessionError::TransactionMismatch { .. }
            | SessionError::UnsupportedLength
            | SessionError::DataTooLarge { .. } => Self::Framing,
            #[cfg(feature = "macos-icc")]
            SessionError::Icc(error) => Self::icc(*error),
            _ => Self::Unclassified,
        }
    }

    fn transport(error: &TransportError) -> Self {
        match error {
            TransportError::Timeout => Self::Timeout,
            TransportError::Disconnected => Self::Disconnected,
            TransportError::Stall | TransportError::Other(_) => Self::Transport,
        }
    }

    #[cfg(feature = "macos-icc")]
    fn icc(error: crate::transport::icc::IccError) -> Self {
        use crate::transport::icc::IccError;
        match error {
            IccError::Permission => Self::Permission,
            IccError::Busy | IccError::AdmissionExpired => Self::Busy,
            IccError::DeadlineExceeded => Self::NativeDeadline,
            IccError::Removed => Self::Disconnected,
            IccError::CleanupFailure => Self::CleanupFailed,
            IccError::Quarantined => Self::Quarantined,
            other => Self::Native(other),
        }
    }
}
