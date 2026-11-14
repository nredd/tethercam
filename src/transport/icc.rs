//! Experimental `ImageCaptureCore` PTP sessions for macOS (feature `macos-icc`, off by default).
//!
//! `ImageCaptureCore` (ICC) provides a cooperative camera session while Apple's camera helpers
//! keep running, so raw USB claiming is not needed. ICC owns session negotiation and sends PTP
//! commands at transaction level, so this is an [`IccSession`] implementing
//! [`PtpSession`](crate::ptp::session::PtpSession), not a [`Transport`](super::Transport). The
//! Sony handshake, property parsing and operations run unchanged above it.
//!
//! Layers, each testable without a camera:
//! - [`Boundary`] and [`Lease`]: the blocking native API as traits
//! - [`run_with`] and [`IccHandle`]: a single-flight bridge from async callers to the one
//!   thread that owns the boundary
//! - [`IccSession`]: validation, typed failure categories and the lifecycle state machine
//! - [`live_view_probe`]: the hardware-owner acceptance consumer over the production stack
//! - [`classify`]: total classification of tagged native events
//! - `native` (macOS only): the thin delegation to the `image-capture` crate, plus `run` for
//!   its main-thread entry point
//!
//! Status: an AI-assisted independent source review of the native crate (not a certification)
//! and one short real-camera sample of that crate. This adapter has not been run on hardware.
//! No production support claim. See `docs/macos-camera.md`.
//!
//! References:
//! - `docs/macos-camera.md`
//! - <https://github.com/nredd/image-capture>

mod boundary;
mod bridge;
mod error;
mod event;
#[cfg(target_os = "macos")]
mod native;
mod probe;
mod sample;
mod session;

#[cfg(test)]
pub(crate) mod fake;
#[cfg(test)]
mod tests;

pub use boundary::{
    Boundary, IccDeviceId, IccReply, IccTimeout, Lease, MAX_DATA_BYTES, MAX_ID_BYTES, MAX_WAIT,
};
pub use bridge::{IccHandle, run_with};
pub use error::{Effect, IccError};
pub use event::{Notice, classify};
#[cfg(target_os = "macos")]
pub use native::run;

pub use probe::{LiveReceipt, ProbeError, live_view_probe};
pub use sample::{DISCOVERY_WINDOW, Receipt, SampleError, Selector, read_only_sample};
pub use session::IccSession;

/// The main-thread native entry point exists only on macOS. Elsewhere there is no native runtime
/// to start: this reports [`IccError::Runtime`] without running `user` or touching anything.
///
/// # Errors
///
/// Always [`IccError::Runtime`].
#[cfg(not(target_os = "macos"))]
pub fn run<F, T>(_user: F) -> Result<T, IccError>
where
    F: FnOnce(IccHandle) -> T + Send + 'static,
    T: Send + 'static,
{
    Err(IccError::Runtime)
}
