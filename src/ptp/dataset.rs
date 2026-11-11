//! PTP datasets: `DeviceInfo`, `ObjectInfo` and `DevicePropDesc`.
//!
//! Datasets are the structured payloads of the data phase of `GetDeviceInfo`, `GetObjectInfo`
//! and `GetDevicePropDesc`. Decoding is total: any input yields a dataset or a [`CodecError`].
//! Bytes after the last field are ignored, because some devices pad data phases.
//!
//! References:
//! - ISO 15740 / PIMA 15740:2000 section 5.5.1 "`DeviceInfo`", 5.3.1 "`ObjectInfo`",
//!   5.2.2 "Device property describing dataset" and section 13 "Device properties"

use super::codec::{CodecError, Reader, Writer};
use super::codes::{DevicePropCode, EventCode, OperationCode};
use super::datatype::{DataType, Value};

/// The `DeviceInfo` dataset returned by `GetDeviceInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct DeviceInfo {
    /// PTP version times 100, e.g. `100` for 1.00.
    pub standard_version: u16,
    /// Vendor extension identifier, e.g. `0x0000_0011` for Microsoft.
    pub vendor_extension_id: u32,
    /// Vendor extension version times 100.
    pub vendor_extension_version: u16,
    /// Human-readable vendor extension description.
    pub vendor_extension_desc: String,
    /// Functional mode (`0` standard, vendor values above that).
    pub functional_mode: u16,
    /// Operations the device supports.
    pub operations_supported: Vec<OperationCode>,
    /// Events the device can send.
    pub events_supported: Vec<EventCode>,
    /// Device properties the device exposes.
    pub device_properties_supported: Vec<DevicePropCode>,
    /// Object format codes the device can capture.
    pub capture_formats: Vec<u16>,
    /// Object format codes the device can store or transfer.
    pub image_formats: Vec<u16>,
    /// Manufacturer name.
    pub manufacturer: String,
    /// Model name.
    pub model: String,
    /// Device firmware version string.
    pub device_version: String,
    /// Serial number.
    pub serial_number: String,
}

impl DeviceInfo {
    /// Decode a `DeviceInfo` dataset from the start of `bytes`.
    ///
    /// # Errors
    ///
    /// Any [`CodecError`] from the underlying reads.
    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        Self::read(&mut Reader::new(bytes))
    }

    /// Read a `DeviceInfo` dataset from `reader`.
    ///
    /// # Errors
    ///
    /// Any [`CodecError`] from the underlying reads.
    pub fn read(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            standard_version: reader.u16()?,
            vendor_extension_id: reader.u32()?,
            vendor_extension_version: reader.u16()?,
            vendor_extension_desc: reader.string()?,
            functional_mode: reader.u16()?,
            operations_supported: reader.array(2, |r| r.u16().map(OperationCode))?,
            events_supported: reader.array(2, |r| r.u16().map(EventCode))?,
            device_properties_supported: reader.array(2, |r| r.u16().map(DevicePropCode))?,
            capture_formats: reader.array(2, Reader::u16)?,
            image_formats: reader.array(2, Reader::u16)?,
            manufacturer: reader.string()?,
            model: reader.string()?,
            device_version: reader.string()?,
            serial_number: reader.string()?,
        })
    }

    /// Append the dataset's wire form to `writer`.
    ///
    /// # Errors
    ///
    /// [`CodecError::StringTooLong`] or [`CodecError::CollectionTooLong`].
    pub fn write(&self, writer: &mut Writer) -> Result<(), CodecError> {
        writer.u16(self.standard_version);
        writer.u32(self.vendor_extension_id);
        writer.u16(self.vendor_extension_version);
        writer.string(&self.vendor_extension_desc)?;
        writer.u16(self.functional_mode);
        writer.array(&self.operations_supported, |w, c| w.u16(c.0))?;
        writer.array(&self.events_supported, |w, c| w.u16(c.0))?;
        writer.array(&self.device_properties_supported, |w, c| w.u16(c.0))?;
        writer.array(&self.capture_formats, |w, c| w.u16(*c))?;
        writer.array(&self.image_formats, |w, c| w.u16(*c))?;
        writer.string(&self.manufacturer)?;
        writer.string(&self.model)?;
        writer.string(&self.device_version)?;
        writer.string(&self.serial_number)
    }

    /// The dataset's wire form.
    ///
    /// # Errors
    ///
    /// [`CodecError::StringTooLong`] or [`CodecError::CollectionTooLong`].
    pub fn to_bytes(&self) -> Result<Vec<u8>, CodecError> {
        let mut writer = Writer::new();
        self.write(&mut writer)?;
        Ok(writer.into_bytes())
    }
}

