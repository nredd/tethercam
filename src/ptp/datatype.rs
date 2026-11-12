//! PTP datatype codes and the dynamically typed [`Value`].
//!
//! Device properties carry values whose type is only known from a datatype code at run time.
//! [`DataType`] names that code and [`Value`] holds a decoded value of any datatype.
//!
//! ```text
//! 0x0001 INT8     0x0002 UINT8     0x0003 INT16    0x0004 UINT16   0x0005 INT32
//! 0x0006 UINT32   0x0007 INT64     0x0008 UINT64   0x0009 INT128   0x000A UINT128
//! 0x4001..0x400A  arrays of the above (0x4000 | scalar code)
//! 0xFFFF          string
//! ```
//!
//! References:
//! - ISO 15740 / PIMA 15740:2000 section 5.2 "Simple types", table 3 "Datatype codes"

use super::codec::{CodecError, Reader, Writer};

/// Bit set on a scalar datatype code to form the matching array code.
const ARRAY_FLAG: u16 = 0x4000;

/// A PTP datatype code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    /// `0x0001`, signed 8-bit integer.
    Int8,
    /// `0x0002`, unsigned 8-bit integer.
    UInt8,
    /// `0x0003`, signed 16-bit integer.
    Int16,
    /// `0x0004`, unsigned 16-bit integer.
    UInt16,
    /// `0x0005`, signed 32-bit integer.
    Int32,
    /// `0x0006`, unsigned 32-bit integer.
    UInt32,
    /// `0x0007`, signed 64-bit integer.
    Int64,
    /// `0x0008`, unsigned 64-bit integer.
    UInt64,
    /// `0x0009`, signed 128-bit integer.
    Int128,
    /// `0x000A`, unsigned 128-bit integer.
    UInt128,
    /// `0x4001`, array of signed 8-bit integers.
    Int8Array,
    /// `0x4002`, array of unsigned 8-bit integers.
    UInt8Array,
    /// `0x4003`, array of signed 16-bit integers.
    Int16Array,
    /// `0x4004`, array of unsigned 16-bit integers.
    UInt16Array,
    /// `0x4005`, array of signed 32-bit integers.
    Int32Array,
    /// `0x4006`, array of unsigned 32-bit integers.
    UInt32Array,
    /// `0x4007`, array of signed 64-bit integers.
    Int64Array,
    /// `0x4008`, array of unsigned 64-bit integers.
    UInt64Array,
    /// `0x4009`, array of signed 128-bit integers.
    Int128Array,
    /// `0x400A`, array of unsigned 128-bit integers.
    UInt128Array,
    /// `0xFFFF`, UTF-16 string.
    String,
}

impl DataType {
    /// The wire code.
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            Self::Int8 => 0x0001,
            Self::UInt8 => 0x0002,
            Self::Int16 => 0x0003,
            Self::UInt16 => 0x0004,
            Self::Int32 => 0x0005,
            Self::UInt32 => 0x0006,
            Self::Int64 => 0x0007,
            Self::UInt64 => 0x0008,
            Self::Int128 => 0x0009,
            Self::UInt128 => 0x000A,
            Self::Int8Array => ARRAY_FLAG | 0x0001,
            Self::UInt8Array => ARRAY_FLAG | 0x0002,
            Self::Int16Array => ARRAY_FLAG | 0x0003,
            Self::UInt16Array => ARRAY_FLAG | 0x0004,
            Self::Int32Array => ARRAY_FLAG | 0x0005,
            Self::UInt32Array => ARRAY_FLAG | 0x0006,
            Self::Int64Array => ARRAY_FLAG | 0x0007,
            Self::UInt64Array => ARRAY_FLAG | 0x0008,
            Self::Int128Array => ARRAY_FLAG | 0x0009,
            Self::UInt128Array => ARRAY_FLAG | 0x000A,
            Self::String => 0xFFFF,
        }
    }

    /// Whether this is one of the array forms.
    #[must_use]
    pub const fn is_array(self) -> bool {
        self.code() & ARRAY_FLAG != 0 && self.code() != 0xFFFF
    }
}

impl TryFrom<u16> for DataType {
    type Error = CodecError;

