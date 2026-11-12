//! Hardware-in-the-loop tests against a real camera in PC Remote mode.
//!
//! Ignored by default: run with `cargo test --test usb_hardware -- --ignored`. Each test skips
//! cleanly (returns, passing) when no usable camera is attached. A session is explicitly closed
//! before checking the transaction result, because an abandoned PTP session wedges
//! the camera until it is power-cycled.
//!
//! References:
//! - USB Still Image Capture Device Definition 1.0, sections 5-7
//! - ISO 15740 (PTP): `OpenSession`, `GetDeviceInfo`, `CloseSession`

use std::time::Duration;

use tethercam::ptp::session::{Session, SessionConfig};
use tethercam::transport::usb::{Mode, NusbBus, NusbTransport, UsbError, list_cameras, open_first};

/// `Ok(None)` when no usable camera is attached.
async fn open_camera() -> Result<Option<NusbTransport>, UsbError> {
    match open_first(&NusbBus).await {
        Ok(transport) => Ok(Some(transport)),
        Err(UsbError::NoCamera | UsbError::WrongMode(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

#[test]
#[ignore = "needs a camera"]
fn enumerates_cameras() {
    let cameras = list_cameras(&NusbBus).expect("enumerate");
    for camera in &cameras {
        assert_eq!(camera.mode, Mode::from_product_id(camera.product_id));
    }
}

/// One test, not two: nextest runs tests in parallel processes and only one can hold the
/// camera.
#[tokio::test]
#[ignore = "needs a camera"]
async fn class_status_then_open_get_device_info_close_twenty_times() {
    let Some(mut transport) = open_camera().await.expect("open the camera") else {
        return;
    };
    let status = transport
        .device_status(Duration::from_secs(2))
        .await
        .expect("Get Device Status");
    assert_eq!(status.code, 0x2001, "{status:?}");
    drop(transport);

    for round in 0..20 {
        let transport = open_camera()
            .await
            .expect("open the camera")
            .expect("camera disappeared mid-test");
        let config = SessionConfig {
            max_packet_size: transport.bulk_max_packet_size(),
            ..SessionConfig::default()
        };
        let mut session = Session::new(transport, config);
        session.open().await.expect("OpenSession");
        let result = session.get_device_info().await;
        let closed = session.close().await;
        let info = result.unwrap_or_else(|error| panic!("round {round}: {error}"));
        closed.expect("CloseSession");
        assert!(!info.is_empty(), "round {round}: empty DeviceInfo");
        assert!(!session.is_open(), "round {round}: session left open");
    }
}
