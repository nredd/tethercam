//! Sony SDIO operation, event, property, control and object-handle codes.
//!
//! Operation codes are plain `u16` because [`Session`](crate::ptp::session::Session) takes raw
//! codes. Property codes are [`DevicePropCode`] because the property table is keyed by them.
//!
//! Where `libgphoto2` and `alphawire` name the same code differently, the name here follows the
//! behavior the driver relies on: `0xD20E` is a battery level *indicator* (it also encodes the
//! power source), `0xD218` is the remaining battery percentage.
//!
//! References:
//! - libgphoto2 `camlibs/ptp2/ptp.h`, Sony sections (`PTP_OC_SONY_*`, `PTP_DPC_SONY_*`)
//! - `watsonmw/alphawire` `src/aw/aw-const.h`

use crate::ptp::DevicePropCode;

/// Operation codes of the Sony SDIO extension.
pub mod op {
    /// `SDIOConnect`: three-phase handshake, parameters `(phase, 0, 0)`.
    pub const SDIO_CONNECT: u16 = 0x9201;
    /// `SDIOGetExtDeviceInfo`: parameters `(protocol version, 1)`.
    pub const SDIO_GET_EXT_DEVICE_INFO: u16 = 0x9202;
    /// `SDIOSetExtDevicePropValue` (`SetControlDeviceA`): parameter is a property code, the
    /// data phase is the new value.
    pub const SDIO_SET_EXT_DEVICE_PROP_VALUE: u16 = 0x9205;
    /// `SDIOControlDevice` (`SetControlDeviceB`): parameter is a control code, the data phase is
    /// the control value.
    pub const SDIO_CONTROL_DEVICE: u16 = 0x9207;
    /// `SDIOGetAllExtDevicePropInfo`: every property descriptor in one data phase.
    pub const SDIO_GET_ALL_EXT_DEVICE_PROP_INFO: u16 = 0x9209;
    /// Standard `GetObjectInfo`.
    pub const GET_OBJECT_INFO: u16 = 0x1008;
    /// Standard `GetObject`.
    pub const GET_OBJECT: u16 = 0x1009;
}

/// Sony vendor event codes.
pub mod event {
    /// An object (a captured still) is available in camera memory. Parameter 1 is its handle.
    pub const OBJECT_ADDED: u16 = 0xC201;
    /// A property changed. Parameter 1 is the property code.
    pub const PROPERTY_CHANGED: u16 = 0xC203;
}

/// Property and control codes.
pub mod prop {
    use super::DevicePropCode;

    /// Battery level indicator. Values `0x0B..=0x10` mean the camera runs from USB power.
    pub const BATTERY_LEVEL_INDICATOR: DevicePropCode = DevicePropCode(0xD20E);
    /// Remaining battery percentage as an `INT8`, `-1` when unknown.
    pub const BATTERY_REMAINING: DevicePropCode = DevicePropCode(0xD218);
    /// Whether a captured image waits in camera memory: `>= 0x8000` means one does.
    pub const OBJECT_IN_MEMORY: DevicePropCode = DevicePropCode(0xD215);
    /// Live view status: `0` not yet, `1` ready, `2` unsupported.
    pub const LIVE_VIEW_STATUS: DevicePropCode = DevicePropCode(0xD221);
    /// Priority mode (`libgphoto2` `PriorityMode`, `alphawire` `DialMode`); the host takes
    /// control with `1`.
    pub const PRIORITY_MODE: DevicePropCode = DevicePropCode(0xD25A);
    /// Shutter half-press control. `UINT16`, `2` presses and `1` releases.
    pub const SHUTTER_HALF_PRESS: DevicePropCode = DevicePropCode(0xD2C1);
    /// Shutter full-press control. `UINT16`, `2` presses and `1` releases.
    pub const SHUTTER_FULL_PRESS: DevicePropCode = DevicePropCode(0xD2C2);
}

/// Fixed object handles the camera serves.
pub mod handle {
    /// The still image just captured.
    pub const CAPTURED_IMAGE: u32 = 0xFFFF_C001;
    /// The current live-view frame.
    pub const LIVE_VIEW: u32 = 0xFFFF_C002;
}
