//! Property descriptors from `SDIOGetAllExtDevicePropInfo` and the table built from them.
//!
//! One data phase carries every property: a little-endian `u64` count, then per property
//!
//! ```text
//! code u16 | datatype u16 | get/set u8 | enabled u8 | default | current | form u8 | form data
//! ```
//!
//! The form is `0` none, `1` range (`min`, `max`, `step`) or `2` enumeration. A 2.00 body sends
//! one list of values (`u16` count, then the values). A 3.00 body sends two: the values the
//! property may be set to, then the values it may report. Control entries on 2.00 bodies share
//! the layout. This differs from the standard `DevicePropDesc` by the `enabled` byte and the
//! second list, so it has its own decoder.
//!
//! References:
//! - `watsonmw/alphawire` `src/aw/aw-control.c`, `SDIO_ProcessDeviceProperties200` / `300`
//! - libgphoto2 `camlibs/ptp2/ptp.c`, `ptp_unpack_Sony_DPD`

use std::collections::BTreeMap;

use super::protocol::Protocol;
use crate::ptp::{CodecError, DataType, DevicePropCode, Reader, Value};

/// Smallest possible encoded property: code, datatype, get/set, enabled, two one-byte
/// values and the form byte. Bounds the count before anything is allocated.
const MIN_RECORD_LEN: usize = 9;

/// The values a property accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SonyForm {
    /// No constraint is reported.
    None,
    /// Any value `minimum + n * step` up to `maximum`.
    Range {
        /// Smallest accepted value.
        minimum: Value,
        /// Largest accepted value.
        maximum: Value,
        /// Increment between accepted values.
        step: Value,
    },
    /// Exactly these values.
    Enumeration {
        /// Values the property may be set to.
        settable: Vec<Value>,
        /// Values the property may report. Empty on 2.00 bodies, which send a single list,
        /// meaning "the same as `settable`".
        reported: Vec<Value>,
    },
}

/// One property descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SonyProperty {
    /// Which property this is.
    pub code: DevicePropCode,
    /// Raw get/set byte: `0` read-only, `1` read-write.
    pub get_set: u8,
    /// Raw enabled byte: `0` the property is currently disabled.
    pub enabled: u8,
    /// Value after a reset.
    pub factory_default: Value,
    /// Value right now.
    pub current: Value,
    /// Accepted values.
    pub form: SonyForm,
}

impl SonyProperty {
    /// The property's datatype.
    #[must_use]
    pub const fn data_type(&self) -> DataType {
        self.factory_default.data_type()
    }

    /// Whether the host may write the property.
    #[must_use]
    pub const fn is_writable(&self) -> bool {
        self.get_set == 1
    }

    /// Whether the camera currently honors the property.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled != 0
    }

    /// The current value as an integer, when it is a scalar integer.
    #[must_use]
    pub fn integer(&self) -> Option<i128> {
        match self.current {
            Value::Int8(v) => Some(i128::from(v)),
            Value::UInt8(v) => Some(i128::from(v)),
            Value::Int16(v) => Some(i128::from(v)),
            Value::UInt16(v) => Some(i128::from(v)),
            Value::Int32(v) => Some(i128::from(v)),
            Value::UInt32(v) => Some(i128::from(v)),
            Value::Int64(v) => Some(i128::from(v)),
            Value::UInt64(v) => Some(i128::from(v)),
            _ => None,
        }
    }

    fn read(protocol: Protocol, reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        let code = DevicePropCode(reader.u16()?);
        let data_type = DataType::try_from(reader.u16()?)?;
        let get_set = reader.u8()?;
        let enabled = reader.u8()?;
        let factory_default = Value::decode(data_type, reader)?;
        let current = Value::decode(data_type, reader)?;
        let form = match reader.u8()? {
            0 => SonyForm::None,
            1 => SonyForm::Range {
                minimum: Value::decode(data_type, reader)?,
                maximum: Value::decode(data_type, reader)?,
                step: Value::decode(data_type, reader)?,
            },
            2 => {
                let settable = read_values(data_type, reader)?;
                let reported = match protocol {
                    Protocol::V200 => Vec::new(),
                    Protocol::V300 => read_values(data_type, reader)?,
                };
                SonyForm::Enumeration { settable, reported }
            }
            other => return Err(CodecError::UnknownFormFlag(other)),
        };
        Ok(Self {
            code,
            get_set,
            enabled,
            factory_default,
            current,
            form,
        })
    }
}

