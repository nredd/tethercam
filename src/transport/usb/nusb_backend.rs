//! The real [`Bus`] and [`Port`] on `nusb`.
//!
//! Every function here is a direct delegation to `nusb` over the pure mappings in
//! `map`, and needs a real USB device to run. Decisions live elsewhere and are
//! tested with fakes. Hosted Linux CI exercises the actual adapter through the kernel's VHCI
//! controller and the loopback-only USB/IP simulator in `tests/usb_emulated.rs`.
//!
//! References:
//! - nusb 0.2 API: <https://docs.rs/nusb/0.2>
//! - USB Still Image Capture Device Definition 1.0, sections 5-7

use std::pin::Pin;
use std::time::Duration;

use futures_core::Stream;
use nusb::hotplug::{HotplugEvent, HotplugWatch};
use nusb::transfer::{
    Buffer, Bulk, ControlIn, ControlOut, ControlType, In, Interrupt, Out, Recipient,
};
use nusb::{Device, DeviceId, DeviceInfo, Interface, MaybeFuture};

use super::UsbError;
use super::bus::{Bus, RawDevice};
use super::camera::{DeviceAddress, DeviceSummary};
use super::descriptors::Endpoints;
use super::hotplug::{EventSource, RawEvent, Seeded};
use super::port::{Port, PortError, UsbTransport};
use super::{factory, map};
use crate::transport::Endpoint;

/// A [`UsbTransport`] over `nusb`.
pub type NusbTransport = UsbTransport<NusbPort>;

/// The system USB bus.
#[derive(Debug, Clone, Copy, Default)]
pub struct NusbBus;

fn usb_error(error: nusb::Error) -> UsbError {
    let kind = error.kind();
    map::usb_error(kind, std::io::Error::from(error).to_string())
}

fn port_error(error: nusb::Error) -> PortError {
    let kind = error.kind();
    map::port_error_from_kind(kind, std::io::Error::from(error).to_string())
}

fn configuration_error(error: nusb::ActiveConfigurationError) -> UsbError {
    UsbError::Other(error.to_string())
}

fn address_of(info: &DeviceInfo) -> DeviceAddress {
    DeviceAddress {
        bus: info.bus_id().to_owned(),
        address: info.device_address(),
    }
}

fn summarize(info: &DeviceInfo) -> DeviceSummary {
    DeviceSummary {
        address: address_of(info),
        vendor_id: info.vendor_id(),
        product_id: info.product_id(),
        product: info.product_string().map(str::to_owned),
        serial: info.serial_number().map(str::to_owned),
        speed: map::speed(info.speed()),
        interfaces: info
            .interfaces()
            .map(|i| (i.class(), i.subclass(), i.protocol()))
            .collect(),
    }
}

impl Bus for NusbBus {
    type Device = NusbDevice;
    type Events = Seeded<NusbEvents>;

    fn devices(&self) -> Result<Vec<DeviceSummary>, UsbError> {
        nusb::list_devices()
            .wait()
            .map_err(usb_error)
            .map(|devices| devices.map(|info| summarize(&info)).collect())
    }

    async fn open(&self, address: &DeviceAddress) -> Result<NusbDevice, UsbError> {
        let devices = nusb::list_devices()
            .await
            .map_err(usb_error)
            .map(Iterator::collect);
        factory::open(devices, address, address_of, open_device).await
    }

    fn watch(&self) -> Result<Seeded<NusbEvents>, UsbError> {
        // Watch first, then list: a device attached in between shows up twice, which the
        // tracker de-duplicates, rather than not at all.
        nusb::watch_devices().map_err(usb_error).and_then(|watch| {
            nusb::list_devices()
                .wait()
                .map_err(usb_error)
                .map(|present| {
                    let backlog =
                        present.map(|info| RawEvent::Connected(info.id(), summarize(&info)));
                    Seeded::new(NusbEvents(Box::pin(watch)), backlog)
                })
        })
    }
}

async fn open_device(info: DeviceInfo) -> Result<NusbDevice, UsbError> {
    info.open().await.map_err(usb_error).map(NusbDevice)
}

/// An opened `nusb` device.
pub struct NusbDevice(Device);

impl RawDevice for NusbDevice {
    type Port = NusbPort;

    fn configuration(&self) -> Result<Vec<u8>, UsbError> {
        let configuration = self.0.active_configuration().map_err(configuration_error)?;
        Ok(configuration.as_bytes().to_vec())
    }

    async fn claim(self, endpoints: &Endpoints) -> Result<NusbPort, UsbError> {
        let fail = usb_error;
        let interface = self
            .0
            .detach_and_claim_interface(endpoints.interface)
            .await
            .map_err(fail)?;
        if endpoints.alternate != 0 {
            interface
                .set_alt_setting(endpoints.alternate)
                .await
                .map_err(fail)?;
        }
        Ok(NusbPort {
            bulk_in: interface
                .endpoint::<Bulk, In>(endpoints.bulk_in)
                .map_err(fail)?,
            bulk_out: interface
                .endpoint::<Bulk, Out>(endpoints.bulk_out)
                .map_err(fail)?,
            interrupt_in: interface
                .endpoint::<Interrupt, In>(endpoints.interrupt_in)
                .map_err(fail)?,
            number: endpoints.interface,
            interface,
            device: self.0,
        })
    }
}

