//! [`UsbTransport`]: the [`Transport`] implementation over any raw [`Port`].
//!
//! A [`Port`] is the smallest surface a USB stack must offer: one untimed transfer per call,
//! cancellation, clear-halt, and the two class requests. Everything with judgement lives here
//! so it is tested against a fake port:
//! - the per-operation timeout, and cancelling the transfer it abandons
//! - packet alignment of receive buffers (USB stacks require whole packets)
//! - short-write detection and oversized-transfer detection
//! - the recovery order of [`Transport::reset`]: class "Device Reset" then clear-halt on both
//!   bulk endpoints, all inside one deadline; a USB port reset only when the camera stalls the
//!   class request, after which the handles are invalid and the caller must reopen
//!
//! References:
//! - USB Still Image Capture Device Definition 1.0, sections 5-7 (bulk and interrupt pipes, Device Reset, Get Device Status)

use std::time::Duration;

use thiserror::Error;
use tokio::time::error::Elapsed;
use tokio::time::{Instant, timeout as bound, timeout_at as bound_at};

use super::status::{
    DeviceStatus, REQUEST_DEVICE_RESET, REQUEST_GET_DEVICE_STATUS, STATUS_REPLY_LEN,
};
use crate::transport::{Endpoint, Transport, TransportError};

/// An error from a raw [`Port`] operation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PortError {
    /// The endpoint is halted.
    #[error("endpoint stalled")]
    Stall,
    /// The device is gone.
    #[error("camera disconnected")]
    Disconnected,
    /// The transfer was cancelled, which is how a control transfer reports its timeout.
    #[error("transfer timed out")]
    Timeout,
    /// Anything else, with the operating system's description.
    #[error("{0}")]
    Other(String),
}

impl From<PortError> for TransportError {
    fn from(error: PortError) -> Self {
        match error {
            PortError::Stall => Self::Stall,
            PortError::Disconnected => Self::Disconnected,
            PortError::Timeout => Self::Timeout,
            PortError::Other(message) => Self::Other(message),
        }
    }
}

/// Raw operations on a claimed still-image interface.
///
/// Implementations do no timeout handling of their own for bulk and interrupt transfers: the
/// caller drops the future on timeout and then calls [`Port::cancel`], which must leave the
/// endpoint with nothing pending.
pub trait Port: Send {
    /// Maximum packet size of `endpoint`, in bytes.
    fn max_packet_size(&self, endpoint: Endpoint) -> usize;

    /// Send `bytes` on the bulk-out endpoint; returns how many bytes the device accepted.
    fn bulk_out(&mut self, bytes: &[u8]) -> impl Future<Output = Result<usize, PortError>> + Send;

    /// Fill `bytes` from the bulk-in endpoint. `bytes.len()` is a nonzero multiple of the
    /// packet size.
    fn bulk_in(
        &mut self, bytes: &mut [u8],
    ) -> impl Future<Output = Result<usize, PortError>> + Send;

    /// Fill `bytes` from the interrupt-in endpoint. `bytes.len()` is a nonzero multiple of
    /// the packet size.
    fn interrupt_in(
        &mut self, bytes: &mut [u8],
    ) -> impl Future<Output = Result<usize, PortError>> + Send;

    /// Cancel whatever is pending on `endpoint` and wait until it has returned.
    fn cancel(&mut self, endpoint: Endpoint) -> impl Future<Output = ()> + Send;

    /// Clear a halt on `endpoint`.
    fn clear_halt(
        &mut self, endpoint: Endpoint,
    ) -> impl Future<Output = Result<(), PortError>> + Send;

    /// Class request (interface recipient, zero value) with no data stage.
    fn class_out(
        &mut self, request: u8, timeout: Duration,
    ) -> impl Future<Output = Result<(), PortError>> + Send;

    /// Class request (interface recipient, zero value) reading up to `length` bytes.
    fn class_in(
        &mut self, request: u8, length: u16, timeout: Duration,
    ) -> impl Future<Output = Result<Vec<u8>, PortError>> + Send;

    /// Reset the whole device at the USB level.
    fn reset_device(&mut self) -> impl Future<Output = Result<(), PortError>> + Send;
}

/// Which receive endpoint a read targets.
#[derive(Clone, Copy)]
enum Inbound {
    Bulk,
    Interrupt,
}

impl Inbound {
    fn endpoint(self) -> Endpoint {
        match self {
            Self::Bulk => Endpoint::BulkIn,
            Self::Interrupt => Endpoint::InterruptIn,
        }
    }
}

