//! RTP header inspection and RTCP sender reports (RFC 3550).
//!
//! References:
//! - [RFC 3550 section 5.1 (RTP header), section 6.4.1 (SR), section 6.5 (SDES)](https://www.rfc-editor.org/rfc/rfc3550)

use std::time::Duration;

/// Seconds between the NTP epoch (1900) and the Unix epoch (1970).
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;
/// The `CNAME` the server announces in every report.
pub const CNAME: &str = "tethercam";

/// The RTP header fields the server needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    /// Payload type.
    pub payload_type: u8,
    /// Sequence number.
    pub sequence: u16,
    /// RTP timestamp.
    pub timestamp: u32,
    /// Synchronization source.
    pub ssrc: u32,
    /// Payload octets after the header (CSRCs, extension) and before any padding.
    pub payload_len: usize,
}

/// Why an RTP packet could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RtpParseError {
    /// The fixed header is incomplete.
    #[error("truncated RTP header")]
    Header,
    /// The RTP version is not two.
    #[error("invalid RTP version")]
    Version,
    /// The CSRC list or extension is incomplete.
    #[error("truncated RTP CSRC list or extension")]
    Extension,
    /// Padding is zero or exceeds the payload.
    #[error("invalid RTP padding")]
    Padding,
}

impl RtpHeader {
    /// Parse and validate the RTP header, including CSRC list, extension and padding.
    ///
    /// # Errors
    ///
    /// Returns [`RtpParseError`] if the header or lengths are invalid.
    pub fn parse(packet: &[u8]) -> Result<Self, RtpParseError> {
        let head: &[u8; 12] = packet
            .get(..12)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or(RtpParseError::Header)?;
        let first = head[0];
        if first >> 6 != 2 {
            return Err(RtpParseError::Version);
        }
        let mut offset = 12 + 4 * usize::from(first & 0x0f);
        if first & 0x10 != 0 {
            let words: [u8; 2] = packet
                .get(offset + 2..offset + 4)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(RtpParseError::Extension)?;
            let words = usize::from(u16::from_be_bytes(words));
            offset += 4 + 4 * words;
        }
        let padding = if first & 0x20 != 0 {
            usize::from(packet.last().copied().unwrap_or_default())
        } else {
            0
        };
        if first & 0x20 != 0 && padding == 0 {
            return Err(RtpParseError::Padding);
        }
        let remaining = packet
            .len()
            .checked_sub(offset)
            .ok_or(RtpParseError::Extension)?;
        let payload_len = remaining
            .checked_sub(padding)
            .ok_or(RtpParseError::Padding)?;
        Ok(Self {
            payload_type: head[1] & 0x7f,
            sequence: u16::from_be_bytes([head[2], head[3]]),
            timestamp: u32::from_be_bytes([head[4], head[5], head[6], head[7]]),
            ssrc: u32::from_be_bytes([head[8], head[9], head[10], head[11]]),
            payload_len,
        })
    }
}

/// Convert time since the Unix epoch to a 64-bit NTP timestamp (32.32 fixed point).
#[must_use]
pub fn ntp_timestamp(since_unix_epoch: Duration) -> u64 {
    let secs = since_unix_epoch.as_secs().wrapping_add(NTP_UNIX_OFFSET) & 0xffff_ffff;
    let frac = (u64::from(since_unix_epoch.subsec_nanos()) << 32) / 1_000_000_000;
    (secs << 32) | frac
}

/// Counters and clock mapping for one sender report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderInfo {
    /// Synchronization source of the stream.
    pub ssrc: u32,
    /// NTP wallclock time, see [`ntp_timestamp`].
    pub ntp: u64,
    /// RTP timestamp corresponding to `ntp`.
    pub rtp_timestamp: u32,
    /// Packets sent so far.
    pub packet_count: u32,
    /// Payload octets sent so far.
    pub octet_count: u32,
}

