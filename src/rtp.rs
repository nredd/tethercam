//! RTP packetizers for H.264 and JPEG.
//!
//! - [`h264`]: Annex B byte stream in, RFC 6184 packets out (single NAL unit and FU-A)
//! - [`jpeg`]: baseline JPEG frame in, RFC 2435 packets out
//! - [`Header`]: the RFC 3550 fixed header
//! - [`PacketPool`]: reusable packet storage; packetizers write into it and, once it has
//!   grown to the steady-state frame size, allocate nothing per packet
//! - [`timestamp_from_instants`]: 90 kHz timestamps from frame instants
//!
//! This layer does no I/O. `rtsp` owns the sockets and sends what the pool holds.
//!
//! References:
//! - [RTP, RFC 3550](https://www.rfc-editor.org/rfc/rfc3550)
//! - [H.264, RFC 6184](https://www.rfc-editor.org/rfc/rfc6184)
//! - [JPEG, RFC 2435](https://www.rfc-editor.org/rfc/rfc2435)

use std::time::{Duration, Instant};

use thiserror::Error;

pub mod h264;
pub mod jpeg;

/// RTP clock rate of both video payload formats, in hertz.
pub const CLOCK_RATE_HZ: u32 = 90_000;

/// Size of the fixed RTP header.
pub const HEADER_LEN: usize = 12;

/// Smallest MTU any packetizer accepts.
const MIN_MTU: usize = HEADER_LEN + 3;

/// Errors returned when handling an RTP header.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum HeaderError {
    /// The input or output buffer is shorter than the fixed header.
    #[error("RTP header requires 12 bytes")]
    Truncated,
    /// RTP version 2 is required.
    #[error("unsupported RTP version")]
    Version,
    /// The fixed-header codec does not support CSRCs, extensions, or padding.
    #[error("unsupported RTP header flags")]
    UnsupportedFlags,
    /// Payload type must fit seven bits.
    #[error("RTP payload type exceeds 127")]
    PayloadType,
}

/// The fixed twelve-byte RTP header, without CSRCs, extensions, or padding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Marker bit.
    pub marker: bool,
    /// Seven-bit payload type.
    pub payload_type: u8,
    /// Sequence number.
    pub sequence: u16,
    /// Media timestamp.
    pub timestamp: u32,
    /// Synchronization source identifier.
    pub ssrc: u32,
}

impl Header {
    /// Encode into a fixed array. The payload type is masked to seven bits.
    pub(crate) fn to_bytes(self) -> [u8; HEADER_LEN] {
        let [s0, s1] = self.sequence.to_be_bytes();
        let [t0, t1, t2, t3] = self.timestamp.to_be_bytes();
        let [i0, i1, i2, i3] = self.ssrc.to_be_bytes();
        let pt = (self.payload_type & 0x7f) | (u8::from(self.marker) << 7);
        [0x80, pt, s0, s1, t0, t1, t2, t3, i0, i1, i2, i3]
    }

    /// Encode the fixed header into the first twelve bytes of `output`.
    ///
    /// # Errors
    /// Returns an error if the output is short or the payload type is invalid.
    pub fn encode(self, output: &mut [u8]) -> Result<(), HeaderError> {
        if self.payload_type > 127 {
            return Err(HeaderError::PayloadType);
        }
        let header = output.get_mut(..HEADER_LEN).ok_or(HeaderError::Truncated)?;
        header.copy_from_slice(&self.to_bytes());
        Ok(())
    }

    /// Decode a fixed RTP header from the beginning of `input`.
    ///
    /// # Errors
    /// Returns an error for a short header, unsupported version, or optional flags.
    pub fn decode(input: &[u8]) -> Result<Self, HeaderError> {
        let header: [u8; 12] = input
            .get(..HEADER_LEN)
            .ok_or(HeaderError::Truncated)?
            .try_into()
            .map_err(|_| HeaderError::Truncated)?;
        let [flags, payload, s0, s1, t0, t1, t2, t3, i0, i1, i2, i3] = header;
        if flags >> 6 != 2 {
            return Err(HeaderError::Version);
        }
        if flags & 0x3f != 0 {
            return Err(HeaderError::UnsupportedFlags);
        }
        Ok(Self {
            marker: payload & 0x80 != 0,
            payload_type: payload & 0x7f,
            sequence: u16::from_be_bytes([s0, s1]),
            timestamp: u32::from_be_bytes([t0, t1, t2, t3]),
            ssrc: u32::from_be_bytes([i0, i1, i2, i3]),
        })
    }
}