/// The `ObjectInfo` dataset returned by `GetObjectInfo`.
///
/// Dates stay as the raw `YYYYMMDDThhmmss[.s][Z|+-hhmm]` strings the device sent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct ObjectInfo {
    /// Storage the object lives on.
    pub storage_id: u32,
    /// Object format code.
    pub object_format: u16,
    /// Protection status (`0` none, `1` read-only).
    pub protection_status: u16,
    /// Object size in bytes (`0xFFFF_FFFF` when it does not fit).
    pub object_compressed_size: u32,
    /// Thumbnail format code.
    pub thumb_format: u16,
    /// Thumbnail size in bytes.
    pub thumb_compressed_size: u32,
    /// Thumbnail width in pixels.
    pub thumb_pix_width: u32,
    /// Thumbnail height in pixels.
    pub thumb_pix_height: u32,
    /// Image width in pixels.
    pub image_pix_width: u32,
    /// Image height in pixels.
    pub image_pix_height: u32,
    /// Image bit depth.
    pub image_bit_depth: u32,
    /// Handle of the parent object (`0` for the root).
    pub parent_object: u32,
    /// Association type (`0` for non-associations).
    pub association_type: u16,
    /// Association description.
    pub association_desc: u32,
    /// Sequence number within a burst.
    pub sequence_number: u32,
    /// File name.
    pub filename: String,
    /// Capture date and time.
    pub capture_date: String,
    /// Last modification date and time.
    pub modification_date: String,
    /// Space-separated keywords.
    pub keywords: String,
}

impl ObjectInfo {
    /// Decode an `ObjectInfo` dataset from the start of `bytes`.
    ///
    /// # Errors
    ///
    /// Any [`CodecError`] from the underlying reads.
    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        Self::read(&mut Reader::new(bytes))
    }

    /// Read an `ObjectInfo` dataset from `reader`.
    ///
    /// # Errors
    ///
    /// Any [`CodecError`] from the underlying reads.
    pub fn read(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            storage_id: reader.u32()?,
            object_format: reader.u16()?,
            protection_status: reader.u16()?,
            object_compressed_size: reader.u32()?,
            thumb_format: reader.u16()?,
            thumb_compressed_size: reader.u32()?,
            thumb_pix_width: reader.u32()?,
            thumb_pix_height: reader.u32()?,
            image_pix_width: reader.u32()?,
            image_pix_height: reader.u32()?,
            image_bit_depth: reader.u32()?,
            parent_object: reader.u32()?,
            association_type: reader.u16()?,
            association_desc: reader.u32()?,
            sequence_number: reader.u32()?,
            filename: reader.string()?,
            capture_date: reader.string()?,
            modification_date: reader.string()?,
            keywords: reader.string()?,
        })
    }

    /// Append the dataset's wire form to `writer`.
    ///
    /// # Errors
    ///
    /// [`CodecError::StringTooLong`].
    pub fn write(&self, writer: &mut Writer) -> Result<(), CodecError> {
        writer.u32(self.storage_id);
        writer.u16(self.object_format);
        writer.u16(self.protection_status);
        writer.u32(self.object_compressed_size);
        writer.u16(self.thumb_format);
        writer.u32(self.thumb_compressed_size);
        writer.u32(self.thumb_pix_width);
        writer.u32(self.thumb_pix_height);
        writer.u32(self.image_pix_width);
        writer.u32(self.image_pix_height);
        writer.u32(self.image_bit_depth);
        writer.u32(self.parent_object);
        writer.u16(self.association_type);
        writer.u32(self.association_desc);
        writer.u32(self.sequence_number);
        writer.string(&self.filename)?;
        writer.string(&self.capture_date)?;
        writer.string(&self.modification_date)?;
        writer.string(&self.keywords)
    }

    /// The dataset's wire form.
    ///
    /// # Errors
    ///
    /// [`CodecError::StringTooLong`].
    pub fn to_bytes(&self) -> Result<Vec<u8>, CodecError> {
        let mut writer = Writer::new();
        self.write(&mut writer)?;
        Ok(writer.into_bytes())
    }
}

