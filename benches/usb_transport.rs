//! Benchmark for the receive path of `UsbTransport` over a fake in-memory port.
//!
//! Live view reads one JPEG per frame in bulk-in transfers, so aligned reads (caller buffer
//! is a whole number of packets) and unaligned reads (through the scratch buffer) are on the
//! per-frame path.
//!
//! References:
//! - USB Still Image Capture Device Definition 1.0, sections 5-7

use std::future::ready;
use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tethercam::transport::Endpoint;
use tethercam::transport::Transport;
use tethercam::transport::usb::{Port, PortError, UsbTransport};

/// Bulk-in packet size on USB 2.
const PACKET: usize = 512;

/// Bytes the camera delivers per read: a short final transfer, not a whole number of packets.
const DELIVERED: usize = 15 * 1024 + 7;

/// A port that delivers `DELIVERED` bytes per read.
struct FullPort;

impl Port for FullPort {
    fn max_packet_size(&self, _endpoint: Endpoint) -> usize {
        PACKET
    }

    fn bulk_out(&mut self, bytes: &[u8]) -> impl Future<Output = Result<usize, PortError>> + Send {
        ready(Ok(bytes.len()))
    }

    fn bulk_in(
        &mut self, bytes: &mut [u8],
    ) -> impl Future<Output = Result<usize, PortError>> + Send {
        let count = bytes.len().min(DELIVERED);
        bytes.iter_mut().take(count).for_each(|byte| *byte = 0xA5);
        ready(Ok(count))
    }

    fn interrupt_in(
        &mut self, _bytes: &mut [u8],
    ) -> impl Future<Output = Result<usize, PortError>> + Send {
        ready(Ok(0))
    }

    fn cancel(&mut self, _endpoint: Endpoint) -> impl Future<Output = ()> + Send {
        ready(())
    }

    fn clear_halt(
        &mut self, _endpoint: Endpoint,
    ) -> impl Future<Output = Result<(), PortError>> + Send {
        ready(Ok(()))
    }

    fn class_out(
        &mut self, _request: u8, _timeout: Duration,
    ) -> impl Future<Output = Result<(), PortError>> + Send {
        ready(Ok(()))
    }

    fn class_in(
        &mut self, _request: u8, _length: u16, _timeout: Duration,
    ) -> impl Future<Output = Result<Vec<u8>, PortError>> + Send {
        ready(Ok(Vec::new()))
    }

    fn reset_device(&mut self) -> impl Future<Output = Result<(), PortError>> + Send {
        ready(Ok(()))
    }
}

fn bulk_in(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build();
    assert!(runtime.is_ok(), "benchmark runtime must build");
    let Ok(runtime) = runtime else { return };
    let mut transport = UsbTransport::new(FullPort);
    let timeout = Duration::from_secs(5);

    let mut group = c.benchmark_group("usb_transport");
    for (name, len) in [
        ("bulk_in_aligned_16k", 16 * 1024),
        ("bulk_in_unaligned_16k", 16 * 1024 - 1),
    ] {
        let mut buffer = vec![0_u8; len];
        group.throughput(Throughput::Bytes(len as u64));
        group.bench_function(name, |b| {
            b.iter(|| {
                let read = runtime.block_on(transport.bulk_in(&mut buffer, timeout));
                assert!(read.is_ok(), "read must succeed");
                black_box(read)
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bulk_in);
criterion_main!(benches);
