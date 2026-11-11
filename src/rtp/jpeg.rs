//! JPEG RTP packetizer (RFC 2435).
//!
//! Parses the JPEG headers of a baseline frame, then sends the entropy-coded scan as RTP
//! fragments behind the RFC 2435 main header. What the packetizer does:
//!
//! - `Q = 255` with the quantization tables in-band on the first fragment of each frame
//! - Type 0 (4:2:2) or 1 (4:2:0) from the SOF sampling factors, plus 64 when a `DRI` marker is
//!   present; in that case every packet carries the restart marker header with `F = L = 1`
//!   and count `0x3fff` (fragments are not restart-aligned)
//! - Dimensions are sent in 8-pixel units; sizes that are not multiples of 8 are rejected, since
//!   RFC 2435 cannot represent them exactly
//! - Marker bit on the last fragment
//!
//! Types 0 and 1 carry one luminance and one shared chrominance table, so Cb and Cr must use
//! the same quantization table. Two tables go in-band on the first fragment.
//!
//! RFC 2435 receivers assume the standard Huffman tables (T.81 Annex K), and the scan must use
//! them with the usual selectors (`0x00` for Y, `0x11` for Cb and Cr). Any `DHT` that differs
//! is rejected with [`JpegError::UnsupportedHuffman`].
//!
//! Rejected with [`JpegError`]: progressive and non-baseline frames, 12-bit or 16-bit tables,
//! non-YCbCr component layouts, scans that are not one interleaved Y/Cb/Cr scan, sampling other than 4:2:0 / 4:2:2, dimensions over 2040 or not a multiple of 8.
//!
//! References:
//! - [RTP payload format for JPEG, RFC 2435](https://www.rfc-editor.org/rfc/rfc2435)
//! - [RTP, RFC 3550](https://www.rfc-editor.org/rfc/rfc3550)

use thiserror::Error;

mod huffman;

use super::{ConfigError, PacketPool, Stream, StreamConfig};

/// Static RTP payload type of JPEG (RFC 3551).
pub const PAYLOAD_TYPE: u8 = 26;
/// Largest width or height RFC 2435 can carry.
pub const MAX_DIMENSION: u16 = 2040;
/// RFC 2435 `Q` value for in-band tables that receivers must not cache.
const Q_IN_BAND: u8 = 255;
/// Bytes in one 8-bit quantization table.
const TABLE_LEN: usize = 64;
/// Quantization tables carried in-band: luminance and shared chrominance.
const TABLES: usize = 2;
/// Fragment offsets are 24 bits.
const MAX_SCAN_LEN: usize = 0x00ff_ffff;
const MAIN_HEADER_LEN: usize = 8;
const RESTART_HEADER_LEN: usize = 4;
const QTABLE_HEADER_LEN: usize = 4;
/// Room for RTP header, main header, restart header, both tables and one payload byte.
const MIN_MTU: usize = super::HEADER_LEN
    + MAIN_HEADER_LEN
    + RESTART_HEADER_LEN
    + QTABLE_HEADER_LEN
    + TABLES * TABLE_LEN
    + 1;

/// Errors from parsing or packetizing a JPEG frame.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum JpegError {
    /// Invalid stream configuration.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The data does not start with an SOI marker.
    #[error("missing JPEG SOI marker")]
    NotJpeg,
    /// A marker segment is cut short or has an invalid length.
    #[error("truncated or malformed JPEG marker segment")]
    Malformed,
    /// A byte that should start a marker does not.
    #[error("expected a JPEG marker")]
    BadMarker,
    /// The frame is progressive; RFC 2435 carries baseline only.
    #[error("progressive JPEG cannot be carried by RFC 2435")]
    Progressive,
    /// A start-of-frame type other than baseline sequential DCT.
    #[error("unsupported JPEG frame type (SOF{0})")]
    UnsupportedFrameType(u8),
    /// Sample precision other than 8 bits, or 16-bit quantization tables.
    #[error("unsupported sample or table precision")]
    UnsupportedPrecision,
    /// Not a three-component frame, or a component layout RFC 2435 cannot describe.
    #[error("unsupported component layout")]
    UnsupportedComponents,
    /// Chroma subsampling other than 4:2:0 or 4:2:2.
    #[error("unsupported chroma sampling")]
    UnsupportedSampling,
    /// Width or height is zero.
    #[error("zero JPEG dimension")]
    ZeroDimension,
    /// Width or height is over 2040.
    #[error("JPEG dimension {0} exceeds 2040")]
    DimensionTooLarge(u16),
    /// A `DHT` that differs from the standard Huffman tables RFC 2435 assumes.
    #[error("non-standard Huffman tables cannot be carried by RFC 2435")]
    UnsupportedHuffman,
    /// The scan is not a single interleaved YCbCr baseline scan matching the frame components.
    #[error("unsupported scan layout")]
    UnsupportedScan,
    /// Width or height is not a multiple of 8; RFC 2435 sends sizes in 8-pixel units.
    #[error("JPEG dimension {0} is not a multiple of 8")]
    UnalignedDimension(u16),
    /// A second SOF marker.
    #[error("duplicate SOF marker")]
    DuplicateFrame,
    /// SOS reached (or EOI hit) without a SOF.
    #[error("no SOF before scan")]
    MissingFrame,
    /// A component references a quantization table that was never defined.
    #[error("missing quantization table {0}")]
    MissingTable(u8),
    /// Quantization table id over 3.
    #[error("invalid quantization table id {0}")]
    BadTableId(u8),
    /// Cb and Cr use different quantization tables; RFC 2435 types 0 and 1 share one.
    #[error("unsupported quantization table assignment")]
    UnsupportedTableLayout,
    /// EOI hit before an SOS marker.
    #[error("no scan in JPEG")]
    MissingScan,
    /// The scan has no terminating EOI marker.
    #[error("missing EOI marker")]
    MissingEoi,
    /// The scan holds no entropy-coded data.
    #[error("empty scan")]
    EmptyScan,
    /// The scan exceeds the 24-bit fragment offset.
    #[error("scan of {0} bytes exceeds the RFC 2435 fragment offset")]
    ScanTooLarge(usize),
}

