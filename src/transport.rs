//! Byte transports to a camera: the [`Transport`] trait and the simulated camera in [`sim`].
//!
//! [`Transport`] is generic rather than object-safe: native async trait methods avoid a boxed
//! future (one heap allocation) on every transfer, and the session engine is monomorphised
//! over exactly one transport per process, so dynamic dispatch buys nothing. Callers that need
//! to erase the type can wrap a transport in their own enum.
//!
//! Implementations must enforce the supplied timeout and preserve USB transfer boundaries,
//! including zero-length packets, because the PTP data phase depends on both.
//!
//! References:
//! - USB Still Image Capture Device Definition 1.0, sections 5-7

pub mod sim;
pub mod usb;

use std::time::Duration;

use thiserror::Error;

/// A USB endpoint that can be cleared after a stall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// Host-to-device bulk endpoint.
    BulkOut,
    /// Device-to-host bulk endpoint.
    BulkIn,
    /// Device-to-host interrupt endpoint.
    InterruptIn,
}

/// An error reported by a camera transport.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TransportError {
    /// The endpoint is halted.
    #[error("endpoint stalled")]
    Stall,
    /// The operation exceeded its deadline.
    #[error("transport operation timed out")]
    Timeout,
    /// The camera is no longer connected.
    #[error("camera disconnected")]
    Disconnected,
    /// The transport could not perform the requested operation.
    #[error("transport error: {0}")]
    Other(String),
}

/// Asynchronous camera I/O, with one timeout per operation.
///
/// Receives return the number of bytes transferred, including zero for a zero-length packet.
/// A successful send transfers the entire slice; an implementation must report partial writes
/// as an error rather than silently truncating them.
pub trait Transport {
    /// Send one bulk transfer.
    ///
    /// # Errors
    ///
    /// Returns a [`TransportError`] if the transfer fails or times out.
    fn bulk_out(
        &mut self, bytes: &[u8], timeout: Duration,
    ) -> impl Future<Output = Result<(), TransportError>> + Send;

    /// Receive one bulk transfer into `bytes`.
    ///
    /// # Errors
    ///
    /// Returns a [`TransportError`] if the transfer fails or times out.
    fn bulk_in(
        &mut self, bytes: &mut [u8], timeout: Duration,
    ) -> impl Future<Output = Result<usize, TransportError>> + Send;

    /// Receive one interrupt transfer into `bytes`.
    ///
    /// # Errors
    ///
    /// Returns a [`TransportError`] if the transfer fails or times out.
    fn interrupt_in(
        &mut self, bytes: &mut [u8], timeout: Duration,
    ) -> impl Future<Output = Result<usize, TransportError>> + Send;

    /// Clear a halted endpoint.
    ///
    /// # Errors
    ///
    /// Returns a [`TransportError`] if recovery fails or times out.
    fn clear_halt(
        &mut self, endpoint: Endpoint, timeout: Duration,
    ) -> impl Future<Output = Result<(), TransportError>> + Send;

    /// Reset the device and its protocol state.
    ///
    /// # Errors
    ///
    /// Returns a [`TransportError`] if reset fails or times out.
    fn reset(
        &mut self, timeout: Duration,
    ) -> impl Future<Output = Result<(), TransportError>> + Send;
}