fn read_values(data_type: DataType, reader: &mut Reader<'_>) -> Result<Vec<Value>, CodecError> {
    let count = reader.u16()?;
    // Every value takes at least one byte, so the count is bounded by the input.
    let mut values = Vec::with_capacity(usize::from(count).min(reader.remaining()));
    for _ in 0..count {
        values.push(Value::decode(data_type, reader)?);
    }
    Ok(values)
}

/// A scalar integer `Value` of `data_type` holding `n`, for the datatypes properties use.
fn integer_value(data_type: DataType, n: u8) -> Option<Value> {
    Some(match data_type {
        DataType::Int8 => Value::Int8(n.cast_signed()),
        DataType::UInt8 => Value::UInt8(n),
        DataType::Int16 => Value::Int16(i16::from(n)),
        DataType::UInt16 => Value::UInt16(u16::from(n)),
        DataType::Int32 => Value::Int32(i32::from(n)),
        DataType::UInt32 => Value::UInt32(u32::from(n)),
        DataType::Int64 => Value::Int64(i64::from(n)),
        DataType::UInt64 => Value::UInt64(u64::from(n)),
        _ => return None,
    })
}

/// Decodes a `SDIOGetAllExtDevicePropInfo` data phase. Trailing bytes are ignored.
///
/// # Errors
///
/// [`CodecError`] when the payload is truncated, announces more properties than it can hold,
/// or contains an unknown datatype or form flag.
pub fn decode_properties(
    protocol: Protocol, bytes: &[u8],
) -> Result<Vec<SonyProperty>, CodecError> {
    let mut reader = Reader::new(bytes);
    let count = usize::try_from(reader.u64()?).unwrap_or(usize::MAX);
    let needed = count.saturating_mul(MIN_RECORD_LEN);
    if needed > reader.remaining() {
        return Err(CodecError::Truncated {
            needed,
            available: reader.remaining(),
        });
    }
    let mut properties = Vec::with_capacity(count);
    for _ in 0..count {
        properties.push(SonyProperty::read(protocol, &mut reader)?);
    }
    Ok(properties)
}

/// The camera's properties, keyed by code, in code order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PropertyTable {
    properties: BTreeMap<u16, SonyProperty>,
}

