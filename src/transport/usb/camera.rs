//! Identifying Sony cameras and their USB connection mode from plain device data.
//!
//! The adapter over `nusb` reduces each enumerated device to a [`DeviceSummary`]; everything
//! here is pure and tested without hardware.
//!
//! References:
//! - USB Still Image Capture Device Definition 1.0, section 5 (class 6, subclass 1, protocol 1)
//! - USB-IF defined class codes: <https://www.usb.org/defined-class-codes>

use std::fmt;

use super::UsbError;

/// USB vendor id of Sony.
pub const SONY_VENDOR_ID: u16 = 0x054c;

/// `(class, subclass, protocol)` of a USB Still Image Capture Device interface.
pub const STILL_IMAGE: (u8, u8, u8) = (6, 1, 1);

/// The USB connection mode a Sony camera is in, derived from its product id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Remote control over PTP (`0x0c34`). The only mode `tethercam` can drive.
    PcRemote,
    /// Media Transfer Protocol (`0x0c03`).
    Mtp,
    /// USB mass storage (`0x0c02`).
    MassStorage,
    /// A product id not in the table that exposes a still-image interface.
    Other(u16),
}

impl Mode {
    /// Classify a Sony product id.
    #[must_use]
    pub fn from_product_id(product_id: u16) -> Self {
        match product_id {
            0x0c34 => Self::PcRemote,
            0x0c03 => Self::Mtp,
            0x0c02 => Self::MassStorage,
            other => Self::Other(other),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PcRemote => f.write_str("PC Remote"),
            Self::Mtp => f.write_str("MTP"),
            Self::MassStorage => f.write_str("Mass Storage"),
            Self::Other(id) => write!(f, "unknown (product id {id:#06x})"),
        }
    }
}

/// Negotiated USB bus speed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusSpeed {
    /// 1.5 Mbit/s.
    Low,
    /// 12 Mbit/s.
    Full,
    /// 480 Mbit/s.
    High,
    /// 5 Gbit/s.
    Super,
    /// 10 Gbit/s and above.
    SuperPlus,
    /// The operating system did not report a speed.
    Unknown,
}

/// Where a device sits on the bus; stable while it stays plugged in.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceAddress {
    /// Operating-system bus identifier.
    pub bus: String,
    /// Device address on that bus.
    pub address: u8,
}

/// Everything enumeration reports about one USB device, as plain data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSummary {
    /// Bus position, used to reopen the device.
    pub address: DeviceAddress,
    /// USB vendor id.
    pub vendor_id: u16,
    /// USB product id.
    pub product_id: u16,
    /// Product string, when the device has one.
    pub product: Option<String>,
    /// Serial number string, when the device has one.
    pub serial: Option<String>,
    /// Negotiated speed.
    pub speed: BusSpeed,
    /// `(class, subclass, protocol)` of every interface.
    pub interfaces: Vec<(u8, u8, u8)>,
}

/// A Sony camera found on the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraInfo {
    /// Bus position.
    pub address: DeviceAddress,
    /// USB product id.
    pub product_id: u16,
    /// Connection mode.
    pub mode: Mode,
    /// Product string, when reported.
    pub product: Option<String>,
    /// Serial number string, when reported.
    pub serial: Option<String>,
    /// Negotiated speed.
    pub speed: BusSpeed,
}

impl CameraInfo {
    /// Whether the camera is in a mode `tethercam` can drive.
    ///
    /// # Errors
    ///
    /// Returns [`UsbError::WrongMode`] for MTP and Mass Storage, so the user is told to set
    /// "USB Connection" to "PC Remote" instead of seeing a transport error.
    pub fn check_mode(&self) -> Result<(), UsbError> {
        match self.mode {
            Mode::Mtp | Mode::MassStorage => Err(UsbError::WrongMode(self.mode)),
            Mode::PcRemote | Mode::Other(_) => Ok(()),
        }
    }
}

/// Recognise a Sony camera: Sony vendor id plus a known camera mode or a still-image interface.
#[must_use]
pub fn classify(device: &DeviceSummary) -> Option<CameraInfo> {
    if device.vendor_id != SONY_VENDOR_ID {
        return None;
    }
    let mode = Mode::from_product_id(device.product_id);
    let still_image = device.interfaces.contains(&STILL_IMAGE);
    if matches!(mode, Mode::Other(_)) && !still_image {
        return None;
    }
    Some(CameraInfo {
        address: device.address.clone(),
        product_id: device.product_id,
        mode,
        product: device.product.clone(),
        serial: device.serial.clone(),
        speed: device.speed,
    })
}

