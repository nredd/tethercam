//! A simulated Sony body: the SDIO extension on top of [`SimCamera`].
//!
//! [`sony_camera`] builds a [`SimCamera`] with a stateful [`Responder`] that speaks the extension
//! as the references describe it, for either protocol generation:
//! - `SDIOConnect` must run phase 1, phase 2, `SDIOGetExtDeviceInfo`, phase 3, in order; every
//!   other extension operation answers `GeneralError` until phase 3 is done
//! - `SDIOGetExtDeviceInfo` answers the lower of the requested and the body's own version, and
//!   can answer busy or empty first
//! - `SDIOGetAllExtDevicePropInfo` encodes the property table in the layout of the negotiated
//!   generation (the 2.00 table also lists the shutter controls, as the real body does). For
//!   [`SonySimConfig::warmup_reads`] reads, values read as zero and live view is not ready, as on
//!   the measured a7 III right after connecting
//! - Property and control writes validate and apply; a property change emits the
//!   property-changed event
//! - Live view (`0xFFFFC002`) fails with `InvalidObjectHandle` until ready, then (optionally
//!   after `AccessDenied` answers) serves a header and the configured JPEG
//! - A full shutter press makes an image appear in camera memory (`0xD215` reads `0x8001`) at
//!   handle `0xFFFFC001`, with an object-added event if configured; downloading it clears it
//! - A new session starts the extension over, as a real body does. Abandoning a session still
//!   wedges the pipe, which [`SimCamera`] models
//!
//! This module speaks raw codes and has no dependency on the driver in [`crate::sony`], so the
//! driver is tested against an independent implementation of the protocol.
//!
//! References:
//! - libgphoto2 `camlibs/ptp2/ptp.c`, `library.c` (Sony sections)
//! - `watsonmw/alphawire` `src/aw/aw-control.c`

use super::{Reply, Responder, SimCamera};
use crate::ptp::session::code;
use crate::ptp::{
    CodecError, DataType, DeviceInfo, EventCode, ObjectInfo, OperationCode, Reader, Value, Writer,
};

const SDIO_CONNECT: u16 = 0x9201;
const SDIO_GET_EXT_DEVICE_INFO: u16 = 0x9202;
const SDIO_SET_EXT_DEVICE_PROP_VALUE: u16 = 0x9205;
const SDIO_CONTROL_DEVICE: u16 = 0x9207;
const SDIO_GET_ALL_EXT_DEVICE_PROP_INFO: u16 = 0x9209;
const GET_OBJECT_INFO: u16 = 0x1008;
const GET_OBJECT: u16 = 0x1009;
const OBJECT_ADDED: u16 = 0xC201;
const PROPERTY_CHANGED: u16 = 0xC203;
const LIVE_VIEW_HANDLE: u32 = 0xFFFF_C002;
const CAPTURED_HANDLE: u32 = 0xFFFF_C001;
const INVALID_OBJECT_HANDLE: u16 = 0x2009;
const ACCESS_DENIED: u16 = 0x200F;
const DEVICE_PROP_NOT_SUPPORTED: u16 = 0x200A;

const COPYRIGHT: u16 = 0xD1CF;
const ASPECT_RATIO: u16 = 0xD211;
const COLOR_TEMPERATURE: u16 = 0xD20F;
const BATTERY_INDICATOR: u16 = 0xD20E;
const BATTERY_REMAINING: u16 = 0xD218;
const OBJECT_IN_MEMORY: u16 = 0xD215;
const LIVE_VIEW_STATUS: u16 = 0xD221;
const PRIORITY_MODE: u16 = 0xD25A;
const HALF_PRESS: u16 = 0xD2C1;
const FULL_PRESS: u16 = 0xD2C2;

/// Value of `0xD215` while an image waits in memory.
const IMAGE_READY: u16 = 0x8001;

/// How the body reports live view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveViewMode {
    /// Live view works, once the warm-up reads are done.
    Works,
    /// The status property reads "not supported".
    Unsupported,
    /// The body has no live-view status property at all.
    Absent,
}

/// What the shutter controls do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shutter {
    /// A full press captures an image at once.
    Works,
    /// The body does not list the shutter controls.
    NoControls,
    /// The controls exist but no image ever appears.
    NoImage,
    /// The image only appears once the full press is released.
    AfterRelease,
}

