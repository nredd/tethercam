//! Benchmarks for the RTP packetizers.
//!
//! Both run once per frame: a 48 KB live-view JPEG and a 40 KB H.264 access unit at MTU 1400.

use std::hint::black_box;

use criterion::{Criterion, Throughput};
use tethercam::rtp::h264::{H264Error, H264Packetizer};
use tethercam::rtp::jpeg::{JpegError, JpegPacketizer, PAYLOAD_TYPE};
use tethercam::rtp::{PacketPool, StreamConfig};
use thiserror::Error;

const MTU: usize = 1400;

fn config(payload_type: u8) -> StreamConfig {
    StreamConfig {
        payload_type,
        ssrc: 0x0102_0304,
        mtu: MTU,
        initial_sequence: 0,
    }
}

/// Synthetic baseline JPEG: two 64-byte tables, 4:2:2, 1024x680, `scan_len` scan bytes.
fn synthetic_jpeg(scan_len: usize) -> Vec<u8> {
    let mut jpeg = vec![0xff, 0xd8];
    for id in 0..2u8 {
        jpeg.extend_from_slice(&[0xff, 0xdb, 0x00, 0x43, id]);
        jpeg.extend((1..=64u8).map(|i| i.wrapping_mul(id + 1)));
    }
    jpeg.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 8, 0x02, 0xa8, 0x04, 0x00]);
    jpeg.extend_from_slice(&[3, 1, 0x21, 0, 2, 0x11, 1, 3, 0x11, 1]);
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

/// Bench setup failures. They fail the run instead of timing a broken packetizer.
#[derive(Debug, Error)]
enum BenchError {
    #[error("JPEG packetizer: {0}")]
    Jpeg(#[from] JpegError),
    #[error("H.264 packetizer: {0}")]
    H264(#[from] H264Error),
    #[error("{0} produced no packets")]
    NoPackets(&'static str),
}

fn main() -> Result<(), BenchError> {
    let jpeg = synthetic_jpeg(48 * 1024);
    let mut jpeg_packetizer = JpegPacketizer::new(config(PAYLOAD_TYPE))?;
    let mut jpeg_pool = PacketPool::with_capacity(64 * 1024, 64);
    jpeg_packetizer.packetize(&jpeg, 1, &mut jpeg_pool)?;
    if jpeg_pool.is_empty() {
        return Err(BenchError::NoPackets("JPEG"));
    }

    let au = synthetic_access_unit(40 * 1000);
    let mut h264_packetizer = H264Packetizer::new(config(96))?;
    let mut h264_pool = PacketPool::with_capacity(64 * 1024, 64);
    h264_packetizer.packetize(&au, 1, &mut h264_pool)?;
    if h264_pool.is_empty() {
        return Err(BenchError::NoPackets("H.264"));
    }

    let mut criterion = Criterion::default().configure_from_args();
    let mut group = criterion.benchmark_group("rtp_packetize");
    group.throughput(Throughput::Bytes(jpeg.len() as u64));
    group.bench_function("jpeg_48k_mtu1400", |b| {
        b.iter(|| jpeg_packetizer.packetize(black_box(&jpeg), black_box(1), &mut jpeg_pool));
    });
    group.throughput(Throughput::Bytes(au.len() as u64));
    group.bench_function("h264_40k_mtu1400", |b| {
        b.iter(|| h264_packetizer.packetize(black_box(&au), black_box(1), &mut h264_pool));
    });
    group.finish();
    criterion.final_summary();
    Ok(())
}
