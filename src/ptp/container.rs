//! PTP generic container, the framing used on USB bulk and interrupt pipes.
//!
//! Every PTP phase (command, data, response, event) travels in a container: a 12-byte
//! little-endian header followed by a payload.
//!
//! ```text
//! offset  size  field
//!      0     4  length (header + payload, bytes)
//!      4     2  kind (1 command, 2 data, 3 response, 4 event)
//!      6     2  code (operation, response or event code)
//!      8     4  transaction id
//!     12     n  payload
//! ```
//!
//! References:
//! - USB Still Image Capture Device Definition 1.0, section 7.1 "Generic Container Structure"

use thiserror::Error;

/// Size of the fixed container header in bytes.
pub const HEADER_LEN: usize = 12;

/// [`HEADER_LEN`] as it appears in the `length` field.
const HEADER_LEN_U32: u32 = 12;

/// The `length` field for a container carrying `payload_len` payload bytes.
fn wire_length(payload_len: usize) -> Result<u32, ContainerError> {
    payload_len
        .checked_add(HEADER_LEN)
        .and_then(|total| u32::try_from(total).ok())
        .ok_or(ContainerError::PayloadTooLarge(payload_len))
}

/// Maximum number of `u32` parameters in a command or response container.
pub const MAX_PARAMS: usize = 5;

/// Why a container could not be decoded or encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ContainerError {
    /// The input is shorter than the fixed header.
    #[error("container needs at least {HEADER_LEN} header bytes, got '{0}'")]
    TruncatedHeader(usize),
    /// The header's length field is smaller than the header itself.
    #[error("container `length` field '{0}' is smaller than the {HEADER_LEN}-byte header")]
    LengthTooSmall(u32),
    /// The header's kind field is not one of the four PTP container kinds.
    #[error("unknown container `kind` '{0:#06x}'")]
    UnknownKind(u16),
    /// The input ends before the payload promised by the length field.
    #[error("container `length` field promises '{expected}' bytes, got '{actual}'")]
    TruncatedPayload {
        /// Total bytes the length field announced.
        expected: usize,
        /// Total bytes actually available.
        actual: usize,
    },
    /// The payload does not fit the 32-bit length field.
    #[error("payload of '{0}' bytes does not fit the 32-bit `length` field")]
    PayloadTooLarge(usize),
    /// More parameters were supplied than a container can carry.
    #[error("a container carries at most {MAX_PARAMS} parameters, got '{0}'")]
    TooManyParams(usize),
}

/// The phase a container belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum ContainerKind {
    /// Host to device: an operation request with up to [`MAX_PARAMS`] parameters.
    Command = 1,
    /// Either direction: the data phase of an operation.
    Data = 2,
    /// Device to host: the result of an operation.
    Response = 3,
    /// Device to host, on the interrupt pipe: an asynchronous event.
    Event = 4,
}

impl TryFrom<u16> for ContainerKind {
    type Error = ContainerError;

    fn try_from(raw: u16) -> Result<Self, Self::Error> {
        match raw {
            1 => Ok(Self::Command),
            2 => Ok(Self::Data),
            3 => Ok(Self::Response),
            4 => Ok(Self::Event),
            other => Err(ContainerError::UnknownKind(other)),
        }
    }
}

/// The fixed 12-byte header that starts every container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerHeader {
    /// Total container size in bytes, header included.
    pub length: u32,
    /// Phase of the container.
    pub kind: ContainerKind,
    /// Operation, response or event code, depending on `kind`.
    pub code: u16,
    /// Transaction this container belongs to.
    pub transaction_id: u32,
}

impl ContainerHeader {
    /// Decodes a header from the start of `bytes`. Trailing bytes are ignored.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError::TruncatedHeader`] when fewer than [`HEADER_LEN`] bytes are
    /// given, [`ContainerError::UnknownKind`] for an invalid kind, and
    /// [`ContainerError::LengthTooSmall`] when the length field is below [`HEADER_LEN`].
    pub fn decode(bytes: &[u8]) -> Result<Self, ContainerError> {
        let truncated = ContainerError::TruncatedHeader(bytes.len());
        let (length, rest) = bytes.split_first_chunk::<4>().ok_or(truncated)?;
        let (kind, rest) = rest.split_first_chunk::<2>().ok_or(truncated)?;
        let (code, rest) = rest.split_first_chunk::<2>().ok_or(truncated)?;
        let (transaction_id, _) = rest.split_first_chunk::<4>().ok_or(truncated)?;

        let length = u32::from_le_bytes(*length);
        let kind = ContainerKind::try_from(u16::from_le_bytes(*kind))?;
        if usize::try_from(length).is_ok_and(|len| len < HEADER_LEN) {
            return Err(ContainerError::LengthTooSmall(length));
        }
        Ok(Self {
            length,
            kind,
            code: u16::from_le_bytes(*code),
            transaction_id: u32::from_le_bytes(*transaction_id),
        })
    }

