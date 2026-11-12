//! Stateful class cancellation through actual native nusb transfers, with no fake interface.
//!
//! References:
//! - USB Still Image Capture Device Definition 1.0, 5.2.1 and 7.2.1.1:
//!   <https://www.usb.org/sites/default/files/usb_still_img10.zip>

use std::time::Duration;

use nusb::transfer::{Buffer, Bulk, ControlIn, ControlOut, ControlType, In, Out, Recipient};
use tethercam::ptp::{Container, ContainerHeader, ContainerKind, PayloadLen};
use tethercam::transport::usb::DeviceAddress;
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(2);

async fn send(
    endpoint: &mut nusb::Endpoint<Bulk, Out>, bytes: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut buffer = Buffer::new(bytes.len());
    buffer.extend_from_slice(bytes);
    endpoint.submit(buffer);
    let done = timeout(LIMIT, endpoint.next_complete()).await?;
    done.status?;
    verify(
        done.actual_len == bytes.len(),
        "short native cancellation write",
    )?;
    Ok(())
}

fn verify(condition: bool, message: &str) -> std::io::Result<()> {
    if !condition {
        return Err(std::io::Error::other(message));
    }
    Ok(())
}

async fn receive(
    endpoint: &mut nusb::Endpoint<Bulk, In>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    endpoint.submit(Buffer::new(4096));
    let done = timeout(LIMIT, endpoint.next_complete()).await?;
    done.status?;
    Ok(done.buffer.to_vec())
}

async fn cancel(
    interface: &nusb::Interface, transaction: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut payload = vec![1, 0x40];
    payload.extend_from_slice(&transaction.to_le_bytes());
    interface
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x64,
                value: 0,
                index: 0,
                data: &payload,
            },
            LIMIT,
        )
        .await?;
    Ok(())
}

async fn status(interface: &nusb::Interface) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    Ok(interface
        .control_in(
            ControlIn {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x67,
                value: 0,
                index: 0,
                length: 4,
            },
            LIMIT,
        )
        .await?)
}

pub(crate) async fn exercise(address: &DeviceAddress) -> Result<(), Box<dyn std::error::Error>> {
    let info = nusb::list_devices()
        .await?
        .find(|d| d.bus_id() == address.bus && d.device_address() == address.address)
        .ok_or("synthetic device missing")?;
    let device = info.open().await?;
    let interface = device.claim_interface(0).await?;
    let mut output = interface.endpoint::<Bulk, Out>(2)?;
    let mut input = interface.endpoint::<Bulk, In>(0x81)?;
    send(&mut output, &Container::command(0x1002, 0, &[1])?).await?;
    receive(&mut input).await?;
    let result = async {
        send(&mut output, &Container::command(0x9991, 1, &[])?).await?;
        // An incomplete DATA OUT must not remain after cancellation.
        let partial = ContainerHeader::for_payload(
            ContainerKind::Data,
            0x9991,
            1,
            PayloadLen::new(2048, 2048)?,
        )
        .encode();
        send(&mut output, &partial).await?;
        cancel(&interface, 99).await?;
        verify(
            status(&interface).await? == [4, 0, 1, 0x20],
            "wrong-id cancellation changed status",
        )?;
        cancel(&interface, 1).await?;
        verify(
            status(&interface).await? == [4, 0, 0x19, 0x20],
            "cancellation must report busy",
        )?;
        verify(
            status(&interface).await? == [4, 0, 1, 0x20],
            "cancellation must finish",
        )?;
        // Cancelling a queued DATA IN must discard both that data and its old response.
        send(&mut output, &Container::command(0x1001, 2, &[])?).await?;
        cancel(&interface, 2).await?;
        verify(
            status(&interface).await? == [4, 0, 0x19, 0x20],
            "queued cancellation must report busy",
        )?;
        verify(
            status(&interface).await? == [4, 0, 1, 0x20],
            "queued cancellation must finish",
        )?;
        send(&mut output, &Container::command(0x1001, 3, &[])?).await?;
        let data = receive(&mut input).await?;
        let (container, _) = Container::decode(&data)?;
        verify(
            container.kind == ContainerKind::Data && container.transaction_id == 3,
            "cancel left stale data",
        )?;
        let reply = receive(&mut input).await?;
        let (container, _) = Container::decode(&reply)?;
        verify(
            container.code == 0x2001 && container.transaction_id == 3,
            "cancel left a stale response",
        )?;
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    // Close before propagating a failed scenario; reset if sequencing made close unusable.
    let closed = async {
        send(&mut output, &Container::command(0x1003, 4, &[])?).await?;
        let bytes = receive(&mut input).await?;
        let (reply, _) = Container::decode(&bytes)?;
        verify(reply.code == 0x2001, "CloseSession rejected")?;
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    if result.is_err() || closed.is_err() {
        interface
            .control_out(
                ControlOut {
                    control_type: ControlType::Class,
                    recipient: Recipient::Interface,
                    request: 0x66,
                    value: 0,
                    index: 0,
                    data: &[],
                },
                LIMIT,
            )
            .await?;
    }
    result?;
    closed
}
