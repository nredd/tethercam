//! Bounds-checked little-endian primitives for PTP payloads.
//!
//! [`Reader`] consumes a byte slice and [`Writer`] appends to a `Vec<u8>`. Every read is
//! bounds-checked and returns [`CodecError`] instead of panicking, so any decoder built on it
//! is total.
//!
//! PTP strings are a `u8` count of UTF-16 code units (including the terminating NUL), then that
//! many little-endian `u16` units. An empty string is a single `0x00` count byte. PTP arrays are
//! a `u32` element count followed by the elements.
//!
//! References:
//! - ISO 15740 / PIMA 15740:2000 section 5.2 "Simple types", 5.2.2 "Strings", 5.2.3 "Arrays"

use thiserror::Error;

/// Longest PTP string in UTF-16 code units, counting the terminating NUL.
pub const MAX_STRING_UNITS: usize = 255;

/// Why a payload could not be decoded or encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CodecError {
    /// The input ended before a field was complete.
    #[error("payload truncated: needed '{needed}' more bytes, '{available}' available")]
    Truncated {
        /// Bytes the field required.
        needed: usize,
        /// Bytes that were left.
        available: usize,
    },
    /// A string's UTF-16 data is not valid (unpaired surrogate).
    #[error("string is not valid UTF-16")]
    InvalidUtf16,
    /// A non-empty string did not end with a NUL unit.
    #[error("string is missing its NUL terminator")]
    MissingTerminator,
    /// A string does not fit the one-byte unit count.
    #[error("string of '{0}' UTF-16 units (with NUL) exceeds the maximum of {MAX_STRING_UNITS}")]
    StringTooLong(usize),
    /// A collection has more elements than its count field can express.
    #[error("collection of '{0}' elements does not fit its count field")]
    CollectionTooLong(usize),
    /// The datatype code is not a PTP datatype.
    #[error("unknown datatype code '{0:#06x}'")]
    UnknownDataType(u16),
    /// A `GetSet` byte other than 0 (read-only) or 1 (read-write).
    #[error("unknown property `GetSet` value '{0:#04x}'")]
    UnknownGetSet(u8),
    /// A form flag other than 0 (none), 1 (range) or 2 (enumeration).
    #[error("unknown property form flag '{0:#04x}'")]
    UnknownFormFlag(u8),
    /// A value's datatype disagrees with the datatype it must have.
    #[error("value datatype '{actual:#06x}' does not match expected '{expected:#06x}'")]
    DataTypeMismatch {
        /// Datatype code the dataset requires.
        expected: u16,
        /// Datatype code of the supplied value.
        actual: u16,
    },
}

/// A cursor over a byte slice that reads little-endian PTP primitives.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
}

macro_rules! reader_ints {
    ($($name:ident: $ty:ty),* $(,)?) => {$(
        #[doc = concat!("Read a little-endian `", stringify!($ty), "`.")]
        ///
        /// # Errors
        ///
        /// [`CodecError::Truncated`] if too few bytes remain.
        pub fn $name(&mut self) -> Result<$ty, CodecError> {
            Ok(<$ty>::from_le_bytes(self.fixed()?))
        }
    )*};
}

