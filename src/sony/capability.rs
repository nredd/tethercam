//! Capability detection from what the camera advertises.
//!
//! Three sources, none keyed by model: the standard `DeviceInfo` operation list, the
//! `SDIOGetExtDeviceInfo` property and control lists, and the property table itself.
//!
//! References:
//! - libgphoto2 `camlibs/ptp2/library.c`, `ptp.c` (Sony sections)
//! - `watsonmw/alphawire` `src/aw/aw-control.c`
//! - `docs/architecture.md`

use super::property::PropertyTable;
use super::protocol::{ExtDeviceInfo, Protocol};
use super::{op, prop};
use crate::ptp::{DeviceInfo, OperationCode};

/// One thing a camera may be able to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// Live view: the camera reports a live-view status and serves objects.
    LiveView,
    /// Remote still capture: shutter controls and object download.
    StillCapture,
    /// Property writes (`SDIOSetExtDevicePropValue`).
    SetProperty,
    /// A battery level or power source is reported.
    Battery,
    /// The host may take control with the priority-mode property.
    PriorityMode,
}

/// What a connected camera can do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    protocol: Protocol,
    supported: Vec<Capability>,
}

impl Capabilities {
    /// Derives the capabilities.
    #[must_use]
    pub fn detect(
        protocol: Protocol, device: &DeviceInfo, ext: &ExtDeviceInfo, table: &PropertyTable,
    ) -> Self {
        let has_op = |code: u16| {
            device.operations_supported.contains(&OperationCode(code)) || ext.lists(code)
        };
        let get_object = has_op(op::GET_OBJECT);
        let set_property = has_op(op::SDIO_SET_EXT_DEVICE_PROP_VALUE);
        let conditions = [
            (
                Capability::LiveView,
                get_object && table.contains(prop::LIVE_VIEW_STATUS),
            ),
            (
                Capability::StillCapture,
                get_object
                    && has_op(op::GET_OBJECT_INFO)
                    && has_op(op::SDIO_CONTROL_DEVICE)
                    && ext.has_control(prop::SHUTTER_HALF_PRESS.0)
                    && ext.has_control(prop::SHUTTER_FULL_PRESS.0),
            ),
            (Capability::SetProperty, set_property),
            (
                Capability::Battery,
                table.contains(prop::BATTERY_LEVEL_INDICATOR)
                    || table.contains(prop::BATTERY_REMAINING),
            ),
            (
                Capability::PriorityMode,
                set_property && table.host_priority_value().is_some(),
            ),
        ];
        Self {
            protocol,
            supported: conditions
                .into_iter()
                .filter_map(|(capability, present)| present.then_some(capability))
                .collect(),
        }
    }

