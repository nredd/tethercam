//! Property tests for PTP payload types through the public API.

use proptest::prelude::*;
use tethercam::ptp::{
    DataType, DeviceInfo, DevicePropCode, DevicePropDesc, EventCode, GetSet, ObjectInfo,
    OperationCode, PropertyForm, Reader, Value, Writer,
};

fn text() -> impl Strategy<Value = String> {
    // Up to 100 chars of up to 2 UTF-16 units each stays under the 254-unit limit.
    proptest::collection::vec(any::<char>(), 0..100).prop_map(|c| c.into_iter().collect())
}

fn vec_of<T: std::fmt::Debug>(s: impl Strategy<Value = T>) -> impl Strategy<Value = Vec<T>> {
    proptest::collection::vec(s, 0..8)
}

fn scalar_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<i8>().prop_map(Value::Int8),
        any::<u8>().prop_map(Value::UInt8),
        any::<i16>().prop_map(Value::Int16),
        any::<u16>().prop_map(Value::UInt16),
        any::<i32>().prop_map(Value::Int32),
        any::<u32>().prop_map(Value::UInt32),
        any::<i64>().prop_map(Value::Int64),
        any::<u64>().prop_map(Value::UInt64),
        any::<i128>().prop_map(Value::Int128),
        any::<u128>().prop_map(Value::UInt128),
        text().prop_map(Value::String),
    ]
}

fn value() -> impl Strategy<Value = Value> {
    prop_oneof![
        scalar_value(),
        vec_of(any::<i8>()).prop_map(Value::Int8Array),
        vec_of(any::<u8>()).prop_map(Value::UInt8Array),
        vec_of(any::<i16>()).prop_map(Value::Int16Array),
        vec_of(any::<u16>()).prop_map(Value::UInt16Array),
        vec_of(any::<i32>()).prop_map(Value::Int32Array),
        vec_of(any::<u32>()).prop_map(Value::UInt32Array),
        vec_of(any::<i64>()).prop_map(Value::Int64Array),
        vec_of(any::<u64>()).prop_map(Value::UInt64Array),
        vec_of(any::<i128>()).prop_map(Value::Int128Array),
        vec_of(any::<u128>()).prop_map(Value::UInt128Array),
    ]
}

/// A value of the same datatype as `template`, derived by decoding random bytes of that type.
fn desc() -> impl Strategy<Value = DevicePropDesc> {
    (
        any::<u16>(),
        any::<bool>(),
        value(),
        proptest::collection::vec(any::<u8>(), 64..256),
        0_u8..3,
    )
        .prop_filter_map(
            "needs decodable filler",
            |(code, rw, default, filler, form)| {
                let data_type = default.data_type();
                let mut reader = Reader::new(&filler);
                let mut next = || Value::decode(data_type, &mut reader).ok();
                let current = next()?;
                let form = match form {
                    0 => PropertyForm::None,
                    1 => PropertyForm::Range {
                        minimum: next()?,
                        maximum: next()?,
                        step: next()?,
                    },
                    _ => PropertyForm::Enumeration(vec![next()?, next()?]),
                };
                Some(DevicePropDesc {
                    code: DevicePropCode(code),
                    get_set: if rw { GetSet::GetSet } else { GetSet::Get },
                    factory_default: default,
                    current,
                    form,
                })
            },
        )
}

fn device_info() -> impl Strategy<Value = DeviceInfo> {
    (
        (
            any::<u16>(),
            any::<u32>(),
            any::<u16>(),
            text(),
            any::<u16>(),
        ),
        (
            vec_of(any::<u16>().prop_map(OperationCode)),
            vec_of(any::<u16>().prop_map(EventCode)),
            vec_of(any::<u16>().prop_map(DevicePropCode)),
            vec_of(any::<u16>()),
            vec_of(any::<u16>()),
        ),
        (text(), text(), text(), text()),
    )
        .prop_map(|(head, lists, strings)| DeviceInfo {
            standard_version: head.0,
            vendor_extension_id: head.1,
            vendor_extension_version: head.2,
            vendor_extension_desc: head.3,
            functional_mode: head.4,
            operations_supported: lists.0,
            events_supported: lists.1,
            device_properties_supported: lists.2,
            capture_formats: lists.3,
            image_formats: lists.4,
            manufacturer: strings.0,
            model: strings.1,
            device_version: strings.2,
            serial_number: strings.3,
        })
}

