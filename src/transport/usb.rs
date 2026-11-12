//! USB transport for a PTP camera, on `nusb` (pure Rust, no libusb).
//!
//! Layering, so that everything except the thinnest calls into `nusb` is tested without
//! hardware:
//! - [`descriptors`]: pure parsing of a configuration descriptor and endpoint selection
//! - [`camera`]: recognising Sony cameras and their USB connection mode
//! - [`status`]: the class-request "Get Device Status" reply
//! - [`hotplug`]: camera arrival and removal from raw bus events
//! - [`port`]: [`Port`], the raw device operations, and [`UsbTransport`], which implements
//!   [`Transport`](super::Transport) over any [`Port`]: timeouts, stall and error mapping,
//!   packet alignment and recovery
//! - [`bus`]: [`Bus`] and [`RawDevice`], enumeration and opening, plus [`open_first`]
//! - [`nusb_backend`]: the real [`Bus`] and [`Port`]; one-line delegations to `nusb` over the
//!   pure mappings in [`map`]
//!
//! Setup: see `docs/usb.md` for the Linux udev rule and macOS notes.
//!
//! References:
//! - USB Still Image Capture Device Definition 1.0, sections 5-7

pub mod bus;
pub mod camera;
pub mod descriptors;
mod factory;
pub mod hotplug;
pub mod map;
pub mod nusb_backend;
pub mod port;
pub mod status;

use thiserror::Error;

pub use bus::{Bus, RawDevice, list_cameras, open, open_first, watch};
pub use camera::{BusSpeed, CameraInfo, DeviceAddress, DeviceSummary, Mode};
pub use descriptors::DescriptorError;
pub use hotplug::{CameraEvent, Watcher};
pub use nusb_backend::{NusbBus, NusbTransport};
pub use port::{Port, PortError, UsbTransport};

/// An error opening or enumerating USB cameras.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UsbError {
    /// No Sony camera is on the bus.
    #[error("no Sony camera found")]
    NoCamera,
    /// The camera is in a mode `tethercam` cannot use.
    #[error("the camera is in {0} mode; set USB Connection to PC Remote")]
    WrongMode(Mode),
    /// The configuration descriptor is unusable.
    #[error("unusable USB descriptors: {0}")]
    Descriptor(#[from] DescriptorError),
    /// The user may not open the device (Linux: install the udev rule from `docs/usb.md`).
    #[error("permission denied opening the camera; see docs/usb.md")]
    PermissionDenied,
    /// Another program or driver holds the camera.
    #[error("the camera is in use by another program")]
    Busy,
    /// The camera went away.
    #[error("camera disconnected")]
    Disconnected,
    /// Any other operating-system error.
    #[error("USB error: {0}")]
    Other(String),
}