    /// The negotiated protocol generation.
    #[must_use]
    pub const fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// Whether the camera can do `capability`.
    #[must_use]
    pub fn supports(&self, capability: Capability) -> bool {
        self.supported.contains(&capability)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp::{DevicePropCode, Value};
    use crate::sony::property::{SonyForm, SonyProperty};

    fn table(entries: &[(DevicePropCode, u8)]) -> PropertyTable {
        let mut table = PropertyTable::new();
        table.merge(
            entries
                .iter()
                .map(|(code, writable)| SonyProperty {
                    code: *code,
                    get_set: *writable,
                    enabled: 1,
                    factory_default: Value::UInt8(0),
                    current: Value::UInt8(0),
                    form: SonyForm::None,
                })
                .collect(),
        );
        table
    }

    fn device(ops: &[u16]) -> DeviceInfo {
        DeviceInfo {
            operations_supported: ops.iter().copied().map(OperationCode).collect(),
            ..DeviceInfo::default()
        }
    }

    fn ext(listed: &[u16], controls: &[u16]) -> ExtDeviceInfo {
        ExtDeviceInfo {
            version: 200,
            properties: listed.to_vec(),
            controls: controls.to_vec(),
        }
    }

    const ALL_OPS: [u16; 4] = [
        op::GET_OBJECT,
        op::GET_OBJECT_INFO,
        op::SDIO_CONTROL_DEVICE,
        op::SDIO_SET_EXT_DEVICE_PROP_VALUE,
    ];
    const SHUTTER: [u16; 2] = [0xD2C1, 0xD2C2];

    fn detect(
        ops: &[u16], listed: &[u16], controls: &[u16], props: &[(DevicePropCode, u8)],
    ) -> Capabilities {
        Capabilities::detect(
            Protocol::V200,
            &device(ops),
            &ext(listed, controls),
            &table(props),
        )
    }

    fn supported(caps: &Capabilities) -> Vec<Capability> {
        [
            Capability::LiveView,
            Capability::StillCapture,
            Capability::SetProperty,
            Capability::Battery,
            Capability::PriorityMode,
        ]
        .into_iter()
        .filter(|c| caps.supports(*c))
        .collect()
    }

    #[test]
    fn nothing_advertised_means_nothing_supported() {
        let caps = detect(&[], &[], &[], &[]);
        assert_eq!(supported(&caps), []);
        assert_eq!(caps.protocol(), Protocol::V200);
    }

    #[test]
    fn everything_advertised_means_everything_supported() {
        let caps = detect(
            &ALL_OPS,
            &[],
            &SHUTTER,
            &[
                (prop::LIVE_VIEW_STATUS, 0),
                (prop::BATTERY_LEVEL_INDICATOR, 0),
                (prop::PRIORITY_MODE, 1),
            ],
        );
        assert_eq!(supported(&caps).len(), 5);
    }

    #[test]
    fn operations_may_come_from_the_extended_list() {
        let caps = detect(&[], &ALL_OPS, &SHUTTER, &[(prop::LIVE_VIEW_STATUS, 0)]);
        assert_eq!(
            supported(&caps),
            [
                Capability::LiveView,
                Capability::StillCapture,
                Capability::SetProperty
            ]
        );
    }

    #[test]
    fn each_still_capture_requirement_is_needed() {
        let without =
            |skip: u16| -> Vec<u16> { ALL_OPS.iter().copied().filter(|o| *o != skip).collect() };
        for skip in [op::GET_OBJECT, op::GET_OBJECT_INFO, op::SDIO_CONTROL_DEVICE] {
            let caps = detect(&without(skip), &[], &SHUTTER, &[]);
            assert!(!caps.supports(Capability::StillCapture), "{skip:#x}");
        }
        for controls in [&SHUTTER[..1], &SHUTTER[1..]] {
            let caps = detect(&ALL_OPS, &[], controls, &[]);
            assert!(!caps.supports(Capability::StillCapture));
        }
    }

    #[test]
    fn live_view_needs_the_object_operation_and_the_status_property() {
        let status = [(prop::LIVE_VIEW_STATUS, 0)];
        assert!(!detect(&[], &[], &[], &status).supports(Capability::LiveView));
        assert!(!detect(&ALL_OPS, &[], &[], &[]).supports(Capability::LiveView));
    }

    #[test]
    fn either_battery_property_counts() {
        for code in [prop::BATTERY_LEVEL_INDICATOR, prop::BATTERY_REMAINING] {
            assert!(detect(&[], &[], &[], &[(code, 0)]).supports(Capability::Battery));
        }
    }

    #[test]
    fn priority_mode_needs_a_writable_property_and_the_write_operation() {
        let ops = [op::SDIO_SET_EXT_DEVICE_PROP_VALUE];
        assert!(
            detect(&ops, &[], &[], &[(prop::PRIORITY_MODE, 1)]).supports(Capability::PriorityMode)
        );
        assert!(
            !detect(&ops, &[], &[], &[(prop::PRIORITY_MODE, 0)]).supports(Capability::PriorityMode)
        );
        assert!(
            !detect(&[], &[], &[], &[(prop::PRIORITY_MODE, 1)]).supports(Capability::PriorityMode)
        );
    }
}
