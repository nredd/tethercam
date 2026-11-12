//! Opt-in, unprivileged verification infrastructure, not the camera runtime.
//!
//! The loopback USB/IP engine accepts arbitrary [`DeviceModel`] implementations. Sony/SICDD
//! and a synthetic vendor-class echo device are supplied as independent personalities.
//! Kernel provisioning is a separate explicit hosted-CI utility operation.
//!
//! References:
//! - <https://docs.kernel.org/usb/usbip_protocol.html>
//! - <https://www.usb.org/document-library/usb-20-specification>

pub mod diagnostics;
mod model;
mod model_io;
pub mod server;
pub mod wire;

pub use model::{DeviceModel, Sony, Synthetic};
pub use server::Server;

use std::io;

/// Failures from the verification engine, with protocol and identity errors kept distinct.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Malformed or unsupported USB/IP request or response data.
    #[error("invalid or unsupported USB/IP request")]
    Protocol,
    /// Inconsistent or unbounded device descriptors.
    #[error("invalid verification device identity")]
    Identity,
    /// Underlying socket or operating-system failure.
    #[error("verification I/O failed")]
    Io(#[from] io::Error),
    /// A model-specific transport operation failed.
    #[error("verification model transfer failed")]
    Transport(#[from] crate::transport::TransportError),
    /// A bounded protocol operation exceeded its deadline.
    #[error("verification operation timed out")]
    Deadline(#[from] tokio::time::error::Elapsed),
    /// An owned server task failed.
    #[error("verification server task failed")]
    Task(#[from] tokio::task::JoinError),
}

impl Error {
    /// Payload-free category suitable for bounded observations and caller retry decisions.
    #[must_use]
    pub fn kind(&self) -> io::ErrorKind {
        match self {
            Self::Protocol => io::ErrorKind::InvalidData,
            Self::Identity => io::ErrorKind::InvalidInput,
            Self::Io(error) => error.kind(),
            Self::Deadline(_) => io::ErrorKind::TimedOut,
            Self::Transport(_) | Self::Task(_) => io::ErrorKind::Other,
        }
    }
}

/// Result of a model-specific control request.
#[derive(Debug)]
pub struct Control {
    /// Data returned by the device, or `None` to stall the request.
    pub data: Option<Vec<u8>>,
    /// Cancel pending bulk transfers after a class reset/cancellation.
    pub flush_bulk: bool,
}

/// Device data required by USB enumeration, independent of the transfer implementation.
#[derive(Debug, Clone)]
pub struct Identity {
    /// Vendor identifier. Synthetic fixtures must never impersonate an attached physical device.
    pub vendor: u16,
    /// Product identifier.
    pub product: u16,
    /// Device descriptor.
    pub device: Vec<u8>,
    /// Configuration descriptor (including interfaces and endpoints).
    pub configuration: Vec<u8>,
    /// Manufacturer, product and unique synthetic serial strings.
    pub strings: [String; 3],
    /// USB/IP interface class, subclass, protocol and padding.
    pub interface: [u8; 4],
    /// Endpoint numbers whose pending work is flushed on class cancellation.
    pub bulk_endpoints: Vec<u32>,
    /// Supported alternate settings of the single interface in configuration one.
    pub alternates: Vec<u8>,
}

impl Identity {
    /// Encode a supported device/configuration/string descriptor.
    #[must_use]
    pub fn descriptor(&self, kind: u8, index: u8) -> Option<Vec<u8>> {
        match (kind, index) {
            (1, 0) => Some(self.device.clone()),
            (2, 0) => Some(self.configuration.clone()),
            (3, 0) => Some(vec![4, 3, 9, 4]),
            (3, 1..=3) => {
                let [manufacturer, product, serial] = &self.strings;
                let text = match index {
                    1 => manufacturer,
                    2 => product,
                    _ => serial,
                };
                let mut bytes = vec![0, 3];
                for value in text.encode_utf16() {
                    bytes.extend_from_slice(&value.to_le_bytes());
                }
                let length = u8::try_from(bytes.len()).ok()?;
                bytes.splice(..1, [length]);
                Some(bytes)
            }
            _ => None,
        }
    }

    /// Reject malformed or unbounded descriptors before opening a listening socket.
    ///
    /// # Errors
    /// Returns an error for inconsistent identity fields or oversized descriptors.
    pub fn validate(&self) -> Result<(), Error> {
        if self.device.len() != 18
            || self.device.get(..2) != Some(&[18, 1])
            || self.device.get(8..10) != Some(self.vendor.to_le_bytes().as_slice())
            || self.device.get(10..12) != Some(self.product.to_le_bytes().as_slice())
            || self.configuration.len() > 4096
            || self.configuration.get(..2) != Some(&[9, 2])
            || self.configuration.get(2..4)
                != u16::try_from(self.configuration.len())
                    .ok()
                    .map(u16::to_le_bytes)
                    .as_ref()
                    .map(<[u8; 2]>::as_slice)
            || self.strings.iter().any(|text| {
                text.is_empty()
                    || text.encode_utf16().count() > 126
                    || text.chars().any(char::is_control)
            })
            || self.alternates.is_empty()
            || !self.alternates.contains(&0)
            || self.bulk_endpoints.is_empty()
            || self
                .bulk_endpoints
                .iter()
                .any(|endpoint| !(1..=15).contains(endpoint))
        {
            return Err(Error::Identity);
        }
        Ok(())
    }
}
