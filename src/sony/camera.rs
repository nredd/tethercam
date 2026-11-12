//! The connected Sony camera: handshake, property table, events, live view.
//!
//! References:
//! - libgphoto2 `camlibs/ptp2/library.c`, `ptp.c` (Sony sections)
//! - `watsonmw/alphawire` `src/aw/aw-control.c`
//! - `docs/architecture.md`

use std::time::{Duration, Instant};

use tokio::time::sleep;

use super::battery::Battery;
use super::capability::{Capabilities, Capability};
use super::error::SonyError;
use super::live_view::extract_jpeg;
use super::property::{PropertyTable, decode_properties};
use super::protocol::{ExtDeviceInfo, Protocol};
use super::quirks::Quirks;
use super::{event, handle, op, prop};
use crate::ptp::session::{Event, Session, SessionError, code};
use crate::ptp::{DeviceInfo, DevicePropCode, OperationCode, ResponseCode, Value};
use crate::transport::{Transport, TransportError};

/// Events read per [`SonyCamera::pump_events`] call, so a camera that never stops sending
/// events cannot hold the caller forever.
const MAX_EVENTS_PER_PUMP: usize = 64;

/// Tunables for [`SonyCamera`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SonyConfig {
    /// The protocol generation to request first, unless the model's [`Quirks`] name another.
    /// The camera may answer with an older one. A refused 3.00 request falls back to 2.00 once.
    pub protocol: Protocol,
    /// Attempts at `SDIOGetExtDeviceInfo`, which can answer empty right after phase 2.
    pub ext_info_attempts: u32,
    /// Deadline for those attempts together, from the first, on the session's clock. A slow
    /// camera could otherwise spend a whole transfer timeout on every attempt.
    pub ext_info_timeout: Duration,
    /// Pause between polls and retries.
    pub poll_interval: Duration,
    /// Longest wait for an event on the interrupt pipe. An idle pipe holds the poll for all of
    /// it on real hardware, so this is what every event pump costs when nothing happened.
    pub event_poll_timeout: Duration,
    /// Deadline for live view to become ready, from the first poll.
    pub live_view_ready_timeout: Duration,
    /// Deadline per live-view frame, before any model quirk adds to it.
    pub live_view_timeout: Duration,
    /// Deadline for a captured image to appear after the shutter is released.
    pub capture_timeout: Duration,
}

impl Default for SonyConfig {
    /// Newest protocol first, 50 ms polls and event waits: live view gets 10 s to become ready,
    /// a frame 2.5 s to arrive, and a capture 35 s (30 s maximum exposure plus 5 s to save).
    /// Every deadline includes the time spent in transactions and event reads.
    fn default() -> Self {
        Self {
            protocol: Protocol::V300,
            ext_info_attempts: 10,
            ext_info_timeout: Duration::from_secs(5),
            poll_interval: Duration::from_millis(50),
            event_poll_timeout: Duration::from_millis(50),
            live_view_ready_timeout: Duration::from_secs(10),
            live_view_timeout: Duration::from_millis(2500),
            capture_timeout: Duration::from_secs(35),
        }
    }
}

/// An event from the camera, classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SonyEvent {
    /// A property changed; the table was refreshed.
    PropertyChanged(DevicePropCode),
    /// An object appeared in camera memory.
    ObjectAdded {
        /// Handle to download it from.
        handle: u32,
    },
    /// Any other event, unmodified.
    Other(Event),
}

/// A connected Sony camera. Only [`SonyCamera::run`] creates one, and it closes the session
/// when the closure returns.
#[derive(Debug)]
pub struct SonyCamera<'s, T> {
    pub(super) session: &'s mut Session<T>,
    pub(super) config: SonyConfig,
    device: DeviceInfo,
    ext: ExtDeviceInfo,
    protocol: Protocol,
    pub(super) properties: PropertyTable,
    quirks: Quirks,
}

impl<T: Transport> SonyCamera<'_, T> {
    /// Opens a session, connects, runs `body` and closes the session on every exit path.
    ///
    /// The body's error wins over a close error; a close error after a successful body is
    /// returned. Run the future to completion: cancelling it skips `CloseSession`.
    ///
    /// # Errors
    ///
    /// Any [`SonyError`] from opening, connecting, `body` or closing.
    pub async fn run<R>(
        session: &mut Session<T>, config: SonyConfig,
        body: impl AsyncFnOnce(&mut SonyCamera<'_, T>) -> Result<R, SonyError>,
    ) -> Result<R, SonyError> {
        session.open().await?;
        let result = match SonyCamera::connect(&mut *session, config).await {
            Ok(mut camera) => body(&mut camera).await,
            Err(err) => Err(err),
        };
        let closed = session.close().await;
        match result {
            Ok(value) => closed.map(|()| value).map_err(SonyError::from),
            Err(err) => Err(err),
        }
    }
}

