//! H.264 RTP packetizer (RFC 6184, packetization mode 1).
//!
//! Input is an Annex B byte stream holding one access unit. Each NAL unit that fits the MTU is
//! sent as a single NAL unit packet; larger ones are split into FU-A fragments. The marker bit
//! is set on the last packet of the access unit.
//!
//! [`ParameterSets`] pulls SPS / PPS out of a stream for the `sprop-parameter-sets` SDP
//! attribute.
//!
//! References:
//! - [RTP payload format for H.264, RFC 6184](https://www.rfc-editor.org/rfc/rfc6184)
//! - [RTP, RFC 3550](https://www.rfc-editor.org/rfc/rfc3550)

use thiserror::Error;

use super::{ConfigError, PacketPool, Stream, StreamConfig};

/// NAL unit type of a sequence parameter set.
const NAL_SPS: u8 = 7;
/// NAL unit type of a picture parameter set.
const NAL_PPS: u8 = 8;
/// NAL unit type of an FU-A fragmentation unit.
const NAL_FU_A: u8 = 28;
/// Smallest MTU: RTP header, FU indicator, FU header and one payload byte.
const MIN_MTU: usize = super::HEADER_LEN + 3;

/// Errors from packetizing H.264.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum H264Error {
    /// Invalid stream configuration.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The access unit holds no NAL units (or no Annex B start code).
    #[error("no NAL units in Annex B input")]
    NoNalUnits,
}

/// Errors from extracting parameter sets.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParameterSetError {
    /// No SPS in the stream.
    #[error("no SPS in stream")]
    MissingSps,
    /// No PPS in the stream.
    #[error("no PPS in stream")]
    MissingPps,
}

/// Iterator over the NAL units of an Annex B byte stream.
///
/// Start codes (`00 00 01` and `00 00 00 01`) and trailing zero bytes are stripped. Bytes
/// before the first start code are ignored. Empty NAL units are skipped. Total: never panics.
#[derive(Debug, Clone)]
pub struct AnnexB<'a> {
    rest: &'a [u8],
}

/// Split `data` at its first `00 00 01` start code into the bytes before it and the bytes after
/// it. `None` when there is no start code.
fn split_at_start_code(data: &[u8]) -> Option<(&[u8], &[u8])> {
    data.windows(3)
        .position(|w| w == [0, 0, 1])
        .and_then(|at| data.split_at_checked(at))
        .and_then(|(before, from)| from.get(3..).map(|after| (before, after)))
}

impl<'a> AnnexB<'a> {
    /// Iterate over the NAL units in `data`.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        let rest = split_at_start_code(data).map_or_else(Default::default, |(_, after)| after);
        Self { rest }
    }
}

impl<'a> Iterator for AnnexB<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        while !self.rest.is_empty() {
            let (nal, tail) =
                split_at_start_code(self.rest).unwrap_or((self.rest, <&[u8]>::default()));
            self.rest = tail;
            let end = nal.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
            if let Some(trimmed) = nal.get(..end).filter(|n| !n.is_empty()) {
                return Some(trimmed);
            }
        }
        None
    }
}

/// NAL unit type (low five bits of the header byte).
fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1f)
}

/// Packetizer turning Annex B access units into RTP packets.
#[derive(Debug, Clone)]
pub struct H264Packetizer {
    stream: Stream,
}

impl H264Packetizer {
    /// Create a packetizer.
    ///
    /// # Errors
    /// [`H264Error::Config`] for a payload type over 127 or an MTU under 15.
    pub fn new(config: StreamConfig) -> Result<Self, H264Error> {
        Ok(Self {
            stream: Stream::new(config, MIN_MTU)?,
        })
    }

    /// Sequence number the next packet will carry.
    #[must_use]
    pub fn next_sequence(&self) -> u16 {
        self.stream.sequence()
    }

    /// Packetize one access unit into `pool`, replacing its contents.
    ///
    /// # Errors
    /// [`H264Error::NoNalUnits`] when `annex_b` holds no NAL unit; the pool is left empty.
    pub fn packetize(
        &mut self, annex_b: &[u8], timestamp: u32, pool: &mut PacketPool,
    ) -> Result<(), H264Error> {
        pool.clear();
        let max_payload = self.stream.max_payload();
        let mut nals = AnnexB::new(annex_b).peekable();
        if nals.peek().is_none() {
            return Err(H264Error::NoNalUnits);
        }
        while let Some(nal) = nals.next() {
            let last_nal = nals.peek().is_none();
            match nal.split_first() {
                Some((&header, body)) if nal.len() > max_payload => {
                    self.fragment(pool, header, body, last_nal, timestamp);
                }
                _ => self.stream.emit(pool, last_nal, timestamp, &[nal]),
            }
        }
        Ok(())
    }

