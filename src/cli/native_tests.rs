//! Native factory wiring through the existing fake bus, never real enumeration or USB I/O.
//!
//! References:
//! - `docs/architecture.md`, CLI and supervisor contract

use super::*;
use crate::cli::{Command, Options, dispatch_runtime};
use crate::config::Config;
use crate::ptp::session::SessionConfig;
use crate::supervisor::testing::jpeg;
use crate::transport::usb::bus::fakes::FakeBus;
use crate::transport::usb::camera::{STILL_IMAGE, fixtures::summary};
use crate::transport::usb::{DescriptorError, Mode};

#[test]
fn sanitizes_every_native_error_without_retaining_untrusted_text() {
    for (error, expected) in [
        (UsbError::PermissionDenied, OpenError::Permission),
        (UsbError::WrongMode(Mode::MassStorage), OpenError::Mode),
        (UsbError::Busy, OpenError::Busy),
        (UsbError::NoCamera, OpenError::Disconnected),
        (UsbError::Disconnected, OpenError::Disconnected),
        (
            UsbError::Descriptor(DescriptorError::Malformed),
            OpenError::Discovery,
        ),
        (
            UsbError::Other("synthetic-do-not-log-serial-or-path".into()),
            OpenError::Discovery,
        ),
    ] {
        assert_eq!(sanitize(&error), expected);
    }
}

#[tokio::test]
async fn concrete_fake_bus_covers_selection_opening_and_real_packet_geometry() {
    let mut bus = FakeBus::with_camera(0x0c34);
    bus.devices = Ok(Vec::new());
    assert!(Native::new(bus, None).connect().await.unwrap().is_none());
    let mut bus = FakeBus::with_camera(0x0c34);
    bus.devices = Err(UsbError::PermissionDenied);
    assert_eq!(
        Native::new(bus, None).connect().await.err(),
        Some(OpenError::Permission)
    );
    assert_eq!(
        Native::new(FakeBus::with_camera(0x0c03), None)
            .connect()
            .await
            .err(),
        Some(OpenError::Mode)
    );
    assert!(
        Native::new(
            FakeBus::with_camera(0x0c34),
            Some("synthetic-absent".into())
        )
        .connect()
        .await
        .unwrap()
        .is_none()
    );
    let mut bus = FakeBus::with_camera(0x0c34);
    bus.open_error = Some(UsbError::Busy);
    assert_eq!(
        Native::new(bus, None).connect().await.err(),
        Some(OpenError::Busy)
    );
    let mut factory = Native::new(FakeBus::with_camera(0x0c34), Some("SERIAL0000".into()));
    let camera = factory.connect().await.unwrap().unwrap();
    assert_eq!(camera.bulk_max_packet_size(), 512);
    let session = factory.session(camera, SessionConfig::default(), std::time::Instant::now);
    let camera = factory.recover(session);
    factory.release(camera); // no session was opened by the factory
    factory.pause(Duration::ZERO).await;
    let mut bus = FakeBus::with_camera(0x0c34);
    *bus.config.as_mut().unwrap().get_mut(14).unwrap() = 4; // 1024-byte USB 3 bulk endpoint
    let mut factory = Native::new(bus, None);
    let camera = factory.connect().await.unwrap().unwrap();
    assert_eq!(camera.bulk_max_packet_size(), 1024);
    factory.release(camera);
}

struct RowFailure;
impl Write for RowFailure {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.starts_with(b"Camera ") {
            Err(std::io::Error::other("synthetic row write failure"))
        } else {
            Ok(bytes.len())
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn reports_modes_not_identifiers_and_propagates_each_output_failure() {
    let mut bus = FakeBus::with_camera(0x0c34);
    bus.devices = Ok(vec![summary(0x0c03, &[]), summary(0x0c34, &[STILL_IMAGE])]);
    let mut bytes = Vec::new();
    report(&bus, &mut bytes).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("Detected cameras: 2"));
    assert!(text.contains("MTP"));
    assert!(text.contains("PC Remote"));
    assert!(!text.contains("SERIAL0000"));
    assert!(!text.contains("bus-1"));
    let mut full: &mut [u8] = &mut [];
    assert!(report(&bus, &mut full).is_err());
    assert!(report(&bus, &mut RowFailure).is_err());
    RowFailure.flush().unwrap();
    bus.devices = Err(UsbError::Other("synthetic-private-error".into()));
    assert!(report(&bus, &mut Vec::new()).is_err());
}

fn dispatch(
    options: &Options, cfg: &Config, out: &mut dyn Write, bus: FakeBus,
) -> Result<(), Error> {
    dispatch_runtime(
        options,
        cfg,
        out,
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build(),
        bus,
    )
}

#[test]
fn dispatcher_uses_the_injected_bus_and_simulation_never_enumerates_it() {
    let cfg = Config::generate().unwrap();
    let mut options = Options {
        command: Command::Help,
        config: None,
        simulate: None,
        no_discovery: true,
    };
    let mut out = Vec::new();
    dispatch(&options, &cfg, &mut out, FakeBus::with_camera(0x0c34)).unwrap();
    options.command = Command::Probe;
    let mut bus = FakeBus::with_camera(0x0c34);
    bus.open_error = Some(UsbError::PermissionDenied);
    assert!(dispatch(&options, &cfg, &mut out, bus).is_err());
    let mut bus = FakeBus::with_camera(0x0c34);
    bus.devices = Err(UsbError::Busy);
    assert!(dispatch(&options, &cfg, &mut out, bus).is_err());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("synthetic.jpg");
    std::fs::write(&path, jpeg(32, 32)).unwrap();
    options.simulate = Some(path.clone());
    let mut bus = FakeBus::with_camera(0x0c34);
    bus.devices = Err(UsbError::Other(
        "simulation must never enumerate this bus".into(),
    ));
    dispatch(&options, &cfg, &mut out, bus).unwrap();
    std::fs::write(&path, b"invalid JPEG").unwrap();
    assert!(dispatch(&options, &cfg, &mut out, FakeBus::with_camera(0x0c34)).is_err());
    std::fs::remove_file(&path).unwrap();
    assert!(dispatch(&options, &cfg, &mut out, FakeBus::with_camera(0x0c34)).is_err());
}