    fn try_from(code: u16) -> Result<Self, Self::Error> {
        Ok(match code {
            0x0001 => Self::Int8,
            0x0002 => Self::UInt8,
            0x0003 => Self::Int16,
            0x0004 => Self::UInt16,
            0x0005 => Self::Int32,
            0x0006 => Self::UInt32,
            0x0007 => Self::Int64,
            0x0008 => Self::UInt64,
            0x0009 => Self::Int128,
            0x000A => Self::UInt128,
            0x4001 => Self::Int8Array,
            0x4002 => Self::UInt8Array,
            0x4003 => Self::Int16Array,
            0x4004 => Self::UInt16Array,
            0x4005 => Self::Int32Array,
            0x4006 => Self::UInt32Array,
            0x4007 => Self::Int64Array,
            0x4008 => Self::UInt64Array,
            0x4009 => Self::Int128Array,
            0x400A => Self::UInt128Array,
            0xFFFF => Self::String,
            other => return Err(CodecError::UnknownDataType(other)),
        })
    }
}

/// A decoded PTP value of any datatype.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Value {
    /// `INT8`.
    Int8(i8),
    /// `UINT8`.
    UInt8(u8),
    /// `INT16`.
    Int16(i16),
    /// `UINT16`.
    UInt16(u16),
    /// `INT32`.
    Int32(i32),
    /// `UINT32`.
    UInt32(u32),
    /// `INT64`.
    Int64(i64),
    /// `UINT64`.
    UInt64(u64),
    /// `INT128`.
    Int128(i128),
    /// `UINT128`.
    UInt128(u128),
    /// `AINT8`.
    Int8Array(Vec<i8>),
    /// `AUINT8`.
    UInt8Array(Vec<u8>),
    /// `AINT16`.
    Int16Array(Vec<i16>),
    /// `AUINT16`.
    UInt16Array(Vec<u16>),
    /// `AINT32`.
    Int32Array(Vec<i32>),
    /// `AUINT32`.
    UInt32Array(Vec<u32>),
    /// `AINT64`.
    Int64Array(Vec<i64>),
    /// `AUINT64`.
    UInt64Array(Vec<u64>),
    /// `AINT128`.
    Int128Array(Vec<i128>),
    /// `AUINT128`.
    UInt128Array(Vec<u128>),
    /// `STR`.
    String(String),
}

impl Value {
    /// The datatype of this value.
    #[must_use]
    pub const fn data_type(&self) -> DataType {
        match self {
            Self::Int8(_) => DataType::Int8,
            Self::UInt8(_) => DataType::UInt8,
            Self::Int16(_) => DataType::Int16,
            Self::UInt16(_) => DataType::UInt16,
            Self::Int32(_) => DataType::Int32,
            Self::UInt32(_) => DataType::UInt32,
            Self::Int64(_) => DataType::Int64,
            Self::UInt64(_) => DataType::UInt64,
            Self::Int128(_) => DataType::Int128,
            Self::UInt128(_) => DataType::UInt128,
            Self::Int8Array(_) => DataType::Int8Array,
            Self::UInt8Array(_) => DataType::UInt8Array,
            Self::Int16Array(_) => DataType::Int16Array,
            Self::UInt16Array(_) => DataType::UInt16Array,
            Self::Int32Array(_) => DataType::Int32Array,
            Self::UInt32Array(_) => DataType::UInt32Array,
            Self::Int64Array(_) => DataType::Int64Array,
            Self::UInt64Array(_) => DataType::UInt64Array,
            Self::Int128Array(_) => DataType::Int128Array,
            Self::UInt128Array(_) => DataType::UInt128Array,
            Self::String(_) => DataType::String,
        }
    }