    /// Split one NAL unit into FU-A fragments (RFC 6184 section 5.8).
    fn fragment(
        &mut self, pool: &mut PacketPool, header: u8, body: &[u8], last_nal: bool, timestamp: u32,
    ) {
        let indicator = (header & 0xe0) | NAL_FU_A;
        let kind = header & 0x1f;
        let chunk_len = self.stream.max_payload().saturating_sub(2).max(1);
        let mut chunks = body.chunks(chunk_len).peekable();
        let mut first = true;
        while let Some(chunk) = chunks.next() {
            let last = chunks.peek().is_none();
            let fu_header = (u8::from(first) << 7) | (u8::from(last) << 6) | kind;
            self.stream.emit(
                pool,
                last && last_nal,
                timestamp,
                &[&[indicator, fu_header], chunk],
            );
            first = false;
        }
    }
}

/// SPS and PPS NAL units found in a stream, for SDP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterSets {
    /// Distinct SPS NAL units, in stream order.
    pub sps: Vec<Vec<u8>>,
    /// Distinct PPS NAL units, in stream order.
    pub pps: Vec<Vec<u8>>,
}

impl ParameterSets {
    /// Collect the SPS and PPS NAL units of an Annex B stream.
    ///
    /// # Errors
    /// [`ParameterSetError`] when the stream has no SPS or no PPS.
    pub fn from_annex_b(annex_b: &[u8]) -> Result<Self, ParameterSetError> {
        let mut sps: Vec<Vec<u8>> = Vec::new();
        let mut pps: Vec<Vec<u8>> = Vec::new();
        for nal in AnnexB::new(annex_b) {
            let list = match nal_type(nal) {
                Some(NAL_SPS) => &mut sps,
                Some(NAL_PPS) => &mut pps,
                _ => continue,
            };
            if !list.iter().any(|known| known == nal) {
                list.push(nal.to_vec());
            }
        }
        if sps.is_empty() {
            return Err(ParameterSetError::MissingSps);
        }
        if pps.is_empty() {
            return Err(ParameterSetError::MissingPps);
        }
        Ok(Self { sps, pps })
    }

    /// The `sprop-parameter-sets` value: base64 of each SPS then each PPS, comma separated.
    #[must_use]
    pub fn sprop_parameter_sets(&self) -> String {
        let mut out = String::new();
        for nal in self.sps.iter().chain(&self.pps) {
            if !out.is_empty() {
                out.push(',');
            }
            base64_encode(nal, &mut out);
        }
        out
    }

    /// `profile-level-id` as six hex digits, from the first SPS (bytes 1 to 3 of the NAL).
    #[must_use]
    pub fn profile_level_id(&self) -> Option<String> {
        let sps = self.sps.first()?;
        let [_, a, b, c] = *sps.first_chunk::<4>()?;
        Some(format!("{a:02x}{b:02x}{c:02x}"))
    }
}