impl<'a> Reader<'a> {
    /// Start reading at the beginning of `buf`.
    #[must_use]
    pub const fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    /// Bytes not yet consumed.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.buf.len()
    }

    /// Whether every byte has been consumed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Consume exactly `n` bytes.
    ///
    /// # Errors
    ///
    /// [`CodecError::Truncated`] if fewer than `n` bytes remain.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        let (head, tail) = self.buf.split_at_checked(n).ok_or(CodecError::Truncated {
            needed: n,
            available: self.buf.len(),
        })?;
        self.buf = tail;
        Ok(head)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        let available = self.buf.len();
        let head = self.take(N)?;
        <[u8; N]>::try_from(head).map_err(|_| CodecError::Truncated {
            needed: N,
            available,
        })
    }

    reader_ints! {
        u8: u8, i8: i8, u16: u16, i16: i16, u32: u32, i32: i32,
        u64: u64, i64: i64, u128: u128, i128: i128,
    }

    /// Read a PTP string.
    ///
    /// # Errors
    ///
    /// [`CodecError::Truncated`], [`CodecError::MissingTerminator`] or
    /// [`CodecError::InvalidUtf16`].
    pub fn string(&mut self) -> Result<String, CodecError> {
        let units = usize::from(self.u8()?);
        if units == 0 {
            return Ok(String::new());
        }
        let raw = self.take(units.saturating_mul(2))?;
        let (pairs, _) = raw.as_chunks::<2>();
        let mut decoded: Vec<u16> = pairs.iter().map(|p| u16::from_le_bytes(*p)).collect();
        if decoded.pop() != Some(0) {
            return Err(CodecError::MissingTerminator);
        }
        char::decode_utf16(decoded)
            .collect::<Result<String, _>>()
            .map_err(|_| CodecError::InvalidUtf16)
    }

    /// Read a `u32`-counted array whose elements are `elem_size` bytes at minimum (`0` is
    /// treated as `1`).
    ///
    /// The count is checked against the bytes left before anything is allocated, so a hostile
    /// count cannot force a large allocation.
    ///
    /// # Errors
    ///
    /// [`CodecError::Truncated`], or whatever `read` returns.
    pub fn array<T>(
        &mut self, elem_size: usize, mut read: impl FnMut(&mut Self) -> Result<T, CodecError>,
    ) -> Result<Vec<T>, CodecError> {
        let count = self.u32()?;
        let count = usize::try_from(count).unwrap_or(usize::MAX);
        let needed = count.saturating_mul(elem_size.max(1));
        if needed > self.buf.len() {
            return Err(CodecError::Truncated {
                needed,
                available: self.buf.len(),
            });
        }
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            items.push(read(self)?);
        }
        Ok(items)
    }
}

/// A growable buffer that writes little-endian PTP primitives.
#[derive(Debug, Clone, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

macro_rules! writer_ints {
    ($($name:ident: $ty:ty),* $(,)?) => {$(
        #[doc = concat!("Append a little-endian `", stringify!($ty), "`.")]
        pub fn $name(&mut self, value: $ty) {
            self.buf.extend_from_slice(&value.to_le_bytes());
        }
    )*};
}

