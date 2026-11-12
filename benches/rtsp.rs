//! Benchmarks for the RTSP request parser and packet fan-out.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tethercam::rtsp::message::{Limits, parse_incoming};
use tethercam::rtsp::source::ScriptedSource;
use tethercam::rtsp::{Fanout, MediaKind, Packet, TrackInfo};
use tokio::sync::Semaphore;

const REQUEST: &[u8] = b"SETUP rtsp://192.0.2.10/stream/trackID=0 RTSP/1.0\r\n\
CSeq: 4\r\nUser-Agent: bench/1.0\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\
Authorization: Digest username=\"camera\", realm=\"tethercam\", nonce=\"0123456789abcdef0123456789abcdef\", \
uri=\"rtsp://192.0.2.10/stream/trackID=0\", algorithm=SHA-256, qop=auth, nc=00000001, cnonce=\"0a4f113b\", \
response=\"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\"\r\n\r\n";

fn parse_request(c: &mut Criterion) {
    let limits = Limits::default();
    let mut group = c.benchmark_group("rtsp_parse");
    group.throughput(Throughput::Bytes(REQUEST.len() as u64));
    group.bench_function("setup_with_digest", |b| {
        b.iter(|| parse_incoming(black_box(REQUEST), &limits));
    });
    group.finish();
}

/// Publish a burst of 1200-byte packets and drain it from every subscriber.
fn fan_out(c: &mut Criterion) {
    const BURST: usize = 64;
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread().build() else {
        return;
    };
    let mut group = c.benchmark_group("rtsp_fanout");
    for clients in [1_usize, 8, 32] {
        let gate = Arc::new(Semaphore::new(0));
        let packet = Packet::new(
            std::iter::once(0x80_u8)
                .chain(std::iter::repeat_n(0, 1199))
                .collect::<Vec<u8>>(),
        );
        let track = TrackInfo {
            kind: MediaKind::Video,
            payload_type: 96,
            codec: "H264".into(),
            clock_rate: 90_000,
            fmtp: None,
            control: "trackID=0".into(),
        };
        let source = ScriptedSource::new(track, vec![packet], true, Some(Arc::clone(&gate)));
        let fanout = Fanout::new(Arc::new(source), BURST * 2);
        group.throughput(Throughput::Elements((BURST * clients) as u64));
        group.bench_with_input(BenchmarkId::from_parameter(clients), &clients, |b, &n| {
            b.iter(|| {
                runtime.block_on(async {
                    let mut subs: Vec<_> = (0..n).map(|_| fanout.subscribe()).collect();
                    gate.add_permits(BURST);
                    for sub in &mut subs {
                        for _ in 0..BURST {
                            black_box(sub.recv().await);
                        }
                    }
                });
            });
        });
    }
    group.finish();
}

criterion_group!(benches, parse_request, fan_out);
criterion_main!(benches);