/// A [`Transport`] over a claimed USB interface.
#[derive(Debug)]
pub struct UsbTransport<P: Port> {
    port: P,
}

impl<P: Port> UsbTransport<P> {
    /// Wrap a claimed interface.
    pub fn new(port: P) -> Self {
        Self { port }
    }

    /// The bulk-in packet size: 512 on USB 2, 1024 on USB 3. Use it for
    /// [`SessionConfig::max_packet_size`](crate::ptp::session::SessionConfig).
    pub fn bulk_max_packet_size(&self) -> usize {
        self.port.max_packet_size(Endpoint::BulkIn)
    }

    /// Ask the camera for its class-level status ("Get Device Status").
    ///
    /// # Errors
    ///
    /// A [`TransportError`] if the request fails, or [`TransportError::Other`] for a
    /// malformed reply.
    pub async fn device_status(
        &mut self, timeout: Duration,
    ) -> Result<DeviceStatus, TransportError> {
        let reply = self
            .port
            .class_in(REQUEST_GET_DEVICE_STATUS, STATUS_REPLY_LEN, timeout)
            .await?;
        DeviceStatus::parse(&reply).map_err(|error| TransportError::Other(error.to_string()))
    }

    /// Resolve a bounded operation: map its error, or cancel the transfer it abandoned.
    async fn settle<T>(
        &mut self, endpoint: Endpoint, outcome: Result<Result<T, PortError>, Elapsed>,
    ) -> Result<T, TransportError> {
        if let Ok(result) = outcome {
            return result.map_err(Into::into);
        }
        self.port.cancel(endpoint).await;
        Err(TransportError::Timeout)
    }

    async fn fetch(&mut self, inbound: Inbound, bytes: &mut [u8]) -> Result<usize, PortError> {
        match inbound {
            Inbound::Bulk => self.port.bulk_in(bytes).await,
            Inbound::Interrupt => self.port.interrupt_in(bytes).await,
        }
    }

    /// Receive into `bytes`, through an aligned scratch buffer when `bytes` is not already a
    /// whole number of packets.
    async fn receive(
        &mut self, inbound: Inbound, bytes: &mut [u8], timeout: Duration,
    ) -> Result<usize, TransportError> {
        let endpoint = inbound.endpoint();
        let packet = self.port.max_packet_size(endpoint).max(1);
        if !bytes.is_empty() && bytes.len().is_multiple_of(packet) {
            let outcome = bound(timeout, self.fetch(inbound, bytes)).await;
            return self.settle(endpoint, outcome).await;
        }
        let mut scratch = vec![0_u8; bytes.len().max(1).next_multiple_of(packet)];
        let outcome = bound(timeout, self.fetch(inbound, &mut scratch)).await;
        let received = self.settle(endpoint, outcome).await?;
        match (bytes.get_mut(..received), scratch.get(..received)) {
            (Some(destination), Some(source)) => {
                destination.copy_from_slice(source);
                Ok(received)
            }
            _ => Err(TransportError::Other(format!(
                "device sent {received} bytes into a {} byte buffer",
                bytes.len()
            ))),
        }
    }
}

impl<P: Port> Transport for UsbTransport<P> {
    async fn bulk_out(&mut self, bytes: &[u8], timeout: Duration) -> Result<(), TransportError> {
        let outcome = bound(timeout, self.port.bulk_out(bytes)).await;
        let written = self.settle(Endpoint::BulkOut, outcome).await?;
        if written == bytes.len() {
            Ok(())
        } else {
            Err(TransportError::Other(format!(
                "short write: {written} of {} bytes",
                bytes.len()
            )))
        }
    }

    async fn bulk_in(
        &mut self, bytes: &mut [u8], timeout: Duration,
    ) -> Result<usize, TransportError> {
        self.receive(Inbound::Bulk, bytes, timeout).await
    }

    async fn interrupt_in(
        &mut self, bytes: &mut [u8], timeout: Duration,
    ) -> Result<usize, TransportError> {
        self.receive(Inbound::Interrupt, bytes, timeout).await
    }

    async fn clear_halt(
        &mut self, endpoint: Endpoint, timeout: Duration,
    ) -> Result<(), TransportError> {
        bound(timeout, self.port.clear_halt(endpoint))
            .await
            .map_err(timed_out)?
            .map_err(Into::into)
    }

