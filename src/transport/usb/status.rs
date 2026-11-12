//! The still-image class "Get Device Status" reply.
//!
//! References:
//! - USB Still Image Capture Device Definition 1.0, section 5.2 (class-specific requests: Device Reset, Get Device Status)

use thiserror::Error;

/// Class request code: Device Reset.
pub const REQUEST_DEVICE_RESET: u8 = 0x66;
/// Class request code: Get Device Status.
pub const REQUEST_GET_DEVICE_STATUS: u8 = 0x67;
/// Largest reply `Get Device Status` asks for: length, code and a few parameters.
pub const STATUS_REPLY_LEN: u16 = 32;

/// The reply to "Get Device Status" is malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("malformed device status reply")]
pub struct StatusError;

/// Decoded "Get Device Status" reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceStatus {
    /// PTP response code; `0x2001` is OK and `0x2019` is Device Busy.
    pub code: u16,
    /// Little-endian parameters following the code (halted endpoint addresses).
    pub parameters: Vec<u32>,
}

impl DeviceStatus {
    /// Parse a reply: `u16` total length, `u16` code, then whole `u32` parameters.
    ///
    /// # Errors
    ///
    /// Returns [`StatusError`] for any reply that is short, whose length field disagrees with
    /// the bytes received, or that carries a partial parameter. Never panics.
    pub fn parse(bytes: &[u8]) -> Result<Self, StatusError> {
        let (head, rest) = bytes.split_first_chunk::<4>().ok_or(StatusError)?;
        let [l0, l1, c0, c1] = *head;
        let length = usize::from(u16::from_le_bytes([l0, l1]));
        if length != bytes.len() {
            return Err(StatusError);
        }
        let (parameters, tail) = rest.as_chunks::<4>();
        if !tail.is_empty() {
            return Err(StatusError);
        }
        Ok(Self {
            code: u16::from_le_bytes([c0, c1]),
            parameters: parameters.iter().map(|p| u32::from_le_bytes(*p)).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parses_ok_without_parameters() {
        assert_eq!(
            DeviceStatus::parse(&[4, 0, 0x01, 0x20]),
            Ok(DeviceStatus {
                code: 0x2001,
                parameters: vec![]
            })
        );
    }

    #[test]
    fn parses_parameters() {
        let bytes = [12, 0, 0x19, 0x20, 0x81, 0, 0, 0, 0x02, 0, 0, 0];
        assert_eq!(
            DeviceStatus::parse(&bytes),
            Ok(DeviceStatus {
                code: 0x2019,
                parameters: vec![0x81, 2]
            })
        );
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(DeviceStatus::parse(&[]), Err(StatusError));
        assert_eq!(DeviceStatus::parse(&[4, 0, 1]), Err(StatusError));
        assert_eq!(DeviceStatus::parse(&[8, 0, 1, 0x20]), Err(StatusError));
        assert_eq!(
            DeviceStatus::parse(&[6, 0, 1, 0x20, 1, 0]),
            Err(StatusError)
        );
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..64)) {
            let _ = DeviceStatus::parse(&data);
        }
    }
}