/// Errors for invalid packetizer configuration.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    /// Payload type must fit seven bits.
    #[error("RTP payload type {0} exceeds 127")]
    PayloadType(u8),
    /// The MTU leaves no room for payload.
    #[error("MTU {mtu} is below the minimum of {min}")]
    MtuTooSmall {
        /// Configured MTU.
        mtu: usize,
        /// Smallest MTU this packetizer supports.
        min: usize,
    },
}

/// Per-stream RTP parameters shared by every packetizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfig {
    /// Seven-bit dynamic or static payload type.
    pub payload_type: u8,
    /// Synchronization source identifier.
    pub ssrc: u32,
    /// Largest RTP packet, header included, in bytes.
    pub mtu: usize,
    /// Sequence number of the first packet.
    pub initial_sequence: u16,
}

/// Sequence counter plus the validated stream parameters.
#[derive(Debug, Clone)]
pub(crate) struct Stream {
    payload_type: u8,
    ssrc: u32,
    mtu: usize,
    sequence: u16,
}

impl Stream {
    /// Validate `config` against a codec's minimum MTU.
    pub(crate) fn new(config: StreamConfig, min_mtu: usize) -> Result<Self, ConfigError> {
        if config.payload_type > 127 {
            return Err(ConfigError::PayloadType(config.payload_type));
        }
        let min = min_mtu.max(MIN_MTU);
        if config.mtu < min {
            return Err(ConfigError::MtuTooSmall {
                mtu: config.mtu,
                min,
            });
        }
        Ok(Self {
            payload_type: config.payload_type,
            ssrc: config.ssrc,
            mtu: config.mtu,
            sequence: config.initial_sequence,
        })
    }

    /// Largest payload that fits behind the RTP header.
    pub(crate) fn max_payload(&self) -> usize {
        self.mtu.saturating_sub(HEADER_LEN)
    }

    /// Sequence number of the next packet.
    pub(crate) fn sequence(&self) -> u16 {
        self.sequence
    }

    /// Append one packet made of `parts` and advance the sequence number.
    pub(crate) fn emit(
        &mut self, pool: &mut PacketPool, marker: bool, timestamp: u32, parts: &[&[u8]],
    ) {
        let header = Header {
            marker,
            payload_type: self.payload_type,
            sequence: self.sequence,
            timestamp,
            ssrc: self.ssrc,
        };
        self.sequence = self.sequence.wrapping_add(1);
        pool.push(&header.to_bytes(), parts);
    }
}

/// Reusable storage for the packets of one frame.
///
/// Packets live back to back in one buffer. [`PacketPool::clear`] keeps the capacity, so a
/// pool that has seen one frame of a given size packetizes later frames without allocating.
#[derive(Debug, Default, Clone)]
pub struct PacketPool {
    data: Vec<u8>,
    ends: Vec<usize>,
}

impl PacketPool {
    /// An empty pool.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty pool with room for `bytes` of packet data and `packets` packets.
    #[must_use]
    pub fn with_capacity(bytes: usize, packets: usize) -> Self {
        Self {
            data: Vec::with_capacity(bytes),
            ends: Vec::with_capacity(packets),
        }
    }

    /// Drop all packets, keeping the allocation.
    pub fn clear(&mut self) {
        self.data.clear();
        self.ends.clear();
    }

    /// Number of packets held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Whether the pool holds no packets.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// The packet at `index`, if any.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&[u8]> {
        let end = *self.ends.get(index)?;
        let start = index
            .checked_sub(1)
            .and_then(|previous| self.ends.get(previous))
            .copied()
            .unwrap_or(0);
        self.data.get(start..end)
    }

    /// Iterate over the packets in transmission order.
    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        (0..self.len()).filter_map(|index| self.get(index))
    }

    fn push(&mut self, header: &[u8], parts: &[&[u8]]) {
        self.data.extend_from_slice(header);
        for part in parts {
            self.data.extend_from_slice(part);
        }
        self.ends.push(self.data.len());
    }
}

/// Convert time since the stream origin to a 90 kHz RTP timestamp.
///
/// `base` is the stream's initial (random) timestamp. The result wraps modulo 2^32 as RTP
/// requires. Sub-tick remainders are truncated.
#[must_use]
pub fn timestamp_from_duration(elapsed: Duration, base: u32) -> u32 {
    let ticks = elapsed.as_nanos() * u128::from(CLOCK_RATE_HZ) / 1_000_000_000;
    let wrapped = u32::try_from(ticks % (1 << 32)).unwrap_or_default();
    base.wrapping_add(wrapped)
}

