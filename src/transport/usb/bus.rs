//! Enumerating and opening cameras over any [`Bus`].
//!
//! References:
//! - USB Still Image Capture Device Definition 1.0, section 5
//! - nusb 0.2 API: <https://docs.rs/nusb/0.2>

use super::UsbError;
use super::camera::{CameraInfo, DeviceAddress, DeviceSummary, classify, pick};
use super::descriptors::{Endpoints, select};
use super::hotplug::{EventSource, Watcher};
use super::port::{Port, UsbTransport};

/// A USB bus: enumeration, opening and hot-plug. `nusb` implements it; tests fake it.
pub trait Bus: Sync {
    /// An opened, not yet claimed device.
    type Device: RawDevice;
    /// Raw hot-plug events.
    type Events: EventSource;

    /// Every device currently on the bus.
    ///
    /// # Errors
    ///
    /// Returns a [`UsbError`] if enumeration fails.
    fn devices(&self) -> Result<Vec<DeviceSummary>, UsbError>;

    /// Open the device at `address`.
    fn open(
        &self, address: &DeviceAddress,
    ) -> impl Future<Output = Result<Self::Device, UsbError>> + Send;

    /// Start watching for hot-plug events.
    ///
    /// # Errors
    ///
    /// Returns a [`UsbError`] if the platform cannot watch.
    fn watch(&self) -> Result<Self::Events, UsbError>;
}

/// An opened device whose interface has not been claimed yet.
pub trait RawDevice: Send {
    /// The claimed-interface handle.
    type Port: Port;

    /// Raw bytes of the active configuration descriptor.
    ///
    /// # Errors
    ///
    /// Returns a [`UsbError`] if the device has no active configuration.
    fn configuration(&self) -> Result<Vec<u8>, UsbError>;

    /// Claim the interface and open its endpoints.
    fn claim(
        self, endpoints: &Endpoints,
    ) -> impl Future<Output = Result<Self::Port, UsbError>> + Send;
}

/// List every Sony camera on the bus, whatever its USB connection mode.
///
/// # Errors
///
/// Returns a [`UsbError`] if enumeration fails.
pub fn list_cameras<B: Bus>(bus: &B) -> Result<Vec<CameraInfo>, UsbError> {
    Ok(bus.devices()?.iter().filter_map(classify).collect())
}

/// Open `camera` as a PTP transport.
///
/// # Errors
///
/// [`UsbError::WrongMode`] for a camera in MTP or Mass Storage mode, [`UsbError::Descriptor`]
/// when it has no usable still-image interface, and any error from the bus.
pub async fn open<B: Bus>(
    bus: &B, camera: &CameraInfo,
) -> Result<UsbTransport<<B::Device as RawDevice>::Port>, UsbError> {
    camera.check_mode()?;
    let device = bus.open(&camera.address).await?;
    let endpoints = select(&device.configuration()?)?;
    Ok(UsbTransport::new(device.claim(&endpoints).await?))
}

/// Open the first usable camera on the bus.
///
/// # Errors
///
/// [`UsbError::NoCamera`] when none is attached, the mode error of the first camera when none
/// is usable, and any error from [`open`].
pub async fn open_first<B: Bus>(
    bus: &B,
) -> Result<UsbTransport<<B::Device as RawDevice>::Port>, UsbError> {
    let cameras = list_cameras(bus)?;
    open(bus, pick(&cameras)?).await
}

/// Watch for cameras being plugged in and removed.
///
/// # Errors
///
/// Returns a [`UsbError`] if the platform cannot watch.
pub fn watch<B: Bus>(bus: &B) -> Result<Watcher<B::Events>, UsbError> {
    Ok(Watcher::new(bus.watch()?))
}

#[cfg(test)]
pub(crate) mod fakes {
    use std::collections::VecDeque;

    use super::super::camera::STILL_IMAGE;
    use super::super::camera::fixtures::summary;
    use super::super::hotplug::RawEvent;
    use super::super::port::fakes::FakePort;
    use super::*;

    /// Configuration descriptor with one complete still-image interface.
    pub(crate) const CONFIG: &[u8] = &[
        9, 4, 2, 0, 3, 6, 1, 1, 0, 7, 5, 0x81, 2, 0, 2, 0, 7, 5, 2, 2, 0, 2, 0, 7, 5, 0x83, 3, 64,
        0, 1,
    ];

    /// A scripted bus.
    pub(crate) struct FakeBus {
        pub(crate) devices: Result<Vec<DeviceSummary>, UsbError>,
        pub(crate) open_error: Option<UsbError>,
        pub(crate) config: Result<Vec<u8>, UsbError>,
        pub(crate) claim_error: Option<UsbError>,
        pub(crate) events: Result<Vec<RawEvent<u8>>, UsbError>,
    }

    impl FakeBus {
        pub(crate) fn with_camera(product_id: u16) -> Self {
            Self {
                devices: Ok(vec![summary(product_id, &[STILL_IMAGE])]),
                open_error: None,
                config: Ok(CONFIG.to_vec()),
                claim_error: None,
                events: Ok(Vec::new()),
            }
        }
    }

    pub(crate) struct FakeDevice {
        config: Result<Vec<u8>, UsbError>,
        claim_error: Option<UsbError>,
    }