    /// Read one value of `data_type` from `reader`.
    ///
    /// # Errors
    ///
    /// Any [`CodecError`] from the underlying reads.
    pub fn decode(data_type: DataType, reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(match data_type {
            DataType::Int8 => Self::Int8(reader.i8()?),
            DataType::UInt8 => Self::UInt8(reader.u8()?),
            DataType::Int16 => Self::Int16(reader.i16()?),
            DataType::UInt16 => Self::UInt16(reader.u16()?),
            DataType::Int32 => Self::Int32(reader.i32()?),
            DataType::UInt32 => Self::UInt32(reader.u32()?),
            DataType::Int64 => Self::Int64(reader.i64()?),
            DataType::UInt64 => Self::UInt64(reader.u64()?),
            DataType::Int128 => Self::Int128(reader.i128()?),
            DataType::UInt128 => Self::UInt128(reader.u128()?),
            DataType::Int8Array => Self::Int8Array(reader.array(1, Reader::i8)?),
            DataType::UInt8Array => Self::UInt8Array(reader.array(1, Reader::u8)?),
            DataType::Int16Array => Self::Int16Array(reader.array(2, Reader::i16)?),
            DataType::UInt16Array => Self::UInt16Array(reader.array(2, Reader::u16)?),
            DataType::Int32Array => Self::Int32Array(reader.array(4, Reader::i32)?),
            DataType::UInt32Array => Self::UInt32Array(reader.array(4, Reader::u32)?),
            DataType::Int64Array => Self::Int64Array(reader.array(8, Reader::i64)?),
            DataType::UInt64Array => Self::UInt64Array(reader.array(8, Reader::u64)?),
            DataType::Int128Array => Self::Int128Array(reader.array(16, Reader::i128)?),
            DataType::UInt128Array => Self::UInt128Array(reader.array(16, Reader::u128)?),
            DataType::String => Self::String(reader.string()?),
        })
    }

    /// Decode one value of `data_type` from the start of `bytes`; trailing bytes are ignored.
    ///
    /// # Errors
    ///
    /// Any [`CodecError`] from the underlying reads.
    pub fn from_bytes(data_type: DataType, bytes: &[u8]) -> Result<Self, CodecError> {
        Self::decode(data_type, &mut Reader::new(bytes))
    }

    /// Append this value's wire form to `writer`.
    ///
    /// # Errors
    ///
    /// [`CodecError::StringTooLong`] or [`CodecError::CollectionTooLong`].
    pub fn encode(&self, writer: &mut Writer) -> Result<(), CodecError> {
        match self {
            Self::Int8(v) => writer.i8(*v),
            Self::UInt8(v) => writer.u8(*v),
            Self::Int16(v) => writer.i16(*v),
            Self::UInt16(v) => writer.u16(*v),
            Self::Int32(v) => writer.i32(*v),
            Self::UInt32(v) => writer.u32(*v),
            Self::Int64(v) => writer.i64(*v),
            Self::UInt64(v) => writer.u64(*v),
            Self::Int128(v) => writer.i128(*v),
            Self::UInt128(v) => writer.u128(*v),
            Self::Int8Array(v) => return writer.array(v, |w, x| w.i8(*x)),
            Self::UInt8Array(v) => return writer.array(v, |w, x| w.u8(*x)),
            Self::Int16Array(v) => return writer.array(v, |w, x| w.i16(*x)),
            Self::UInt16Array(v) => return writer.array(v, |w, x| w.u16(*x)),
            Self::Int32Array(v) => return writer.array(v, |w, x| w.i32(*x)),
            Self::UInt32Array(v) => return writer.array(v, |w, x| w.u32(*x)),
            Self::Int64Array(v) => return writer.array(v, |w, x| w.i64(*x)),
            Self::UInt64Array(v) => return writer.array(v, |w, x| w.u64(*x)),
            Self::Int128Array(v) => return writer.array(v, |w, x| w.i128(*x)),
            Self::UInt128Array(v) => return writer.array(v, |w, x| w.u128(*x)),
            Self::String(v) => return writer.string(v),
        }
        Ok(())
    }