    /// Encodes the header into its 12-byte wire form.
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0_u8; HEADER_LEN];
        let (length, rest) = out.split_at_mut(4);
        let (kind, rest) = rest.split_at_mut(2);
        let (code, transaction_id) = rest.split_at_mut(2);
        length.copy_from_slice(&self.length.to_le_bytes());
        kind.copy_from_slice(&(self.kind as u16).to_le_bytes());
        code.copy_from_slice(&self.code.to_le_bytes());
        transaction_id.copy_from_slice(&self.transaction_id.to_le_bytes());
        out
    }

    /// Number of payload bytes that follow the header.
    #[must_use]
    pub fn payload_len(&self) -> usize {
        usize::try_from(self.length).map_or(usize::MAX, |len| len.saturating_sub(HEADER_LEN))
    }
}

/// A decoded container: header fields plus a borrowed payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Container<'a> {
    /// Phase of the container.
    pub kind: ContainerKind,
    /// Operation, response or event code, depending on `kind`.
    pub code: u16,
    /// Transaction this container belongs to.
    pub transaction_id: u32,
    /// Bytes after the header. Parameters for command, response and event containers.
    pub payload: &'a [u8],
}

impl<'a> Container<'a> {
    /// Decodes one complete container from the start of `bytes`.
    ///
    /// Returns the container and the bytes that follow it, so back-to-back containers in one
    /// buffer can be walked.
    ///
    /// # Errors
    ///
    /// Returns any [`ContainerHeader::decode`] error, or
    /// [`ContainerError::TruncatedPayload`] when `bytes` ends before the announced length.
    pub fn decode(bytes: &'a [u8]) -> Result<(Self, &'a [u8]), ContainerError> {
        let header = ContainerHeader::decode(bytes)?;
        let body = bytes.get(HEADER_LEN..).unwrap_or_default();
        let (payload, rest) = body.split_at_checked(header.payload_len()).ok_or(
            ContainerError::TruncatedPayload {
                expected: header.payload_len().saturating_add(HEADER_LEN),
                actual: bytes.len(),
            },
        )?;
        let container = Self {
            kind: header.kind,
            code: header.code,
            transaction_id: header.transaction_id,
            payload,
        };
        Ok((container, rest))
    }