    pub(crate) struct FakeEvents(VecDeque<RawEvent<u8>>);

    impl EventSource for FakeEvents {
        type Id = u8;

        fn next_event(&mut self) -> impl Future<Output = Option<RawEvent<u8>>> + Send {
            std::future::ready(self.0.pop_front())
        }
    }

    impl RawDevice for FakeDevice {
        type Port = FakePort;

        fn configuration(&self) -> Result<Vec<u8>, UsbError> {
            self.config.clone()
        }

        fn claim(
            self, endpoints: &Endpoints,
        ) -> impl Future<Output = Result<FakePort, UsbError>> + Send {
            std::future::ready(match self.claim_error {
                Some(error) => Err(error),
                None => Ok(FakePort::new(endpoints.max_packet_size.into())),
            })
        }
    }

    impl Bus for FakeBus {
        type Device = FakeDevice;
        type Events = FakeEvents;

        fn devices(&self) -> Result<Vec<DeviceSummary>, UsbError> {
            self.devices.clone()
        }

        fn open(
            &self, _address: &DeviceAddress,
        ) -> impl Future<Output = Result<FakeDevice, UsbError>> + Send {
            std::future::ready(match &self.open_error {
                Some(error) => Err(error.clone()),
                None => Ok(FakeDevice {
                    config: self.config.clone(),
                    claim_error: self.claim_error.clone(),
                }),
            })
        }

        fn watch(&self) -> Result<FakeEvents, UsbError> {
            self.events.clone().map(|events| FakeEvents(events.into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::camera::{Mode, classify, fixtures::summary};
    use super::super::descriptors::DescriptorError;
    use super::super::hotplug::{CameraEvent, RawEvent};
    use super::fakes::FakeBus;
    use super::*;

    #[test]
    fn lists_only_sony_cameras() {
        let mut bus = FakeBus::with_camera(0x0c34);
        let mut other = summary(0x0c34, &[]);
        other.vendor_id = 1;
        bus.devices = Ok(vec![other, summary(0x0c03, &[])]);
        let cameras = list_cameras(&bus).expect("list");
        assert_eq!(cameras.len(), 1);
        assert_eq!(cameras[0].mode, Mode::Mtp);
    }

    #[test]
    fn list_propagates_enumeration_errors() {
        let mut bus = FakeBus::with_camera(0x0c34);
        bus.devices = Err(UsbError::PermissionDenied);
        assert_eq!(list_cameras(&bus), Err(UsbError::PermissionDenied));
    }

    #[tokio::test]
    async fn opens_the_first_usable_camera() {
        let bus = FakeBus::with_camera(0x0c34);
        let transport = open_first(&bus).await.expect("open");
        assert_eq!(transport.bulk_max_packet_size(), 512);
    }

    #[tokio::test]
    async fn open_first_reports_no_camera_and_wrong_mode() {
        let mut bus = FakeBus::with_camera(0x0c34);
        bus.devices = Ok(vec![]);
        assert_eq!(open_first(&bus).await.err(), Some(UsbError::NoCamera));
        bus.devices = Ok(vec![summary(0x0c03, &[])]);
        let error = open_first(&bus).await.err().expect("wrong mode");
        assert_eq!(error, UsbError::WrongMode(Mode::Mtp));
        bus.devices = Err(UsbError::Busy);
        assert_eq!(open_first(&bus).await.err(), Some(UsbError::Busy));
    }

    #[tokio::test]
    async fn open_surfaces_each_failure_stage() {
        let mut bus = FakeBus::with_camera(0x0c34);
        let camera = list_cameras(&bus).expect("list").remove(0);
        let mut mtp = camera.clone();
        mtp.mode = Mode::Mtp;
        assert_eq!(
            open(&bus, &mtp).await.err(),
            Some(UsbError::WrongMode(Mode::Mtp))
        );

        bus.open_error = Some(UsbError::Busy);
        assert_eq!(open(&bus, &camera).await.err(), Some(UsbError::Busy));
        bus.open_error = None;

        bus.config = Err(UsbError::Disconnected);
        assert_eq!(
            open(&bus, &camera).await.err(),
            Some(UsbError::Disconnected)
        );

        bus.config = Ok(vec![]);
        assert_eq!(
            open(&bus, &camera).await.err(),
            Some(UsbError::Descriptor(DescriptorError::NoInterface))
        );
        bus.config = Ok(super::fakes::CONFIG.to_vec());

        bus.claim_error = Some(UsbError::PermissionDenied);
        assert_eq!(
            open(&bus, &camera).await.err(),
            Some(UsbError::PermissionDenied)
        );
    }

    #[tokio::test]
    async fn watches_cameras() {
        let mut bus = FakeBus::with_camera(0x0c34);
        bus.events = Ok(vec![RawEvent::Connected(1, summary(0x0c34, &[]))]);
        let mut watcher = watch(&bus).expect("watch");
        let arrived = classify(&summary(0x0c34, &[])).expect("camera");
        assert_eq!(watcher.next().await, Some(CameraEvent::Arrived(arrived)));
        assert_eq!(watcher.next().await, None);

        bus.events = Err(UsbError::Other("no hotplug".into()));
        assert_eq!(
            watch(&bus).err(),
            Some(UsbError::Other("no hotplug".into()))
        );
    }
}