    /// Class "Device Reset", then clear-halt on both bulk endpoints, all within one `timeout`.
    ///
    /// When the camera stalls the class request, a USB-level
    /// port reset stands in for it. That invalidates the open handles, so the transport
    /// reports [`TransportError::Disconnected`] afterwards: the caller must reopen the camera.
    /// An unrepresentable deadline returns [`TransportError::Other`] before any port I/O.
    async fn reset(&mut self, timeout: Duration) -> Result<(), TransportError> {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            TransportError::Other("USB reset deadline exceeds the clock range".into())
        })?;
        let class = bound_at(deadline, self.port.class_out(REQUEST_DEVICE_RESET, timeout)).await;
        match class.map_err(timed_out)? {
            Ok(()) => {}
            Err(PortError::Stall) => {
                bound_at(deadline, self.port.reset_device())
                    .await
                    .map_err(timed_out)??;
                return Err(TransportError::Disconnected);
            }
            Err(other) => return Err(other.into()),
        }
        self.clear_halt(Endpoint::BulkIn, remaining(deadline))
            .await?;
        self.clear_halt(Endpoint::BulkOut, remaining(deadline))
            .await
    }
}

fn timed_out(_: Elapsed) -> TransportError {
    TransportError::Timeout
}

fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

#[cfg(test)]
pub(crate) mod fakes {
    use std::collections::VecDeque;

    use super::*;

    /// One scripted answer for a receive.
    pub(crate) enum Reply {
        /// Deliver these bytes.
        Data(Vec<u8>),
        /// Fail.
        Fail(PortError),
        /// Never complete, as a camera that stops answering.
        Hang,
    }

    /// One scripted answer for a send; an empty script accepts everything.
    pub(crate) enum Written {
        /// Accept only this many bytes.
        Accept(usize),
        /// Fail.
        Fail(PortError),
        /// Never complete.
        Hang,
    }