    /// Encodes the container, header first, appending to `out`.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError::PayloadTooLarge`] when header plus payload exceed `u32::MAX`.
    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), ContainerError> {
        wire_length(self.payload.len()).map(|length| {
            let header = ContainerHeader {
                length,
                kind: self.kind,
                code: self.code,
                transaction_id: self.transaction_id,
            };
            out.reserve(HEADER_LEN + self.payload.len());
            out.extend_from_slice(&header.encode());
            out.extend_from_slice(self.payload);
        })
    }

    /// Builds the wire bytes of a command container with `params` as its parameters.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError::TooManyParams`] when more than [`MAX_PARAMS`] are given.
    pub fn command(
        code: u16, transaction_id: u32, params: &[u32],
    ) -> Result<Vec<u8>, ContainerError> {
        if params.len() > MAX_PARAMS {
            return Err(ContainerError::TooManyParams(params.len()));
        }
        // At most `MAX_PARAMS` words, so the length fits `u32` by construction.
        let length = params
            .iter()
            .fold(HEADER_LEN_U32, |len, _| len.saturating_add(4));
        let header = ContainerHeader {
            length,
            kind: ContainerKind::Command,
            code,
            transaction_id,
        };
        let mut out = Vec::with_capacity(HEADER_LEN + params.len() * 4);
        out.extend_from_slice(&header.encode());
        for param in params {
            out.extend_from_slice(&param.to_le_bytes());
        }
        Ok(out)
    }

    /// Iterates the payload as little-endian `u32` parameters.
    ///
    /// A trailing partial word is ignored. Meaningful for command, response and event
    /// containers, which carry at most [`MAX_PARAMS`] parameters.
    pub fn params(&self) -> impl Iterator<Item = u32> + '_ {
        let (words, _) = self.payload.as_chunks::<4>();
        words
            .iter()
            .take(MAX_PARAMS)
            .map(|word| u32::from_le_bytes(*word))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `OpenSession` (0x1002), transaction 0, session id 1.
    const OPEN_SESSION: [u8; 16] = [
        0x10, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x10, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x00,
    ];

    #[test]
    fn command_matches_known_open_session_bytes() {
        assert_eq!(Container::command(0x1002, 0, &[1]).unwrap(), OPEN_SESSION);
    }

    #[test]
    fn decode_reads_fields_and_params() {
        let (container, rest) = Container::decode(&OPEN_SESSION).unwrap();
        assert_eq!(container.kind, ContainerKind::Command);
        assert_eq!(container.code, 0x1002);
        assert_eq!(container.transaction_id, 0);
        assert_eq!(container.params().collect::<Vec<_>>(), [1]);
        assert_eq!(rest, []);
    }

    #[test]
    fn decode_returns_trailing_bytes() {
        let mut bytes = OPEN_SESSION.to_vec();
        bytes.extend_from_slice(&[0xAA, 0xBB]);
        let (_, rest) = Container::decode(&bytes).unwrap();
        assert_eq!(rest, [0xAA, 0xBB]);
    }

    #[test]
    fn decode_rejects_short_header() {
        assert_eq!(
            Container::decode(&[0; 11]),
            Err(ContainerError::TruncatedHeader(11))
        );
        assert_eq!(
            Container::decode(&[]),
            Err(ContainerError::TruncatedHeader(0))
        );
    }

    #[test]
    fn decode_rejects_every_short_header() {
        for len in 0..HEADER_LEN {
            assert_eq!(
                ContainerHeader::decode(&OPEN_SESSION[..len]),
                Err(ContainerError::TruncatedHeader(len))
            );
        }
    }

    #[test]
    fn decode_rejects_length_below_header() {
        let mut bytes = OPEN_SESSION;
        bytes[..4].copy_from_slice(&11_u32.to_le_bytes());
        assert_eq!(
            Container::decode(&bytes),
            Err(ContainerError::LengthTooSmall(11))
        );
    }

    #[test]
    fn decode_accepts_every_kind() {
        for (raw, kind) in [
            (1_u16, ContainerKind::Command),
            (2, ContainerKind::Data),
            (3, ContainerKind::Response),
            (4, ContainerKind::Event),
        ] {
            let mut bytes = OPEN_SESSION;
            bytes[4..6].copy_from_slice(&raw.to_le_bytes());
            assert_eq!(Container::decode(&bytes).unwrap().0.kind, kind);
        }
    }

    #[test]
    fn decode_rejects_unknown_kind() {
        let mut bytes = OPEN_SESSION;
        bytes[4..6].copy_from_slice(&9_u16.to_le_bytes());
        assert_eq!(
            Container::decode(&bytes),
            Err(ContainerError::UnknownKind(9))
        );
    }

    #[test]
    fn decode_rejects_truncated_payload() {
        assert_eq!(
            Container::decode(&OPEN_SESSION[..14]),
            Err(ContainerError::TruncatedPayload {
                expected: 16,
                actual: 14
            })
        );
    }

    #[test]
    fn command_rejects_too_many_params() {
        assert_eq!(
            Container::command(0x1001, 0, &[0; 6]),
            Err(ContainerError::TooManyParams(6))
        );
    }

    #[test]
    fn wire_length_counts_the_header_and_rejects_overflow() {
        assert_eq!(wire_length(0), Ok(HEADER_LEN_U32));
        assert_eq!(
            wire_length(usize::MAX),
            Err(ContainerError::PayloadTooLarge(usize::MAX))
        );
        #[cfg(target_pointer_width = "64")]
        {
            let max = usize::try_from(u32::MAX - HEADER_LEN_U32).unwrap();
            assert_eq!(wire_length(max), Ok(u32::MAX));
            assert_eq!(
                wire_length(max + 1),
                Err(ContainerError::PayloadTooLarge(max + 1))
            );
        }
    }

    #[test]
    fn encode_into_appends_header_then_payload() {
        let mut out = vec![0xEE];
        Container {
            kind: ContainerKind::Response,
            code: 0x2001,
            transaction_id: 9,
            payload: &[1, 2],
        }
        .encode_into(&mut out)
        .unwrap();
        assert_eq!(out[0], 0xEE);
        let (container, rest) = Container::decode(&out[1..]).unwrap();
        assert_eq!(container.payload, [1, 2]);
        assert_eq!(rest, []);
    }

    #[test]
    fn params_ignores_partial_trailing_word() {
        let container = Container {
            kind: ContainerKind::Response,
            code: 0x2001,
            transaction_id: 7,
            payload: &[1, 0, 0, 0, 2, 0],
        };
        assert_eq!(container.params().collect::<Vec<_>>(), [1]);
    }
}