/// How the body answers a request for protocol version 3.00 in `SDIOGetExtDeviceInfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version300 {
    /// Answers with its own version, the lower of the two.
    Negotiates,
    /// Answers with this non-OK response code and stays connected.
    Rejects(u16),
    /// Answers this non-OK response code to every version, so no fallback can succeed.
    RefusesAll(u16),
    /// Stalls the bulk pipe, as a body that mishandles the request may.
    Stalls,
}

/// Whether the body has a priority-mode property the host can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// The property exists and is writable, as `libgphoto2` expects.
    Writable,
    /// The body has no such property.
    Absent,
}

/// A malformed answer the body gives, to exercise the driver's error paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Defect {
    /// No defect.
    None,
    /// The extended-info dataset is truncated.
    BadExtInfo,
    /// The property table announces a property it does not contain.
    TruncatedTable,
    /// The captured image's `ObjectInfo` is garbage.
    BadObjectInfo,
    /// The first live-view image is valid; later images have only SOI/EOI markers. This keeps
    /// PTP transactions valid while exercising runtime media failure and orderly cleanup.
    BadLiveViewAfterFirst,
}

/// How a simulated Sony body behaves. [`Default`] is an older-generation body such as the
/// a7 III: protocol 2.00, and live view that works after a short warm-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SonySimConfig {
    /// Model string in `DeviceInfo`.
    pub model: String,
    /// Newest protocol version the body speaks: `200` or `300`.
    pub max_version: u16,
    /// Whether `GetDeviceInfo` lists the SDIO operations (they are absent outside PC Remote).
    pub advertise_sdio: bool,
    /// How a request for version 3.00 is answered.
    pub version_300: Version300,
    /// `SDIOGetExtDeviceInfo` answers `DeviceBusy` this many times first.
    pub ext_info_busy: u32,
    /// `SDIOGetExtDeviceInfo` then answers an empty data phase this many times.
    pub ext_info_empty: u32,
    /// Property reads that report zero values and not-ready live view.
    pub warmup_reads: u32,
    /// Live-view fetches that answer `AccessDenied` after live view is ready.
    pub live_view_denied: u32,
    /// How live view is reported.
    pub live_view: LiveViewMode,
    /// What the shutter does.
    pub shutter: Shutter,
    /// Whether the priority-mode property exists.
    pub priority: Priority,
    /// A malformed answer, if any.
    pub defect: Defect,
    /// Whether a captured image also raises the object-added event.
    pub emit_object_added: bool,
    /// Whether an image already waits in memory when the body powers up.
    pub stale_image: bool,
    /// The JPEG served as live view.
    pub live_view_jpeg: Vec<u8>,
    /// The bytes served for a captured still.
    pub still: Vec<u8>,
}

impl Default for SonySimConfig {
    fn default() -> Self {
        Self {
            model: "ILCE-SIM".to_owned(),
            version_300: Version300::Negotiates,
            max_version: 200,
            advertise_sdio: true,
            ext_info_busy: 0,
            ext_info_empty: 0,
            warmup_reads: 2,
            live_view_denied: 0,
            live_view: LiveViewMode::Works,
            shutter: Shutter::Works,
            priority: Priority::Writable,
            defect: Defect::None,
            emit_object_added: true,
            stale_image: false,
            live_view_jpeg: vec![0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x04, 0x01, 0x02, 0xFF, 0xD9],
            still: vec![0xFF, 0xD8, 0xFF, 0xE1, 0xAA, 0xBB, 0xFF, 0xD9],
        }
    }
}

/// Builds a [`SimCamera`] that behaves like a Sony body in PC Remote mode.
#[must_use]
pub fn sony_camera(config: SonySimConfig) -> SimCamera {
    let mut operations = vec![
        code::GET_DEVICE_INFO,
        code::OPEN_SESSION,
        code::CLOSE_SESSION,
        GET_OBJECT_INFO,
        GET_OBJECT,
    ];
    if config.advertise_sdio {
        operations.extend([
            SDIO_CONNECT,
            SDIO_GET_EXT_DEVICE_INFO,
            SDIO_SET_EXT_DEVICE_PROP_VALUE,
            SDIO_CONTROL_DEVICE,
            SDIO_GET_ALL_EXT_DEVICE_PROP_INFO,
        ]);
    }
    let info = DeviceInfo {
        standard_version: 100,
        vendor_extension_id: 0x11,
        vendor_extension_version: 100,
        vendor_extension_desc: "sony.net/SDIO".to_owned(),
        operations_supported: operations.into_iter().map(OperationCode).collect(),
        events_supported: vec![EventCode(OBJECT_ADDED), EventCode(PROPERTY_CHANGED)],
        manufacturer: "Sony Corporation".to_owned(),
        model: config.model.chars().take(100).collect(),
        device_version: "1.00".to_owned(),
        serial_number: "0000000000".to_owned(),
        ..DeviceInfo::default()
    };
    SimCamera::new()
        .with_device_info(info.to_bytes().unwrap_or_default())
        .with_responder(SonyResponder::new(config))
}