/// Whether a property can be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GetSet {
    /// `0x00`, read-only.
    Get,
    /// `0x01`, read-write.
    GetSet,
}

impl TryFrom<u8> for GetSet {
    type Error = CodecError;

    fn try_from(raw: u8) -> Result<Self, Self::Error> {
        match raw {
            0 => Ok(Self::Get),
            1 => Ok(Self::GetSet),
            other => Err(CodecError::UnknownGetSet(other)),
        }
    }
}

impl From<GetSet> for u8 {
    fn from(value: GetSet) -> Self {
        match value {
            GetSet::Get => 0,
            GetSet::GetSet => 1,
        }
    }
}

/// The values a property accepts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PropertyForm {
    /// `0x00`, no constraint is reported.
    None,
    /// `0x01`, any value `minimum + n * step` up to `maximum`.
    Range {
        /// Smallest accepted value.
        minimum: Value,
        /// Largest accepted value.
        maximum: Value,
        /// Increment between accepted values.
        step: Value,
    },
    /// `0x02`, exactly these values.
    Enumeration(Vec<Value>),
}

/// The `DevicePropDesc` dataset returned by `GetDevicePropDesc`.
///
/// The datatype is not stored separately: it is [`DevicePropDesc::data_type`], taken from
/// `factory_default`, and encoding rejects any other value of a different datatype.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DevicePropDesc {
    /// Which property this describes.
    pub code: DevicePropCode,
    /// Whether the property can be written.
    pub get_set: GetSet,
    /// Value after a reset.
    pub factory_default: Value,
    /// Value right now.
    pub current: Value,
    /// Accepted values.
    pub form: PropertyForm,
}

impl DevicePropDesc {
    /// The property's datatype.
    #[must_use]
    pub const fn data_type(&self) -> DataType {
        self.factory_default.data_type()
    }