fn object_info() -> impl Strategy<Value = ObjectInfo> {
    (
        (
            any::<u32>(),
            any::<u16>(),
            any::<u16>(),
            any::<u32>(),
            any::<u16>(),
        ),
        (
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
        ),
        (
            any::<u32>(),
            any::<u32>(),
            any::<u16>(),
            any::<u32>(),
            any::<u32>(),
        ),
        (text(), text(), text(), text()),
    )
        .prop_map(|(a, b, c, s)| ObjectInfo {
            storage_id: a.0,
            object_format: a.1,
            protection_status: a.2,
            object_compressed_size: a.3,
            thumb_format: a.4,
            thumb_compressed_size: b.0,
            thumb_pix_width: b.1,
            thumb_pix_height: b.2,
            image_pix_width: b.3,
            image_pix_height: b.4,
            image_bit_depth: c.0,
            parent_object: c.1,
            association_type: c.2,
            association_desc: c.3,
            sequence_number: c.4,
            filename: s.0,
            capture_date: s.1,
            modification_date: s.2,
            keywords: s.3,
        })
}

proptest! {
    #[test]
    fn value_round_trips(v in value()) {
        let wire = v.to_bytes().unwrap();
        prop_assert_eq!(Value::from_bytes(v.data_type(), &wire).unwrap(), v);
    }

    #[test]
    fn string_round_trips(s in text()) {
        let mut w = Writer::new();
        w.string(&s).unwrap();
        let bytes = w.into_bytes();
        prop_assert_eq!(Reader::new(&bytes).string().unwrap(), s);
    }

    #[test]
    fn device_info_round_trips(info in device_info(), pad in vec_of(any::<u8>())) {
        let mut wire = info.to_bytes().unwrap();
        wire.extend(pad);
        prop_assert_eq!(DeviceInfo::decode(&wire).unwrap(), info);
    }

    #[test]
    fn object_info_round_trips(info in object_info()) {
        prop_assert_eq!(ObjectInfo::decode(&info.to_bytes().unwrap()).unwrap(), info);
    }

    #[test]
    fn prop_desc_round_trips(d in desc()) {
        prop_assert_eq!(DevicePropDesc::decode(&d.to_bytes().unwrap()).unwrap(), d);
    }

    #[test]
    fn decoders_never_panic_on_arbitrary_bytes(
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
        code in any::<u16>(),
    ) {
        let _ = DeviceInfo::decode(&bytes);
        let _ = ObjectInfo::decode(&bytes);
        let _ = DevicePropDesc::decode(&bytes);
        let _ = DataType::try_from(code);
        let every_code = (0x0001_u16..=0x000A).chain(0x4001..=0x400A).chain([0xFFFF]);
        for raw in every_code {
            let dt = DataType::try_from(raw).unwrap();
            if let Ok(v) = Value::from_bytes(dt, &bytes) {
                prop_assert_eq!(v.data_type(), dt);
            }
        }
        let mut r = Reader::new(&bytes);
        let _ = r.string();
        let _ = r.array(1, Reader::u8);
        let _ = r.u128();
    }

    #[test]
    fn decoders_never_panic_on_mutated_valid_input(
        info in device_info(),
        flips in vec_of((any::<usize>(), any::<u8>())),
        cut in any::<usize>(),
    ) {
        let mut wire = info.to_bytes().unwrap();
        for (at, byte) in flips {
            let len = wire.len();
            if let Some(slot) = wire.get_mut(at % len.max(1)) {
                *slot = byte;
            }
        }
        wire.truncate(cut % (wire.len() + 1));
        let _ = DeviceInfo::decode(&wire);
    }

    #[test]
    fn code_display_never_panics(code in any::<u16>()) {
        let _ = OperationCode(code).to_string();
        let _ = EventCode(code).to_string();
    }
}
