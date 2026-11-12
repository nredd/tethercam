//! Pure protocol tests, including framing, bounds and asynchronous interrupt/unlink behavior.
//!
//! References:
//! - <https://docs.kernel.org/usb/usbip_protocol.html>

use std::io;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::{server::Server, wire};

fn header(command: u32, seq: u32, endpoint: u32, direction: u32, length: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    for word in [
        command,
        seq,
        0x0001_0001,
        direction,
        endpoint,
        0,
        length,
        0,
        0,
        0,
        0,
        0,
    ] {
        bytes.extend_from_slice(&word.to_be_bytes());
    }
    bytes
}

async fn import(server: &Server) -> io::Result<TcpStream> {
    let mut socket = TcpStream::connect(server.address).await?;
    socket.write_all(&wire::operation(0x8003, 0)).await?;
    let mut bus = [0; 32];
    for (slot, byte) in bus.iter_mut().zip(b"1-1") {
        *slot = *byte;
    }
    socket.write_all(&bus).await?;
    let mut reply = [0; 320];
    timeout(Duration::from_secs(2), socket.read_exact(&mut reply)).await??;
    if reply.get(..8) != Some(wire::operation(3, 0).as_slice()) {
        return Err(wire::invalid());
    }
    Ok(socket)
}

async fn control(socket: &mut TcpStream, seq: u32, setup: [u8; 8]) -> io::Result<(u32, Vec<u8>)> {
    let [kind, _, _, _, _, _, low, high] = setup;
    let length = u16::from_le_bytes([low, high]);
    let mut request = header(1, seq, 0, u32::from(kind >> 7), u32::from(length));
    for (slot, byte) in request.iter_mut().skip(40).zip(setup) {
        *slot = byte;
    }
    socket.write_all(&request).await?;
    let mut reply = [0; 48];
    timeout(Duration::from_secs(2), socket.read_exact(&mut reply)).await??;
    let length = usize::try_from(wire::word(&reply, 24)?).map_err(io::Error::other)?;
    if length > wire::MAX_TRANSFER {
        return Err(wire::invalid());
    }
    let mut payload = vec![0; length];
    timeout(Duration::from_secs(2), socket.read_exact(&mut payload)).await??;
    Ok((wire::word(&reply, 20)?, payload))
}

#[tokio::test]
async fn standard_control_requests_validate_and_preserve_usb_state()
-> Result<(), Box<dyn std::error::Error>> {
    let server = Server::start(0, tethercam::test_support::Sony::default()).await?;
    let mut socket = import(&server).await?;
    let (status, configuration) = control(&mut socket, 1, [0x80, 6, 0, 2, 0, 0, 9, 0]).await?;
    assert_eq!(status, 0);
    assert_eq!(configuration, [9, 2, 69, 0, 1, 1, 0, 0x80, 50]);
    assert_eq!(
        control(&mut socket, 2, [1, 11, 1, 0, 0, 0, 0, 0]).await?.0,
        0
    );
    assert_eq!(
        control(&mut socket, 3, [0x81, 10, 0, 0, 0, 0, 1, 0])
            .await?
            .1,
        [1]
    );
    assert_ne!(
        control(&mut socket, 4, [1, 11, 7, 0, 0, 0, 0, 0]).await?.0,
        0
    );
    assert_eq!(
        control(&mut socket, 5, [0, 9, 0, 0, 0, 0, 0, 0]).await?.0,
        0
    );
    assert_eq!(
        control(&mut socket, 6, [0x80, 8, 0, 0, 0, 0, 1, 0])
            .await?
            .1,
        [0]
    );
    assert_ne!(
        control(&mut socket, 7, [1, 11, 1, 0, 0, 0, 0, 0]).await?.0,
        0
    );
    assert_eq!(
        control(&mut socket, 8, [0, 9, 1, 0, 0, 0, 0, 0]).await?.0,
        0
    );
    assert_eq!(
        control(&mut socket, 9, [0x81, 10, 0, 0, 0, 0, 1, 0])
            .await?
            .1,
        [0]
    );
    assert_ne!(
        control(&mut socket, 10, [0x80, 6, 99, 3, 0, 0, 16, 0])
            .await?
            .0,
        0
    );
    assert_ne!(
        control(&mut socket, 11, [0xa1, 0x67, 0, 0, 1, 0, 4, 0])
            .await?
            .0,
        0
    );
    Ok(())
}

