//! JPEG marker-structure check, run before decoding.
//!
//! `zune-jpeg` decodes a stream that ends inside the last MCUs of the entropy-coded scan, filling
//! the missing blocks, even in strict mode. For live view a truncated frame must be dropped, not
//! shown, so the scan must be seen to end: this walks the marker structure and requires an `EOI`.
//! It also rejects undefined marker codes inside the scan (a corrupted `0xFF` escape). It does not
//! decode anything; it is total over arbitrary bytes. Known limit: a forged `FF D9` inside the scan
//! reads as an early end of image, and bytes after `EOI` are ignored, so such a frame passes.
//!
//! References:
//! - ITU-T T.81 B.1.1.2 (markers), B.1.1.5 (entropy-coded segments, byte stuffing)

use thiserror::Error;

/// Why the marker structure is not a complete JPEG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(super) enum StructureError {
    /// The data does not start with `SOI` (`FF D8`).
    #[error("missing SOI marker")]
    NoSoi,
    /// The data ends before `EOI`: truncated.
    #[error("data ends at byte '{0}' before the EOI marker")]
    Truncated(usize),
    /// A marker code that cannot appear at this point.
    #[error("invalid marker 'FF {marker:02X}' at byte '{at}'")]
    BadMarker {
        /// Offset of the `0xFF`.
        at: usize,
        /// The code byte after it.
        marker: u8,
    },
    /// A segment length shorter than its own two length bytes.
    #[error("invalid segment length at byte '{0}'")]
    BadLength(usize),
}

const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;

/// Check that `jpeg` is `SOI`, well-formed segments and scans, then `EOI`. Bytes after `EOI` are
/// ignored.
pub(super) fn validate(jpeg: &[u8]) -> Result<(), StructureError> {
    if jpeg.get(..2) != Some(&[0xFF, SOI]) {
        return Err(StructureError::NoSoi);
    }
    let mut pos = 2;
    loop {
        // Marker mode: `FF`, optional fill `FF`s, then the code.
        let at = pos;
        match jpeg.get(pos) {
            Some(0xFF) => {}
            Some(&other) => return Err(StructureError::BadMarker { at, marker: other }),
            None => return Err(StructureError::Truncated(pos)),
        }
        pos += 1;
        let code = loop {
            match jpeg.get(pos) {
                Some(0xFF) => pos += 1,
                Some(&code) => break code,
                None => return Err(StructureError::Truncated(pos)),
            }
        };
        pos += 1;
        match code {
            EOI => return Ok(()),
            // Standalone markers without a length: TEM, RSTn, SOI.
            0x01 | 0xD0..=0xD8 => {}
            // Everything from SOF0 to COM carries a length. Below 0xC0 (and 0x00) nothing is defined.
            0xC0..=0xFE => {
                let len = match jpeg.get(pos..pos + 2) {
                    Some(&[hi, lo]) => usize::from(u16::from_be_bytes([hi, lo])),
                    _ => return Err(StructureError::Truncated(pos)),
                };
                if len < 2 {
                    return Err(StructureError::BadLength(pos));
                }
                pos += len;
                if pos > jpeg.len() {
                    return Err(StructureError::Truncated(jpeg.len()));
                }
                if code == SOS {
                    pos = skip_entropy_data(jpeg, pos)?;
                }
            }
            marker => return Err(StructureError::BadMarker { at, marker }),
        }
    }
}

/// Advance past entropy-coded data starting at `pos` to the `FF` of the next real marker.
/// `FF 00` (stuffed byte), `FF D0..=D7` (restart) and `FF FF` (fill) stay inside the data.
fn skip_entropy_data(jpeg: &[u8], mut pos: usize) -> Result<usize, StructureError> {
    loop {
        let Some(&byte) = jpeg.get(pos) else {
            return Err(StructureError::Truncated(pos));
        };
        if byte != 0xFF {
            pos += 1;
            continue;
        }
        match jpeg.get(pos + 1) {
            Some(0x00 | 0xD0..=0xD7) => pos += 2,
            Some(0xFF) => pos += 1,
            Some(_) => return Ok(pos),
            None => return Err(StructureError::Truncated(pos + 1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::media::jpeg::fixtures::QUADRANTS_420;

    #[test]
    fn accepts_the_fixture_and_ignores_trailing_bytes() {
        assert_eq!(validate(&QUADRANTS_420), Ok(()));
        let mut padded = QUADRANTS_420.to_vec();
        padded.extend_from_slice(&[0, 0, 0xAB]);
        assert_eq!(validate(&padded), Ok(()));
    }

    #[test]
    fn reports_the_failure_kind() {
        assert_eq!(validate(&[]), Err(StructureError::NoSoi));
        assert_eq!(validate(&[0xFF, 0xD8]), Err(StructureError::Truncated(2)));
        assert_eq!(
            validate(&[0xFF, 0xD8, 0x12]),
            Err(StructureError::BadMarker {
                at: 2,
                marker: 0x12
            })
        );
        assert_eq!(
            validate(&[0xFF, 0xD8, 0xFF, 0x7F]),
            Err(StructureError::BadMarker {
                at: 2,
                marker: 0x7F
            })
        );
        assert_eq!(
            validate(&[0xFF, 0xD8, 0xFF, 0xFE, 0, 1]),
            Err(StructureError::BadLength(4))
        );
        assert_eq!(
            validate(&[0xFF, 0xD8, 0xFF, 0xFE, 0, 9, 1]),
            Err(StructureError::Truncated(7))
        );
        assert_eq!(validate(&[0xFF, 0xD8, 0xFF, 0xD9]), Ok(()));
    }

    #[test]
    fn stuffing_restarts_and_fill_stay_inside_the_scan() {
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02, // SOS, empty header
            0x12, 0xFF, 0x00, 0xFF, 0xD0, 0x34, // data: stuffed byte, RST0
            0xFF, 0xFF, 0xFF, 0xD9, // fill bytes, EOI
        ];
        assert_eq!(validate(&jpeg), Ok(()));
        let mut bad = jpeg;
        bad[8] = 0x7F; // `FF 7F` is not a marker
        assert_eq!(
            validate(&bad),
            Err(StructureError::BadMarker {
                at: 7,
                marker: 0x7F
            })
        );
    }

    proptest! {
        #[test]
        fn never_panics_on_arbitrary_bytes(data in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = validate(&data);
        }

        #[test]
        fn never_panics_with_a_valid_prefix(tail in proptest::collection::vec(any::<u8>(), 0..64)) {
            let mut data = vec![0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02];
            data.extend(tail);
            let _ = validate(&data);
        }
    }
}