impl<'s, T: Transport> SonyCamera<'s, T> {
    /// The Sony handshake on an open session.
    async fn connect(session: &'s mut Session<T>, config: SonyConfig) -> Result<Self, SonyError> {
        let device = DeviceInfo::decode(&session.get_device_info().await?)?;
        if !device
            .operations_supported
            .contains(&OperationCode(op::SDIO_GET_EXT_DEVICE_INFO))
        {
            return Err(SonyError::SdioUnsupported);
        }
        let quirks = Quirks::for_model(&device.model);
        connect_phase(session, 1).await?;
        connect_phase(session, 2).await?;
        let start = quirks.start_protocol.unwrap_or(config.protocol);
        let ext = negotiate_ext_info(session, &config, start).await?;
        connect_phase(session, 3).await?;
        let protocol = Protocol::from_version(ext.version)
            .ok_or(SonyError::UnsupportedProtocol(ext.version))?;
        let mut camera = Self {
            session,
            config,
            device,
            ext,
            protocol,
            properties: PropertyTable::new(),
            quirks,
        };
        camera.refresh_properties().await?;
        if let Some(value) = camera.properties.host_priority_value() {
            camera.set_property(prop::PRIORITY_MODE, &value).await?;
        }
        Ok(camera)
    }

    /// The negotiated protocol generation.
    #[must_use]
    pub const fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// The standard `DeviceInfo` the camera reported.
    #[must_use]
    pub const fn device_info(&self) -> &DeviceInfo {
        &self.device
    }

    /// The extended device info the camera reported.
    #[must_use]
    pub const fn ext_info(&self) -> &ExtDeviceInfo {
        &self.ext
    }

    /// The property table as of the last refresh.
    #[must_use]
    pub const fn properties(&self) -> &PropertyTable {
        &self.properties
    }

    /// The model exceptions in force.
    #[must_use]
    pub const fn quirks(&self) -> Quirks {
        self.quirks
    }

    /// What the camera can do, as of the last refresh.
    #[must_use]
    pub fn capabilities(&self) -> Capabilities {
        Capabilities::detect(self.protocol, &self.device, &self.ext, &self.properties)
    }

    /// The battery as of the last refresh. See [`Battery`] for the warm-up caveat.
    #[must_use]
    pub fn battery(&self) -> Option<Battery> {
        Battery::read(&self.properties)
    }

    pub(super) async fn pause(&self) {
        sleep(self.config.poll_interval).await;
    }

    /// Time since `started`, on the session's (injectable) clock.
    pub(super) fn waited(&self, started: Instant) -> Duration {
        self.session.now().saturating_duration_since(started)
    }

    /// Re-reads every property and returns the codes that are new or changed.
    ///
    /// Every refresh is a full read: incremental reads exist only on 3.00 bodies and save
    /// little, so one code path serves both generations.
    ///
    /// # Errors
    ///
    /// [`SonyError::Session`] or [`SonyError::Codec`].
    pub async fn refresh_properties(&mut self) -> Result<Vec<DevicePropCode>, SonyError> {
        // 3.00 takes (incremental, extended); 2.00 takes no parameters.
        let params: &[u32] = match self.protocol {
            Protocol::V200 => &[],
            Protocol::V300 => &[0, 1],
        };
        let outcome = self
            .session
            .transaction(op::SDIO_GET_ALL_EXT_DEVICE_PROP_INFO, params, None)
            .await?;
        let updates = decode_properties(self.protocol, &outcome.data)?;
        Ok(self.properties.merge(updates))
    }

    /// Writes a property with `SDIOSetExtDevicePropValue`.
    ///
    /// The local table is not updated: the camera confirms with a property-changed event, which
    /// [`SonyCamera::pump_events`] turns into a refresh.
    ///
    /// # Errors
    ///
    /// [`SonyError::UnknownProperty`], [`SonyError::NotWritable`], [`SonyError::TypeMismatch`],
    /// or a session or encoding error.
    pub async fn set_property(
        &mut self, code: DevicePropCode, value: &Value,
    ) -> Result<(), SonyError> {
        let property = self
            .properties
            .get(code)
            .ok_or(SonyError::UnknownProperty(code))?;
        if !property.is_writable() {
            return Err(SonyError::NotWritable(code));
        }
        let expected = property.data_type().code();
        let actual = value.data_type().code();
        if expected != actual {
            return Err(SonyError::TypeMismatch {
                code,
                expected,
                actual,
            });
        }
        let data = value.to_bytes()?;
        self.session
            .transaction(
                op::SDIO_SET_EXT_DEVICE_PROP_VALUE,
                &[u32::from(code.0)],
                Some(&data),
            )
            .await?;
        Ok(())
    }