    /// Decode a `DevicePropDesc` dataset from the start of `bytes`.
    ///
    /// # Errors
    ///
    /// Any [`CodecError`] from the underlying reads.
    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        Self::read(&mut Reader::new(bytes))
    }

    /// Read a `DevicePropDesc` dataset from `reader`.
    ///
    /// # Errors
    ///
    /// Any [`CodecError`] from the underlying reads.
    pub fn read(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        let code = DevicePropCode(reader.u16()?);
        let data_type = DataType::try_from(reader.u16()?)?;
        let get_set = GetSet::try_from(reader.u8()?)?;
        let factory_default = Value::decode(data_type, reader)?;
        let current = Value::decode(data_type, reader)?;
        let form = match reader.u8()? {
            0 => PropertyForm::None,
            1 => PropertyForm::Range {
                minimum: Value::decode(data_type, reader)?,
                maximum: Value::decode(data_type, reader)?,
                step: Value::decode(data_type, reader)?,
            },
            2 => {
                let count = reader.u16()?;
                // Each value takes at least one byte, so `count` is bounded by the input.
                let mut values = Vec::with_capacity(usize::from(count).min(reader.remaining()));
                for _ in 0..count {
                    values.push(Value::decode(data_type, reader)?);
                }
                PropertyForm::Enumeration(values)
            }
            other => return Err(CodecError::UnknownFormFlag(other)),
        };
        Ok(Self {
            code,
            get_set,
            factory_default,
            current,
            form,
        })
    }

    /// Append the dataset's wire form to `writer`.
    ///
    /// # Errors
    ///
    /// [`CodecError::DataTypeMismatch`] if a value's datatype differs from
    /// [`DevicePropDesc::data_type`], plus the string and collection limits.
    pub fn write(&self, writer: &mut Writer) -> Result<(), CodecError> {
        let data_type = self.data_type();
        let check = |value: &Value| {
            if value.data_type() == data_type {
                Ok(())
            } else {
                Err(CodecError::DataTypeMismatch {
                    expected: data_type.code(),
                    actual: value.data_type().code(),
                })
            }
        };
        check(&self.current)?;
        match &self.form {
            PropertyForm::None => {}
            PropertyForm::Range {
                minimum,
                maximum,
                step,
            } => {
                check(minimum)?;
                check(maximum)?;
                check(step)?;
            }
            PropertyForm::Enumeration(values) => {
                u16::try_from(values.len())
                    .map_err(|_| CodecError::CollectionTooLong(values.len()))?;
                values.iter().try_for_each(check)?;
            }
        }

        writer.u16(self.code.0);
        writer.u16(data_type.code());
        writer.u8(self.get_set.into());
        self.factory_default.encode(writer)?;
        self.current.encode(writer)?;
        match &self.form {
            PropertyForm::None => writer.u8(0),
            PropertyForm::Range {
                minimum,
                maximum,
                step,
            } => {
                writer.u8(1);
                minimum.encode(writer)?;
                maximum.encode(writer)?;
                step.encode(writer)?;
            }
            PropertyForm::Enumeration(values) => {
                writer.u8(2);
                let count = u16::try_from(values.len())
                    .map_err(|_| CodecError::CollectionTooLong(values.len()))?;
                writer.u16(count);
                for value in values {
                    value.encode(writer)?;
                }
            }
        }
        Ok(())
    }

    /// The dataset's wire form.
    ///
    /// # Errors
    ///
    /// See [`DevicePropDesc::write`].
    pub fn to_bytes(&self) -> Result<Vec<u8>, CodecError> {
        let mut writer = Writer::new();
        self.write(&mut writer)?;
        Ok(writer.into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-assembled PTP string, independent of `Writer`.
    fn ptp_str(text: &str) -> Vec<u8> {
        if text.is_empty() {
            return vec![0];
        }
        let mut out = vec![u8::try_from(text.len() + 1).unwrap()];
        for b in text.bytes() {
            out.extend([b, 0]);
        }
        out.extend([0, 0]);
        out
    }

    fn u16_array(items: &[u16]) -> Vec<u8> {
        let mut out = u32::try_from(items.len()).unwrap().to_le_bytes().to_vec();
        for item in items {
            out.extend(item.to_le_bytes());
        }
        out
    }

    fn golden_device_info() -> (Vec<u8>, DeviceInfo) {
        let mut wire = Vec::new();
        wire.extend(100_u16.to_le_bytes()); // StandardVersion 1.00
        wire.extend(0x0000_0011_u32.to_le_bytes()); // VendorExtensionID
        wire.extend(100_u16.to_le_bytes()); // VendorExtensionVersion
        wire.extend(ptp_str("example.org: 1.0"));
        wire.extend(0_u16.to_le_bytes()); // FunctionalMode
        wire.extend(u16_array(&[0x1001, 0x1002, 0x9201]));
        wire.extend(u16_array(&[0x4002]));
        wire.extend(u16_array(&[0x5001, 0xD20D]));
        wire.extend(u16_array(&[0x3801]));
        wire.extend(u16_array(&[0x3801, 0x3000]));
        wire.extend(ptp_str("Example Corp"));
        wire.extend(ptp_str("Model 1"));
        wire.extend(ptp_str("1.00"));
        wire.extend(ptp_str("SN000000"));

        let info = DeviceInfo {
            standard_version: 100,
            vendor_extension_id: 0x11,
            vendor_extension_version: 100,
            vendor_extension_desc: "example.org: 1.0".into(),
            functional_mode: 0,
            operations_supported: vec![
                OperationCode::GET_DEVICE_INFO,
                OperationCode::OPEN_SESSION,
                OperationCode(0x9201),
            ],
            events_supported: vec![EventCode::OBJECT_ADDED],
            device_properties_supported: vec![
                DevicePropCode::BATTERY_LEVEL,
                DevicePropCode(0xD20D),
            ],
            capture_formats: vec![0x3801],
            image_formats: vec![0x3801, 0x3000],
            manufacturer: "Example Corp".into(),
            model: "Model 1".into(),
            device_version: "1.00".into(),
            serial_number: "SN000000".into(),
        };
        (wire, info)
    }

    #[test]
    fn device_info_golden_decode_and_encode() {
        let (wire, info) = golden_device_info();
        assert_eq!(DeviceInfo::decode(&wire).unwrap(), info);
        assert_eq!(info.to_bytes().unwrap(), wire);
    }

    #[test]
    fn device_info_ignores_trailing_padding() {
        let (mut wire, info) = golden_device_info();
        wire.extend([0xAA; 7]);
        assert_eq!(DeviceInfo::decode(&wire).unwrap(), info);
    }

    #[test]
    fn device_info_every_truncation_is_an_error() {
        let (wire, _) = golden_device_info();
        for len in 0..wire.len() {
            assert!(DeviceInfo::decode(&wire[..len]).is_err(), "len {len}");
        }
    }

    #[test]
    fn device_info_default_round_trips() {
        let info = DeviceInfo::default();
        assert_eq!(DeviceInfo::decode(&info.to_bytes().unwrap()).unwrap(), info);
    }

    #[test]
    fn object_info_golden_decode_and_encode() {
        let mut wire = Vec::new();
        wire.extend(0x0001_0001_u32.to_le_bytes()); // StorageID
        wire.extend(0x3801_u16.to_le_bytes()); // JPEG
        wire.extend(0_u16.to_le_bytes());
        wire.extend(1_234_567_u32.to_le_bytes());
        wire.extend(0x3808_u16.to_le_bytes());
        wire.extend(4096_u32.to_le_bytes());
        wire.extend(160_u32.to_le_bytes());
        wire.extend(120_u32.to_le_bytes());
        wire.extend(6000_u32.to_le_bytes());
        wire.extend(4000_u32.to_le_bytes());
        wire.extend(24_u32.to_le_bytes());
        wire.extend(0_u32.to_le_bytes());
        wire.extend(0_u16.to_le_bytes());
        wire.extend(0_u32.to_le_bytes());
        wire.extend(3_u32.to_le_bytes());
        wire.extend(ptp_str("DSC00001.JPG"));
        wire.extend(ptp_str("20260101T120000"));
        wire.extend(ptp_str(""));
        wire.extend(ptp_str(""));

        let info = ObjectInfo::decode(&wire).unwrap();
        assert_eq!(info.storage_id, 0x0001_0001);
        assert_eq!(info.object_format, 0x3801);
        assert_eq!(info.object_compressed_size, 1_234_567);
        assert_eq!(info.image_pix_width, 6000);
        assert_eq!(info.sequence_number, 3);
        assert_eq!(info.filename, "DSC00001.JPG");
        assert_eq!(info.capture_date, "20260101T120000");
        assert_eq!(info.keywords, "");
        assert_eq!(info.to_bytes().unwrap(), wire);
        for len in 0..wire.len() {
            assert!(ObjectInfo::decode(&wire[..len]).is_err(), "len {len}");
        }
    }

    #[test]
    fn prop_desc_range_golden() {
        // BatteryLevel, UINT8, Get, default 100, current 50, range 0..=100 step 1
        let wire = [
            0x01, 0x50, 0x02, 0x00, 0x00, 0x64, 0x32, 0x01, 0x00, 0x64, 0x01,
        ];
        let desc = DevicePropDesc::decode(&wire).unwrap();
        assert_eq!(
            desc,
            DevicePropDesc {
                code: DevicePropCode::BATTERY_LEVEL,
                get_set: GetSet::Get,
                factory_default: Value::UInt8(100),
                current: Value::UInt8(50),
                form: PropertyForm::Range {
                    minimum: Value::UInt8(0),
                    maximum: Value::UInt8(100),
                    step: Value::UInt8(1),
                },
            }
        );
        assert_eq!(desc.data_type(), DataType::UInt8);
        assert_eq!(desc.to_bytes().unwrap(), wire);
    }

    #[test]
    fn prop_desc_enumeration_golden() {
        // ExposureProgramMode, UINT16, GetSet, default 1, current 2, enum {1, 2, 3}
        let wire = [
            0x0E, 0x50, 0x04, 0x00, 0x01, 0x01, 0x00, 0x02, 0x00, 0x02, 0x03, 0x00, 0x01, 0x00,
            0x02, 0x00, 0x03, 0x00,
        ];
        let desc = DevicePropDesc::decode(&wire).unwrap();
        assert_eq!(desc.get_set, GetSet::GetSet);
        assert_eq!(
            desc.form,
            PropertyForm::Enumeration(vec![Value::UInt16(1), Value::UInt16(2), Value::UInt16(3)])
        );
        assert_eq!(desc.to_bytes().unwrap(), wire);
    }

    #[test]
    fn prop_desc_form_none_with_string_type() {
        let mut wire = vec![0x1E, 0x50, 0xFF, 0xFF, 0x01];
        wire.extend(ptp_str(""));
        wire.extend(ptp_str("me"));
        wire.push(0);
        let desc = DevicePropDesc::decode(&wire).unwrap();
        assert_eq!(desc.code, DevicePropCode::ARTIST);
        assert_eq!(desc.current, Value::String("me".into()));
        assert_eq!(desc.form, PropertyForm::None);
        assert_eq!(desc.to_bytes().unwrap(), wire);
    }

    #[test]
    fn prop_desc_rejects_bad_enums() {
        let base = [0x01, 0x50, 0x02, 0x00, 0x00, 0x64, 0x32];
        let with = |tail: &[u8]| [&base[..], tail].concat();
        assert_eq!(
            DevicePropDesc::decode(&with(&[0x03])),
            Err(CodecError::UnknownFormFlag(3))
        );
        let mut bad_get_set = with(&[0]);
        bad_get_set[4] = 2;
        assert_eq!(
            DevicePropDesc::decode(&bad_get_set),
            Err(CodecError::UnknownGetSet(2))
        );
        let mut bad_type = with(&[0]);
        bad_type[2] = 0x0B;
        assert_eq!(
            DevicePropDesc::decode(&bad_type),
            Err(CodecError::UnknownDataType(0x000B))
        );
    }

    #[test]
    fn prop_desc_hostile_enumeration_count_is_an_error() {
        let wire = [
            0x01, 0x50, 0x02, 0x00, 0x00, 0x64, 0x32, 0x02, 0xFF, 0xFF, 0x01,
        ];
        assert!(matches!(
            DevicePropDesc::decode(&wire),
            Err(CodecError::Truncated { .. })
        ));
    }

    #[test]
    fn prop_desc_encode_rejects_mismatched_types() {
        let desc = DevicePropDesc {
            code: DevicePropCode::BATTERY_LEVEL,
            get_set: GetSet::Get,
            factory_default: Value::UInt8(1),
            current: Value::UInt16(1),
            form: PropertyForm::None,
        };
        assert_eq!(
            desc.to_bytes(),
            Err(CodecError::DataTypeMismatch {
                expected: 0x0002,
                actual: 0x0004
            })
        );
    }

    #[test]
    fn prop_desc_every_truncation_is_an_error() {
        let wire = [
            0x0E, 0x50, 0x04, 0x00, 0x01, 0x01, 0x00, 0x02, 0x00, 0x02, 0x03, 0x00, 0x01, 0x00,
            0x02, 0x00, 0x03, 0x00,
        ];
        for len in 0..wire.len() {
            assert!(DevicePropDesc::decode(&wire[..len]).is_err(), "len {len}");
        }
    }
}