#[test]
fn total_codec_rejects_invalid_fields_and_encodes_network_order() {
    let good = header(1, 0x1234_5678, 1, 1, 512);
    let urb = wire::Urb::parse(&good).expect("valid header");
    assert_eq!(urb.seq, 0x1234_5678);
    assert_eq!(urb.length, 512);
    let reply = urb.reply(-32, &[], 0).expect("stall");
    assert_eq!(wire::word(&reply, 0).expect("command"), 3);
    assert_eq!(
        wire::word(&reply, 20).expect("status"),
        u32::from_be_bytes((-32_i32).to_be_bytes())
    );
    assert!(urb.reply(0, &[1], 0).is_err());
    assert!(urb.reply(0, &[0; 513], 513).is_err());
    for (offset, value) in [(0, 3), (8, 0), (12, 2), (16, 16), (24, 1_048_577), (32, 1)] {
        let mut bad = good.clone();
        for (slot, byte) in bad.iter_mut().skip(offset).zip(u32::to_be_bytes(value)) {
            *slot = byte;
        }
        assert!(wire::Urb::parse(&bad).is_err(), "offset {offset}");
    }
    for length in 0..48 {
        assert!(wire::Urb::parse(good.get(..length).expect("prefix")).is_err());
    }
    assert!(wire::word(&good, usize::MAX).is_err());
    assert_eq!(
        descriptor(1, 0, 0x0c03).expect("wrong mode").get(10..12),
        Some([3, 0x0c].as_slice())
    );
}

