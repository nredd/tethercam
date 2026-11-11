//! Newtypes for the standard PTP operation, response, event and device property codes.
//!
//! Each newtype wraps the raw `u16` so vendor-defined codes (Sony's `0x9xxx` operations and
//! `0xCxxx` properties, say) pass through untouched. `Display` names the standard codes and
//! prints anything else as hex, e.g. `0x9201`.
//!
//! References:
//! - ISO 15740 / PIMA 15740:2000 section 10 "Operations", 11 "Responses", 12 "Events",
//!   13 "Device properties"

use std::fmt;

macro_rules! code_table {
    (
        $(#[$meta:meta])*
        $name:ident {
            $($konst:ident = $value:literal => $label:literal,)+
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u16);

        impl $name {
            $(
                #[doc = concat!("`", $label, "` (`", stringify!($value), "`).")]
                pub const $konst: Self = Self($value);
            )+

            /// The raw wire value.
            #[must_use]
            pub const fn code(self) -> u16 {
                self.0
            }

            /// The standard name of this code, or `None` for vendor-defined and unknown codes.
            #[must_use]
            pub const fn name(self) -> Option<&'static str> {
                match self.0 {
                    $($value => Some($label),)+
                    _ => None,
                }
            }
        }

        impl From<u16> for $name {
            fn from(code: u16) -> Self {
                Self(code)
            }
        }

        impl From<$name> for u16 {
            fn from(code: $name) -> Self {
                code.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self.name() {
                    Some(label) => f.write_str(label),
                    None => write!(f, "{:#06x}", self.0),
                }
            }
        }
    };
}

code_table! {
    /// A PTP operation code (`0x1001..=0x101C` standard, `0x9000..` vendor).
    OperationCode {
        GET_DEVICE_INFO = 0x1001 => "GetDeviceInfo",
        OPEN_SESSION = 0x1002 => "OpenSession",
        CLOSE_SESSION = 0x1003 => "CloseSession",
        GET_STORAGE_IDS = 0x1004 => "GetStorageIDs",
        GET_STORAGE_INFO = 0x1005 => "GetStorageInfo",
        GET_NUM_OBJECTS = 0x1006 => "GetNumObjects",
        GET_OBJECT_HANDLES = 0x1007 => "GetObjectHandles",
        GET_OBJECT_INFO = 0x1008 => "GetObjectInfo",
        GET_OBJECT = 0x1009 => "GetObject",
        GET_THUMB = 0x100A => "GetThumb",
        DELETE_OBJECT = 0x100B => "DeleteObject",
        SEND_OBJECT_INFO = 0x100C => "SendObjectInfo",
        SEND_OBJECT = 0x100D => "SendObject",
        INITIATE_CAPTURE = 0x100E => "InitiateCapture",
        FORMAT_STORE = 0x100F => "FormatStore",
        RESET_DEVICE = 0x1010 => "ResetDevice",
        SELF_TEST = 0x1011 => "SelfTest",
        SET_OBJECT_PROTECTION = 0x1012 => "SetObjectProtection",
        POWER_DOWN = 0x1013 => "PowerDown",
        GET_DEVICE_PROP_DESC = 0x1014 => "GetDevicePropDesc",
        GET_DEVICE_PROP_VALUE = 0x1015 => "GetDevicePropValue",
        SET_DEVICE_PROP_VALUE = 0x1016 => "SetDevicePropValue",
        RESET_DEVICE_PROP_VALUE = 0x1017 => "ResetDevicePropValue",
        TERMINATE_OPEN_CAPTURE = 0x1018 => "TerminateOpenCapture",
        MOVE_OBJECT = 0x1019 => "MoveObject",
        COPY_OBJECT = 0x101A => "CopyObject",
        GET_PARTIAL_OBJECT = 0x101B => "GetPartialObject",
        INITIATE_OPEN_CAPTURE = 0x101C => "InitiateOpenCapture",
    }
}