/// Why a host property value cannot be accepted by the simulated body.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
enum HostValueError {
    /// Preserve the underlying malformed-value diagnostic.
    #[error("invalid host property value: {0}")]
    Decode(#[from] CodecError),
    /// Exact values may not leave unused payload bytes.
    #[error("host property value has '{0}' trailing bytes")]
    TrailingBytes(usize),
}

/// Decodes exactly one value, preserving malformed and trailing-payload errors.
fn decode_exact(data_type: DataType, payload: &[u8]) -> Result<Value, HostValueError> {
    let mut reader = Reader::new(payload);
    let value = Value::decode(data_type, &mut reader)?;
    if !reader.is_empty() {
        return Err(HostValueError::TrailingBytes(reader.remaining()));
    }
    Ok(value)
}

fn put_bytes(writer: &mut Writer, bytes: &[u8]) {
    for byte in bytes {
        writer.u8(*byte);
    }
}

fn put_value(writer: &mut Writer, value: &Value) {
    put_bytes(writer, &value.to_bytes().unwrap_or_default());
}

fn put_count(writer: &mut Writer, count: usize) {
    writer.u16(u16::try_from(count).unwrap_or_default());
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Form {
    None,
    Range(Value, Value, Value),
    Enumeration(Vec<Value>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SimProp {
    code: u16,
    get_set: u8,
    current: Value,
    /// What the property reads while the body is still warming up.
    zero: Value,
    form: Form,
}

impl SimProp {
    const fn new(code: u16, get_set: u8, current: Value, zero: Value, form: Form) -> Self {
        Self {
            code,
            get_set,
            current,
            zero,
            form,
        }
    }

    /// Appends the record for this property. `warm` selects the real value over the zero one.
    fn write(&self, writer: &mut Writer, v300: bool, warm: bool) {
        writer.u16(self.code);
        writer.u16(self.current.data_type().code());
        writer.u8(self.get_set);
        writer.u8(1);
        put_value(writer, &self.zero);
        put_value(writer, if warm { &self.current } else { &self.zero });
        match &self.form {
            Form::None => writer.u8(0),
            Form::Range(min, max, step) => {
                writer.u8(1);
                put_value(writer, min);
                put_value(writer, max);
                put_value(writer, step);
            }
            Form::Enumeration(values) => {
                writer.u8(2);
                // 3.00 sends the settable list, then the reportable list; 2.00 only one.
                for _ in 0..(if v300 { 2 } else { 1 }) {
                    put_count(writer, values.len());
                    for value in values {
                        put_value(writer, value);
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
struct SonyResponder {
    config: SonySimConfig,
    props: Vec<SimProp>,
    phase: u32,
    negotiated: u16,
    ext_info_calls: u32,
    reads: u32,
    denied: u32,
    image_present: bool,
    events: Vec<(u16, Vec<u32>)>,
}

impl SonyResponder {
    fn new(config: SonySimConfig) -> Self {
        let plain = |code, get_set, current: Value, zero| {
            SimProp::new(code, get_set, current, zero, Form::None)
        };
        let mut props = vec![
            plain(BATTERY_INDICATOR, 0, Value::UInt8(0x0E), Value::UInt8(0)),
            plain(BATTERY_REMAINING, 0, Value::Int8(80), Value::Int8(0)),
            plain(OBJECT_IN_MEMORY, 0, Value::UInt16(0), Value::UInt16(0)),
            plain(
                COPYRIGHT,
                1,
                Value::String("synthetic".to_owned()),
                Value::String(String::new()),
            ),
            SimProp::new(
                ASPECT_RATIO,
                1,
                Value::UInt8(2),
                Value::UInt8(0),
                Form::Enumeration(vec![Value::UInt8(1), Value::UInt8(2)]),
            ),
            SimProp::new(
                COLOR_TEMPERATURE,
                1,
                Value::UInt16(5500),
                Value::UInt16(0),
                Form::Range(Value::UInt16(2500), Value::UInt16(9900), Value::UInt16(100)),
            ),
        ];
        let status = match config.live_view {
            LiveViewMode::Works => Some(1),
            LiveViewMode::Unsupported => Some(2),
            LiveViewMode::Absent => None,
        };
        if config.priority == Priority::Writable {
            props.push(plain(PRIORITY_MODE, 1, Value::Int8(0), Value::Int8(0)));
        }
        if let Some(status) = status {
            props.push(plain(
                LIVE_VIEW_STATUS,
                0,
                Value::UInt8(status),
                Value::UInt8(0),
            ));
        }
        let mut responder = Self {
            denied: config.live_view_denied,
            image_present: config.stale_image,
            config,
            props,
            phase: 0,
            negotiated: 0,
            ext_info_calls: 0,
            reads: 0,
            events: Vec::new(),
        };
        responder.sync_image_property();
        responder
    }

    fn has_shutter_controls(&self) -> bool {
        self.config.shutter != Shutter::NoControls
    }

    fn v300(&self) -> bool {
        self.negotiated >= 300
    }

    fn ready(&self) -> bool {
        self.reads > self.config.warmup_reads
    }

    fn sync_image_property(&mut self) {
        let value = if self.image_present { IMAGE_READY } else { 0 };
        for prop in self.props.iter_mut().filter(|p| p.code == OBJECT_IN_MEMORY) {
            prop.current = Value::UInt16(value);
        }
    }

    fn changed(&mut self, code: u16) {
        self.events.push((PROPERTY_CHANGED, vec![u32::from(code)]));
    }

    fn connect(&mut self, params: &[u32]) -> Reply {
        let phase = self.phase + 1;
        // Phase 3 only follows a successful `SDIOGetExtDeviceInfo`; there is no phase 4.
        let in_order = phase < 3 || (phase == 3 && self.negotiated != 0);
        if in_order && params == [phase, 0, 0] {
            self.phase = phase;
            Reply::data(vec![0; 8])
        } else {
            Reply::with_code(code::GENERAL_ERROR)
        }
    }

    fn ext_device_info(&mut self, params: &[u32]) -> Reply {
        let [requested, 1] = params else {
            return Reply::with_code(code::INVALID_PARAMETER);
        };
        if self.phase != 2 {
            return Reply::with_code(code::GENERAL_ERROR);
        }
        match self.config.version_300 {
            Version300::Rejects(response) if *requested >= 300 => {
                return Reply::with_code(response);
            }
            Version300::RefusesAll(response) => return Reply::with_code(response),
            _ => {}
        }
        self.ext_info_calls += 1;
        if self.ext_info_calls <= self.config.ext_info_busy {
            return Reply::with_code(code::DEVICE_BUSY);
        }
        if self.ext_info_calls
            <= self
                .config
                .ext_info_busy
                .saturating_add(self.config.ext_info_empty)
        {
            return Reply::data(Vec::new());
        }
        let version = u16::try_from(*requested)
            .unwrap_or(u16::MAX)
            .min(self.config.max_version);
        self.negotiated = version;
        if self.config.defect == Defect::BadExtInfo {
            return Reply::data(vec![1]);
        }
        let mut writer = Writer::new();
        writer.u16(version);
        writer.u32(u32::try_from(self.props.len()).unwrap_or_default());
        for prop in &self.props {
            writer.u16(prop.code);
        }
        let controls: &[u16] = if self.has_shutter_controls() {
            &[HALF_PRESS, FULL_PRESS]
        } else {
            &[]
        };
        writer.u32(u32::try_from(controls.len()).unwrap_or_default());
        for control in controls {
            writer.u16(*control);
        }
        Reply::data(writer.into_bytes())
    }

    fn all_properties(&mut self, params: &[u32]) -> Reply {
        let expected: &[u32] = if self.v300() { &[0, 1] } else { &[] };
        if params != expected {
            return Reply::with_code(code::INVALID_PARAMETER);
        }
        self.reads += 1;
        if self.config.defect == Defect::TruncatedTable {
            return Reply::data(1_u64.to_le_bytes().to_vec());
        }
        let warm = self.ready();
        let mut records = self.props.clone();
        if !self.v300() && self.has_shutter_controls() {
            // 2.00 lists the controls in the same stream.
            records.extend([HALF_PRESS, FULL_PRESS].map(|code| {
                SimProp::new(
                    code,
                    1,
                    Value::UInt16(1),
                    Value::UInt16(1),
                    Form::Enumeration(vec![Value::UInt16(1), Value::UInt16(2)]),
                )
            }));
        }
        let mut writer = Writer::new();
        writer.u64(u64::try_from(records.len()).unwrap_or_default());
        for record in &records {
            record.write(&mut writer, self.v300(), warm);
        }
        Reply::data(writer.into_bytes())
    }

    /// Validates the target of a property or control write.
    fn write_target(&self, operation: u16, params: &[u32]) -> Reply {
        let [raw] = params else {
            return Reply::with_code(code::INVALID_PARAMETER);
        };
        let code = u16::try_from(*raw).unwrap_or_default();
        let known = if operation == SDIO_CONTROL_DEVICE {
            self.has_shutter_controls() && (code == HALF_PRESS || code == FULL_PRESS)
        } else {
            self.props.iter().any(|p| p.code == code && p.get_set == 1)
        };
        if known {
            Reply::accepting_data()
        } else {
            Reply::with_code(DEVICE_PROP_NOT_SUPPORTED)
        }
    }

    fn live_view_object(&mut self) -> Reply {
        if !self.ready() {
            return Reply::with_code(INVALID_OBJECT_HANDLE);
        }
        if self.denied > 0 {
            self.denied -= 1;
            return Reply::with_code(ACCESS_DENIED);
        }
        let jpeg_len = u32::try_from(self.config.live_view_jpeg.len()).unwrap_or_default();
        let header_len: u32 = if self.v300() { 16 } else { 8 };
        let mut writer = Writer::new();
        writer.u32(header_len);
        writer.u32(jpeg_len);
        if self.v300() {
            // Focus-frame block: offset just past the JPEG, size zero.
            writer.u32(header_len + jpeg_len);
            writer.u32(0);
        }
        put_bytes(&mut writer, &self.config.live_view_jpeg);
        if self.config.defect == Defect::BadLiveViewAfterFirst {
            self.config.live_view_jpeg = vec![0xff, 0xd8, 0xff, 0xd9];
        }
        Reply::data(writer.into_bytes())
    }

    fn captured_info(&self) -> Reply {
        if self.config.defect == Defect::BadObjectInfo {
            return Reply::data(vec![1, 2, 3]);
        }
        let info = ObjectInfo {
            object_format: 0x3801,
            object_compressed_size: u32::try_from(self.config.still.len()).unwrap_or_default(),
            filename: "DSC00001.JPG".to_owned(),
            ..ObjectInfo::default()
        };
        Reply::data(info.to_bytes().unwrap_or_default())
    }

    fn captured_object(&mut self) -> Reply {
        self.image_present = false;
        self.sync_image_property();
        self.changed(OBJECT_IN_MEMORY);
        Reply::data(self.config.still.clone())
    }

    fn press(&mut self, code: u16, value: u16) -> Reply {
        if value != 1 && value != 2 {
            return Reply::with_code(code::INVALID_PARAMETER);
        }
        let fires = match self.config.shutter {
            Shutter::Works => value == 2,
            Shutter::AfterRelease => value == 1,
            Shutter::NoControls | Shutter::NoImage => false,
        };
        if code == FULL_PRESS && fires {
            self.image_present = true;
            self.sync_image_property();
            self.changed(OBJECT_IN_MEMORY);
            if self.config.emit_object_added {
                self.events.push((OBJECT_ADDED, vec![CAPTURED_HANDLE]));
            }
        }
        Reply::ok()
    }

    fn write_property(&mut self, code: u16, payload: &[u8]) -> Reply {
        let mut reply = Reply::ok();
        for prop in self.props.iter_mut().filter(|p| p.code == code) {
            match decode_exact(prop.current.data_type(), payload) {
                Ok(value) => prop.current = value,
                Err(_) => reply = Reply::with_code(code::INVALID_PARAMETER),
            }
        }
        if reply.response == code::RESPONSE_OK {
            self.changed(code);
        }
        reply
    }
}

impl Responder for SonyResponder {
    fn command(&mut self, operation: u16, params: &[u32]) -> Option<Reply> {
        let sdio = matches!(
            operation,
            SDIO_CONNECT
                | SDIO_GET_EXT_DEVICE_INFO
                | SDIO_SET_EXT_DEVICE_PROP_VALUE
                | SDIO_CONTROL_DEVICE
                | SDIO_GET_ALL_EXT_DEVICE_PROP_INFO
        );
        let object = matches!(operation, GET_OBJECT | GET_OBJECT_INFO);
        if !(sdio || object) {
            return None;
        }
        if sdio && !self.config.advertise_sdio {
            return Some(Reply::with_code(code::OPERATION_NOT_SUPPORTED));
        }
        let handshake = matches!(operation, SDIO_CONNECT | SDIO_GET_EXT_DEVICE_INFO);
        if !handshake && self.phase != 3 {
            return Some(Reply::with_code(code::GENERAL_ERROR));
        }
        Some(match (operation, params) {
            (SDIO_CONNECT, _) => self.connect(params),
            (SDIO_GET_EXT_DEVICE_INFO, _) => self.ext_device_info(params),
            (SDIO_GET_ALL_EXT_DEVICE_PROP_INFO, _) => self.all_properties(params),
            (SDIO_SET_EXT_DEVICE_PROP_VALUE | SDIO_CONTROL_DEVICE, _) => {
                self.write_target(operation, params)
            }
            (GET_OBJECT, [LIVE_VIEW_HANDLE]) => self.live_view_object(),
            (GET_OBJECT, [CAPTURED_HANDLE]) if self.image_present => self.captured_object(),
            (GET_OBJECT_INFO, [CAPTURED_HANDLE]) if self.image_present => self.captured_info(),
            _ => Reply::with_code(INVALID_OBJECT_HANDLE),
        })
    }

    fn data_out(&mut self, operation: u16, params: &[u32], payload: &[u8]) -> Option<Reply> {
        let code = params
            .first()
            .and_then(|raw| u16::try_from(*raw).ok())
            .unwrap_or_default();
        match operation {
            SDIO_SET_EXT_DEVICE_PROP_VALUE => Some(self.write_property(code, payload)),
            SDIO_CONTROL_DEVICE => Some(match decode_exact(DataType::UInt16, payload) {
                Ok(Value::UInt16(value)) => self.press(code, value),
                _ => Reply::with_code(code::INVALID_PARAMETER),
            }),
            _ => None,
        }
    }

    fn session_changed(&mut self, _open: bool) {
        self.phase = 0;
        self.negotiated = 0;
        self.ext_info_calls = 0;
        self.reads = 0;
        self.denied = self.config.live_view_denied;
        self.events.clear();
    }

    fn stall(&mut self, operation: u16, params: &[u32]) -> bool {
        self.config.version_300 == Version300::Stalls
            && self.phase == 2
            && operation == SDIO_GET_EXT_DEVICE_INFO
            && params.first().is_some_and(|requested| *requested >= 300)
    }

    fn take_events(&mut self) -> Vec<(u16, Vec<u32>)> {
        std::mem::take(&mut self.events)
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::*;
    use crate::ptp::session::{Session, SessionConfig, SessionError};

    proptest::proptest! {
        #[test]
        fn exact_host_values_never_panic_on_arbitrary_bytes(payload in proptest::collection::vec(proptest::num::u8::ANY, 0..512)) {
            for code in (1..=10).chain(0x4001..=0x400a).chain([0xffff]) {
                let data_type = DataType::try_from(code).unwrap();
                let _ = decode_exact(data_type, &payload);
            }
        }
    }

    #[test]
    fn exact_host_values_reject_truncated_and_trailing_payloads_for_every_type() {
        for value in [
            Value::Int8(-1),
            Value::UInt8(1),
            Value::Int16(-1),
            Value::UInt16(1),
            Value::Int32(-1),
            Value::UInt32(1),
            Value::Int64(-1),
            Value::UInt64(1),
            Value::Int128(-1),
            Value::UInt128(1),
            Value::Int8Array(vec![-1]),
            Value::UInt8Array(vec![1]),
            Value::Int16Array(vec![-1]),
            Value::UInt16Array(vec![1]),
            Value::Int32Array(vec![-1]),
            Value::UInt32Array(vec![1]),
            Value::Int64Array(vec![-1]),
            Value::UInt64Array(vec![1]),
            Value::Int128Array(vec![-1]),
            Value::UInt128Array(vec![1]),
            Value::String("synthetic".into()),
        ] {
            let data_type = value.data_type();
            let mut payload = value.to_bytes().unwrap();
            assert_eq!(decode_exact(data_type, &payload).unwrap(), value);
            for length in 0..payload.len() {
                assert!(decode_exact(data_type, &payload[..length]).is_err());
            }
            payload.push(0);
            assert_eq!(
                decode_exact(data_type, &payload).unwrap_err(),
                HostValueError::TrailingBytes(1)
            );
        }
    }

    #[test]
    fn exact_host_value_errors_preserve_decode_diagnostics_and_trailing_counts() {
        let decode = decode_exact(DataType::UInt16, &[]).unwrap_err();
        assert_eq!(
            decode,
            HostValueError::Decode(CodecError::Truncated {
                needed: 2,
                available: 0
            })
        );
        assert_eq!(
            decode.to_string(),
            "invalid host property value: payload truncated: needed '2' more bytes, '0' available"
        );
        assert!(decode.source().is_some());
        let trailing = decode_exact(DataType::UInt8, &[1, 2]).unwrap_err();
        assert_eq!(
            trailing.to_string(),
            "host property value has '1' trailing bytes"
        );
        assert!(trailing.source().is_none());
    }

    fn config(version: u16) -> SonySimConfig {
        SonySimConfig {
            max_version: version,
            ..SonySimConfig::default()
        }
    }

    async fn open(sim: SonySimConfig) -> Session<SimCamera> {
        let mut session = Session::new(sony_camera(sim), SessionConfig::default());
        session.open().await.unwrap();
        session
    }

    async fn answer(
        session: &mut Session<SimCamera>, op: u16, params: &[u32], data: Option<&[u8]>,
    ) -> u16 {
        let result = session.transaction(op, params, data).await;
        let Err(SessionError::Response { code, .. }) = result else {
            assert!(result.is_ok(), "unexpected failure: {result:?}");
            return code::RESPONSE_OK;
        };
        code
    }

    /// Runs the handshake up to and including phase 3.
    async fn connect(session: &mut Session<SimCamera>) {
        for (op, params) in [
            (SDIO_CONNECT, vec![1, 0, 0]),
            (SDIO_CONNECT, vec![2, 0, 0]),
            (SDIO_GET_EXT_DEVICE_INFO, vec![300, 1]),
            (SDIO_CONNECT, vec![3, 0, 0]),
        ] {
            assert_eq!(answer(session, op, &params, None).await, code::RESPONSE_OK);
        }
    }

    #[tokio::test]
    async fn the_handshake_must_run_in_order() {
        let mut session = open(config(200)).await;
        let general = code::GENERAL_ERROR;
        // Everything but the handshake is refused first.
        for op in [
            SDIO_GET_ALL_EXT_DEVICE_PROP_INFO,
            GET_OBJECT,
            GET_OBJECT_INFO,
        ] {
            assert_eq!(answer(&mut session, op, &[], None).await, general);
        }
        assert_eq!(
            answer(&mut session, SDIO_CONNECT, &[2, 0, 0], None).await,
            general
        );
        assert_eq!(
            answer(&mut session, SDIO_CONNECT, &[1, 0, 1], None).await,
            general
        );
        assert_eq!(
            answer(&mut session, SDIO_GET_EXT_DEVICE_INFO, &[300, 1], None).await,
            general
        );
        assert_eq!(
            answer(&mut session, SDIO_CONNECT, &[1, 0, 0], None).await,
            code::RESPONSE_OK
        );
        assert_eq!(
            answer(&mut session, SDIO_CONNECT, &[2, 0, 0], None).await,
            code::RESPONSE_OK
        );
        // Phase 3 needs the extended info first.
        assert_eq!(
            answer(&mut session, SDIO_CONNECT, &[3, 0, 0], None).await,
            general
        );
        assert_eq!(
            answer(&mut session, SDIO_GET_EXT_DEVICE_INFO, &[300], None).await,
            code::INVALID_PARAMETER
        );
        assert_eq!(
            answer(&mut session, SDIO_GET_EXT_DEVICE_INFO, &[300, 1], None).await,
            code::RESPONSE_OK
        );
        assert_eq!(
            answer(&mut session, SDIO_CONNECT, &[3, 0, 0], None).await,
            code::RESPONSE_OK
        );
        // There is no phase 4.
        assert_eq!(
            answer(&mut session, SDIO_CONNECT, &[4, 0, 0], None).await,
            general
        );
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_body_outside_pc_remote_mode_rejects_the_extension() {
        let sim = SonySimConfig {
            advertise_sdio: false,
            ..config(200)
        };
        let mut session = open(sim).await;
        let unsupported = code::OPERATION_NOT_SUPPORTED;
        assert_eq!(
            answer(&mut session, SDIO_CONNECT, &[1, 0, 0], None).await,
            unsupported
        );
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn properties_and_writes_validate_their_input() {
        for version in [200, 300] {
            let mut session = open(config(version)).await;
            connect(&mut session).await;
            let (right, wrong): (&[u32], &[u32]) = if version == 200 {
                (&[], &[0, 1])
            } else {
                (&[0, 1], &[])
            };
            let all = SDIO_GET_ALL_EXT_DEVICE_PROP_INFO;
            assert_eq!(
                answer(&mut session, all, wrong, None).await,
                code::INVALID_PARAMETER
            );
            assert_eq!(
                answer(&mut session, all, right, None).await,
                code::RESPONSE_OK
            );

            let set = SDIO_SET_EXT_DEVICE_PROP_VALUE;
            let invalid = code::INVALID_PARAMETER;
            assert_eq!(answer(&mut session, set, &[], None).await, invalid);
            let unsupported = DEVICE_PROP_NOT_SUPPORTED;
            // Unknown, read-only (live view status) and not-a-property (a control).
            for target in [0xD999, 0xD221, 0xD2C1, 0x1_0000] {
                assert_eq!(
                    answer(&mut session, set, &[target], None).await,
                    unsupported
                );
            }
            // Wrong payload size for an `UINT8` property.
            assert_eq!(
                answer(&mut session, set, &[0xD211], Some(&[])).await,
                invalid
            );
            assert_eq!(
                answer(&mut session, set, &[0xD211], Some(&[1])).await,
                code::RESPONSE_OK
            );

            let control = SDIO_CONTROL_DEVICE;
            assert_eq!(
                answer(&mut session, control, &[0xD211], None).await,
                unsupported
            );
            assert_eq!(
                answer(&mut session, control, &[0xD2C1], Some(&[7, 0])).await,
                invalid
            );
            assert_eq!(
                answer(&mut session, control, &[0xD2C1], Some(&[1])).await,
                invalid
            );
            assert_eq!(
                answer(&mut session, control, &[0xD2C1], Some(&[1, 0])).await,
                code::RESPONSE_OK
            );
            session.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn objects_are_only_served_when_they_exist() {
        let mut session = open(config(200)).await;
        connect(&mut session).await;
        let missing = INVALID_OBJECT_HANDLE;
        for op in [GET_OBJECT, GET_OBJECT_INFO] {
            for handle in [0x1234, CAPTURED_HANDLE] {
                assert_eq!(answer(&mut session, op, &[handle], None).await, missing);
            }
        }
        // Not ready yet: zero property reads so far.
        assert_eq!(
            answer(&mut session, GET_OBJECT, &[LIVE_VIEW_HANDLE], None).await,
            missing
        );
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn other_operations_fall_through_to_the_scripted_ones() {
        let camera = sony_camera(config(200)).with_operation(0x9999, Reply::accepting_data());
        let mut session = Session::new(camera, SessionConfig::default());
        session.open().await.unwrap();
        connect(&mut session).await;
        assert_eq!(
            answer(&mut session, 0x9999, &[], Some(&[1])).await,
            code::RESPONSE_OK
        );
        assert_eq!(
            answer(&mut session, 0x1006, &[], None).await,
            code::OPERATION_NOT_SUPPORTED
        );
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_new_session_starts_the_extension_over() {
        let mut session = open(config(200)).await;
        connect(&mut session).await;
        session.close().await.unwrap();
        session.open().await.unwrap();
        assert_eq!(
            answer(&mut session, SDIO_GET_ALL_EXT_DEVICE_PROP_INFO, &[], None).await,
            code::GENERAL_ERROR
        );
        session.close().await.unwrap();
    }

    #[tokio::test]
    async fn oversized_payloads_are_refused_without_side_effects() {
        let mut session = open(config(200)).await;
        connect(&mut session).await;
        let all = SDIO_GET_ALL_EXT_DEVICE_PROP_INFO;
        let before = session.transaction(all, &[], None).await.unwrap().data;
        let invalid = code::INVALID_PARAMETER;
        // Four bytes for a `UINT8` property, and for a `UINT16` control.
        let set = SDIO_SET_EXT_DEVICE_PROP_VALUE;
        assert_eq!(
            answer(&mut session, set, &[0xD211], Some(&[1, 0, 0, 0])).await,
            invalid
        );
        let control = SDIO_CONTROL_DEVICE;
        for target in [0xD2C1, 0xD2C2] {
            assert_eq!(
                answer(&mut session, control, &[target], Some(&[2, 0, 0, 0])).await,
                invalid
            );
        }
        let after = session.transaction(all, &[], None).await.unwrap().data;
        assert_eq!(before, after, "no property changed");
        assert_eq!(
            answer(&mut session, GET_OBJECT_INFO, &[CAPTURED_HANDLE], None).await,
            INVALID_OBJECT_HANDLE,
            "the shutter did not fire"
        );
        session.close().await.unwrap();
    }
}