#[tokio::test]
async fn fragmented_devlist_and_pending_interrupt_unlink() -> Result<(), Box<dyn std::error::Error>>
{
    let server = Server::start(0, tethercam::test_support::Sony::default()).await?;
    assert!(server.address.ip().is_loopback());
    let mut list = TcpStream::connect(server.address).await?;
    for byte in wire::operation(0x8005, 0) {
        list.write_all(&[byte]).await?;
    }
    let mut reply = [0; 328];
    timeout(Duration::from_secs(2), list.read_exact(&mut reply)).await??;
    assert_eq!(wire::word(&reply, 8)?, 1);
    drop(list);
    let mut socket = import(&server).await?;
    socket.write_all(&header(1, 1, 3, 1, 64)).await?;
    timeout(Duration::from_secs(2), async {
        while server.state.lock().await.submitted < 1 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    server
        .model
        .lock()
        .await
        .camera
        .push_event(0x4002, 0, &[7])?;
    server.changed.notify_one();
    let mut event = [0; 64];
    timeout(Duration::from_secs(2), socket.read_exact(&mut event)).await??;
    assert_eq!(wire::word(&event, 24)?, 16);
    assert_eq!(event.get(48..52), Some([16, 0, 0, 0].as_slice()));
    socket.write_all(&header(1, 2, 3, 1, 64)).await?;
    let mut unlink = header(2, 3, 0, 0, 0);
    for (slot, byte) in unlink.iter_mut().skip(20).zip(2_u32.to_be_bytes()) {
        *slot = byte;
    }
    socket.write_all(&unlink).await?;
    let mut cancelled = [0; 48];
    timeout(Duration::from_secs(2), socket.read_exact(&mut cancelled)).await??;
    assert_eq!(wire::word(&cancelled, 0)?, 4);
    assert_eq!(
        wire::word(&cancelled, 20)?,
        u32::from_be_bytes((-104_i32).to_be_bytes())
    );
    assert!(
        timeout(Duration::from_millis(30), socket.read_u8())
            .await
            .is_err(),
        "UNLINK must not also send RET_SUBMIT"
    );
    socket.write_all(&unlink).await?;
    timeout(Duration::from_secs(2), socket.read_exact(&mut cancelled)).await??;
    assert_eq!(wire::word(&cancelled, 20)?, 0);
    assert_eq!(server.state.lock().await.unlinked, 1);
    drop(socket);
    Ok(())
}

#[tokio::test]
async fn oversized_urb_and_duplicate_import_fail_closed() -> Result<(), Box<dyn std::error::Error>>
{
    let server = Server::start(0, tethercam::test_support::Sony::default()).await?;
    let mut socket = import(&server).await?;
    let mut duplicate = TcpStream::connect(server.address).await?;
    duplicate.write_all(&wire::operation(0x8003, 0)).await?;
    let mut bus = [0; 32];
    for (slot, byte) in bus.iter_mut().zip(b"1-1") {
        *slot = *byte;
    }
    duplicate.write_all(&bus).await?;
    let mut response = [0; 8];
    timeout(Duration::from_secs(2), duplicate.read_exact(&mut response)).await??;
    assert_eq!(response.as_slice(), wire::operation(3, 1));
    socket.write_all(&header(1, 1, 1, 1, 1_048_577)).await?;
    assert_eq!(
        timeout(Duration::from_secs(2), socket.read_u8())
            .await?
            .expect_err("closed invalid URB")
            .kind(),
        io::ErrorKind::UnexpectedEof
    );
    assert_eq!(server.state.lock().await.errors.len(), 1);
    Ok(())
}

#[tokio::test]
async fn pending_urb_count_and_memory_are_bounded() -> Result<(), Box<dyn std::error::Error>> {
    for (count, length) in [(33, 64), (5, 1_048_576)] {
        let server = Server::start(0, tethercam::test_support::Sony::default()).await?;
        let mut socket = import(&server).await?;
        for seq in 1..=count {
            socket.write_all(&header(1, seq, 1, 1, length)).await?;
        }
        assert_eq!(
            timeout(Duration::from_secs(2), socket.read_u8())
                .await?
                .expect_err("budget closes peer")
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(server.state.lock().await.errors.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn completed_out_history_and_queued_work_have_an_export_lifetime_budget()
-> Result<(), Box<dyn std::error::Error>> {
    for commands_without_reads in [false, true] {
        let server = Server::start(0, tethercam::test_support::Sony::default()).await?;
        let mut socket = import(&server).await?;
        let mut completed = 0;
        for seq in 1..=128 {
            let payload = if commands_without_reads {
                tethercam::ptp::Container::command(0x1001, seq, &[])?
            } else {
                Vec::new()
            };
            socket
                .write_all(&header(1, seq, 2, 0, u32::try_from(payload.len())?))
                .await?;
            socket.write_all(&payload).await?;
            let mut reply = [0; 48];
            match timeout(Duration::from_secs(2), socket.read_exact(&mut reply)).await? {
                Ok(_) => {
                    assert_eq!(wire::word(&reply, 20)?, 0);
                    completed += 1;
                }
                Err(error) => {
                    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
                    break;
                }
            }
        }
        assert!(completed > 0 && completed < 64);
        assert_eq!(
            server.model.lock().await.camera.bulk_out_transfers().len(),
            completed
        );
        let mut imported_again = import(&server).await?;
        imported_again.write_all(&header(1, 200, 2, 0, 0)).await?;
        assert_eq!(
            timeout(Duration::from_secs(2), imported_again.read_u8())
                .await?
                .expect_err("reimport cannot refund device memory budget")
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(
            server.model.lock().await.camera.bulk_out_transfers().len(),
            completed
        );
    }
    Ok(())
}

#[tokio::test]
async fn control_history_capacity_cannot_be_bypassed_by_reimport()
-> Result<(), Box<dyn std::error::Error>> {
    let server = Server::start(0, tethercam::test_support::Sony::default()).await?;
    server.state.lock().await.controls.resize(4095, [0; 8]);
    let mut socket = import(&server).await?;
    assert_eq!(
        control(&mut socket, 1, [0x80, 8, 0, 0, 0, 0, 1, 0])
            .await?
            .1,
        [1]
    );
    assert_eq!(server.state.lock().await.controls.len(), 4096);
    assert_eq!(
        control(&mut socket, 2, [0x80, 8, 0, 0, 0, 0, 1, 0])
            .await
            .expect_err("control history cap")
            .kind(),
        io::ErrorKind::UnexpectedEof
    );
    for _ in 0..3 {
        let mut socket = import(&server).await?;
        assert_eq!(
            control(&mut socket, 3, [0x80, 8, 0, 0, 0, 0, 1, 0])
                .await
                .expect_err("reimport cannot grow control history")
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(server.state.lock().await.controls.len(), 4096);
    }
    Ok(())
}

#[tokio::test]
async fn pending_urb_has_a_deadline() -> Result<(), Box<dyn std::error::Error>> {
    let server = Server::start(0, tethercam::test_support::Sony::default()).await?;
    let mut socket = import(&server).await?;
    socket.write_all(&header(1, 1, 3, 1, 64)).await?;
    let mut reply = [0; 48];
    timeout(Duration::from_secs(7), socket.read_exact(&mut reply)).await??;
    assert_eq!(
        wire::word(&reply, 20)?,
        u32::from_be_bytes((-110_i32).to_be_bytes())
    );
    assert_eq!(server.state.lock().await.errors, [] as [String; 0]);
    Ok(())
}

fn descriptor(kind: u8, index: u8, product: u16) -> Option<Vec<u8>> {
    use tethercam::test_support::{DeviceModel, Sony};
    let mut model = Sony::default();
    model.product = product;
    model.identity().descriptor(kind, index)
}