    /// The wire form of this value.
    ///
    /// # Errors
    ///
    /// [`CodecError::StringTooLong`] or [`CodecError::CollectionTooLong`].
    pub fn to_bytes(&self) -> Result<Vec<u8>, CodecError> {
        let mut writer = Writer::new();
        self.encode(&mut writer)?;
        Ok(writer.into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [DataType; 21] = [
        DataType::Int8,
        DataType::UInt8,
        DataType::Int16,
        DataType::UInt16,
        DataType::Int32,
        DataType::UInt32,
        DataType::Int64,
        DataType::UInt64,
        DataType::Int128,
        DataType::UInt128,
        DataType::Int8Array,
        DataType::UInt8Array,
        DataType::Int16Array,
        DataType::UInt16Array,
        DataType::Int32Array,
        DataType::UInt32Array,
        DataType::Int64Array,
        DataType::UInt64Array,
        DataType::Int128Array,
        DataType::UInt128Array,
        DataType::String,
    ];

    #[test]
    fn codes_round_trip_and_match_the_spec() {
        for dt in ALL {
            assert_eq!(DataType::try_from(dt.code()), Ok(dt));
        }
        assert_eq!(DataType::UInt16.code(), 0x0004);
        assert_eq!(DataType::UInt16Array.code(), 0x4004);
        assert_eq!(DataType::String.code(), 0xFFFF);
    }

    #[test]
    fn unknown_codes_are_rejected() {
        for code in [0x0000, 0x000B, 0x4000, 0x400B, 0x8001, 0xFFFE] {
            assert_eq!(
                DataType::try_from(code),
                Err(CodecError::UnknownDataType(code))
            );
        }
    }

    #[test]
    fn array_flag() {
        assert!(DataType::UInt8Array.is_array());
        assert!(!DataType::UInt8.is_array());
        assert!(!DataType::String.is_array());
    }

    #[test]
    fn golden_scalars() {
        let cases: [(Value, &[u8]); 6] = [
            (Value::Int8(-2), &[0xFE]),
            (Value::UInt16(0x1234), &[0x34, 0x12]),
            (Value::Int32(-1), &[0xFF; 4]),
            (Value::UInt64(1), &[1, 0, 0, 0, 0, 0, 0, 0]),
            (
                Value::UInt128(1),
                &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            ),
            (Value::String("A".into()), &[2, 0x41, 0, 0, 0]),
        ];
        for (value, wire) in cases {
            assert_eq!(value.to_bytes().unwrap(), wire);
            assert_eq!(Value::from_bytes(value.data_type(), wire), Ok(value));
        }
    }

    #[test]
    fn every_variant_round_trips() {
        let values = [
            Value::Int8(-8),
            Value::UInt8(8),
            Value::Int16(-16),
            Value::UInt16(16),
            Value::Int32(-32),
            Value::UInt32(32),
            Value::Int64(-64),
            Value::UInt64(64),
            Value::Int128(-128),
            Value::UInt128(128),
            Value::Int8Array(vec![-1, 2]),
            Value::UInt8Array(vec![1, 2]),
            Value::Int16Array(vec![-1, 2]),
            Value::UInt16Array(vec![1, 2]),
            Value::Int32Array(vec![-1, 2]),
            Value::UInt32Array(vec![1, 2]),
            Value::Int64Array(vec![-1, 2]),
            Value::UInt64Array(vec![1, 2]),
            Value::Int128Array(vec![-1, 2]),
            Value::UInt128Array(vec![1, 2]),
            Value::String("text".into()),
        ];
        for (value, dt) in values.into_iter().zip(ALL) {
            assert_eq!(value.data_type(), dt);
            let wire = value.to_bytes().unwrap();
            assert_eq!(Value::from_bytes(dt, &wire), Ok(value));
        }
    }

    #[test]
    fn overlong_string_fails_to_encode() {
        let value = Value::String("a".repeat(255));
        assert_eq!(value.to_bytes(), Err(CodecError::StringTooLong(256)));
    }

    #[test]
    fn golden_array() {
        let value = Value::UInt16Array(vec![0x5001, 0x5002]);
        let wire = [2, 0, 0, 0, 0x01, 0x50, 0x02, 0x50];
        assert_eq!(value.to_bytes().unwrap(), wire);
        assert_eq!(Value::from_bytes(DataType::UInt16Array, &wire), Ok(value));
    }

    #[test]
    fn every_type_reports_truncation_on_empty_input() {
        for dt in ALL {
            let err = Value::from_bytes(dt, &[]).unwrap_err();
            assert_eq!(
                std::mem::discriminant(&err),
                std::mem::discriminant(&CodecError::Truncated {
                    needed: 0,
                    available: 0
                }),
                "{dt:?}"
            );
        }
    }

    #[test]
    fn value_data_type_matches_decode_request() {
        for dt in ALL {
            let wire = [0_u8; 32];
            let value = Value::from_bytes(dt, &wire).unwrap();
            assert_eq!(value.data_type(), dt);
        }
    }
}