    /// What the transport asked of the port.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum Call {
        BulkOut(Vec<u8>),
        BulkIn(usize),
        InterruptIn(usize),
        Cancel(Endpoint),
        ClearHalt(Endpoint),
        ClassOut(u8),
        ClassIn(u8, u16),
        ResetDevice,
    }

    /// A scripted [`Port`]; an empty script answers success.
    #[derive(Default)]
    pub(crate) struct FakePort {
        pub(crate) packet: usize,
        pub(crate) calls: Vec<Call>,
        pub(crate) outs: VecDeque<Written>,
        pub(crate) ins: VecDeque<Reply>,
        pub(crate) interrupts: VecDeque<Reply>,
        pub(crate) halts: VecDeque<Result<(), PortError>>,
        pub(crate) class_outs: VecDeque<Result<(), PortError>>,
        pub(crate) class_ins: VecDeque<Result<Vec<u8>, PortError>>,
        pub(crate) resets: VecDeque<Result<(), PortError>>,
        pub(crate) hang_halts: bool,
        pub(crate) hang_class: bool,
        pub(crate) hang_resets: bool,
    }

    impl FakePort {
        pub(crate) fn new(packet: usize) -> Self {
            Self {
                packet,
                ..Self::default()
            }
        }
    }

    async fn deliver(reply: Option<Reply>, bytes: &mut [u8]) -> Result<usize, PortError> {
        match reply.unwrap_or(Reply::Data(Vec::new())) {
            Reply::Data(data) => {
                let count = data.len().min(bytes.len());
                bytes[..count].copy_from_slice(&data[..count]);
                Ok(data.len())
            }
            Reply::Fail(error) => Err(error),
            Reply::Hang => std::future::pending().await,
        }
    }

    impl Port for FakePort {
        fn max_packet_size(&self, _endpoint: Endpoint) -> usize {
            self.packet
        }

        async fn bulk_out(&mut self, bytes: &[u8]) -> Result<usize, PortError> {
            self.calls.push(Call::BulkOut(bytes.to_vec()));
            match self.outs.pop_front() {
                Some(Written::Accept(count)) => Ok(count),
                Some(Written::Fail(error)) => Err(error),
                Some(Written::Hang) => std::future::pending().await,
                None => Ok(bytes.len()),
            }
        }

        async fn bulk_in(&mut self, bytes: &mut [u8]) -> Result<usize, PortError> {
            self.calls.push(Call::BulkIn(bytes.len()));
            deliver(self.ins.pop_front(), bytes).await
        }

        async fn interrupt_in(&mut self, bytes: &mut [u8]) -> Result<usize, PortError> {
            self.calls.push(Call::InterruptIn(bytes.len()));
            deliver(self.interrupts.pop_front(), bytes).await
        }

        async fn cancel(&mut self, endpoint: Endpoint) {
            self.calls.push(Call::Cancel(endpoint));
        }

        fn clear_halt(
            &mut self, endpoint: Endpoint,
        ) -> impl Future<Output = Result<(), PortError>> + Send {
            self.calls.push(Call::ClearHalt(endpoint));
            let hang = self.hang_halts;
            let result = self.halts.pop_front().unwrap_or(Ok(()));
            async move {
                if hang {
                    std::future::pending().await
                } else {
                    result
                }
            }
        }

        fn class_out(
            &mut self, request: u8, _timeout: Duration,
        ) -> impl Future<Output = Result<(), PortError>> + Send {
            self.calls.push(Call::ClassOut(request));
            let hang = self.hang_class;
            let result = self.class_outs.pop_front().unwrap_or(Ok(()));
            async move {
                if hang {
                    std::future::pending().await
                } else {
                    result
                }
            }
        }

        fn class_in(
            &mut self, request: u8, length: u16, _timeout: Duration,
        ) -> impl Future<Output = Result<Vec<u8>, PortError>> + Send {
            self.calls.push(Call::ClassIn(request, length));
            let reply = self
                .class_ins
                .pop_front()
                .unwrap_or(Ok(vec![4, 0, 1, 0x20]));
            std::future::ready(reply)
        }

        fn reset_device(&mut self) -> impl Future<Output = Result<(), PortError>> + Send {
            self.calls.push(Call::ResetDevice);
            let hang = self.hang_resets;
            let result = self.resets.pop_front().unwrap_or(Ok(()));
            async move {
                if hang {
                    std::future::pending().await
                } else {
                    result
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::{Call, FakePort, Reply, Written};
    use super::*;

    const LONG: Duration = Duration::from_secs(30);
    const SHORT: Duration = Duration::from_millis(5);

    fn transport(port: FakePort) -> UsbTransport<FakePort> {
        UsbTransport::new(port)
    }

    #[test]
    fn port_errors_map_onto_transport_errors() {
        assert_eq!(
            TransportError::from(PortError::Stall),
            TransportError::Stall
        );
        assert_eq!(
            TransportError::from(PortError::Disconnected),
            TransportError::Disconnected
        );
        assert_eq!(
            TransportError::from(PortError::Other("boom".into())),
            TransportError::Other("boom".into())
        );
        assert_eq!(PortError::Stall.to_string(), "endpoint stalled");
        assert_eq!(PortError::Disconnected.to_string(), "camera disconnected");
        assert_eq!(
            TransportError::from(PortError::Timeout),
            TransportError::Timeout
        );
        assert_eq!(PortError::Timeout.to_string(), "transfer timed out");
        assert_eq!(PortError::Other("boom".into()).to_string(), "boom");
    }

    #[test]
    fn reports_bulk_packet_size() {
        assert_eq!(transport(FakePort::new(1024)).bulk_max_packet_size(), 1024);
    }

    #[tokio::test]
    async fn bulk_out_sends_the_whole_slice_including_zero_length() {
        let mut usb = transport(FakePort::new(512));
        usb.bulk_out(&[1, 2, 3], LONG).await.expect("send");
        usb.bulk_out(&[], LONG).await.expect("zlp");
        assert_eq!(
            usb.port.calls,
            vec![Call::BulkOut(vec![1, 2, 3]), Call::BulkOut(vec![])]
        );
    }

    #[tokio::test]
    async fn bulk_out_rejects_partial_writes_and_maps_errors() {
        let mut port = FakePort::new(512);
        port.outs
            .extend([Written::Accept(2), Written::Fail(PortError::Stall)]);
        let mut usb = transport(port);
        assert_eq!(
            usb.bulk_out(&[1, 2, 3], LONG).await,
            Err(TransportError::Other("short write: 2 of 3 bytes".into()))
        );
        assert_eq!(usb.bulk_out(&[1], LONG).await, Err(TransportError::Stall));
    }

    #[tokio::test]
    async fn bulk_out_timeout_cancels_the_transfer() {
        let mut port = FakePort::new(512);
        port.outs.push_back(Written::Hang);
        let mut usb = transport(port);
        assert_eq!(
            usb.bulk_out(&[1], SHORT).await,
            Err(TransportError::Timeout)
        );
        assert_eq!(
            usb.port.calls,
            vec![Call::BulkOut(vec![1]), Call::Cancel(Endpoint::BulkOut)]
        );
    }

    #[tokio::test]
    async fn aligned_receive_uses_the_caller_buffer() {
        let mut port = FakePort::new(4);
        port.ins.push_back(Reply::Data(vec![9, 8, 7]));
        let mut usb = transport(port);
        let mut bytes = [0_u8; 8];
        assert_eq!(usb.bulk_in(&mut bytes, LONG).await, Ok(3));
        assert_eq!(&bytes[..3], &[9, 8, 7]);
        assert_eq!(usb.port.calls, vec![Call::BulkIn(8)]);
    }

    #[tokio::test]
    async fn unaligned_receive_goes_through_an_aligned_scratch_buffer() {
        let mut port = FakePort::new(4);
        port.ins.push_back(Reply::Data(vec![1, 2, 3]));
        port.interrupts.push_back(Reply::Data(vec![5]));
        let mut usb = transport(port);
        let mut bytes = [0_u8; 6];
        assert_eq!(usb.bulk_in(&mut bytes, LONG).await, Ok(3));
        assert_eq!(&bytes[..3], &[1, 2, 3]);
        assert_eq!(usb.interrupt_in(&mut bytes, LONG).await, Ok(1));
        assert_eq!(usb.port.calls, vec![Call::BulkIn(8), Call::InterruptIn(8)]);
    }

    #[tokio::test]
    async fn zero_length_receive_reads_one_packet() {
        let mut port = FakePort::new(4);
        port.ins.push_back(Reply::Data(vec![]));
        let mut usb = transport(port);
        assert_eq!(usb.bulk_in(&mut [], LONG).await, Ok(0));
        assert_eq!(usb.port.calls, vec![Call::BulkIn(4)]);
    }

    #[tokio::test]
    async fn oversized_transfer_is_an_error() {
        let mut port = FakePort::new(4);
        port.ins.push_back(Reply::Data(vec![0; 7]));
        let mut usb = transport(port);
        let error = usb
            .bulk_in(&mut [0_u8; 6], LONG)
            .await
            .expect_err("oversized");
        assert_eq!(
            error,
            TransportError::Other("device sent 7 bytes into a 6 byte buffer".into())
        );
    }

    #[tokio::test]
    async fn receive_maps_stalls_and_disconnects() {
        let mut port = FakePort::new(4);
        port.ins.push_back(Reply::Fail(PortError::Stall));
        port.interrupts
            .push_back(Reply::Fail(PortError::Disconnected));
        let mut usb = transport(port);
        assert_eq!(
            usb.bulk_in(&mut [0_u8; 4], LONG).await,
            Err(TransportError::Stall)
        );
        assert_eq!(
            usb.interrupt_in(&mut [0_u8; 4], LONG).await,
            Err(TransportError::Disconnected)
        );
    }

    #[tokio::test]
    async fn receive_timeout_cancels_the_right_endpoint() {
        let mut port = FakePort::new(4);
        port.ins.push_back(Reply::Hang);
        port.interrupts.push_back(Reply::Hang);
        let mut usb = transport(port);
        assert_eq!(
            usb.bulk_in(&mut [0_u8; 4], SHORT).await,
            Err(TransportError::Timeout)
        );
        assert_eq!(
            usb.interrupt_in(&mut [0_u8; 3], SHORT).await,
            Err(TransportError::Timeout)
        );
        assert_eq!(
            usb.port.calls,
            vec![
                Call::BulkIn(4),
                Call::Cancel(Endpoint::BulkIn),
                Call::InterruptIn(4),
                Call::Cancel(Endpoint::InterruptIn),
            ]
        );
    }

    #[tokio::test]
    async fn clear_halt_maps_errors_and_times_out() {
        let mut port = FakePort::new(4);
        port.halts.push_back(Err(PortError::Stall));
        let mut usb = transport(port);
        assert_eq!(
            usb.clear_halt(Endpoint::BulkIn, LONG).await,
            Err(TransportError::Stall)
        );
        assert_eq!(usb.clear_halt(Endpoint::InterruptIn, LONG).await, Ok(()));
        assert_eq!(
            usb.port.calls,
            vec![
                Call::ClearHalt(Endpoint::BulkIn),
                Call::ClearHalt(Endpoint::InterruptIn)
            ]
        );
    }

    #[tokio::test]
    async fn reset_rejects_unrepresentable_deadline_before_io_and_remains_usable() {
        let mut usb = transport(FakePort::new(4));
        assert_eq!(
            usb.reset(Duration::MAX).await,
            Err(TransportError::Other(
                "USB reset deadline exceeds the clock range".into()
            ))
        );
        assert_eq!(usb.port.calls, [] as [Call; 0]);
        usb.reset(LONG).await.expect("subsequent reset");
        assert_eq!(
            usb.port.calls,
            vec![
                Call::ClassOut(0x66),
                Call::ClearHalt(Endpoint::BulkIn),
                Call::ClearHalt(Endpoint::BulkOut)
            ]
        );
    }

    #[tokio::test]
    async fn reset_sends_class_reset_then_clears_both_bulk_halts() {
        let mut usb = transport(FakePort::new(4));
        usb.reset(LONG).await.expect("reset");
        assert_eq!(
            usb.port.calls,
            vec![
                Call::ClassOut(0x66),
                Call::ClearHalt(Endpoint::BulkIn),
                Call::ClearHalt(Endpoint::BulkOut),
            ]
        );
    }

    #[tokio::test]
    async fn refused_class_reset_falls_back_to_a_port_reset_and_requires_reopening() {
        let mut port = FakePort::new(4);
        port.class_outs.push_back(Err(PortError::Stall));
        let mut usb = transport(port);
        assert_eq!(usb.reset(LONG).await, Err(TransportError::Disconnected));
        assert_eq!(
            usb.port.calls,
            vec![Call::ClassOut(0x66), Call::ResetDevice]
        );
    }

    #[tokio::test]
    async fn reset_failure_modes() {
        let mut port = FakePort::new(4);
        port.class_outs.extend([
            Err(PortError::Disconnected),
            Err(PortError::Timeout),
            Err(PortError::Other("fault".into())),
            Err(PortError::Stall),
            Ok(()),
            Ok(()),
        ]);
        port.resets.push_back(Err(PortError::Other("gone".into())));
        port.halts
            .extend([Err(PortError::Stall), Ok(()), Err(PortError::Stall)]);
        let mut usb = transport(port);
        assert_eq!(usb.reset(LONG).await, Err(TransportError::Disconnected));
        // only a stall triggers the port reset: other failures are reported as they are
        assert_eq!(usb.reset(LONG).await, Err(TransportError::Timeout));
        assert_eq!(
            usb.reset(LONG).await,
            Err(TransportError::Other("fault".into()))
        );
        // class request stalled, port reset fails
        assert_eq!(
            usb.reset(LONG).await,
            Err(TransportError::Other("gone".into()))
        );
        // class request fine, clear-halt of bulk-in stalls
        assert_eq!(usb.reset(LONG).await, Err(TransportError::Stall));
        // class request fine, bulk-in ok, bulk-out stalls
        assert_eq!(usb.reset(LONG).await, Err(TransportError::Stall));
    }

    #[tokio::test]
    async fn reset_steps_time_out() {
        // A hanging port reset is bounded by the timeout.
        let mut port = FakePort::new(4);
        port.hang_halts = true;
        port.hang_resets = true;
        let mut hanging = UsbTransport::new(port);
        assert_eq!(
            hanging.clear_halt(Endpoint::BulkIn, SHORT).await,
            Err(TransportError::Timeout)
        );
        assert_eq!(hanging.reset(SHORT).await, Err(TransportError::Timeout));
        hanging.port.class_outs.push_back(Err(PortError::Stall));
        assert_eq!(hanging.reset(SHORT).await, Err(TransportError::Timeout));

        hanging.port.hang_class = true;
        assert_eq!(hanging.reset(SHORT).await, Err(TransportError::Timeout));
    }

    #[tokio::test]
    async fn device_status_parses_and_maps_errors() {
        let mut port = FakePort::new(4);
        port.class_ins.extend([
            Ok(vec![8, 0, 0x19, 0x20, 0x81, 0, 0, 0]),
            Ok(vec![1]),
            Err(PortError::Stall),
        ]);
        let mut usb = transport(port);
        let status = usb.device_status(LONG).await.expect("status");
        assert_eq!((status.code, status.parameters), (0x2019, vec![0x81]));
        assert_eq!(
            usb.device_status(LONG).await,
            Err(TransportError::Other(
                "malformed device status reply".into()
            ))
        );
        assert_eq!(usb.device_status(LONG).await, Err(TransportError::Stall));
        assert_eq!(usb.port.calls.first(), Some(&Call::ClassIn(0x67, 32)));
    }
}
