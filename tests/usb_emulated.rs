//! Exercise the actual nusb Linux backend through hosted-kernel VHCI, never a physical camera.
//!
//! References:
//! - <https://docs.kernel.org/usb/usbip_protocol.html>

#[path = "usbip/cancellation.rs"]
mod cancellation;
#[path = "usbip/exercise.rs"]
mod exercise;
#[path = "usbip/scenarios.rs"]
mod scenarios;
#[path = "usbip/server.rs"]
mod server;
#[path = "usbip/wire.rs"]
mod wire;

use std::process::Command;
use std::time::Duration;

use tethercam::transport::usb::{self, Bus, CameraEvent, NusbBus, UsbError};
use tokio::time::timeout;

fn setup(action: &str) -> std::io::Result<()> {
    let status = Command::new("sh")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/usbip-ci.sh"))
        .arg(action)
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!("USB/IP {action}: {status}")));
    }
    Ok(())
}

struct Attached;
impl Drop for Attached {
    fn drop(&mut self) {
        if let Err(error) = setup("detach") {
            log::error!("USB/IP cleanup failed: {error}");
        }
    }
}

fn emulation_enabled() -> bool {
    let enabled = std::env::var("TETHERCAM_USBIP").as_deref() == Ok("1");
    assert!(
        enabled || std::env::var_os("CI").is_none() || !cfg!(target_os = "linux"),
        "Linux CI must enable real USB/IP backend execution"
    );
    enabled
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emulated_usb() {
    if !emulation_enabled() {
        return;
    }
    timeout(Duration::from_secs(60), async {
        let server = server::Server::start(3240)
            .await
            .expect("loopback USB/IP server");
        let mut watcher = usb::watch(&NusbBus).expect("watch actual nusb hotplug");
        let _attached = Attached;
        setup("attach").expect("attach synthetic USB/IP device");
        let Some(CameraEvent::Arrived(camera)) = timeout(Duration::from_secs(10), watcher.next())
            .await
            .expect("hotplug deadline")
        else {
            panic!("missing arrival");
        };
        assert_eq!(camera.serial.as_deref(), Some("TETHERCAM-USBIP-0001"));
        let address = camera.address;
        let mut seeded = usb::watch(&NusbBus).expect("seeded native watch");
        assert!(matches!(
            timeout(Duration::from_secs(2), seeded.next())
                .await
                .expect("seeded deadline"),
            Some(CameraEvent::Arrived(_))
        ));
        drop(seeded);
        exercise::roundtrip(&server, &address)
            .await
            .expect("PTP session + multi-packet data + ZLP");
        exercise::raw_port(&server, &address)
            .await
            .expect("bulk + interrupt + timeout/unlink + stall/clear");
        cancellation::exercise(&address)
            .await
            .expect("stateful native cancellation");
        exercise::claim_errors(&address)
            .await
            .expect("claim errors");
        exercise::configuration_error(&address)
            .await
            .expect("unconfigured native descriptor");
        exercise::resets(&server, &address)
            .await
            .expect("class and port reset fallback");
        setup("detach").expect("detach synthetic device");
        assert!(matches!(
            timeout(Duration::from_secs(10), watcher.next())
                .await
                .expect("removal deadline"),
            Some(CameraEvent::Removed(_))
        ));
        server.state.lock().await.product = 0x0c03;
        setup("attach").expect("attach wrong-mode synthetic device");
        let Some(CameraEvent::Arrived(wrong)) = timeout(Duration::from_secs(10), watcher.next())
            .await
            .expect("wrong-mode arrival deadline")
        else {
            panic!("missing wrong-mode arrival");
        };
        assert!(matches!(
            usb::open(&NusbBus, &wrong).await,
            Err(UsbError::WrongMode(_))
        ));
        setup("detach").expect("detach wrong mode");
        assert!(matches!(
            timeout(Duration::from_secs(10), watcher.next())
                .await
                .expect("wrong-mode removal"),
            Some(CameraEvent::Removed(_))
        ));
        {
            let mut state = server.state.lock().await;
            state.product = 0x0c34;
            state.disconnect = false;
        }
        setup("attach").expect("reattach for mid-transfer unplug");
        let Some(CameraEvent::Arrived(camera)) = timeout(Duration::from_secs(10), watcher.next())
            .await
            .expect("reattach deadline")
        else {
            panic!("missing reattach");
        };
        exercise::disconnected(&server, &camera.address)
            .await
            .expect("mid-transfer disconnect");
        assert!(matches!(
            timeout(Duration::from_secs(10), watcher.next())
                .await
                .expect("disconnection event"),
            Some(CameraEvent::Removed(_))
        ));
        assert!(matches!(
            NusbBus.open(&camera.address).await,
            Err(UsbError::Disconnected)
        ));
        verify_emulation(&server).await;
    })
    .await
    .expect("native USB suite deadline");
}

async fn verify_emulation(server: &server::Server) {
    let state = server.state.lock().await;
    assert!(state.errors.is_empty(), "{:?}", state.errors);
    assert!(
        state.unlinked >= 3,
        "pending URBs must really reach UNLINK: {}",
        state.unlinked
    );
    assert!(state.submitted > 30);
    assert!(!state.camera.session_open());
}

#[test]
fn usbip_codec_and_descriptors() {
    assert_eq!(wire::exported(0x0c34).len(), 312);
    assert_eq!(wire::descriptor(2, 0, 0x0c34).expect("config").len(), 69);
    assert!(wire::descriptor(3, 3, 0x0c34).is_some());
    assert!(wire::descriptor(99, 0, 0x0c34).is_none());
    assert!(wire::Urb::parse(&[0; 48]).is_err());
}

proptest::proptest! {
    #[test]
    fn usbip_header_never_panics(bytes in proptest::collection::vec(proptest::num::u8::ANY, 0..128)) {
        let _ = wire::Urb::parse(&bytes);
    }
}
