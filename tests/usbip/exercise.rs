//! Scenarios driven only through real nusb handles on the imported synthetic device.
//!
//! References:
//! - <https://docs.rs/nusb/0.2>

use std::time::Duration;

use tethercam::ptp::session::{Session, SessionConfig};
use tethercam::transport::sim::{Fault, Reply};
use tethercam::transport::usb::descriptors::select;
use tethercam::transport::usb::{
    self, Bus, DeviceAddress, NusbBus, Port, PortError, RawDevice, UsbError,
};
use tethercam::transport::{Endpoint, Transport};
use tokio::time::timeout;

use super::server::Server;

const LIMIT: Duration = Duration::from_secs(2);

pub(crate) async fn roundtrip(
    server: &Server, address: &DeviceAddress,
) -> Result<(), Box<dyn std::error::Error>> {
    {
        let mut state = server.state.lock().await;
        let camera = std::mem::take(&mut state.camera);
        state.camera = camera
            .with_operation(0x9990, Reply::data(vec![0x5a; 1524]))
            .with_operation(0x9991, Reply::accepting_data());
    }
    let camera = usb::list_cameras(&NusbBus)?
        .into_iter()
        .find(|c| c.address == *address)
        .ok_or("synthetic camera absent")?;
    let mut transport = usb::open(&NusbBus, &camera).await?;
    assert_eq!(transport.device_status(LIMIT).await?.code, 0x2001);
    let cfg = SessionConfig {
        max_packet_size: transport.bulk_max_packet_size(),
        ..SessionConfig::default()
    };
    let mut session = Session::new(transport, cfg);
    session.open().await?;
    let result = async {
        let info = session.get_device_info().await?;
        assert_ne!(info, [] as [u8; 0]);
        let data = session.transaction(0x9990, &[], None).await?;
        assert_eq!(data.data, vec![0x5a; 1524]); // 1536-byte container and its terminating ZLP.
        session
            .transaction(0x9991, &[], Some(&vec![0x55; 1536]))
            .await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    let closed = session.close().await;
    result?;
    closed?;
    assert!(!session.is_open());
    assert!(!server.state.lock().await.camera.session_open());
    Ok(())
}

pub(crate) async fn raw_port(
    server: &Server, address: &DeviceAddress,
) -> Result<(), Box<dyn std::error::Error>> {
    let device = NusbBus.open(address).await?;
    let mut endpoints = select(&device.configuration()?)?;
    endpoints.alternate = 1;
    let mut port = device.claim(&endpoints).await?;
    assert!(matches!(
        NusbBus.open(address).await?.claim(&endpoints).await,
        Err(UsbError::Busy)
    ));
    for (endpoint, size) in [
        (Endpoint::BulkIn, 512),
        (Endpoint::BulkOut, 512),
        (Endpoint::InterruptIn, 64),
    ] {
        assert_eq!(port.max_packet_size(endpoint), size);
        port.cancel(endpoint).await;
        port.clear_halt(endpoint).await?;
    }
    assert_eq!(port.bulk_out(&[]).await?, 0);
    let mut bytes = vec![0; 2048];
    server
        .state
        .lock()
        .await
        .camera
        .push_raw_bulk_in(vec![0x42; 1024]);
    assert_eq!(port.bulk_in(&mut bytes).await?, 1024);
    assert_eq!(bytes.get(..1024), Some(vec![0x42; 1024].as_slice()));
    server
        .state
        .lock()
        .await
        .camera
        .push_raw_bulk_in(Vec::new());
    assert_eq!(port.bulk_in(&mut bytes).await?, 0);
    server
        .state
        .lock()
        .await
        .camera
        .inject(Endpoint::BulkIn, 0, Fault::Stall);
    assert_eq!(port.bulk_in(&mut bytes).await, Err(PortError::Stall));
    port.clear_halt(Endpoint::BulkIn).await?;
    server
        .state
        .lock()
        .await
        .camera
        .inject(Endpoint::BulkOut, 0, Fault::Stall);
    assert_eq!(port.bulk_out(&[]).await, Err(PortError::Stall));
    port.clear_halt(Endpoint::BulkOut).await?;
    assert!(
        timeout(Duration::from_millis(100), port.bulk_in(&mut bytes))
            .await
            .is_err()
    );
    // Drop left a pending bulk URB; read must cancel and drain it before queuing another.
    server
        .state
        .lock()
        .await
        .camera
        .push_raw_bulk_in(vec![7; 12]);
    assert_eq!(port.bulk_in(&mut bytes).await?, 12);
    assert!(
        timeout(Duration::from_millis(100), port.interrupt_in(&mut bytes))
            .await
            .is_err()
    );
    port.cancel(Endpoint::InterruptIn).await;
    pending_event(server, &mut port, &mut bytes).await?;
    server.state.lock().await.hold_out = true;
    assert!(
        timeout(Duration::from_millis(100), port.bulk_out(&[0; 512]))
            .await
            .is_err()
    );
    port.cancel(Endpoint::BulkOut).await;
    server.state.lock().await.hold_out = false;
    assert_eq!(port.class_out(0x64, LIMIT).await, Err(PortError::Stall));
    port.class_out(0x66, LIMIT).await?;
    assert_eq!(port.class_in(0x67, 4, LIMIT).await?, [4, 0, 1, 0x20]);
    server.state.lock().await.status = 0x2019;
    assert_eq!(port.class_in(0x67, 4, LIMIT).await?, [4, 0, 0x19, 0x20]);
    server.state.lock().await.status = 0x2001;
    assert_eq!(port.class_in(0xff, 4, LIMIT).await, Err(PortError::Stall));
    assert_eq!(port.class_out(0xff, LIMIT).await, Err(PortError::Stall));
    drop(port);
    Ok(())
}

async fn pending_event(
    server: &Server, port: &mut usb::nusb_backend::NusbPort, bytes: &mut [u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let before = server.state.lock().await.submitted;
    let (received, queued) = timeout(LIMIT, async {
        tokio::join!(port.interrupt_in(bytes), async {
            while server.state.lock().await.submitted <= before {
                tokio::task::yield_now().await;
            }
            server
                .state
                .lock()
                .await
                .camera
                .push_event(0x4002, 0, &[7])?;
            server.changed.notify_one();
            Ok::<(), Box<dyn std::error::Error>>(())
        })
    })
    .await?;
    queued?;
    assert_eq!(received?, 16);
    Ok(())
}

pub(crate) async fn claim_errors(
    address: &DeviceAddress,
) -> Result<(), Box<dyn std::error::Error>> {
    let template = NusbBus.open(address).await?;
    let endpoints = select(&template.configuration()?)?;
    drop(template);
    for (input, output, interrupt, alternate) in [
        (0x8f, 2, 0x83, 0),
        (0x81, 15, 0x83, 0),
        (0x81, 2, 0x8f, 0),
        (0x81, 2, 0x83, 7),
    ] {
        let mut invalid = endpoints;
        invalid.bulk_in = input;
        invalid.bulk_out = output;
        invalid.interrupt_in = interrupt;
        invalid.alternate = alternate;
        assert!(NusbBus.open(address).await?.claim(&invalid).await.is_err());
    }
    let nonexistent = DeviceAddress {
        bus: "synthetic-absent".into(),
        address: 255,
    };
    assert!(matches!(
        NusbBus.open(&nonexistent).await,
        Err(UsbError::Disconnected)
    ));
    Ok(())
}

pub(crate) async fn configuration_error(
    address: &DeviceAddress,
) -> Result<(), Box<dyn std::error::Error>> {
    let raw = NusbBus.open(address).await?;
    let info = nusb::list_devices()
        .await?
        .find(|d| d.bus_id() == address.bus && d.device_address() == address.address)
        .ok_or("device missing")?;
    let device = info.open().await?;
    let interface = device.claim_interface(0).await?;
    interface
        .control_out(
            nusb::transfer::ControlOut {
                control_type: nusb::transfer::ControlType::Class,
                recipient: nusb::transfer::Recipient::Interface,
                request: 0x64,
                value: 0,
                index: 0,
                data: &[1, 0x40, 0, 0, 0, 0],
            },
            LIMIT,
        )
        .await?;
    drop(interface);
    device.set_configuration(0).await?;
    let result = raw.configuration();
    device.set_configuration(1).await?;
    assert!(result.is_err());
    Ok(())
}

pub(crate) async fn disconnected(
    server: &Server, address: &DeviceAddress,
) -> Result<(), Box<dyn std::error::Error>> {
    let device = NusbBus.open(address).await?;
    let endpoints = select(&device.configuration()?)?;
    let mut port = device.claim(&endpoints).await?;
    let mut bytes = [0; 512];
    let pending = std::sync::Arc::clone(&server.state.lock().await.pending_bulk_in);
    let (received, detached) = timeout(Duration::from_secs(20), async {
        tokio::join!(port.bulk_in(&mut bytes), async {
            timeout(LIMIT, async {
                while pending.load(std::sync::atomic::Ordering::Acquire) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            // Only the script's serial-verified synthetic VHCI port can be detached.
            super::setup("detach")?;
            Ok::<(), Box<dyn std::error::Error>>(())
        })
    })
    .await?;
    detached?;
    assert_eq!(received, Err(PortError::Disconnected));
    for endpoint in [Endpoint::BulkIn, Endpoint::BulkOut, Endpoint::InterruptIn] {
        assert!(port.clear_halt(endpoint).await.is_err());
    }
    assert!(port.reset_device().await.is_err());
    Ok(())
}

pub(crate) async fn resets(
    server: &Server, address: &DeviceAddress,
) -> Result<(), Box<dyn std::error::Error>> {
    let device = NusbBus.open(address).await?;
    let endpoints = select(&device.configuration()?)?;
    let port = device.claim(&endpoints).await?;
    let mut transport = usb::UsbTransport::new(port);
    transport.reset(LIMIT).await?;
    server.state.lock().await.class_stall = true;
    assert!(transport.reset(LIMIT).await.is_err());
    server.state.lock().await.class_stall = false;
    Ok(())
}
