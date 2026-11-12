//! H.264 Annex B byte stream: splitting into NAL units and reading the SPS header.
//!
//! An access unit leaves the encoder as `00 00 01` / `00 00 00 01` start codes followed by NAL
//! units. The RTP packetizer, the RTSP `DESCRIBE` response (`sprop-parameter-sets`) and the
//! tests all need to take that apart. Everything here is total: any byte slice yields a result,
//! never a panic.
//!
//! References:
//! - ITU-T H.264, Annex B "Byte stream format" and section 7.3.1 "NAL unit syntax"
//! - [RFC 6184 section 8.1](https://www.rfc-editor.org/rfc/rfc6184#section-8.1), `profile-level-id`

/// NAL unit type of a coded slice of a non-IDR picture.
pub const NAL_SLICE: u8 = 1;
/// NAL unit type of a coded slice of an IDR picture.
pub const NAL_IDR: u8 = 5;
/// NAL unit type of supplemental enhancement information.
pub const NAL_SEI: u8 = 6;
/// NAL unit type of a sequence parameter set.
pub const NAL_SPS: u8 = 7;
/// NAL unit type of a picture parameter set.
pub const NAL_PPS: u8 = 8;
/// NAL unit type of an access unit delimiter.
pub const NAL_AUD: u8 = 9;

/// Iterator over the NAL units of an Annex B byte stream, each without its start code.
///
/// Bytes before the first start code are ignored. Zero bytes that precede the next start code
/// (the leading zero of a four-byte start code, `trailing_zero_8bits`) are not part of the NAL
/// unit, and empty NAL units are skipped.
#[derive(Debug, Clone)]
pub struct NalUnits<'a> {
    data: &'a [u8],
    pos: usize,
}

/// Split `data` into NAL units. See [`NalUnits`].
#[must_use]
pub fn nal_units(data: &[u8]) -> NalUnits<'_> {
    NalUnits { data, pos: 0 }
}

/// Offset of the next `00 00 01` at or after `from`.
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?
        .windows(3)
        .position(|w| w == [0, 0, 1])
        .map(|i| i + from)
}

impl<'a> Iterator for NalUnits<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            let body = find_start_code(self.data, self.pos)? + 3;
            let next = find_start_code(self.data, body);
            let end = next.unwrap_or(self.data.len());
            self.pos = end;
            let nal = self.data.get(body..end).unwrap_or_default();
            let used = nal.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
            if let Some(nal) = nal.get(..used).filter(|n| !n.is_empty()) {
                return Some(nal);
            }
            next?;
        }
    }
}

/// The `nal_unit_type` of a NAL unit, or `None` when it is empty or its forbidden bit is set.
#[must_use]
pub fn nal_unit_type(nal: &[u8]) -> Option<u8> {
    let header = *nal.first()?;
    (header & 0x80 == 0).then_some(header & 0x1F)
}

/// Whether the access unit in `data` contains an IDR slice.
#[must_use]
pub fn contains_idr(data: &[u8]) -> bool {
    nal_units(data).any(|nal| nal_unit_type(nal) == Some(NAL_IDR))
}

/// The three bytes after the NAL header of an SPS: `profile_idc`, the constraint flags byte and
/// `level_idc`. Hex-encoded they are the RFC 6184 `profile-level-id` parameter.
///
/// Returns `None` when `sps` is not an SPS NAL unit or is shorter than four bytes.
#[must_use]
pub fn sps_profile_level_id(sps: &[u8]) -> Option<[u8; 3]> {
    if nal_unit_type(sps)? != NAL_SPS {
        return None;
    }
    match sps {
        [_, profile, constraints, level, ..] => Some([*profile, *constraints, *level]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let stream = [
            0, 0, 0, 1, 0x67, 1, 2, // SPS
            0, 0, 1, 0x68, 3, // PPS
            0, 0, 0, 1, 0x65, 4, 5, 6, // IDR
        ];
        let nals: Vec<_> = nal_units(&stream).collect();
        assert_eq!(
            nals,
            [&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5, 6][..]]
        );
        let types: Vec<_> = nals.iter().map(|n| nal_unit_type(n)).collect();
        assert_eq!(types, [Some(NAL_SPS), Some(NAL_PPS), Some(NAL_IDR)]);
    }

    #[test]
    fn ignores_leading_garbage_trailing_zeros_and_empty_nals() {
        let stream = [
            9, 9, 0, 0, 1, 0, 0, 1, 0x41, 7, 0, 0, 0, 0, 0, 1, 0x41, 8, 0, 0,
        ];
        let nals: Vec<_> = nal_units(&stream).collect();
        assert_eq!(nals, [&[0x41, 7][..], &[0x41, 8][..]]);
    }

    #[test]
    fn no_start_code_means_no_nals() {
        assert_eq!(nal_units(&[]).count(), 0);
        assert_eq!(nal_units(&[1, 2, 3, 4]).count(), 0);
        assert_eq!(nal_units(&[0, 0, 1]).count(), 0);
        assert_eq!(nal_units(&[0, 0, 0, 0]).count(), 0);
    }

    #[test]
    fn forbidden_bit_has_no_type() {
        assert_eq!(nal_unit_type(&[0x80 | 5]), None);
        assert_eq!(nal_unit_type(&[]), None);
        assert_eq!(nal_unit_type(&[0x65]), Some(NAL_IDR));
    }

    #[test]
    fn contains_idr_finds_type_five_only() {
        assert!(contains_idr(&[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x65, 2]));
        assert!(!contains_idr(&[0, 0, 0, 1, 0x41, 1, 0, 0, 1, 0x41, 2]));
    }

    #[test]
    fn profile_level_id_reads_sps_bytes() {
        assert_eq!(
            sps_profile_level_id(&[0x67, 0x42, 0xC0, 0x1F, 0xAA]),
            Some([0x42, 0xC0, 0x1F])
        );
        assert_eq!(sps_profile_level_id(&[0x67, 0x42, 0xC0]), None);
        assert_eq!(sps_profile_level_id(&[0x68, 0x42, 0xC0, 0x1F]), None);
        assert_eq!(sps_profile_level_id(&[]), None);
    }

    proptest! {
        #[test]
        fn never_panics_on_arbitrary_bytes(data in proptest::collection::vec(any::<u8>(), 0..512)) {
            for nal in nal_units(&data) {
                prop_assert!(!nal.is_empty());
                prop_assert_ne!(nal.last(), Some(&0));
                let _ = nal_unit_type(nal);
                let _ = sps_profile_level_id(nal);
            }
            let _ = contains_idr(&data);
        }

        #[test]
        fn nals_roundtrip_through_start_codes(
            bodies in proptest::collection::vec(
                proptest::collection::vec(1_u8..=255, 1..32), 0..8
            )
        ) {
            let mut stream = Vec::new();
            for body in &bodies {
                stream.extend_from_slice(&[0, 0, 0, 1]);
                stream.extend_from_slice(body);
            }
            let split: Vec<Vec<u8>> = nal_units(&stream).map(<[u8]>::to_vec).collect();
            prop_assert_eq!(split, bodies);
        }
    }
}
