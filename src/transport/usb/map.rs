//! Pure mappings between `nusb` types and this crate's types.
//!
//! Kept apart from `nusb_backend` so the real-hardware glue there stays
//! a line or two per call and every decision is tested here.
//!
//! References:
//! - nusb 0.2 `TransferError` and `ErrorKind`: <https://docs.rs/nusb/0.2>

use nusb::transfer::TransferError;
use nusb::{ErrorKind, Speed};

use super::UsbError;
use super::camera::BusSpeed;
use super::port::PortError;

/// Map a reported connection speed.
#[must_use]
pub fn speed(speed: Option<Speed>) -> BusSpeed {
    match speed {
        Some(Speed::Low) => BusSpeed::Low,
        Some(Speed::Full) => BusSpeed::Full,
        Some(Speed::High) => BusSpeed::High,
        Some(Speed::Super) => BusSpeed::Super,
        Some(Speed::SuperPlus) => BusSpeed::SuperPlus,
        _ => BusSpeed::Unknown,
    }
}

/// Map an `nusb` open, enumeration or claim error.
#[must_use]
pub fn usb_error(kind: ErrorKind, message: String) -> UsbError {
    match kind {
        ErrorKind::Disconnected => UsbError::Disconnected,
        ErrorKind::Busy => UsbError::Busy,
        ErrorKind::PermissionDenied => UsbError::PermissionDenied,
        _ => UsbError::Other(message),
    }
}

/// Map an `nusb` error on an already-open device.
#[must_use]
pub fn port_error_from_kind(kind: ErrorKind, message: String) -> PortError {
    match kind {
        ErrorKind::Disconnected => PortError::Disconnected,
        _ => PortError::Other(message),
    }
}

/// Map a failed transfer.
#[must_use]
pub fn transfer_error(error: TransferError) -> PortError {
    match error {
        TransferError::Stall => PortError::Stall,
        TransferError::Disconnected => PortError::Disconnected,
        TransferError::Cancelled => PortError::Timeout,
        other => PortError::Other(other.to_string()),
    }
}

/// Copy a completed transfer into the caller's buffer; returns the bytes the device sent.
///
/// The count is what the device transferred, even when `destination` is smaller, so a caller
/// can detect an oversized transfer.
#[must_use]
pub fn copy_received(destination: &mut [u8], received: &[u8]) -> usize {
    for (slot, byte) in destination.iter_mut().zip(received) {
        *slot = *byte;
    }
    received.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_every_speed() {
        assert_eq!(speed(Some(Speed::Low)), BusSpeed::Low);
        assert_eq!(speed(Some(Speed::Full)), BusSpeed::Full);
        assert_eq!(speed(Some(Speed::High)), BusSpeed::High);
        assert_eq!(speed(Some(Speed::Super)), BusSpeed::Super);
        assert_eq!(speed(Some(Speed::SuperPlus)), BusSpeed::SuperPlus);
        assert_eq!(speed(None), BusSpeed::Unknown);
    }

    #[test]
    fn maps_open_errors() {
        let map = |kind| usb_error(kind, "detail".into());
        assert_eq!(map(ErrorKind::Disconnected), UsbError::Disconnected);
        assert_eq!(map(ErrorKind::Busy), UsbError::Busy);
        assert_eq!(map(ErrorKind::PermissionDenied), UsbError::PermissionDenied);
        assert_eq!(map(ErrorKind::NotFound), UsbError::Other("detail".into()));
        assert_eq!(map(ErrorKind::Other), UsbError::Other("detail".into()));
    }

    #[test]
    fn maps_device_errors() {
        assert_eq!(
            port_error_from_kind(ErrorKind::Disconnected, "x".into()),
            PortError::Disconnected
        );
        assert_eq!(
            port_error_from_kind(ErrorKind::Busy, "x".into()),
            PortError::Other("x".into())
        );
    }

    #[test]
    fn maps_transfer_errors() {
        assert_eq!(transfer_error(TransferError::Stall), PortError::Stall);
        assert_eq!(
            transfer_error(TransferError::Disconnected),
            PortError::Disconnected
        );
        assert_eq!(transfer_error(TransferError::Cancelled), PortError::Timeout);
        assert_eq!(
            transfer_error(TransferError::Fault),
            PortError::Other("hardware fault or protocol violation".into())
        );
    }

    #[test]
    fn copies_what_fits_and_reports_what_arrived() {
        let mut small = [0_u8; 2];
        assert_eq!(copy_received(&mut small, &[1, 2, 3]), 3);
        assert_eq!(small, [1, 2]);
        let mut large = [9_u8; 4];
        assert_eq!(copy_received(&mut large, &[1, 2]), 2);
        assert_eq!(large, [1, 2, 9, 9]);
    }
}