code_table! {
    /// A PTP response code (`0x2001..=0x2020` standard, `0xA000..` vendor).
    ResponseCode {
        OK = 0x2001 => "OK",
        GENERAL_ERROR = 0x2002 => "GeneralError",
        SESSION_NOT_OPEN = 0x2003 => "SessionNotOpen",
        INVALID_TRANSACTION_ID = 0x2004 => "InvalidTransactionID",
        OPERATION_NOT_SUPPORTED = 0x2005 => "OperationNotSupported",
        PARAMETER_NOT_SUPPORTED = 0x2006 => "ParameterNotSupported",
        INCOMPLETE_TRANSFER = 0x2007 => "IncompleteTransfer",
        INVALID_STORAGE_ID = 0x2008 => "InvalidStorageID",
        INVALID_OBJECT_HANDLE = 0x2009 => "InvalidObjectHandle",
        DEVICE_PROP_NOT_SUPPORTED = 0x200A => "DevicePropNotSupported",
        INVALID_OBJECT_FORMAT_CODE = 0x200B => "InvalidObjectFormatCode",
        STORE_FULL = 0x200C => "StoreFull",
        OBJECT_WRITE_PROTECTED = 0x200D => "ObjectWriteProtected",
        STORE_READ_ONLY = 0x200E => "StoreReadOnly",
        ACCESS_DENIED = 0x200F => "AccessDenied",
        NO_THUMBNAIL_PRESENT = 0x2010 => "NoThumbnailPresent",
        SELF_TEST_FAILED = 0x2011 => "SelfTestFailed",
        PARTIAL_DELETION = 0x2012 => "PartialDeletion",
        STORE_NOT_AVAILABLE = 0x2013 => "StoreNotAvailable",
        SPECIFICATION_BY_FORMAT_UNSUPPORTED = 0x2014 => "SpecificationByFormatUnsupported",
        NO_VALID_OBJECT_INFO = 0x2015 => "NoValidObjectInfo",
        INVALID_CODE_FORMAT = 0x2016 => "InvalidCodeFormat",
        UNKNOWN_VENDOR_CODE = 0x2017 => "UnknownVendorCode",
        CAPTURE_ALREADY_TERMINATED = 0x2018 => "CaptureAlreadyTerminated",
        DEVICE_BUSY = 0x2019 => "DeviceBusy",
        INVALID_PARENT_OBJECT = 0x201A => "InvalidParentObject",
        INVALID_DEVICE_PROP_FORMAT = 0x201B => "InvalidDevicePropFormat",
        INVALID_DEVICE_PROP_VALUE = 0x201C => "InvalidDevicePropValue",
        INVALID_PARAMETER = 0x201D => "InvalidParameter",
        SESSION_ALREADY_OPEN = 0x201E => "SessionAlreadyOpen",
        TRANSACTION_CANCELLED = 0x201F => "TransactionCancelled",
        SPECIFICATION_OF_DESTINATION_UNSUPPORTED = 0x2020 => "SpecificationOfDestinationUnsupported",
    }
}

code_table! {
    /// A PTP event code (`0x4000..=0x400E` standard, `0xC000..` vendor).
    EventCode {
        UNDEFINED = 0x4000 => "Undefined",
        CANCEL_TRANSACTION = 0x4001 => "CancelTransaction",
        OBJECT_ADDED = 0x4002 => "ObjectAdded",
        OBJECT_REMOVED = 0x4003 => "ObjectRemoved",
        STORE_ADDED = 0x4004 => "StoreAdded",
        STORE_REMOVED = 0x4005 => "StoreRemoved",
        DEVICE_PROP_CHANGED = 0x4006 => "DevicePropChanged",
        OBJECT_INFO_CHANGED = 0x4007 => "ObjectInfoChanged",
        DEVICE_INFO_CHANGED = 0x4008 => "DeviceInfoChanged",
        REQUEST_OBJECT_TRANSFER = 0x4009 => "RequestObjectTransfer",
        STORE_FULL = 0x400A => "StoreFull",
        DEVICE_RESET = 0x400B => "DeviceReset",
        STORAGE_INFO_CHANGED = 0x400C => "StorageInfoChanged",
        CAPTURE_COMPLETE = 0x400D => "CaptureComplete",
        UNREPORTED_STATUS = 0x400E => "UnreportedStatus",
    }
}