impl PropertyTable {
    /// An empty table.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            properties: BTreeMap::new(),
        }
    }

    /// The property with `code`, if the camera reported it.
    #[must_use]
    pub fn get(&self, code: DevicePropCode) -> Option<&SonyProperty> {
        self.properties.get(&code.0)
    }

    /// Whether the camera reported `code`.
    #[must_use]
    pub fn contains(&self, code: DevicePropCode) -> bool {
        self.properties.contains_key(&code.0)
    }

    /// The current integer value of `code`.
    #[must_use]
    pub fn integer(&self, code: DevicePropCode) -> Option<i128> {
        self.get(code).and_then(SonyProperty::integer)
    }

    /// The value that takes host control (`1`) in the priority-mode property, typed as the
    /// camera declares it, or `None` when the property is missing, read-only or not an
    /// integer.
    #[must_use]
    pub fn host_priority_value(&self) -> Option<Value> {
        let property = self.get(super::prop::PRIORITY_MODE)?;
        property
            .is_writable()
            .then(|| integer_value(property.data_type(), 1))
            .flatten()
    }

    /// Number of properties.
    #[must_use]
    pub fn len(&self) -> usize {
        self.properties.len()
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.properties.is_empty()
    }

    /// Every property, in code order.
    pub fn iter(&self) -> impl Iterator<Item = &SonyProperty> {
        self.properties.values()
    }

    /// Merges `updates` in, returning the codes that are new or whose descriptor changed, in
    /// update order.
    pub fn merge(&mut self, updates: Vec<SonyProperty>) -> Vec<DevicePropCode> {
        let mut changed = Vec::new();
        for update in updates {
            let code = update.code;
            if self.properties.get(&code.0) != Some(&update) {
                changed.push(code);
                self.properties.insert(code.0, update);
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn record(code: u16, data_type: u16, get_set: u8, enabled: u8, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&code.to_le_bytes());
        out.extend_from_slice(&data_type.to_le_bytes());
        out.extend_from_slice(&[get_set, enabled]);
        out.extend_from_slice(body);
        out
    }

    fn table_bytes(records: &[Vec<u8>]) -> Vec<u8> {
        let mut out = (records.len() as u64).to_le_bytes().to_vec();
        for record in records {
            out.extend_from_slice(record);
        }
        out
    }

    #[test]
    fn decodes_each_form() {
        let none = record(0xD221, 0x0002, 0, 1, &[0, 1, 0]);
        let range = record(0xD20F, 0x0004, 1, 1, &[5, 0, 6, 0, 1, 2, 0, 3, 0, 1, 0]);
        let enum200 = record(0xD203, 0x0002, 1, 0, &[1, 1, 2, 2, 0, 1, 2]);
        let bytes = table_bytes(&[none, range.clone(), enum200.clone()]);
        let props = decode_properties(Protocol::V200, &bytes).unwrap();
        assert_eq!(props.len(), 3);
        assert_eq!(props[0].code, DevicePropCode(0xD221));
        assert_eq!(props[0].form, SonyForm::None);
        assert_eq!(props[0].integer(), Some(1));
        assert!(!props[0].is_writable());
        assert!(props[0].is_enabled());
        assert_eq!(
            props[1].form,
            SonyForm::Range {
                minimum: Value::UInt16(2),
                maximum: Value::UInt16(3),
                step: Value::UInt16(1),
            }
        );
        assert!(props[1].is_writable());
        assert_eq!(
            props[2].form,
            SonyForm::Enumeration {
                settable: vec![Value::UInt8(1), Value::UInt8(2)],
                reported: Vec::new(),
            }
        );
        assert!(!props[2].is_enabled());
        assert_eq!(props[2].data_type(), DataType::UInt8);
    }

    #[test]
    fn v300_enumerations_carry_two_lists() {
        let enum300 = record(0xD203, 0x0002, 1, 1, &[1, 1, 2, 2, 0, 1, 2, 1, 0, 9]);
        let bytes = table_bytes(&[enum300]);
        let props = decode_properties(Protocol::V300, &bytes).unwrap();
        assert_eq!(
            props[0].form,
            SonyForm::Enumeration {
                settable: vec![Value::UInt8(1), Value::UInt8(2)],
                reported: vec![Value::UInt8(9)],
            }
        );
        assert!(decode_properties(Protocol::V200, &bytes).is_ok());
    }

    #[test]
    fn rejects_bad_input() {
        assert!(decode_properties(Protocol::V200, &[]).is_err());
        let hostile = u64::MAX.to_le_bytes();
        assert_eq!(
            decode_properties(Protocol::V200, &hostile),
            Err(CodecError::Truncated {
                needed: usize::MAX,
                available: 0
            })
        );
        let unknown_type = table_bytes(&[record(0xD221, 0x0042, 0, 1, &[0; 8])]);
        assert!(decode_properties(Protocol::V200, &unknown_type).is_err());
        let unknown_form = table_bytes(&[record(0xD221, 0x0002, 0, 1, &[0, 0, 9, 0, 0, 0])]);
        assert_eq!(
            decode_properties(Protocol::V200, &unknown_form),
            Err(CodecError::UnknownFormFlag(9))
        );
    }

    #[test]
    fn every_truncation_is_an_error() {
        for protocol in [Protocol::V200, Protocol::V300] {
            let enumeration: &[u8] = match protocol {
                Protocol::V200 => &[1, 1, 2, 2, 0, 3, 4],
                Protocol::V300 => &[1, 1, 2, 2, 0, 3, 4, 2, 0, 5, 6],
            };
            let records = [
                record(0xD221, 0x0002, 0, 1, &[0, 1, 0]),
                record(0xD20F, 0x0004, 1, 1, &[5, 0, 6, 0, 1, 2, 0, 3, 0, 1, 0]),
                record(0xD203, 0x0002, 1, 1, enumeration),
                record(0xD1CF, 0xFFFF, 1, 1, &[0, 0, 0]),
            ];
            // A long first record keeps the count check from rejecting every prefix, so each
            // field of the second record is reached with too few bytes left.
            let mut long_body = vec![0; 16];
            long_body.push(1);
            long_body.extend_from_slice(&[0; 24]);
            let long = record(0xD20F, 0x0008, 1, 1, &long_body);
            for record in records {
                let whole = table_bytes(&[long.clone(), record]);
                assert_eq!(decode_properties(protocol, &whole).unwrap().len(), 2);
                for cut in 0..whole.len() {
                    assert!(
                        decode_properties(protocol, &whole[..cut]).is_err(),
                        "{protocol:?} cut at {cut}"
                    );
                }
            }
        }
    }

    #[test]
    fn integer_covers_every_scalar_and_nothing_else() {
        let make = |current: Value| SonyProperty {
            code: DevicePropCode(1),
            get_set: 0,
            enabled: 1,
            factory_default: current.clone(),
            current,
            form: SonyForm::None,
        };
        let cases = [
            (Value::Int8(-1), Some(-1)),
            (Value::UInt8(1), Some(1)),
            (Value::Int16(-2), Some(-2)),
            (Value::UInt16(2), Some(2)),
            (Value::Int32(-3), Some(-3)),
            (Value::UInt32(3), Some(3)),
            (Value::Int64(-4), Some(-4)),
            (Value::UInt64(4), Some(4)),
            (Value::String(String::new()), None),
            (Value::UInt128(5), None),
        ];
        for (value, expected) in cases {
            assert_eq!(make(value).integer(), expected);
        }
    }

    #[test]
    fn merge_reports_new_and_changed_codes_only() {
        let make = |code: u16, current: u8| SonyProperty {
            code: DevicePropCode(code),
            get_set: 0,
            enabled: 1,
            factory_default: Value::UInt8(0),
            current: Value::UInt8(current),
            form: SonyForm::None,
        };
        let mut table = PropertyTable::new();
        assert!(table.is_empty());
        assert_eq!(
            table.merge(vec![make(2, 0), make(1, 0)]),
            [DevicePropCode(2), DevicePropCode(1)]
        );
        assert_eq!(table.len(), 2);
        assert_eq!(table.merge(vec![make(2, 0)]), Vec::<DevicePropCode>::new());
        assert_eq!(table.merge(vec![make(2, 7)]), [DevicePropCode(2)]);
        assert_eq!(table.integer(DevicePropCode(2)), Some(7));
        assert_eq!(table.integer(DevicePropCode(9)), None);
        assert!(table.contains(DevicePropCode(1)));
        assert!(!table.contains(DevicePropCode(9)));
        assert!(table.get(DevicePropCode(9)).is_none());
        let order: Vec<u16> = table.iter().map(|p| p.code.0).collect();
        assert_eq!(order, [1, 2]);
    }

    #[test]
    fn host_priority_value_follows_the_declared_type() {
        let make = |get_set: u8, value: Value| {
            let mut table = PropertyTable::new();
            table.merge(vec![SonyProperty {
                code: crate::sony::prop::PRIORITY_MODE,
                get_set,
                enabled: 1,
                factory_default: value.clone(),
                current: value,
                form: SonyForm::None,
            }]);
            table
        };
        assert_eq!(PropertyTable::new().host_priority_value(), None);
        assert_eq!(make(0, Value::Int8(0)).host_priority_value(), None);
        assert_eq!(
            make(1, Value::String(String::new())).host_priority_value(),
            None
        );
        let cases = [
            (Value::Int8(0), Value::Int8(1)),
            (Value::UInt8(0), Value::UInt8(1)),
            (Value::Int16(0), Value::Int16(1)),
            (Value::UInt16(0), Value::UInt16(1)),
            (Value::Int32(0), Value::Int32(1)),
            (Value::UInt32(0), Value::UInt32(1)),
            (Value::Int64(0), Value::Int64(1)),
            (Value::UInt64(0), Value::UInt64(1)),
        ];
        for (declared, expected) in cases {
            assert_eq!(make(1, declared).host_priority_value(), Some(expected));
        }
    }

    proptest! {
        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = decode_properties(Protocol::V200, &bytes);
            let _ = decode_properties(Protocol::V300, &bytes);
        }
    }
}