/// RTP timestamp of a frame captured at `frame`, for a stream that started at `origin`.
///
/// A frame instant before `origin` maps to `base`.
#[must_use]
pub fn timestamp_from_instants(origin: Instant, frame: Instant, base: u32) -> u32 {
    timestamp_from_duration(frame.saturating_duration_since(origin), base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn header_golden() {
        let header = Header {
            marker: true,
            payload_type: 96,
            sequence: 0x1234,
            timestamp: 0x5678_9abc,
            ssrc: 0xdef0_1234,
        };
        let expected = [
            0x80, 0xe0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x12, 0x34,
        ];
        let mut output = [0; 12];
        assert_eq!(header.encode(&mut output), Ok(()));
        assert_eq!(output, expected);
        assert_eq!(Header::decode(&expected), Ok(header));
    }

    #[test]
    fn header_errors() {
        let header = Header {
            marker: false,
            payload_type: 128,
            sequence: 0,
            timestamp: 0,
            ssrc: 0,
        };
        assert_eq!(header.encode(&mut [0; 12]), Err(HeaderError::PayloadType));
        let ok = Header {
            payload_type: 26,
            ..header
        };
        assert_eq!(ok.encode(&mut [0; 11]), Err(HeaderError::Truncated));
        assert_eq!(Header::decode(&[0; 11]), Err(HeaderError::Truncated));
        assert_eq!(Header::decode(&[0x40; 12]), Err(HeaderError::Version));
        assert_eq!(
            Header::decode(&[0x81; 12]),
            Err(HeaderError::UnsupportedFlags)
        );
    }

    #[test]
    fn timestamps_use_90khz_and_wrap() {
        assert_eq!(timestamp_from_duration(Duration::from_secs(1), 0), 90_000);
        assert_eq!(
            timestamp_from_duration(Duration::from_millis(40), 10),
            3_610
        );
        assert_eq!(
            timestamp_from_duration(Duration::from_secs(1), u32::MAX),
            89_999
        );
        // 47_722 s is 4_294_980_000 ticks: 12_704 past one wrap.
        assert_eq!(
            timestamp_from_duration(Duration::from_secs(47_722), 7),
            12_711
        );
        let origin = Instant::now();
        let later = origin + Duration::from_millis(100);
        assert_eq!(timestamp_from_instants(origin, later, 0), 9_000);
        assert_eq!(timestamp_from_instants(later, origin, 5), 5);
    }

    #[test]
    fn config_is_validated() {
        let config = StreamConfig {
            payload_type: 96,
            ssrc: 1,
            mtu: 1400,
            initial_sequence: 0,
        };
        assert!(Stream::new(config, 20).is_ok());
        assert_eq!(
            Stream::new(
                StreamConfig {
                    payload_type: 200,
                    ..config
                },
                20
            )
            .err(),
            Some(ConfigError::PayloadType(200))
        );
        assert_eq!(
            Stream::new(StreamConfig { mtu: 19, ..config }, 20).err(),
            Some(ConfigError::MtuTooSmall { mtu: 19, min: 20 })
        );
    }

    #[test]
    fn pool_keeps_packets_and_capacity() {
        let mut pool = PacketPool::new();
        assert!(pool.is_empty());
        pool.push(&[1, 2], &[&[3], &[], &[4, 5]]);
        pool.push(&[6], &[]);
        assert_eq!(pool.len(), 2);
        assert_eq!(pool.get(0), Some(&[1, 2, 3, 4, 5][..]));
        assert_eq!(pool.get(1), Some(&[6][..]));
        assert_eq!(pool.get(2), None);
        assert_eq!(pool.iter().count(), 2);
        let capacity = pool.data.capacity();
        pool.clear();
        assert!(pool.is_empty());
        assert_eq!(pool.data.capacity(), capacity);
    }

    proptest! {
        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
            let _ = Header::decode(&bytes);
        }

        #[test]
        fn header_round_trips(
            marker: bool,
            payload_type in 0u8..128,
            sequence: u16,
            timestamp: u32,
            ssrc: u32,
        ) {
            let header = Header { marker, payload_type, sequence, timestamp, ssrc };
            prop_assert_eq!(Header::decode(&header.to_bytes()), Ok(header));
        }
    }
}
