//! Benchmarks for the PTP container codec.
//!
//! Live view moves one data container per frame, so decode cost is on the per-frame path.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tethercam::ptp::{Container, ContainerKind};

/// Payload sizes: a response, a typical live-view JPEG, a large live-view JPEG.
const PAYLOAD_SIZES: [usize; 3] = [20, 48 * 1024, 512 * 1024];

fn container_codec(c: &mut Criterion) {
    let mut group = c.benchmark_group("ptp_container");
    for size in PAYLOAD_SIZES {
        let payload = vec![0xA5_u8; size];
        let container = Container {
            kind: ContainerKind::Data,
            code: 0x1009,
            transaction_id: 42,
            payload: &payload,
        };
        let mut wire = Vec::new();
        if container.encode_into(&mut wire).is_err() {
            // Unreachable for these sizes: only payloads past `u32::MAX` fail to encode.
            continue;
        }

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("decode", size), &wire, |b, wire| {
            b.iter(|| Container::decode(black_box(wire)));
        });
        group.bench_with_input(
            BenchmarkId::new("encode", size),
            &container,
            |b, container| {
                let mut out = Vec::with_capacity(wire.len());
                b.iter(|| {
                    out.clear();
                    black_box(container).encode_into(&mut out)
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, container_codec);
criterion_main!(benches);
