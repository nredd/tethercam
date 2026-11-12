//! Sony SDIO PTP extension: handshake, properties, live view and still capture.
//!
//! [`SonyCamera::run`] opens a PTP session on a [`Session`](crate::ptp::session::Session), runs
//! the Sony connect handshake, hands the connected camera to a closure and closes the session on
//! every exit path. A session abandoned without `CloseSession` wedges the camera until it is
//! power-cycled, so there is deliberately no way to hold a connected camera past the closure.
//!
//! What the driver does:
//! - Handshake: `GetDeviceInfo`, `SDIOConnect` phases 1 and 2, `SDIOGetExtDeviceInfo`
//!   (requesting the newest protocol generation and accepting the camera's answer, so a
//!   pre-2020 body negotiates down to [`Protocol::V200`]), `SDIOConnect` phase 3, the first
//!   property table, and priority mode when the camera advertises it
//! - Properties: [`PropertyTable`] from `SDIOGetAllExtDevicePropInfo`, refreshed on the
//!   property-changed event ([`SonyCamera::pump_events`]); [`SonyCamera::set_property`] and
//!   [`SonyCamera::control`]
//! - Live view: [`SonyCamera::wait_live_view_ready`], then
//!   [`SonyCamera::next_live_view_frame`] hands out JPEG frames with Sony's header stripped
//! - Stills: [`SonyCamera::capture_still`] half-presses, full-presses, checks once whether the
//!   image appeared, releases both buttons, waits for it if not, and downloads it from handle
//!   `0xFFFFC001`
//! - Capabilities come from the advertised operations, properties and controls
//!   ([`Capabilities`]); the only per-model data is [`Quirks`]
//!
//! Where the references disagree:
//! - Property reads: `libgphoto2` sends `SDIOGetAllExtDevicePropInfo` with no parameters on every
//!   generation; `alphawire` sends `(incremental, extended)` on 3.00 and nothing on 2.00. This
//!   driver follows `alphawire`, sending `(0, 1)` on 3.00, because the extra parameters exist
//!   only in the 3.00 specification and 2.00 (the measured generation) is identical in both
//! - Priority mode (`0xD25A`): `libgphoto2` always writes `1` after the handshake; `alphawire`
//!   never does. This driver writes it only when the camera lists the property as writable
//! - Live-view readiness: `libgphoto2` retries the fetch on `InvalidObjectHandle` and
//!   `AccessDenied`; `alphawire` waits for the live-view status property. This driver does
//!   both: it waits for the property, and still retries the fetch because the first frames can
//!   fail after the property reports ready
//!
//! References:
//! - libgphoto2 `camlibs/ptp2/ptp.h`, `ptp.c`, `library.c` (Sony sections)
//! - `watsonmw/alphawire` `src/aw/aw-control.c`, `aw-const.h`
//! - `docs/architecture.md`

mod battery;
mod camera;
mod capability;
mod capture;
pub mod codes;
mod error;
mod live_view;
mod property;
mod protocol;
mod quirks;

pub use battery::{Battery, PowerSource};
pub use camera::{SonyCamera, SonyConfig, SonyEvent};
pub use capability::{Capabilities, Capability};
pub use capture::CapturedImage;
pub use codes::{event, handle, op, prop};
pub use error::{LiveViewError, SonyError};
pub use live_view::extract_jpeg;
pub use property::{PropertyTable, SonyForm, SonyProperty, decode_properties};
pub use protocol::{ExtDeviceInfo, Protocol};
pub use quirks::Quirks;

#[cfg(test)]
mod tests;