impl Writer {
    /// An empty writer.
    #[must_use]
    pub const fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// The bytes written so far.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Consume the writer, returning the bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    writer_ints! {
        u8: u8, i8: i8, u16: u16, i16: i16, u32: u32, i32: i32,
        u64: u64, i64: i64, u128: u128, i128: i128,
    }

    /// Append a PTP string.
    ///
    /// # Errors
    ///
    /// [`CodecError::StringTooLong`] past [`MAX_STRING_UNITS`] units including the NUL. Nothing
    /// is written on error.
    pub fn string(&mut self, value: &str) -> Result<(), CodecError> {
        if value.is_empty() {
            self.u8(0);
            return Ok(());
        }
        let units = value.encode_utf16().count().saturating_add(1);
        let count = u8::try_from(units).map_err(|_| CodecError::StringTooLong(units))?;
        self.u8(count);
        for unit in value.encode_utf16() {
            self.u16(unit);
        }
        self.u16(0);
        Ok(())
    }

    /// Append a `u32`-counted array.
    ///
    /// # Errors
    ///
    /// [`CodecError::CollectionTooLong`] if `items` has more than `u32::MAX` elements. Nothing
    /// is written on error.
    pub fn array<T>(
        &mut self, items: &[T], mut write: impl FnMut(&mut Self, &T),
    ) -> Result<(), CodecError> {
        let count =
            u32::try_from(items.len()).map_err(|_| CodecError::CollectionTooLong(items.len()))?;
        self.u32(count);
        for item in items {
            write(self, item);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_little_endian_integers() {
        let mut r = Reader::new(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07]);
        assert_eq!(r.u8(), Ok(0x01));
        assert_eq!(r.u16(), Ok(0x0302));
        assert_eq!(r.u32(), Ok(0x0706_0504));
        assert!(r.is_empty());
    }

    #[test]
    fn truncated_read_reports_sizes_and_consumes_nothing() {
        let mut r = Reader::new(&[1, 2, 3]);
        assert_eq!(
            r.u32(),
            Err(CodecError::Truncated {
                needed: 4,
                available: 3
            })
        );
        assert_eq!(r.remaining(), 3);
    }

    #[test]
    fn signed_and_wide_integers() {
        let mut r = Reader::new(&[0xFF, 0xFE, 0xFF]);
        assert_eq!(r.i8(), Ok(-1));
        assert_eq!(r.i16(), Ok(-2));
        let wide = [0xFF; 16];
        assert_eq!(Reader::new(&wide).u128(), Ok(u128::MAX));
        assert_eq!(Reader::new(&wide).i128(), Ok(-1));
    }

    #[test]
    fn string_golden_bytes() {
        // "Hi" -> count 3, 'H', 'i', NUL
        let wire = [3, b'H', 0, b'i', 0, 0, 0];
        assert_eq!(Reader::new(&wire).string().as_deref(), Ok("Hi"));
        let mut w = Writer::new();
        w.string("Hi").unwrap();
        assert_eq!(w.as_bytes(), wire);
    }

    #[test]
    fn empty_string_is_one_zero_byte() {
        assert_eq!(Reader::new(&[0]).string().as_deref(), Ok(""));
        let mut w = Writer::new();
        w.string("").unwrap();
        assert_eq!(w.as_bytes(), [0]);
        // A count of one with just the NUL is also an empty string.
        assert_eq!(Reader::new(&[1, 0, 0]).string().as_deref(), Ok(""));
    }

    #[test]
    fn string_errors() {
        assert_eq!(
            Reader::new(&[2, b'a', 0, b'b', 0]).string(),
            Err(CodecError::MissingTerminator)
        );
        assert_eq!(
            Reader::new(&[2, 0x00, 0xD8, 0, 0]).string(),
            Err(CodecError::InvalidUtf16)
        );
        assert!(matches!(
            Reader::new(&[3, b'a', 0]).string(),
            Err(CodecError::Truncated { .. })
        ));
    }

    #[test]
    fn string_limit_is_254_units_plus_nul() {
        let mut w = Writer::new();
        assert!(w.string(&"a".repeat(254)).is_ok());
        let mut w = Writer::new();
        assert_eq!(
            w.string(&"a".repeat(255)),
            Err(CodecError::StringTooLong(256))
        );
        assert_eq!(w.as_bytes(), []);
    }

    #[test]
    fn astral_characters_use_surrogate_pairs() {
        let mut w = Writer::new();
        w.string("\u{1F4F7}").unwrap();
        assert_eq!(w.as_bytes()[0], 3);
        let bytes = w.into_bytes();
        assert_eq!(Reader::new(&bytes).string().as_deref(), Ok("\u{1F4F7}"));
    }

    #[test]
    fn array_golden_bytes() {
        let wire = [2, 0, 0, 0, 0x01, 0x10, 0x02, 0x10];
        let items = Reader::new(&wire).array(2, Reader::u16).unwrap();
        assert_eq!(items, [0x1001, 0x1002]);
        let mut w = Writer::new();
        w.array(&items, |w, v| w.u16(*v)).unwrap();
        assert_eq!(w.as_bytes(), wire);
    }

    #[test]
    fn zero_elem_size_cannot_bypass_the_bound() {
        let wire = [0xFF, 0xFF, 0xFF, 0xFF];
        assert!(matches!(
            Reader::new(&wire).array(0, Reader::u16),
            Err(CodecError::Truncated { .. })
        ));
    }

    #[test]
    fn hostile_array_count_does_not_allocate() {
        let wire = [0xFF, 0xFF, 0xFF, 0xFF, 0x00];
        assert!(matches!(
            Reader::new(&wire).array(1, Reader::u8),
            Err(CodecError::Truncated { .. })
        ));
    }
}