/// Standard base64 with padding.
fn base64_encode(data: &[u8], out: &mut String) {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let symbol = |bits: u32| {
        char::from(
            ALPHABET
                .get((bits & 0x3f) as usize)
                .copied()
                .unwrap_or(b'='),
        )
    };
    for chunk in data.chunks(3) {
        let mut group = 0u32;
        for i in 0..3 {
            group = (group << 8) | u32::from(chunk.get(i).copied().unwrap_or(0));
        }
        out.push(symbol(group >> 18));
        out.push(symbol(group >> 12));
        out.push(if chunk.len() > 1 {
            symbol(group >> 6)
        } else {
            '='
        });
        out.push(if chunk.len() > 2 { symbol(group) } else { '=' });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::{HEADER_LEN, Header};
    use proptest::prelude::*;

    fn config(mtu: usize) -> StreamConfig {
        StreamConfig {
            payload_type: 96,
            ssrc: 0x0102_0304,
            mtu,
            initial_sequence: 65_534,
        }
    }

    fn annex_b(nals: &[Vec<u8>], long_start: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            if long_start {
                out.push(0);
            }
            out.extend_from_slice(&[0, 0, 1]);
            out.extend_from_slice(nal);
        }
        out
    }

    /// Reassemble NAL units from packets, checking headers along the way.
    fn depacketize(pool: &PacketPool, mtu: usize, timestamp: u32, first_seq: u16) -> Vec<Vec<u8>> {
        let mut nals = Vec::new();
        let mut current: Option<Vec<u8>> = None;
        let count = pool.len();
        for (i, packet) in pool.iter().enumerate() {
            assert!(packet.len() <= mtu);
            let header = Header::decode(packet).unwrap();
            assert_eq!(header.payload_type, 96);
            assert_eq!(header.ssrc, 0x0102_0304);
            assert_eq!(header.timestamp, timestamp);
            assert_eq!(
                header.sequence,
                first_seq.wrapping_add(u16::try_from(i).unwrap())
            );
            assert_eq!(header.marker, i + 1 == count, "marker only on last packet");
            let payload = &packet[HEADER_LEN..];
            let nal_type = payload[0] & 0x1f;
            if nal_type == 28 {
                let fu = payload[1];
                if fu & 0x80 != 0 {
                    assert!(current.is_none());
                    current = Some(vec![(payload[0] & 0xe0) | (fu & 0x1f)]);
                }
                let nal = current.as_mut().unwrap();
                nal.extend_from_slice(&payload[2..]);
                if fu & 0x40 != 0 {
                    nals.push(current.take().unwrap());
                }
            } else {
                assert!(
                    (1..=23).contains(&nal_type),
                    "unexpected NAL type {nal_type}"
                );
                assert!(current.is_none());
                nals.push(payload.to_vec());
            }
        }
        assert!(current.is_none());
        nals
    }

    #[test]
    fn profile_level_id_needs_a_full_first_sps() {
        let sets = |sps: Vec<Vec<u8>>| ParameterSets {
            sps,
            pps: vec![vec![0x68]],
        };
        assert_eq!(sets(vec![]).profile_level_id(), None);
        assert_eq!(sets(vec![vec![0x67, 0x42, 0xc0]]).profile_level_id(), None);
        assert_eq!(
            sets(vec![vec![0x67, 0x42, 0xc0, 0x1f]]).profile_level_id(),
            Some("42c01f".to_owned())
        );
    }

    #[test]
    fn annex_b_splitting() {
        let data = [
            9, 9, // leading junk, ignored
            0, 0, 0, 1, 0x67, 1, 2, // 4-byte start code
            0, 0, 1, 0x68, 3, 0, 0, // 3-byte start code, trailing zeros
            0, 0, 1, // empty NAL
            0, 0, 0, 1, 0x65, 4,
        ];
        let nals: Vec<_> = AnnexB::new(&data).collect();
        assert_eq!(
            nals,
            vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4][..]]
        );
        assert_eq!(AnnexB::new(&[1, 2, 3]).count(), 0);
        assert_eq!(AnnexB::new(&[]).count(), 0);
    }

    #[test]
    fn single_nal_golden() {
        let mut packetizer = H264Packetizer::new(config(1400)).unwrap();
        let mut pool = PacketPool::new();
        let au = annex_b(
            &[vec![0x67, 0x42, 0x00, 0x1f], vec![0x68, 0xce, 0x3c]],
            true,
        );
        packetizer.packetize(&au, 0x1000, &mut pool).unwrap();
        assert_eq!(pool.len(), 2);
        assert_eq!(
            pool.get(0).unwrap(),
            [
                0x80, 0x60, 0xff, 0xfe, 0x00, 0x00, 0x10, 0x00, 1, 2, 3, 4, 0x67, 0x42, 0x00, 0x1f
            ]
        );
        assert_eq!(
            pool.get(1).unwrap(),
            [
                0x80, 0xe0, 0xff, 0xff, 0x00, 0x00, 0x10, 0x00, 1, 2, 3, 4, 0x68, 0xce, 0x3c
            ]
        );
        assert_eq!(packetizer.next_sequence(), 0);
        assert_eq!(
            depacketize(&pool, 1400, 0x1000, 65_534),
            vec![vec![0x67, 0x42, 0x00, 0x1f], vec![0x68, 0xce, 0x3c]]
        );
    }

    #[test]
    fn fu_a_golden() {
        // MTU 20 -> 8 payload bytes -> 6 NAL body bytes per fragment.
        let mut packetizer = H264Packetizer::new(config(20)).unwrap();
        let mut pool = PacketPool::new();
        let nal: Vec<u8> = std::iter::once(0x65).chain(1..=13).collect();
        packetizer
            .packetize(&annex_b(std::slice::from_ref(&nal), false), 7, &mut pool)
            .unwrap();
        let payloads: Vec<Vec<u8>> = pool.iter().map(|p| p[HEADER_LEN..].to_vec()).collect();
        assert_eq!(
            payloads,
            vec![
                vec![0x7c, 0x85, 1, 2, 3, 4, 5, 6],
                vec![0x7c, 0x05, 7, 8, 9, 10, 11, 12],
                vec![0x7c, 0x45, 13],
            ]
        );
        assert!(Header::decode(pool.get(2).unwrap()).unwrap().marker);
        assert!(!Header::decode(pool.get(1).unwrap()).unwrap().marker);
        assert_eq!(depacketize(&pool, 20, 7, 65_534), vec![nal]);
    }

    #[test]
    fn nal_exactly_at_mtu_is_single() {
        let mut packetizer = H264Packetizer::new(config(32)).unwrap();
        let mut pool = PacketPool::new();
        let nal: Vec<u8> = std::iter::once(0x41).chain(1..20).collect();
        assert_eq!(nal.len(), 20);
        packetizer
            .packetize(&annex_b(std::slice::from_ref(&nal), false), 0, &mut pool)
            .unwrap();
        assert_eq!(pool.len(), 1);
        let longer: Vec<u8> = std::iter::once(0x41).chain(1..21).collect();
        packetizer
            .packetize(&annex_b(std::slice::from_ref(&longer), false), 0, &mut pool)
            .unwrap();
        assert_eq!(pool.len(), 2);
        assert_eq!(depacketize(&pool, 32, 0, 65_535), vec![longer]);
    }

    #[test]
    fn errors() {
        assert_eq!(
            H264Packetizer::new(config(14)).err(),
            Some(H264Error::Config(ConfigError::MtuTooSmall {
                mtu: 14,
                min: 15
            }))
        );
        let mut packetizer = H264Packetizer::new(config(1400)).unwrap();
        let mut pool = PacketPool::new();
        pool.clear();
        assert_eq!(
            packetizer.packetize(&[1, 2, 3], 0, &mut pool),
            Err(H264Error::NoNalUnits)
        );
        assert!(pool.is_empty());
    }

    #[test]
    fn steady_state_reuses_pool() {
        let mut packetizer = H264Packetizer::new(config(1400)).unwrap();
        let mut pool = PacketPool::new();
        let nal: Vec<u8> = std::iter::once(0x65)
            .chain((0..40_000u32).map(|i| u8::try_from(i % 250 + 1).unwrap()))
            .collect();
        let au = annex_b(&[nal], true);
        packetizer.packetize(&au, 0, &mut pool).unwrap();
        let (data_ptr, cap) = (pool.data.as_ptr(), pool.data.capacity());
        packetizer.packetize(&au, 3000, &mut pool).unwrap();
        assert_eq!((pool.data.as_ptr(), pool.data.capacity()), (data_ptr, cap));
    }

    #[test]
    fn parameter_sets() {
        let sps = vec![0x67, 0x42, 0xc0, 0x1f, 0xda];
        let pps = vec![0x68, 0xce, 0x3c, 0x80];
        let au = annex_b(
            &[sps.clone(), pps.clone(), sps.clone(), vec![0x65, 1]],
            true,
        );
        let sets = ParameterSets::from_annex_b(&au).unwrap();
        assert_eq!(sets.sps, vec![sps]);
        assert_eq!(sets.pps, vec![pps]);
        assert_eq!(sets.sprop_parameter_sets(), "Z0LAH9o=,aM48gA==");
        assert_eq!(sets.profile_level_id().as_deref(), Some("42c01f"));
        assert_eq!(
            ParameterSets::from_annex_b(&annex_b(&[vec![0x68, 1]], false)),
            Err(ParameterSetError::MissingSps)
        );
        assert_eq!(
            ParameterSets::from_annex_b(&annex_b(&[vec![0x67, 1, 2, 3]], false)),
            Err(ParameterSetError::MissingPps)
        );
    }

    #[test]
    fn base64_vectors() {
        let mut out = String::new();
        for (input, want) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
        ] {
            out.clear();
            base64_encode(input, &mut out);
            assert_eq!(out, want);
        }
    }

    fn nal_strategy() -> impl Strategy<Value = Vec<u8>> {
        // Header byte with type 1..=23, body bytes nonzero so no start code can form.
        (
            0u8..4,
            1u8..24,
            proptest::collection::vec(1u8..=255, 0..5000),
        )
            .prop_map(|(nri, kind, body)| {
                let mut nal = vec![(nri << 5) | kind];
                nal.extend(body);
                nal
            })
    }

    proptest! {
        #[test]
        fn round_trips(
            nals in proptest::collection::vec(nal_strategy(), 1..6),
            mtu in 15usize..2000,
            long_start: bool,
            timestamp: u32,
            seq: u16,
        ) {
            let mut packetizer = H264Packetizer::new(StreamConfig {
                payload_type: 96,
                ssrc: 0x0102_0304,
                mtu,
                initial_sequence: seq,
            }).unwrap();
            let mut pool = PacketPool::new();
            packetizer.packetize(&annex_b(&nals, long_start), timestamp, &mut pool).unwrap();
            prop_assert_eq!(depacketize(&pool, mtu, timestamp, seq), nals);
        }

        #[test]
        fn annex_b_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = AnnexB::new(&bytes).count();
            let _ = ParameterSets::from_annex_b(&bytes);
            let mut packetizer = H264Packetizer::new(StreamConfig {
                payload_type: 96, ssrc: 1, mtu: 40, initial_sequence: 0,
            }).unwrap();
            let mut pool = PacketPool::new();
            let _ = packetizer.packetize(&bytes, 0, &mut pool);
        }
    }
}