code_table! {
    /// A PTP device property code (`0x5001..=0x501F` standard, `0xD000..` vendor).
    DevicePropCode {
        BATTERY_LEVEL = 0x5001 => "BatteryLevel",
        FUNCTIONAL_MODE = 0x5002 => "FunctionalMode",
        IMAGE_SIZE = 0x5003 => "ImageSize",
        COMPRESSION_SETTING = 0x5004 => "CompressionSetting",
        WHITE_BALANCE = 0x5005 => "WhiteBalance",
        RGB_GAIN = 0x5006 => "RGBGain",
        F_NUMBER = 0x5007 => "FNumber",
        FOCAL_LENGTH = 0x5008 => "FocalLength",
        FOCUS_DISTANCE = 0x5009 => "FocusDistance",
        FOCUS_MODE = 0x500A => "FocusMode",
        EXPOSURE_METERING_MODE = 0x500B => "ExposureMeteringMode",
        FLASH_MODE = 0x500C => "FlashMode",
        EXPOSURE_TIME = 0x500D => "ExposureTime",
        EXPOSURE_PROGRAM_MODE = 0x500E => "ExposureProgramMode",
        EXPOSURE_INDEX = 0x500F => "ExposureIndex",
        EXPOSURE_BIAS_COMPENSATION = 0x5010 => "ExposureBiasCompensation",
        DATE_TIME = 0x5011 => "DateTime",
        CAPTURE_DELAY = 0x5012 => "CaptureDelay",
        STILL_CAPTURE_MODE = 0x5013 => "StillCaptureMode",
        CONTRAST = 0x5014 => "Contrast",
        SHARPNESS = 0x5015 => "Sharpness",
        DIGITAL_ZOOM = 0x5016 => "DigitalZoom",
        EFFECT_MODE = 0x5017 => "EffectMode",
        BURST_NUMBER = 0x5018 => "BurstNumber",
        BURST_INTERVAL = 0x5019 => "BurstInterval",
        TIMELAPSE_NUMBER = 0x501A => "TimelapseNumber",
        TIMELAPSE_INTERVAL = 0x501B => "TimelapseInterval",
        FOCUS_METERING_MODE = 0x501C => "FocusMeteringMode",
        UPLOAD_URL = 0x501D => "UploadURL",
        ARTIST = 0x501E => "Artist",
        COPYRIGHT_INFO = 0x501F => "CopyrightInfo",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_codes_display_their_names() {
        assert_eq!(OperationCode::GET_DEVICE_INFO.to_string(), "GetDeviceInfo");
        assert_eq!(OperationCode(0x1009).to_string(), "GetObject");
        assert_eq!(ResponseCode::OK.to_string(), "OK");
        assert_eq!(EventCode::OBJECT_ADDED.to_string(), "ObjectAdded");
        assert_eq!(DevicePropCode::BATTERY_LEVEL.to_string(), "BatteryLevel");
    }

    #[test]
    fn unknown_codes_display_as_hex() {
        assert_eq!(OperationCode(0x9201).to_string(), "0x9201");
        assert_eq!(ResponseCode(0xA001).to_string(), "0xa001");
        assert_eq!(EventCode(0x0001).to_string(), "0x0001");
        assert_eq!(DevicePropCode(0xD20D).name(), None);
    }

    #[test]
    fn constants_match_the_spec_values() {
        assert_eq!(OperationCode::OPEN_SESSION.code(), 0x1002);
        assert_eq!(OperationCode::INITIATE_OPEN_CAPTURE.code(), 0x101C);
        assert_eq!(ResponseCode::DEVICE_BUSY.code(), 0x2019);
        assert_eq!(
            ResponseCode::SPECIFICATION_OF_DESTINATION_UNSUPPORTED.code(),
            0x2020
        );
        assert_eq!(EventCode::UNREPORTED_STATUS.code(), 0x400E);
        assert_eq!(DevicePropCode::COPYRIGHT_INFO.code(), 0x501F);
    }

    #[test]
    fn raw_conversions() {
        assert_eq!(OperationCode::from(0x1001), OperationCode::GET_DEVICE_INFO);
        assert_eq!(u16::from(EventCode::DEVICE_RESET), 0x400B);
    }

    #[test]
    fn every_standard_code_has_a_unique_name() {
        let mut seen = std::collections::HashSet::new();
        for raw in 0x1001..=0x101C {
            assert!(seen.insert(OperationCode(raw).name().unwrap()));
        }
        for raw in 0x2001..=0x2020 {
            assert!(ResponseCode(raw).name().is_some());
        }
        for raw in 0x4000..=0x400E {
            assert!(EventCode(raw).name().is_some());
        }
        for raw in 0x5001..=0x501F {
            assert!(DevicePropCode(raw).name().is_some());
        }
    }
}
