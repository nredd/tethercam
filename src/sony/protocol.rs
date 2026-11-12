//! Protocol generations and the `SDIOGetExtDeviceInfo` dataset.
//!
//! Sony ships two generations of the extension: 2.00 for bodies earlier than 2020 and 3.00 for
//! later ones. The host asks for the newest in `SDIOGetExtDeviceInfo`; the camera answers with
//! the version it speaks, which may be lower. Both references agree on this ("cameras that dont
//! support it will return their max supported version"), so the driver never assumes the newest.
//!
//! References:
//! - libgphoto2 `camlibs/ptp2/ptp.c`, `ptp_sony_get_vendorpropcodes`
//! - `watsonmw/alphawire` `src/aw/aw-control.c`, `SDIO_GetExtDeviceInfo`

use crate::ptp::{CodecError, Reader};

/// A generation of the Sony SDIO extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Protocol {
    /// Version 2.00 (`0x00C8`), bodies earlier than 2020, e.g. the a7 III.
    V200,
    /// Version 3.00 (`0x012C`), 2020 and later bodies.
    V300,
}

impl Protocol {
    /// The version number sent and received on the wire.
    #[must_use]
    pub const fn version(self) -> u16 {
        match self {
            Self::V200 => 200,
            Self::V300 => 300,
        }
    }

    /// The next older generation to fall back to, if there is one.
    #[must_use]
    pub const fn older(self) -> Option<Self> {
        match self {
            Self::V200 => None,
            Self::V300 => Some(Self::V200),
        }
    }

    /// The generation a camera speaks when it reports `version`.
    ///
    /// Versions from 300 up use the 3.00 layout; 200 up to 300 use 2.00. Anything older has
    /// a layout nothing documents, so it is refused.
    #[must_use]
    pub const fn from_version(version: u16) -> Option<Self> {
        match version {
            0..200 => None,
            200..300 => Some(Self::V200),
            300.. => Some(Self::V300),
        }
    }
}

/// The `SDIOGetExtDeviceInfo` dataset: the camera's protocol version and the property and
/// control codes it supports.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExtDeviceInfo {
    /// Protocol version the camera speaks, as sent.
    pub version: u16,
    /// Supported property codes. Older bodies may mix operation and event codes in.
    pub properties: Vec<u16>,
    /// Supported control codes.
    pub controls: Vec<u16>,
}

impl ExtDeviceInfo {
    /// Decodes the dataset; trailing bytes are ignored.
    ///
    /// # Errors
    ///
    /// [`CodecError::Truncated`] when a field or array runs past the end.
    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let mut reader = Reader::new(bytes);
        Ok(Self {
            version: reader.u16()?,
            properties: reader.array(2, Reader::u16)?,
            controls: reader.array(2, Reader::u16)?,
        })
    }

    /// Whether `code` is listed as a property, event or operation.
    #[must_use]
    pub fn lists(&self, code: u16) -> bool {
        self.properties.contains(&code)
    }

    /// Whether `code` is a listed control.
    #[must_use]
    pub fn has_control(&self, code: u16) -> bool {
        self.controls.contains(&code)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn versions_map_to_generations() {
        assert_eq!(Protocol::from_version(199), None);
        assert_eq!(Protocol::from_version(200), Some(Protocol::V200));
        assert_eq!(Protocol::from_version(299), Some(Protocol::V200));
        assert_eq!(Protocol::from_version(300), Some(Protocol::V300));
        assert_eq!(Protocol::from_version(0xFFFF), Some(Protocol::V300));
        assert_eq!(Protocol::V200.version(), 200);
        assert_eq!(Protocol::V300.version(), 300);
        assert!(Protocol::V200 < Protocol::V300);
        assert_eq!(Protocol::V300.older(), Some(Protocol::V200));
        assert_eq!(Protocol::V200.older(), None);
    }

    fn sample() -> Vec<u8> {
        let mut bytes = vec![0xC8, 0x00];
        bytes.extend_from_slice(&2_u32.to_le_bytes());
        bytes.extend_from_slice(&[0x21, 0xD2, 0x0E, 0xD2]);
        bytes.extend_from_slice(&1_u32.to_le_bytes());
        bytes.extend_from_slice(&[0xC2, 0xD2]);
        bytes
    }

    #[test]
    fn decodes_both_lists() {
        let info = ExtDeviceInfo::decode(&sample()).unwrap();
        assert_eq!(info.version, 200);
        assert_eq!(info.properties, [0xD221, 0xD20E]);
        assert_eq!(info.controls, [0xD2C2]);
        assert!(info.lists(0xD221));
        assert!(!info.lists(0xD2C2));
        assert!(info.has_control(0xD2C2));
        assert!(!info.has_control(0xD221));
    }

    #[test]
    fn trailing_bytes_are_ignored_and_truncation_is_an_error() {
        let mut bytes = sample();
        bytes.extend_from_slice(&[0; 5]);
        assert!(ExtDeviceInfo::decode(&bytes).is_ok());
        for cut in 0..sample().len() {
            assert!(
                ExtDeviceInfo::decode(&sample()[..cut]).is_err(),
                "cut at {cut}"
            );
        }
    }

    proptest! {
        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
            let _ = ExtDeviceInfo::decode(&bytes);
        }
    }
}