/// Chroma subsampling RFC 2435 can describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sampling {
    /// 4:2:2, Y sampled 2x1 (RFC 2435 type 0).
    Yuv422,
    /// 4:2:0, Y sampled 2x2 (RFC 2435 type 1).
    Yuv420,
}

impl Sampling {
    fn rtp_type(self) -> u8 {
        match self {
            Self::Yuv422 => 0,
            Self::Yuv420 => 1,
        }
    }
}

/// What RFC 2435 needs from a JPEG frame, borrowed from the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JpegInfo<'a> {
    /// Frame width in pixels (at most 2040).
    pub width: u16,
    /// Frame height in pixels (at most 2040).
    pub height: u16,
    /// Chroma subsampling.
    pub sampling: Sampling,
    /// Restart interval in MCUs, when a non-zero `DRI` is present.
    pub restart_interval: Option<u16>,
    /// Entropy-coded scan, from after the SOS header to before EOI.
    pub scan: &'a [u8],
    tables: [&'a [u8]; TABLES],
}

impl<'a> JpegInfo<'a> {
    /// Luminance then shared chrominance quantization table, 64 zigzag-ordered bytes each.
    #[must_use]
    pub fn tables(&self) -> &[&'a [u8]; TABLES] {
        &self.tables
    }

    /// Parse the headers of a baseline JPEG. Total: returns an error on any input.
    ///
    /// # Errors
    /// [`JpegError`] for malformed input or anything RFC 2435 cannot carry.
    pub fn parse(data: &'a [u8]) -> Result<Self, JpegError> {
        if data.get(..2) != Some(&[0xff, 0xd8]) {
            return Err(JpegError::NotJpeg);
        }
        let mut pos = 2usize;
        let mut quant: [Option<&'a [u8]>; 4] = [None; 4];
        let mut frame: Option<Frame> = None;
        let mut restart: u16 = 0;
        let sos;
        loop {
            if data.get(pos) != Some(&0xff) {
                return Err(JpegError::BadMarker);
            }
            while data.get(pos) == Some(&0xff) {
                pos += 1;
            }
            let marker = *data.get(pos).ok_or(JpegError::Malformed)?;
            pos += 1;
            match marker {
                0xd9 => return Err(JpegError::MissingScan),
                0x00 | 0x01 | 0xd0..=0xd8 => return Err(JpegError::BadMarker),
                _ => {}
            }
            let length = usize::from(u16::from_be_bytes(
                *data
                    .get(pos..)
                    .and_then(<[u8]>::first_chunk::<2>)
                    .ok_or(JpegError::Malformed)?,
            ));
            let body_end = pos + length;
            let body = length
                .checked_sub(2)
                .and_then(|_| data.get(pos + 2..body_end))
                .ok_or(JpegError::Malformed)?;
            pos = body_end;
            match marker {
                0xdb => parse_dqt(body, &mut quant)?,
                0xc0 => {
                    if frame.is_some() {
                        return Err(JpegError::DuplicateFrame);
                    }
                    frame = Some(parse_sof(body)?);
                }
                0xc2 => return Err(JpegError::Progressive),
                0xc1 | 0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf => {
                    return Err(JpegError::UnsupportedFrameType(marker & 0x0f));
                }
                0xdd => {
                    restart =
                        u16::from_be_bytes(*body.first_chunk::<2>().ok_or(JpegError::Malformed)?);
                }
                0xc4 => huffman::check(body)?,
                0xda => {
                    sos = body;
                    break;
                }
                _ => {}
            }
        }
        let frame = frame.ok_or(JpegError::MissingFrame)?;
        frame.check_scan(sos)?;
        let tables = frame.tables(&quant)?;
        let rest = data.get(pos..).ok_or(JpegError::Malformed)?;
        let eoi = rest
            .windows(2)
            .position(|w| w == [0xff, 0xd9])
            .ok_or(JpegError::MissingEoi)?;
        let scan = rest.get(..eoi).ok_or(JpegError::Malformed)?;
        if scan.is_empty() {
            return Err(JpegError::EmptyScan);
        }
        Ok(Self {
            width: frame.width,
            height: frame.height,
            sampling: frame.sampling,
            restart_interval: (restart != 0).then_some(restart),
            scan,
            tables,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    width: u16,
    height: u16,
    sampling: Sampling,
    /// Quantization table id of Y, Cb, Cr.
    table_of: [u8; 3],
    /// Component identifiers of Y, Cb, Cr.
    component_ids: [u8; 3],
}

impl Frame {
    /// Require one interleaved scan over all three components with baseline parameters.
    fn check_scan(self, sos: &[u8]) -> Result<(), JpegError> {
        let [
            count,
            c0,
            s0,
            c1,
            s1,
            c2,
            s2,
            spectral_start,
            spectral_end,
            approx,
            ..,
        ] = *sos
        else {
            return Err(JpegError::Malformed);
        };
        if count == 3
            && [c0, c1, c2] == self.component_ids
            && [s0, s1, s2] == [0x00, 0x11, 0x11]
            && (spectral_start, spectral_end, approx) == (0, 63, 0)
        {
            Ok(())
        } else {
            Err(JpegError::UnsupportedScan)
        }
    }

    /// The luminance and shared chrominance tables, from the `DQT` definitions seen.
    fn tables<'a>(self, quant: &[Option<&'a [u8]>; 4]) -> Result<[&'a [u8]; TABLES], JpegError> {
        let [y, cb, cr] = self.table_of.map(|id| {
            quant
                .get(usize::from(id))
                .copied()
                .flatten()
                .ok_or(JpegError::MissingTable(id))
        });
        let (y, cb, cr) = (y?, cb?, cr?);
        if cb == cr {
            Ok([y, cb])
        } else {
            Err(JpegError::UnsupportedTableLayout)
        }
    }
}

fn parse_dqt<'a>(mut body: &'a [u8], quant: &mut [Option<&'a [u8]>; 4]) -> Result<(), JpegError> {
    while let Some((&info, rest)) = body.split_first() {
        if info >> 4 != 0 {
            return Err(JpegError::UnsupportedPrecision);
        }
        let id = info & 0x0f;
        let (table, rest) = rest
            .split_at_checked(TABLE_LEN)
            .ok_or(JpegError::Malformed)?;
        *quant
            .get_mut(usize::from(id))
            .ok_or(JpegError::BadTableId(id))? = Some(table);
        body = rest;
    }
    Ok(())
}

fn parse_sof(body: &[u8]) -> Result<Frame, JpegError> {
    let [precision, h0, h1, w0, w1, count, comps @ ..] = body else {
        return Err(JpegError::Malformed);
    };
    if *precision != 8 {
        return Err(JpegError::UnsupportedPrecision);
    }
    let height = u16::from_be_bytes([*h0, *h1]);
    let width = u16::from_be_bytes([*w0, *w1]);
    for dimension in [width, height] {
        if dimension == 0 {
            return Err(JpegError::ZeroDimension);
        }
        if dimension > MAX_DIMENSION {
            return Err(JpegError::DimensionTooLarge(dimension));
        }
        if dimension % 8 != 0 {
            return Err(JpegError::UnalignedDimension(dimension));
        }
    }
    if *count != 3 {
        return Err(JpegError::UnsupportedComponents);
    }
    let mut components = comps
        .as_chunks::<3>()
        .0
        .iter()
        .map(|&[id, sampling, table]| (id, sampling, table));
    let mut next = || components.next().ok_or(JpegError::Malformed);
    let (y_id, y_sampling, y_table) = next()?;
    let chroma = [next()?, next()?];
    let sampling = match y_sampling {
        0x22 => Sampling::Yuv420,
        0x21 => Sampling::Yuv422,
        _ => return Err(JpegError::UnsupportedSampling),
    };
    if chroma.iter().any(|&(_, sampling, _)| sampling != 0x11) {
        return Err(JpegError::UnsupportedSampling);
    }
    Ok(Frame {
        width,
        height,
        sampling,
        table_of: [y_table, chroma[0].2, chroma[1].2],
        component_ids: [y_id, chroma[0].0, chroma[1].0],
    })
}

/// Packetizer turning baseline JPEG frames into RFC 2435 RTP packets.
#[derive(Debug, Clone)]
pub struct JpegPacketizer {
    stream: Stream,
}

impl JpegPacketizer {
    /// Create a packetizer. Use [`PAYLOAD_TYPE`] unless negotiating something else.
    ///
    /// # Errors
    /// [`JpegError::Config`] for a payload type over 127 or an MTU under 157 (room for both
    /// tables and a restart header on the first fragment).
    pub fn new(config: StreamConfig) -> Result<Self, JpegError> {
        Ok(Self {
            stream: Stream::new(config, MIN_MTU)?,
        })
    }

    /// Sequence number the next packet will carry.
    #[must_use]
    pub fn next_sequence(&self) -> u16 {
        self.stream.sequence()
    }

    /// Packetize one JPEG frame into `pool`, replacing its contents.
    ///
    /// # Errors
    /// [`JpegError`] when the frame cannot be parsed or carried; the pool is left empty and
    /// the sequence number is unchanged.
    pub fn packetize(
        &mut self, jpeg: &[u8], timestamp: u32, pool: &mut PacketPool,
    ) -> Result<(), JpegError> {
        pool.clear();
        let info = JpegInfo::parse(jpeg)?;
        if info.scan.len() > MAX_SCAN_LEN {
            return Err(JpegError::ScanTooLarge(info.scan.len()));
        }
        // Parsing guarantees multiples of 8 up to 2040, so these always fit.
        let to_units = |pixels: u16| u8::try_from(pixels / 8).unwrap_or(u8::MAX);
        let kind = info.sampling.rtp_type()
            + if info.restart_interval.is_some() {
                64
            } else {
                0
            };
        let [r0, r1] = info.restart_interval.unwrap_or(0).to_be_bytes();
        let restart_header = [r0, r1, 0xff, 0xff];
        let restart: &[u8] = if info.restart_interval.is_some() {
            &restart_header
        } else {
            &[]
        };
        let tables = info.tables();
        let tables_len = tables.len() * TABLE_LEN;
        let [l0, l1] = u16::try_from(tables_len).unwrap_or_default().to_be_bytes();
        let qtable_header = [0, 0, l0, l1];
        let table_part = |i: usize| tables.get(i).copied().unwrap_or_default();

        let mut offset = 0usize;
        while offset < info.scan.len() {
            let first = offset == 0;
            let mut overhead = MAIN_HEADER_LEN + restart.len();
            if first {
                overhead += QTABLE_HEADER_LEN + tables_len;
            }
            let capacity = self.stream.max_payload().saturating_sub(overhead).max(1);
            let rest = info.scan.get(offset..).unwrap_or_default();
            let (chunk, _) = rest.split_at(rest.len().min(capacity));
            let [_, o0, o1, o2] = u32::try_from(offset).unwrap_or_default().to_be_bytes();
            let main = [
                0,
                o0,
                o1,
                o2,
                kind,
                Q_IN_BAND,
                to_units(info.width),
                to_units(info.height),
            ];
            let (qheader, t0, t1): (&[u8], &[u8], &[u8]) = if first {
                (&qtable_header, table_part(0), table_part(1))
            } else {
                (&[], &[], &[])
            };
            offset += chunk.len();
            let last = offset == info.scan.len();
            self.stream.emit(
                pool,
                last,
                timestamp,
                &[&main, restart, qheader, t0, t1, chunk],
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod fixtures;

#[cfg(test)]
mod tests {
    use super::fixtures;
    use super::huffman;
    use super::*;
    use crate::rtp::{HEADER_LEN, Header};
    use proptest::prelude::*;

    /// Synthetic scan bytes, with FF00 stuffing and restart markers sprinkled in.
    pub(super) fn scan_bytes(len: usize) -> Vec<u8> {
        let mut scan = Vec::with_capacity(len);
        let mut x = 0x2545_f491_u32;
        while scan.len() < len {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let [b, ..] = x.to_be_bytes();
            scan.push(b);
            if b == 0xff {
                scan.push(0x00);
            }
        }
        scan.truncate(len);
        // Never end on a lone 0xFF, which would pair with the EOI marker.
        if scan.last() == Some(&0xff) {
            scan.pop();
            scan.push(0x01);
        }
        scan
    }

    pub(super) struct Spec {
        pub width: u16,
        pub height: u16,
        pub y_sampling: u8,
        pub tables: [u8; 3],
        pub restart: u16,
        pub scan_len: usize,
    }

    impl Default for Spec {
        fn default() -> Self {
            Self {
                width: 64,
                height: 48,
                y_sampling: 0x22,
                tables: [0, 1, 1],
                restart: 0,
                scan_len: 3000,
            }
        }
    }

    pub(super) fn table(id: u8) -> Vec<u8> {
        (0..64)
            .map(|i| id.wrapping_mul(64).wrapping_add(i).wrapping_add(1))
            .collect()
    }

    /// Hand-built baseline JPEG with synthetic tables and scan data.
    pub(super) fn build(spec: &Spec) -> Vec<u8> {
        let mut out = vec![0xff, 0xd8];
        // APP0 with junk the parser must skip.
        out.extend_from_slice(&[0xff, 0xe0, 0x00, 0x08, b'T', b'C', 0, 0, 0, 0]);
        let mut ids: Vec<u8> = spec.tables.to_vec();
        ids.sort_unstable();
        ids.dedup();
        for id in ids {
            out.extend_from_slice(&[0xff, 0xdb, 0x00, 0x43, id]);
            out.extend(table(id));
        }
        out.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 8]);
        out.extend_from_slice(&spec.height.to_be_bytes());
        out.extend_from_slice(&spec.width.to_be_bytes());
        out.extend_from_slice(&[
            3,
            1,
            spec.y_sampling,
            spec.tables[0],
            2,
            0x11,
            spec.tables[1],
            3,
            0x11,
            spec.tables[2],
        ]);
        for entry in [
            huffman::DC_LUMA,
            huffman::AC_LUMA,
            huffman::DC_CHROMA,
            huffman::AC_CHROMA,
        ] {
            out.extend_from_slice(&[0xff, 0xc4]);
            out.extend_from_slice(&u16::try_from(entry.len() + 2).unwrap().to_be_bytes());
            out.extend_from_slice(entry);
        }
        if spec.restart != 0 {
            out.extend_from_slice(&[0xff, 0xdd, 0x00, 0x04]);
            out.extend_from_slice(&spec.restart.to_be_bytes());
        }
        out.extend_from_slice(&[0xff, 0xda, 0x00, 0x0c, 3, 1, 0, 2, 0x11, 3, 0x11, 0, 63, 0]);
        out.extend(scan_bytes(spec.scan_len));
        out.extend_from_slice(&[0xff, 0xd9]);
        out
    }

    fn config(mtu: usize) -> StreamConfig {
        StreamConfig {
            payload_type: PAYLOAD_TYPE,
            ssrc: 0xaabb_ccdd,
            mtu,
            initial_sequence: 100,
        }
    }

    /// What a receiver learns from the packets alone.
    struct Received {
        width: u16,
        height: u16,
        sampling: Sampling,
        restart: Option<u16>,
        tables: Vec<Vec<u8>>,
        scan: Vec<u8>,
    }

    /// Reassemble from packets, checking every header on the way. Takes everything from the
    /// packets, never from the source JPEG.
    fn depacketize(pool: &PacketPool, mtu: usize) -> Received {
        let mut scan = Vec::new();
        let mut tables = Vec::new();
        let (mut width, mut height, mut kind) = (0, 0, 0);
        let mut restart = None;
        let count = pool.len();
        for (i, packet) in pool.iter().enumerate() {
            assert!(packet.len() <= mtu);
            let header = Header::decode(packet).unwrap();
            assert_eq!(header.payload_type, 26);
            assert_eq!(header.marker, i + 1 == count);
            assert_eq!(header.sequence, 100 + u16::try_from(i).unwrap());
            let p = &packet[HEADER_LEN..];
            assert_eq!(p[0], 0);
            let offset = u32::from_be_bytes([0, p[1], p[2], p[3]]) as usize;
            assert_eq!(offset, scan.len(), "fragments are contiguous");
            assert_eq!(p[5], 255);
            let this = (u16::from(p[6]) * 8, u16::from(p[7]) * 8, p[4]);
            if i == 0 {
                (width, height, kind) = this;
            }
            assert_eq!((width, height, kind), this, "headers agree across packets");
            let mut body = &p[8..];
            if kind >= 64 {
                let interval = u16::from_be_bytes([body[0], body[1]]);
                assert_eq!(restart.get_or_insert(interval), &interval);
                assert_eq!(&body[2..4], &[0xff, 0xff]);
                body = &body[4..];
            }
            if i == 0 {
                let len = usize::from(u16::from_be_bytes([body[2], body[3]]));
                assert_eq!((body[0], body[1]), (0, 0));
                tables = body[4..4 + len].chunks(64).map(<[u8]>::to_vec).collect();
                body = &body[4 + len..];
            }
            scan.extend_from_slice(body);
        }
        Received {
            width,
            height,
            sampling: if kind % 64 == 0 {
                Sampling::Yuv422
            } else {
                Sampling::Yuv420
            },
            restart,
            tables,
            scan,
        }
    }

    #[test]
    fn parses_headers() {
        let jpeg = build(&Spec::default());
        let info = JpegInfo::parse(&jpeg).unwrap();
        assert_eq!((info.width, info.height), (64, 48));
        assert_eq!(info.sampling, Sampling::Yuv420);
        assert_eq!(info.restart_interval, None);
        assert_eq!(info.tables().len(), 2);
        assert_eq!(info.tables()[0], table(0));
        assert_eq!(info.tables()[1], table(1));
        assert_eq!(info.scan, scan_bytes(3000));
    }

    #[test]
    fn single_packet_golden() {
        let mut packetizer = JpegPacketizer::new(config(1400)).unwrap();
        let mut pool = PacketPool::new();
        let jpeg = build(&Spec {
            scan_len: 5,
            y_sampling: 0x21,
            width: 24,
            ..Spec::default()
        });
        packetizer.packetize(&jpeg, 0x0102_0304, &mut pool).unwrap();
        assert_eq!(pool.len(), 1);
        let packet = pool.get(0).unwrap();
        let mut want = vec![
            0x80, 0x9a, 0x00, 0x64, 0x01, 0x02, 0x03, 0x04, 0xaa, 0xbb, 0xcc, 0xdd, // RTP
            0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0x03, 0x06, // main: type 0, Q 255, 24x48
            0x00, 0x00, 0x00, 0x80, // Q table header, 128 bytes
        ];
        want.extend(table(0));
        want.extend(table(1));
        want.extend(scan_bytes(5));
        assert_eq!(packet, want);
    }

    #[test]
    fn fragments_and_restart_header_golden() {
        let mtu = 300;
        let jpeg = build(&Spec {
            restart: 0x0102,
            scan_len: 600,
            ..Spec::default()
        });
        let mut packetizer = JpegPacketizer::new(config(mtu)).unwrap();
        let mut pool = PacketPool::new();
        packetizer.packetize(&jpeg, 9, &mut pool).unwrap();
        // First: 300-12-8-4-4-128 = 144 bytes; then 276 per packet.
        assert_eq!(pool.len(), 3);
        let second = pool.get(1).unwrap();
        assert_eq!(
            &second[HEADER_LEN..HEADER_LEN + 12],
            [0, 0, 0, 144, 65, 255, 8, 6, 0x01, 0x02, 0xff, 0xff]
        );
        let info = JpegInfo::parse(&jpeg).unwrap();
        assert_eq!(info.restart_interval, Some(0x0102));
        let received = depacketize(&pool, mtu);
        assert_eq!(received.scan, info.scan);
        assert_eq!(received.restart, Some(0x0102));
    }

    #[test]
    fn distinct_chroma_tables_are_rejected() {
        let jpeg = build(&Spec {
            tables: [0, 1, 2],
            ..Spec::default()
        });
        assert_eq!(
            JpegInfo::parse(&jpeg),
            Err(JpegError::UnsupportedTableLayout)
        );
    }

    #[test]
    fn non_standard_huffman_is_rejected() {
        let mut jpeg = build(&Spec::default());
        let dht = jpeg.windows(2).position(|w| w == [0xff, 0xc4]).unwrap();
        jpeg[dht + 6] ^= 1; // first code count
        assert_eq!(JpegInfo::parse(&jpeg), Err(JpegError::UnsupportedHuffman));
        let mut short = build(&Spec::default());
        let at = short.windows(2).position(|w| w == [0xff, 0xc4]).unwrap();
        short[at + 3] = 10;
        assert!(JpegInfo::parse(&short).is_err());
    }

    /// Rebuild the JPEG a receiver would from RFC 2435 packets (RFC Appendix B), in the
    /// header layout the fixtures use.
    fn reassemble(pool: &PacketPool, mtu: usize) -> Vec<u8> {
        let got = depacketize(pool, mtu);
        let (w, h) = (got.width.to_be_bytes(), got.height.to_be_bytes());
        let y_sampling = match got.sampling {
            Sampling::Yuv420 => 0x22,
            Sampling::Yuv422 => 0x21,
        };
        let scan = got.scan.clone();
        let mut out = vec![0xff, 0xd8];
        for (id, table) in got.tables.iter().enumerate() {
            out.extend_from_slice(&[0xff, 0xdb, 0x00, 0x43, u8::try_from(id).unwrap()]);
            out.extend_from_slice(table);
        }
        out.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 8, h[0], h[1], w[0], w[1], 3]);
        out.extend_from_slice(&[1, y_sampling, 0, 2, 0x11, 1, 3, 0x11, 1]);
        for entry in [
            huffman::DC_LUMA,
            huffman::AC_LUMA,
            huffman::DC_CHROMA,
            huffman::AC_CHROMA,
        ] {
            out.extend_from_slice(&[0xff, 0xc4]);
            out.extend_from_slice(&u16::try_from(entry.len() + 2).unwrap().to_be_bytes());
            out.extend_from_slice(entry);
        }
        if let Some(interval) = got.restart {
            out.extend_from_slice(&[0xff, 0xdd, 0x00, 0x04]);
            out.extend_from_slice(&interval.to_be_bytes());
        }
        out.extend_from_slice(&[
            0xff, 0xda, 0x00, 0x0c, 3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0,
        ]);
        out.extend(scan);
        out.extend_from_slice(&[0xff, 0xd9]);
        out
    }

    #[test]
    fn real_fixtures_round_trip_byte_for_byte() {
        for (fixture, sampling, size, restart, kind) in [
            (fixtures::YUV420, Sampling::Yuv420, (32, 16), None, 1),
            (
                fixtures::YUV422_RESTART,
                Sampling::Yuv422,
                (24, 16),
                Some(2),
                64,
            ),
        ] {
            let info = JpegInfo::parse(fixture).unwrap();
            assert_eq!((info.width, info.height), size);
            assert_eq!(info.sampling, sampling);
            assert_eq!(info.restart_interval, restart);
            for mtu in [157, 200, 1400] {
                let mut packetizer = JpegPacketizer::new(config(mtu)).unwrap();
                let mut pool = PacketPool::new();
                packetizer.packetize(fixture, 3, &mut pool).unwrap();
                assert_eq!(pool.get(0).unwrap()[HEADER_LEN + 4], kind);
                assert_eq!(reassemble(&pool, mtu), fixture);
            }
        }
    }

    #[test]
    fn rejects_unsupported() {
        let jpeg = build(&Spec::default());
        let sof = jpeg.windows(2).position(|w| w == [0xff, 0xc0]).unwrap();

        let mut progressive = jpeg.clone();
        progressive[sof + 1] = 0xc2;
        assert_eq!(JpegInfo::parse(&progressive), Err(JpegError::Progressive));
        let mut extended = jpeg.clone();
        extended[sof + 1] = 0xc1;
        assert_eq!(
            JpegInfo::parse(&extended),
            Err(JpegError::UnsupportedFrameType(1))
        );

        for (spec, want) in [
            (
                Spec {
                    width: 2048,
                    ..Spec::default()
                },
                JpegError::DimensionTooLarge(2048),
            ),
            (
                Spec {
                    height: 2041,
                    ..Spec::default()
                },
                JpegError::DimensionTooLarge(2041),
            ),
            (
                Spec {
                    width: 0,
                    ..Spec::default()
                },
                JpegError::ZeroDimension,
            ),
            (
                Spec {
                    width: 17,
                    ..Spec::default()
                },
                JpegError::UnalignedDimension(17),
            ),
            (
                Spec {
                    height: 52,
                    ..Spec::default()
                },
                JpegError::UnalignedDimension(52),
            ),
            (
                Spec {
                    y_sampling: 0x11,
                    ..Spec::default()
                },
                JpegError::UnsupportedSampling,
            ),
            (
                Spec {
                    tables: [0, 1, 0],
                    ..Spec::default()
                },
                JpegError::UnsupportedTableLayout,
            ),
            (
                Spec {
                    scan_len: 0,
                    ..Spec::default()
                },
                JpegError::EmptyScan,
            ),
        ] {
            assert_eq!(JpegInfo::parse(&build(&spec)), Err(want));
        }
        assert!(
            JpegInfo::parse(&build(&Spec {
                width: 2040,
                height: 2040,
                ..Spec::default()
            }))
            .is_ok()
        );
    }

    #[test]
    fn rejects_malformed_headers() {
        let mut jpeg = build(&Spec::default());
        let sos = jpeg.windows(2).position(|w| w == [0xff, 0xda]).unwrap();
        let mut swapped = jpeg.clone();
        swapped[sos + 5] = 9;
        assert_eq!(JpegInfo::parse(&swapped), Err(JpegError::UnsupportedScan));
        let mut spectral = jpeg.clone();
        spectral[sos + 12] = 30;
        assert_eq!(JpegInfo::parse(&spectral), Err(JpegError::UnsupportedScan));

        // 16-bit table, grayscale, missing pieces.
        let dqt = jpeg.windows(2).position(|w| w == [0xff, 0xdb]).unwrap();
        let mut wide = jpeg.clone();
        wide[dqt + 4] |= 0x10;
        assert_eq!(JpegInfo::parse(&wide), Err(JpegError::UnsupportedPrecision));
        assert_eq!(JpegInfo::parse(&[1, 2]), Err(JpegError::NotJpeg));
        assert_eq!(
            JpegInfo::parse(&[0xff, 0xd8, 0xff, 0xd9]),
            Err(JpegError::MissingScan)
        );
        assert_eq!(
            JpegInfo::parse(&[0xff, 0xd8, 0x12]),
            Err(JpegError::BadMarker)
        );
        assert_eq!(
            JpegInfo::parse(&[0xff, 0xd8, 0xff, 0xe0, 0x00]),
            Err(JpegError::Malformed)
        );
        jpeg.truncate(jpeg.len() - 2);
        assert_eq!(JpegInfo::parse(&jpeg), Err(JpegError::MissingEoi));
        let no_frame = [0xff, 0xd8, 0xff, 0xda, 0x00, 0x02, 1, 0xff, 0xd9];
        assert_eq!(JpegInfo::parse(&no_frame), Err(JpegError::MissingFrame));
    }

    #[test]
    fn errors_leave_pool_empty_and_sequence_unchanged() {
        let mut packetizer = JpegPacketizer::new(config(1400)).unwrap();
        let mut pool = PacketPool::new();
        packetizer
            .packetize(&build(&Spec::default()), 0, &mut pool)
            .unwrap();
        assert!(!pool.is_empty());
        let before = packetizer.next_sequence();
        assert_eq!(
            packetizer.packetize(&[0, 1], 0, &mut pool),
            Err(JpegError::NotJpeg)
        );
        assert!(pool.is_empty());
        assert_eq!(packetizer.next_sequence(), before);
        assert_eq!(
            JpegPacketizer::new(config(156)).err(),
            Some(JpegError::Config(ConfigError::MtuTooSmall {
                mtu: 156,
                min: 157
            }))
        );
    }

    /// Shape of real a7 III live view: 1024x680, 4:2:2, two tables, standard Huffman, no DRI.
    #[test]
    fn live_view_shape_golden() {
        let jpeg = build(&Spec {
            width: 1024,
            height: 680,
            y_sampling: 0x21,
            scan_len: 46 * 1024,
            ..Spec::default()
        });
        let mut packetizer = JpegPacketizer::new(config(1400)).unwrap();
        let mut pool = PacketPool::new();
        packetizer.packetize(&jpeg, 1, &mut pool).unwrap();
        let first = pool.get(0).unwrap();
        // type 0, Q 255, 128 x 85 units
        assert_eq!(&first[HEADER_LEN + 4..HEADER_LEN + 8], [0, 255, 128, 85]);
        let got = depacketize(&pool, 1400);
        assert_eq!((got.width, got.height), (1024, 680));
        assert_eq!(got.sampling, Sampling::Yuv422);
        assert_eq!(got.restart, None);
        assert_eq!(got.tables, vec![table(0), table(1)]);
        assert_eq!(got.scan, JpegInfo::parse(&jpeg).unwrap().scan);
    }

    #[test]
    fn steady_state_reuses_pool() {
        let mut packetizer = JpegPacketizer::new(config(1400)).unwrap();
        let mut pool = PacketPool::new();
        let jpeg = build(&Spec {
            scan_len: 48 * 1024,
            ..Spec::default()
        });
        packetizer.packetize(&jpeg, 0, &mut pool).unwrap();
        let (ptr, cap) = (pool.data.as_ptr(), pool.data.capacity());
        packetizer.packetize(&jpeg, 1, &mut pool).unwrap();
        assert_eq!((pool.data.as_ptr(), pool.data.capacity()), (ptr, cap));
    }

    proptest! {
        #[test]
        fn parser_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..1024)) {
            let _ = JpegInfo::parse(&bytes);
            let mut packetizer = JpegPacketizer::new(config(400)).unwrap();
            let mut pool = PacketPool::new();
            let _ = packetizer.packetize(&bytes, 0, &mut pool);
        }

        #[test]
        fn parser_never_panics_on_mutated_jpeg(
            spec_len in 0usize..400,
            edits in proptest::collection::vec((any::<usize>(), any::<u8>()), 0..8),
            cut in any::<usize>(),
        ) {
            let mut jpeg = build(&Spec { scan_len: spec_len, restart: 4, ..Spec::default() });
            for (at, byte) in edits {
                let len = jpeg.len();
                jpeg[at % len] = byte;
            }
            jpeg.truncate(cut % (jpeg.len() + 1));
            let _ = JpegInfo::parse(&jpeg);
        }

        #[test]
        fn round_trips(
            scan_len in 1usize..20_000,
            mtu in 157usize..2000,
            restart in prop_oneof![Just(0u16), 1u16..500],
            wide: bool,
            distinct: bool,
            width_units in 1u16..=255,
            height_units in 1u16..=255,
        ) {
            let (width, height) = (width_units * 8, height_units * 8);
            let jpeg = build(&Spec {
                width,
                height,
                y_sampling: if wide { 0x21 } else { 0x22 },
                tables: if distinct { [2, 3, 3] } else { [0, 1, 1] },
                restart,
                scan_len,
            });
            let info = JpegInfo::parse(&jpeg).unwrap();
            let mut packetizer = JpegPacketizer::new(config(mtu)).unwrap();
            let mut pool = PacketPool::new();
            packetizer.packetize(&jpeg, 5, &mut pool).unwrap();
            let got = depacketize(&pool, mtu);
            prop_assert_eq!((got.width, got.height), (width, height));
            prop_assert_eq!(got.sampling, if wide { Sampling::Yuv422 } else { Sampling::Yuv420 });
            prop_assert_eq!(got.restart, (restart != 0).then_some(restart));
            prop_assert_eq!(got.tables, vec![table(if distinct { 2 } else { 0 }), table(if distinct { 3 } else { 1 })]);
            prop_assert_eq!(got.scan, info.scan.to_vec());
        }
    }
}