/// Choose the camera to open: the first usable one, else report why the first one is not.
///
/// # Errors
///
/// [`UsbError::NoCamera`] when the list is empty, or the first mode error when none is usable.
pub fn pick(cameras: &[CameraInfo]) -> Result<&CameraInfo, UsbError> {
    let mut first_error = UsbError::NoCamera;
    for (index, camera) in cameras.iter().enumerate() {
        match camera.check_mode() {
            Ok(()) => return Ok(camera),
            Err(error) if index == 0 => first_error = error,
            Err(_) => {}
        }
    }
    Err(first_error)
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    /// A synthetic camera summary in the given mode.
    pub(crate) fn summary(product_id: u16, interfaces: &[(u8, u8, u8)]) -> DeviceSummary {
        DeviceSummary {
            address: DeviceAddress {
                bus: "bus-1".into(),
                address: 7,
            },
            vendor_id: SONY_VENDOR_ID,
            product_id,
            product: Some("ILCE-0000".into()),
            serial: Some("SERIAL0000".into()),
            speed: BusSpeed::High,
            interfaces: interfaces.to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::summary;
    use super::*;

    #[test]
    fn modes_from_product_id() {
        assert_eq!(Mode::from_product_id(0x0c34), Mode::PcRemote);
        assert_eq!(Mode::from_product_id(0x0c03), Mode::Mtp);
        assert_eq!(Mode::from_product_id(0x0c02), Mode::MassStorage);
        assert_eq!(Mode::from_product_id(0x1234), Mode::Other(0x1234));
    }

    #[test]
    fn mode_display_names_the_setting() {
        assert_eq!(Mode::PcRemote.to_string(), "PC Remote");
        assert_eq!(Mode::Mtp.to_string(), "MTP");
        assert_eq!(Mode::MassStorage.to_string(), "Mass Storage");
        assert_eq!(
            Mode::Other(0x1234).to_string(),
            "unknown (product id 0x1234)"
        );
    }

    #[test]
    fn classifies_known_modes_without_still_image_interface() {
        let camera = classify(&summary(0x0c02, &[(8, 6, 80)])).expect("camera");
        assert_eq!(camera.mode, Mode::MassStorage);
        assert_eq!(camera.serial.as_deref(), Some("SERIAL0000"));
        assert_eq!(camera.speed, BusSpeed::High);
        assert_eq!(camera.product_id, 0x0c02);
        assert_eq!(camera.product.as_deref(), Some("ILCE-0000"));
    }

    #[test]
    fn classifies_unknown_product_only_with_still_image_interface() {
        assert!(classify(&summary(0x7777, &[(3, 1, 1)])).is_none());
        let camera = classify(&summary(0x7777, &[(3, 1, 1), STILL_IMAGE])).expect("camera");
        assert_eq!(camera.mode, Mode::Other(0x7777));
    }

    #[test]
    fn ignores_other_vendors() {
        let mut device = summary(0x0c34, &[STILL_IMAGE]);
        device.vendor_id = 0x1234;
        assert!(classify(&device).is_none());
    }

    #[test]
    fn only_pc_remote_and_unknown_modes_are_usable() {
        let usable = |pid| {
            classify(&summary(pid, &[STILL_IMAGE]))
                .expect("camera")
                .check_mode()
        };
        assert_eq!(usable(0x0c34), Ok(()));
        assert_eq!(usable(0x7777), Ok(()));
        assert_eq!(usable(0x0c03), Err(UsbError::WrongMode(Mode::Mtp)));
        assert_eq!(usable(0x0c02), Err(UsbError::WrongMode(Mode::MassStorage)));
        let message = usable(0x0c03).expect_err("wrong mode").to_string();
        assert!(message.contains("PC Remote"), "{message}");
    }

    #[test]
    fn pick_prefers_usable_and_reports_first_mode_error() {
        let camera = |pid| classify(&summary(pid, &[STILL_IMAGE])).expect("camera");
        assert_eq!(pick(&[]), Err(UsbError::NoCamera));
        let mtp = camera(0x0c03);
        let msc = camera(0x0c02);
        let remote = camera(0x0c34);
        assert_eq!(pick(&[mtp.clone(), remote.clone()]), Ok(&remote));
        assert_eq!(
            pick(&[mtp.clone(), msc.clone()]),
            Err(UsbError::WrongMode(Mode::Mtp))
        );
        assert_eq!(
            pick(&[msc, mtp]),
            Err(UsbError::WrongMode(Mode::MassStorage))
        );
    }
}
