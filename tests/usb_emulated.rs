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
    let status = Command::new(env!("CARGO_BIN_EXE_tethercam-tools"))
        .args([
            if std::env::var_os("CI").is_some() {
                "usbip"
            } else {
                "usbip-local"
            },
            action,
        ])
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
    timeout(Duration::from_secs(90), async {
        let server = server::Server::start(3240, tethercam::test_support::Sony::default())
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
        verify_seeded_watch().await.expect("seeded native watch");
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
        server.model.lock().await.product = 0x0c03;
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
            server.model.lock().await.product = 0x0c34;
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
        setup("detach").expect("final Sony detach");
        server.shutdown().await.expect("stop Sony export");
        generic_export()
            .await
            .expect("independent native vendor-class fixture");
    })
    .await
    .expect("native USB suite deadline");
}

async fn verify_seeded_watch() -> Result<(), Box<dyn std::error::Error>> {
    let mut seeded = usb::watch(&NusbBus)?;
    assert!(matches!(
        timeout(Duration::from_secs(2), seeded.next()).await?,
        Some(CameraEvent::Arrived(_))
    ));
    Ok(())
}

async fn generic_export() -> Result<(), Box<dyn std::error::Error>> {
    let generic =
        tethercam::test_support::Server::start(3240, tethercam::test_support::Synthetic::default())
            .await?;
    setup("attach")?;
    exercise::generic_native().await?;
    setup("detach")?;
    generic.shutdown().await?;
    Ok(())
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
    assert!(!server.model.lock().await.camera.session_open());
}

#[test]
fn usbip_codec_and_descriptors() {
    assert_eq!(
        wire::exported(&tethercam::test_support::DeviceModel::identity(
            &tethercam::test_support::Sony::default()
        ))
        .len(),
        312
    );
    assert_eq!(descriptor(2, 0, 0x0c34).expect("config").len(), 69);
    assert!(descriptor(3, 3, 0x0c34).is_some());
    assert!(descriptor(99, 0, 0x0c34).is_none());
    assert!(wire::Urb::parse(&[0; 48]).is_err());
}

proptest::proptest! {
    #[test]
    fn usbip_header_never_panics(bytes in proptest::collection::vec(proptest::num::u8::ANY, 0..128)) {
        let _ = wire::Urb::parse(&bytes);
    }
}

fn descriptor(kind: u8, index: u8, product: u16) -> Option<Vec<u8>> {
    use tethercam::test_support::{DeviceModel, Sony};
    let mut model = Sony::default();
    model.product = product;
    model.identity().descriptor(kind, index)
}

#[test]
#[cfg(target_os = "linux")]
fn hosted_native_adapter_cannot_be_disabled() -> Result<(), Box<dyn std::error::Error>> {
    let result = Command::new(std::env::current_exe()?)
        .args(["--exact", "emulated_usb", "--nocapture"])
        .env("CI", "true")
        .env("TETHERCAM_USBIP", "0")
        .output()?;
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("Linux CI must enable real USB/IP backend execution")
    );
    Ok(())
}