    /// Sends a control value with `SDIOControlDevice`, e.g. a shutter press.
    ///
    /// # Errors
    ///
    /// [`SonyError::UnsupportedControl`] when the camera does not list `code`, or a session or
    /// encoding error.
    pub async fn control(&mut self, code: DevicePropCode, value: &Value) -> Result<(), SonyError> {
        if !self.ext.has_control(code.0) {
            return Err(SonyError::UnsupportedControl(code));
        }
        let data = value.to_bytes()?;
        self.session
            .transaction(op::SDIO_CONTROL_DEVICE, &[u32::from(code.0)], Some(&data))
            .await?;
        Ok(())
    }

    /// Drains pending events, classifying each. A property-changed event triggers one
    /// refresh of the property table after the drain.
    ///
    /// # Errors
    ///
    /// [`SonyError::Session`] from polling or refreshing.
    pub async fn pump_events(&mut self) -> Result<Vec<SonyEvent>, SonyError> {
        let mut events = Vec::new();
        for _ in 0..MAX_EVENTS_PER_PUMP {
            let Some(raw) = self
                .session
                .poll_event_within(self.config.event_poll_timeout)
                .await?
            else {
                break;
            };
            events.push(classify(raw));
        }
        if events
            .iter()
            .any(|event| matches!(event, SonyEvent::PropertyChanged(_)))
        {
            self.refresh_properties().await?;
        }
        Ok(events)
    }

    /// Polls the property table until the camera reports live view ready.
    ///
    /// # Errors
    ///
    /// [`SonyError::LiveViewUnsupported`] when the camera cannot provide live view,
    /// [`SonyError::LiveViewNotReady`] when it does not become ready in time, or a session
    /// error.
    pub async fn wait_live_view_ready(&mut self) -> Result<(), SonyError> {
        let started = self.session.now();
        while self.waited(started) < self.config.live_view_ready_timeout {
            self.refresh_properties().await?;
            if !self.capabilities().supports(Capability::LiveView) {
                return Err(SonyError::LiveViewUnsupported);
            }
            match self.properties.integer(prop::LIVE_VIEW_STATUS) {
                Some(1) => return Ok(()),
                Some(2) => return Err(SonyError::LiveViewUnsupported),
                _ => self.pause().await,
            }
        }
        Err(SonyError::LiveViewNotReady)
    }

