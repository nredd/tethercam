//! Native USB factory over the existing generic bus API; no protocol logic is duplicated.
//!
//! References:
//! - `docs/usb.md`
//! - `docs/architecture.md`, camera actor contract

use std::io::Write;
use std::time::Duration;

use super::Error;
use crate::supervisor::{CameraFactory, OpenError};
use crate::transport::usb::{
    Bus, CameraInfo, RawDevice, UsbError, UsbTransport, camera, list_cameras, open,
};

#[cfg(test)]
#[path = "native_tests.rs"]
mod tests;

/// The only error material allowed out of a bus implementation is a fixed category.
fn sanitize(error: &UsbError) -> OpenError {
    match error {
        UsbError::PermissionDenied => OpenError::Permission,
        UsbError::WrongMode(_) => OpenError::Mode,
        UsbError::Busy => OpenError::Busy,
        UsbError::NoCamera | UsbError::Disconnected => OpenError::Disconnected,
        UsbError::Descriptor(_) | UsbError::Other(_) => OpenError::Discovery,
    }
}

fn select<'a>(cameras: &'a [CameraInfo], serial: Option<&str>) -> Result<&'a CameraInfo, UsbError> {
    match serial {
        Some(serial) => cameras
            .iter()
            .find(|camera| camera.serial.as_deref() == Some(serial))
            .ok_or(UsbError::NoCamera),
        None => camera::pick(cameras),
    }
}

/// Report candidate count and modes without printing USB serials or OS bus paths.
pub(super) fn report<B: Bus>(bus: &B, out: &mut dyn Write) -> Result<(), Error> {
    let cameras = list_cameras(bus).map_err(|error| sanitize(&error))?;
    writeln!(out, "Detected cameras: {}", cameras.len())?;
    for (index, camera) in cameras.iter().enumerate() {
        writeln!(
            out,
            "Camera {}: USB mode {}",
            index.saturating_add(1),
            camera.mode
        )?;
    }
    Ok(())
}

/// The raw-USB listing: counts and modes as `probe` reports them, plus where selection lives.
/// Serials stay unprinted, so the operator reads them from the camera or the OS.
pub(super) fn list<B: Bus>(bus: &B, out: &mut dyn Write) -> Result<(), Error> {
    report(bus, out)?;
    writeln!(
        out,
        "Raw USB selects by `camera_serial` (the camera's USB serial number), never printed here; --describe is for camera_backend = \"icc\"."
    )?;
    Ok(())
}

pub(super) struct Native<B> {
    bus: B,
    serial: Option<String>,
}

impl<B> Native<B> {
    /// Retain the selection criteria; construction never enumerates or opens a device.
    pub(super) fn new(bus: B, serial: Option<String>) -> Self {
        Self { bus, serial }
    }
}

impl<B: Bus + Send> CameraFactory for Native<B> {
    type Camera = UsbTransport<<B::Device as RawDevice>::Port>;
    type Session = crate::ptp::session::Session<Self::Camera>;

    fn session(
        &self, camera: Self::Camera, mut cfg: crate::ptp::session::SessionConfig,
        clock: fn() -> std::time::Instant,
    ) -> Self::Session {
        cfg.max_packet_size = camera.bulk_max_packet_size();
        Self::Session::new(camera, cfg).with_clock(clock)
    }
    fn recover(&self, session: Self::Session) -> Self::Camera {
        session.into_transport()
    }

    async fn connect(&mut self) -> Result<Option<Self::Camera>, OpenError> {
        let cameras = list_cameras(&self.bus).map_err(|error| sanitize(&error))?;
        let camera = match select(&cameras, self.serial.as_deref()) {
            Ok(camera) => camera,
            Err(UsbError::NoCamera) => return Ok(None),
            Err(error) => return Err(sanitize(&error)),
        };
        open(&self.bus, camera)
            .await
            .map(Some)
            .map_err(|error| sanitize(&error))
    }

    fn release(&mut self, camera: Self::Camera) {
        drop(camera);
    }
    fn pause(&self, delay: Duration) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(delay)
    }
}
