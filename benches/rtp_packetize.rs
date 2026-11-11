//! Benchmarks for the RTP packetizers.
//!
//! Both run once per frame: a 48 KB live-view JPEG and a 40 KB H.264 access unit at MTU 1400.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tethercam::rtp::h264::H264Packetizer;
use tethercam::rtp::jpeg::{JpegPacketizer, PAYLOAD_TYPE};
use tethercam::rtp::{PacketPool, StreamConfig};

const MTU: usize = 1400;

fn config(payload_type: u8) -> StreamConfig {
    StreamConfig {
        payload_type,
        ssrc: 0x0102_0304,
        mtu: MTU,
        initial_sequence: 0,
    }
}

/// Synthetic baseline JPEG: two 64-byte tables, 4:2:0, 1024x680, `scan_len` scan bytes.
fn synthetic_jpeg(scan_len: usize) -> Vec<u8> {
    let mut jpeg = vec![0xff, 0xd8];
    for id in 0..2u8 {
        jpeg.extend_from_slice(&[0xff, 0xdb, 0x00, 0x43, id]);
        jpeg.extend((1..=64u8).map(|i| i.wrapping_mul(id + 1)));
    }
    jpeg.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 8, 0x02, 0xa8, 0x04, 0x00]);
    jpeg.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    jpeg.extend_from_slice(&[0xff, 0xda, 0x00, 0x0c, 3, 1, 0, 2, 0x11, 3, 0x11, 0, 63, 0]);
    jpeg.extend((0..scan_len).map(|i| u8::try_from(i % 251).unwrap_or(0)));
    jpeg.extend_from_slice(&[0xff, 0xd9]);
    jpeg
}

/// Annex B access unit: SPS, PPS and one IDR slice of `nal_len` bytes.
fn synthetic_access_unit(nal_len: usize) -> Vec<u8> {
    let mut au = vec![
        0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80,
    ];
    au.extend_from_slice(&[0, 0, 0, 1, 0x65]);
    au.extend((0..nal_len).map(|i| u8::try_from(i % 250 + 1).unwrap_or(1)));
    au
}

fn packetize(c: &mut Criterion) {
    let mut group = c.benchmark_group("rtp_packetize");

    let jpeg = synthetic_jpeg(48 * 1024);
    group.throughput(Throughput::Bytes(jpeg.len() as u64));
    group.bench_function("jpeg_48k_mtu1400", |b| {
        let mut packetizer = JpegPacketizer::new(config(PAYLOAD_TYPE));
        let mut pool = PacketPool::with_capacity(64 * 1024, 64);
        let Ok(packetizer) = packetizer.as_mut() else {
            return;
        };
        b.iter(|| packetizer.packetize(black_box(&jpeg), black_box(1), &mut pool));
    });

    let au = synthetic_access_unit(40 * 1000);
    group.throughput(Throughput::Bytes(au.len() as u64));
    group.bench_function("h264_40k_mtu1400", |b| {
        let mut packetizer = H264Packetizer::new(config(96));
        let mut pool = PacketPool::with_capacity(64 * 1024, 64);
        let Ok(packetizer) = packetizer.as_mut() else {
            return;
        };
        b.iter(|| packetizer.packetize(black_box(&au), black_box(1), &mut pool));
    });

    group.finish();
}

criterion_group!(benches, packetize);
criterion_main!(benches);