    /// Fetches one live-view frame, once.
    ///
    /// # Errors
    ///
    /// [`SonyError::LiveViewNotReady`] when the camera refuses the object, which it does until
    /// live view has started; [`SonyError::LiveView`] for a malformed object; or a session
    /// error.
    pub async fn fetch_live_view_frame(&mut self) -> Result<Vec<u8>, SonyError> {
        let not_ready = [
            ResponseCode::INVALID_OBJECT_HANDLE.code(),
            ResponseCode::ACCESS_DENIED.code(),
        ];
        match self
            .session
            .transaction(op::GET_OBJECT, &[handle::LIVE_VIEW], None)
            .await
        {
            Ok(outcome) => Ok(extract_jpeg(outcome.data)?),
            Err(SessionError::Response { code, .. }) if not_ready.contains(&code) => {
                Err(SonyError::LiveViewNotReady)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Fetches the next live-view JPEG, retrying while the camera is not ready.
    ///
    /// The deadline is [`SonyConfig::live_view_timeout`] plus the model's
    /// [`Quirks::extra_live_view_wait`].
    ///
    /// # Errors
    ///
    /// [`SonyError::LiveViewNotReady`] when the budget runs out, or any other error from
    /// [`SonyCamera::fetch_live_view_frame`].
    pub async fn next_live_view_frame(&mut self) -> Result<Vec<u8>, SonyError> {
        let limit = self
            .config
            .live_view_timeout
            .saturating_add(self.quirks.extra_live_view_wait);
        let started = self.session.now();
        while self.waited(started) < limit {
            match self.fetch_live_view_frame().await {
                Err(SonyError::LiveViewNotReady) => self.pause().await,
                other => return other,
            }
        }
        Err(SonyError::LiveViewNotReady)
    }
}

async fn connect_phase<T: Transport>(
    session: &mut Session<T>, phase: u32,
) -> Result<(), SonyError> {
    session
        .transaction(op::SDIO_CONNECT, &[phase, 0, 0], None)
        .await?;
    Ok(())
}

/// Whether a failed `SDIOGetExtDeviceInfo` means "this generation is not accepted": a non-OK
/// response, a stall or timeout, or a camera that kept answering busy or empty. Anything else (a disconnect, a malformed container) is
/// fatal.
fn is_refusal(err: &SonyError) -> bool {
    matches!(
        err,
        SonyError::ExtInfoUnavailable { .. }
            | SonyError::Session(
                SessionError::Response { .. }
                    | SessionError::Transport(TransportError::Stall | TransportError::Timeout)
            )
    )
}

/// Requests `start`, then the next older generation once if `start` is refused.
///
/// A non-OK response leaves the pipe and session intact, so the older request follows on the
/// same session. A stall or timeout makes the session layer clear the halts and reset the
/// device, which ends the session: it is reopened and the handshake replayed to phase 2 first,
/// so the camera is never left wedged.
async fn negotiate_ext_info<T: Transport>(
    session: &mut Session<T>, config: &SonyConfig, start: Protocol,
) -> Result<ExtDeviceInfo, SonyError> {
    let first = match ext_device_info(session, config, start).await {
        Err(err) if is_refusal(&err) => err,
        other => return other,
    };
    let Some(fallback) = start.older() else {
        return Err(first);
    };
    if !session.is_open() {
        session.open().await?;
        connect_phase(session, 1).await?;
        connect_phase(session, 2).await?;
    }
    ext_device_info(session, config, fallback)
        .await
        .map_err(|second| SonyError::ProtocolRefused {
            requested: start,
            requested_error: Box::new(first),
            fallback,
            fallback_error: Box::new(second),
        })
}

/// `SDIOGetExtDeviceInfo` for `protocol`, retried while the camera answers empty or busy.
async fn ext_device_info<T: Transport>(
    session: &mut Session<T>, config: &SonyConfig, protocol: Protocol,
) -> Result<ExtDeviceInfo, SonyError> {
    let params = [u32::from(protocol.version()), 1];
    let started = session.now();
    let mut made = 0;
    // The attempt cap and the deadline both bound the loop; one attempt is always made.
    while made < config.ext_info_attempts
        && (made == 0 || session.now().saturating_duration_since(started) < config.ext_info_timeout)
    {
        made += 1;
        match session
            .transaction(op::SDIO_GET_EXT_DEVICE_INFO, &params, None)
            .await
        {
            Ok(outcome) if !outcome.data.is_empty() => {
                return ExtDeviceInfo::decode(&outcome.data).map_err(SonyError::from);
            }
            Ok(_)
            | Err(SessionError::Response {
                code: code::DEVICE_BUSY,
                ..
            }) => sleep(config.poll_interval).await,
            Err(err) => return Err(err.into()),
        }
    }
    Err(SonyError::ExtInfoUnavailable { attempts: made })
}

fn classify(raw: Event) -> SonyEvent {
    match (raw.code, raw.params.first().copied()) {
        (event::PROPERTY_CHANGED, Some(code)) => {
            SonyEvent::PropertyChanged(DevicePropCode(u16::try_from(code).unwrap_or(u16::MAX)))
        }
        (event::OBJECT_ADDED, first) => SonyEvent::ObjectAdded {
            handle: first.unwrap_or(handle::CAPTURED_IMAGE),
        },
        _ => SonyEvent::Other(raw),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(code: u16, params: &[u32]) -> Event {
        Event {
            code,
            transaction_id: 0xFFFF_FFFF,
            params: params.to_vec(),
        }
    }

    #[test]
    fn classifies_sony_events() {
        assert_eq!(
            classify(event(event::PROPERTY_CHANGED, &[0xD221])),
            SonyEvent::PropertyChanged(prop::LIVE_VIEW_STATUS)
        );
        assert_eq!(
            classify(event(event::PROPERTY_CHANGED, &[0x1_0000])),
            SonyEvent::PropertyChanged(DevicePropCode(u16::MAX))
        );
        assert_eq!(
            classify(event(event::OBJECT_ADDED, &[7, 8])),
            SonyEvent::ObjectAdded { handle: 7 }
        );
        assert_eq!(
            classify(event(event::OBJECT_ADDED, &[])),
            SonyEvent::ObjectAdded {
                handle: handle::CAPTURED_IMAGE
            }
        );
    }

    #[test]
    fn everything_else_passes_through() {
        let property_changed_without_a_code = event(event::PROPERTY_CHANGED, &[]);
        assert_eq!(
            classify(property_changed_without_a_code.clone()),
            SonyEvent::Other(property_changed_without_a_code)
        );
        let other = event(0xC206, &[1]);
        assert_eq!(classify(other.clone()), SonyEvent::Other(other));
    }

    #[test]
    fn defaults_give_live_view_ten_seconds() {
        let config = SonyConfig::default();
        assert_eq!(config.protocol, Protocol::V300);
        assert_eq!(config.live_view_ready_timeout, Duration::from_secs(10));
        assert!(config.event_poll_timeout < Duration::from_secs(1));
    }
}