/// A claimed still-image interface with its three endpoints open.
pub struct NusbPort {
    device: Device,
    interface: Interface,
    number: u8,
    bulk_in: nusb::Endpoint<Bulk, In>,
    bulk_out: nusb::Endpoint<Bulk, Out>,
    interrupt_in: nusb::Endpoint<Interrupt, In>,
}

async fn read<T: nusb::transfer::BulkOrInterrupt>(
    endpoint: &mut nusb::Endpoint<T, In>, bytes: &mut [u8],
) -> Result<usize, PortError> {
    // A caller that dropped an earlier call left its transfer queued; discard it first.
    cancel_all(endpoint).await;
    endpoint.submit(Buffer::new(bytes.len()));
    let done = endpoint.next_complete().await;
    done.status.map_err(map::transfer_error)?;
    Ok(map::copy_received(bytes, &done.buffer))
}

async fn cancel_all<T: nusb::transfer::BulkOrInterrupt, D: nusb::transfer::EndpointDirection>(
    endpoint: &mut nusb::Endpoint<T, D>,
) {
    endpoint.cancel_all();
    while endpoint.pending() > 0 {
        endpoint.next_complete().await;
    }
}

impl Port for NusbPort {
    fn max_packet_size(&self, endpoint: Endpoint) -> usize {
        match endpoint {
            Endpoint::BulkOut => self.bulk_out.max_packet_size(),
            Endpoint::BulkIn => self.bulk_in.max_packet_size(),
            Endpoint::InterruptIn => self.interrupt_in.max_packet_size(),
        }
    }

    async fn bulk_out(&mut self, bytes: &[u8]) -> Result<usize, PortError> {
        cancel_all(&mut self.bulk_out).await;
        let mut buffer = Buffer::new(bytes.len());
        buffer.extend_from_slice(bytes);
        self.bulk_out.submit(buffer);
        let done = self.bulk_out.next_complete().await;
        done.status.map_err(map::transfer_error)?;
        Ok(done.actual_len)
    }

    async fn bulk_in(&mut self, bytes: &mut [u8]) -> Result<usize, PortError> {
        read(&mut self.bulk_in, bytes).await
    }

    async fn interrupt_in(&mut self, bytes: &mut [u8]) -> Result<usize, PortError> {
        read(&mut self.interrupt_in, bytes).await
    }

    async fn cancel(&mut self, endpoint: Endpoint) {
        match endpoint {
            Endpoint::BulkOut => cancel_all(&mut self.bulk_out).await,
            Endpoint::BulkIn => cancel_all(&mut self.bulk_in).await,
            Endpoint::InterruptIn => cancel_all(&mut self.interrupt_in).await,
        }
    }

    async fn clear_halt(&mut self, endpoint: Endpoint) -> Result<(), PortError> {
        // `nusb` requires nothing pending; a dropped caller may have left a transfer queued.
        self.cancel(endpoint).await;
        let result = match endpoint {
            Endpoint::BulkOut => self.bulk_out.clear_halt().await,
            Endpoint::BulkIn => self.bulk_in.clear_halt().await,
            Endpoint::InterruptIn => self.interrupt_in.clear_halt().await,
        };
        result.map_err(port_error)
    }

    async fn class_out(&mut self, request: u8, timeout: Duration) -> Result<(), PortError> {
        let control = ControlOut {
            control_type: ControlType::Class,
            recipient: Recipient::Interface,
            request,
            value: 0,
            index: self.number.into(),
            data: &[],
        };
        self.interface
            .control_out(control, timeout)
            .await
            .map_err(map::transfer_error)
    }

    async fn class_in(
        &mut self, request: u8, length: u16, timeout: Duration,
    ) -> Result<Vec<u8>, PortError> {
        let control = ControlIn {
            control_type: ControlType::Class,
            recipient: Recipient::Interface,
            request,
            value: 0,
            index: self.number.into(),
            length,
        };
        self.interface
            .control_in(control, timeout)
            .await
            .map_err(map::transfer_error)
    }

    async fn reset_device(&mut self) -> Result<(), PortError> {
        self.device.reset().await.map_err(port_error)
    }
}

/// `nusb` hot-plug events.
pub struct NusbEvents(Pin<Box<HotplugWatch>>);

impl EventSource for NusbEvents {
    type Id = DeviceId;

    async fn next_event(&mut self) -> Option<RawEvent<DeviceId>> {
        std::future::poll_fn(|cx| self.0.as_mut().poll_next(cx))
            .await
            .map(|event| match event {
                HotplugEvent::Connected(info) => RawEvent::Connected(info.id(), summarize(&info)),
                HotplugEvent::Disconnected(id) => RawEvent::Disconnected(id),
            })
    }
}
