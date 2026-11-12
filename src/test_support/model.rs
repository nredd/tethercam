//! Independent Sony/SICDD and synthetic vendor-class device personalities.
//!
//! References:
//! - <https://www.usb.org/document-library/still-image-capture-device-definition-10-and-errata-16-mar-2007>

use std::future::Future;
use std::io;
use std::time::Duration;

use crate::transport::sim::SimCamera;
use crate::transport::sim::sony::{SonySimConfig, sony_camera};
use crate::transport::{Endpoint, Transport, TransportError};

use super::{Control, Identity, wire};

const LIMIT: Duration = Duration::from_secs(5);

/// An independent USB device personality. The engine owns framing, budgets and deadlines.
///
/// Methods must return promptly; pending transfers return [`TransportError::Timeout`] and
/// are retried on notification. Implementations must bound their retained state by the
/// engine's non-refundable admission budget, not allocate indefinitely on reads.
pub trait DeviceModel: Send + 'static {
    /// Current enumeration descriptors. Evaluated at each enumeration/control request.
    fn identity(&self) -> Identity;
    /// Execute a transfer on a model-defined endpoint number.
    fn transfer(
        &mut self, endpoint: u32, direction: u32, payload: &[u8], bytes: &mut [u8],
    ) -> impl Future<Output = Result<usize, TransportError>> + Send;
    /// Execute class/vendor control or endpoint halt clearing; unknown requests stall.
    fn control(
        &mut self, setup: [u8; 8], payload: &[u8],
    ) -> impl Future<Output = io::Result<Control>> + Send;
}

/// Sony/SICDD fixture using the production simulator and its transaction cancellation.
pub struct Sony {
    /// Stateful simulated body, accessible for explicit fault injection.
    pub camera: SimCamera,
    /// Product code, including the wrong-mode fixture used by native-adapter tests.
    pub product: u16,
    /// Inject a class-reset stall.
    pub class_stall: bool,
    /// SICDD status response.
    pub status: u16,
    busy_status: bool,
}

impl Default for Sony {
    fn default() -> Self {
        Self {
            camera: sony_camera(SonySimConfig::default()).with_max_payload(wire::MAX_TRANSFER),
            product: 0x0c34,
            class_stall: false,
            status: 0x2001,
            busy_status: false,
        }
    }
}

impl DeviceModel for Sony {
    fn identity(&self) -> Identity {
        let mut device = wire::DEVICE;
        for (slot, byte) in device.iter_mut().skip(10).zip(self.product.to_le_bytes()) {
            *slot = byte;
        }
        Identity {
            vendor: 0x054c,
            product: self.product,
            device: device.to_vec(),
            configuration: wire::CONFIG.to_vec(),
            strings: [
                "Synthetic".into(),
                "USBIP simulated Sony".into(),
                "TETHERCAM-USBIP-0001".into(),
            ],
            interface: [6, 1, 1, 0],
            bulk_endpoints: vec![1, 2],
            alternates: vec![0, 1],
        }
    }

    async fn transfer(
        &mut self, endpoint: u32, direction: u32, payload: &[u8], bytes: &mut [u8],
    ) -> Result<usize, TransportError> {
        match (endpoint, direction) {
            (2, 0) => self
                .camera
                .bulk_out(payload, LIMIT)
                .await
                .map(|()| payload.len()),
            (1, 1) => self.camera.bulk_in(bytes, LIMIT).await,
            (3, 1) => self.camera.interrupt_in(bytes, LIMIT).await,
            _ => Err(TransportError::Stall),
        }
    }

    async fn control(&mut self, setup: [u8; 8], payload: &[u8]) -> io::Result<Control> {
        let [
            kind,
            request,
            low,
            high,
            index_low,
            index_high,
            length_low,
            length_high,
        ] = setup;
        let index = u16::from_le_bytes([index_low, index_high]);
        let length = u16::from_le_bytes([length_low, length_high]);
        let mut flush_bulk = false;
        let data = match (kind, request) {
            (0x21, 0x64) if index == 0 && low == 0 && high == 0 => match payload {
                [1, 0x40, a, b, c, d] => {
                    if self
                        .camera
                        .cancel_transaction(u32::from_le_bytes([*a, *b, *c, *d]))
                    {
                        flush_bulk = true;
                        self.busy_status = true;
                        self.status = 0x2019;
                    }
                    Some(Vec::new())
                }
                _ => None,
            },
            (0x02, 1) if low == 0 && high == 0 && index_high == 0 && length == 0 => {
                let endpoint = match index_low {
                    0x81 => Some(Endpoint::BulkIn),
                    2 => Some(Endpoint::BulkOut),
                    0x83 => Some(Endpoint::InterruptIn),
                    _ => None,
                };
                if let Some(endpoint) = endpoint {
                    self.camera
                        .clear_halt(endpoint, LIMIT)
                        .await
                        .map_err(io::Error::other)?;
                    Some(Vec::new())
                } else {
                    None
                }
            }
            (0x21, 0x66) if !self.class_stall && index == 0 && length == 0 => {
                self.camera.reset(LIMIT).await.map_err(io::Error::other)?;
                self.status = 0x2001;
                self.busy_status = false;
                flush_bulk = true;
                Some(Vec::new())
            }
            (0xa1, 0x67) if index == 0 => {
                let [low, high] = self.status.to_le_bytes();
                if std::mem::take(&mut self.busy_status) {
                    self.status = 0x2001;
                }
                Some(vec![4, 0, low, high])
            }
            _ => None,
        };
        Ok(Control { data, flush_bulk })
    }
}

/// Non-Sony, non-PTP vendor-class echo fixture with entirely different endpoints.
#[derive(Default)]
pub struct Synthetic {
    pending: Vec<u8>,
}

#[cfg(test)]
mod tests;

impl DeviceModel for Synthetic {
    fn identity(&self) -> Identity {
        Identity {
            vendor: 0xffff,
            product: 1,
            device: vec![18, 1, 0, 2, 0, 0, 0, 64, 255, 255, 1, 0, 0, 1, 1, 2, 3, 1],
            configuration: vec![
                9, 2, 32, 0, 1, 1, 0, 0x80, 50, 9, 4, 0, 0, 2, 255, 0, 0, 0, 7, 5, 4, 2, 0, 2, 0,
                7, 5, 0x85, 2, 0, 2, 0,
            ],
            strings: [
                "Synthetic".into(),
                "Generic echo".into(),
                "TETHERCAM-ECHO-0001".into(),
            ],
            interface: [255, 0, 0, 0],
            bulk_endpoints: vec![4, 5],
            alternates: vec![0],
        }
    }

    fn transfer(
        &mut self, endpoint: u32, direction: u32, payload: &[u8], bytes: &mut [u8],
    ) -> impl Future<Output = Result<usize, TransportError>> + Send {
        std::future::ready(match (endpoint, direction) {
            (4, 0) => {
                self.pending = payload.to_vec();
                Ok(payload.len())
            }
            (5, 1) if !self.pending.is_empty() => {
                let length = bytes.len().min(self.pending.len());
                for (slot, byte) in bytes.iter_mut().zip(self.pending.drain(..length)) {
                    *slot = byte;
                }
                Ok(length)
            }
            (5, 1) => Err(TransportError::Timeout),
            _ => Err(TransportError::Stall),
        })
    }

    fn control(
        &mut self, _setup: [u8; 8], _payload: &[u8],
    ) -> impl Future<Output = io::Result<Control>> + Send {
        std::future::ready(Ok(Control {
            data: None,
            flush_bulk: false,
        }))
    }
}
