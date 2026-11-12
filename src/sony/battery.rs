//! Battery level and power source, from two properties.
//!
//! `0xD20E` is a level indicator whose values also say where power comes from (`0x0B..=0x10`
//! are the USB-powered variants); `0xD218` is the remaining percentage as an `INT8`. Right after
//! connecting, property values can read as zero for a few seconds, so read the battery after
//! [`SonyCamera::wait_live_view_ready`](super::SonyCamera::wait_live_view_ready) or a later
//! refresh.
//!
//! References:
//! - `watsonmw/alphawire` `src/aw/aw-control.c`, `sProp_BatteryLevel`

use super::prop;
use super::property::PropertyTable;

/// Where the camera draws power from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerSource {
    /// The battery.
    Battery,
    /// USB bus power.
    Usb,
    /// The camera reports no battery.
    NoBattery,
    /// The indicator is missing or not a documented value.
    Unknown,
}

/// A battery reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    /// Remaining charge, `0..=100`, when the camera reports one.
    pub percent: Option<u8>,
    /// Raw level indicator, when the camera reports one.
    pub indicator: Option<u8>,
    /// Where power comes from.
    pub source: PowerSource,
}

impl Battery {
    /// Reads the battery from the property table, or `None` when the camera reports neither
    /// property.
    #[must_use]
    pub fn read(table: &PropertyTable) -> Option<Self> {
        let indicator = table
            .integer(prop::BATTERY_LEVEL_INDICATOR)
            .map(|raw| u8::try_from(raw).unwrap_or(u8::MAX));
        let percent = table
            .integer(prop::BATTERY_REMAINING)
            .and_then(|raw| u8::try_from(raw).ok())
            .map(|raw| raw.min(100));
        if indicator.is_none() && percent.is_none() {
            return None;
        }
        let source = match indicator {
            Some(0x03..=0x0A) => PowerSource::Battery,
            Some(0x0B..=0x10) => PowerSource::Usb,
            Some(0xFF) => PowerSource::NoBattery,
            _ => PowerSource::Unknown,
        };
        Some(Self {
            percent,
            indicator,
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp::{DevicePropCode, Value};
    use crate::sony::property::{SonyForm, SonyProperty};

    fn table(entries: &[(DevicePropCode, Value)]) -> PropertyTable {
        let mut table = PropertyTable::new();
        table.merge(
            entries
                .iter()
                .map(|(code, value)| SonyProperty {
                    code: *code,
                    get_set: 0,
                    enabled: 1,
                    factory_default: value.clone(),
                    current: value.clone(),
                    form: SonyForm::None,
                })
                .collect(),
        );
        table
    }

    #[test]
    fn absent_properties_mean_no_reading() {
        assert_eq!(Battery::read(&PropertyTable::new()), None);
    }

    #[test]
    fn classifies_power_sources() {
        let cases = [
            (0x05, PowerSource::Battery),
            (0x0E, PowerSource::Usb),
            (0x10, PowerSource::Usb),
            (0xFF, PowerSource::NoBattery),
            (0x00, PowerSource::Unknown),
            (0x01, PowerSource::Unknown),
        ];
        for (raw, source) in cases {
            let t = table(&[(prop::BATTERY_LEVEL_INDICATOR, Value::UInt8(raw))]);
            let battery = Battery::read(&t).unwrap();
            assert_eq!(battery.source, source, "indicator {raw:#x}");
            assert_eq!(battery.indicator, Some(raw));
            assert_eq!(battery.percent, None);
        }
    }

    #[test]
    fn percent_is_clamped_and_unknown_is_none() {
        let t = table(&[(prop::BATTERY_REMAINING, Value::Int8(87))]);
        let battery = Battery::read(&t).unwrap();
        assert_eq!(battery.percent, Some(87));
        assert_eq!(battery.source, PowerSource::Unknown);
        assert_eq!(battery.indicator, None);
        let t = table(&[(prop::BATTERY_REMAINING, Value::Int8(-1))]);
        assert_eq!(Battery::read(&t), None);
        let t = table(&[
            (prop::BATTERY_REMAINING, Value::Int8(-1)),
            (prop::BATTERY_LEVEL_INDICATOR, Value::UInt8(5)),
        ]);
        assert_eq!(Battery::read(&t).unwrap().percent, None);
        let t = table(&[(prop::BATTERY_REMAINING, Value::Int16(150))]);
        assert_eq!(Battery::read(&t).unwrap().percent, Some(100));
        let t = table(&[(prop::BATTERY_LEVEL_INDICATOR, Value::Int32(-5))]);
        assert_eq!(Battery::read(&t).unwrap().indicator, Some(u8::MAX));
    }
}