/// Append a compound RTCP packet (sender report followed by an SDES with a `CNAME`) to `out`.
pub fn write_sender_report(info: &SenderInfo, out: &mut Vec<u8>) {
    // SR: V=2, P=0, RC=0, PT=200, length = 6 words after the header word.
    out.extend_from_slice(&[0x80, 200, 0, 6]);
    out.extend_from_slice(&info.ssrc.to_be_bytes());
    out.extend_from_slice(&info.ntp.to_be_bytes());
    out.extend_from_slice(&info.rtp_timestamp.to_be_bytes());
    out.extend_from_slice(&info.packet_count.to_be_bytes());
    out.extend_from_slice(&info.octet_count.to_be_bytes());

    // SDES: one chunk (SSRC, CNAME item, terminating null) padded to a 32-bit boundary.
    let cname = CNAME.as_bytes();
    let item_len = u8::try_from(cname.len()).unwrap_or(u8::MAX);
    let chunk = 4 + 2 + usize::from(item_len);
    // At least one terminating null, then pad to a 32-bit boundary.
    let padded = (chunk + 1).next_multiple_of(4);
    let words = u16::try_from(padded / 4).unwrap_or(0);
    out.extend_from_slice(&[0x81, 202]);
    out.extend_from_slice(&words.to_be_bytes());
    out.extend_from_slice(&info.ssrc.to_be_bytes());
    out.extend_from_slice(&[1, item_len]);
    out.extend_from_slice(cname.get(..usize::from(item_len)).unwrap_or_default());
    out.extend(std::iter::repeat_n(0_u8, padded - chunk));
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn parses_rtp_header() {
        let mut p = vec![0x80, 0xe0, 0x12, 0x34, 0, 0, 1, 0, 0xde, 0xad, 0xbe, 0xef];
        p.extend_from_slice(b"payload");
        assert_eq!(
            RtpHeader::parse(&p),
            Ok(RtpHeader {
                payload_type: 96,
                sequence: 0x1234,
                timestamp: 256,
                ssrc: 0xdead_beef,
                payload_len: 7,
            })
        );
        assert_eq!(RtpHeader::parse(&p[..11]), Err(RtpParseError::Header));
        p[0] = 0x40;
        assert_eq!(RtpHeader::parse(&p), Err(RtpParseError::Version));
    }

    #[test]
    fn payload_length_accounts_for_csrc_extension_and_padding() {
        let base = [0x80_u8, 96, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3];
        // Two CSRCs, then a one-word extension, 5 payload bytes, 3 bytes padding.
        let mut p = base.to_vec();
        p[0] = 0x80 | 0x20 | 0x10 | 2;
        p.extend_from_slice(&[0; 8]);
        p.extend_from_slice(&[0xbe, 0xde, 0, 1, 0, 0, 0, 0]);
        p.extend_from_slice(&[1, 2, 3, 4, 5, 0, 0, 3]);
        assert_eq!(RtpHeader::parse(&p).map(|h| h.payload_len), Ok(5));
        // Padding longer than the payload, zero padding count, truncated CSRC list or extension.
        let mut bad = p.clone();
        *bad.last_mut().unwrap() = 200;
        assert_eq!(RtpHeader::parse(&bad), Err(RtpParseError::Padding));
        *bad.last_mut().unwrap() = 0;
        assert_eq!(RtpHeader::parse(&bad), Err(RtpParseError::Padding));
        assert_eq!(RtpHeader::parse(&p[..14]), Err(RtpParseError::Extension));
        assert_eq!(RtpHeader::parse(&p[..22]), Err(RtpParseError::Extension));
    }

    #[test]
    fn rejects_csrc_count_exceeding_packet_length() {
        let packet = [0x81, 96, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3];
        assert_eq!(RtpHeader::parse(&packet), Err(RtpParseError::Extension));
    }

    #[test]
    fn ntp_conversion() {
        assert_eq!(ntp_timestamp(Duration::ZERO), NTP_UNIX_OFFSET << 32);
        let half = ntp_timestamp(Duration::from_millis(500));
        assert_eq!(half & 0xffff_ffff, 0x8000_0000);
    }

    #[test]
    fn sender_report_layout() {
        let mut out = Vec::new();
        write_sender_report(
            &SenderInfo {
                ssrc: 0x0102_0304,
                ntp: 0x1111_2222_3333_4444,
                rtp_timestamp: 90_000,
                packet_count: 7,
                octet_count: 700,
            },
            &mut out,
        );
        assert_eq!(&out[..4], &[0x80, 200, 0, 6]);
        assert_eq!(&out[4..8], &[1, 2, 3, 4]);
        assert_eq!(&out[8..16], &0x1111_2222_3333_4444_u64.to_be_bytes());
        assert_eq!(&out[16..20], &90_000_u32.to_be_bytes());
        assert_eq!(&out[20..24], &7_u32.to_be_bytes());
        assert_eq!(&out[24..28], &700_u32.to_be_bytes());
        // SDES: header, SSRC, CNAME item "tethercam" (9 bytes), null padding to 4.
        assert_eq!(&out[28..30], &[0x81, 202]);
        assert_eq!(out.len() % 4, 0);
        let words = usize::from(u16::from_be_bytes([out[30], out[31]]));
        assert_eq!(out.len(), 28 + 4 + words * 4);
        assert_eq!(&out[36..38], &[1, 9]);
        assert_eq!(&out[38..47], b"tethercam");
        assert!(out[47..].iter().all(|&b| b == 0));
    }

    proptest! {
        #[test]
        fn rtp_header_parse_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
            let _ = RtpHeader::parse(&bytes);
        }
    }
}
