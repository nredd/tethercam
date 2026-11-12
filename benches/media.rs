//! Benchmarks for the per-frame media path at the live-view size, 1024x680.
//!
//! Throughput is reported in elements per second, one element being one frame, so the
//! `thrpt` column reads as frames per second. The target is 15 fps. Setup failures exit with an
//! error instead of benchmarking nothing.

use std::cell::Cell;
use std::hint::black_box;

use criterion::{Criterion, Throughput};
use jpeg_encoder::{ColorType, Encoder as JpegEncoder, SamplingFactor};
use std::time::Duration;

use tethercam::media::{
    Encoder, EncoderConfig, I420Frame, JpegDecoder, ManualClock, Pacer, Profile,
    SoftwareH264Encoder,
};
use thiserror::Error;

const WIDTH: u16 = 1024;
const HEIGHT: u16 = 680;

/// A deterministic textured, moving synthetic scene as RGB.
fn scene_rgb(t: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_u32 ^ u32::try_from(t).unwrap_or(0);
    let mut rgb = Vec::with_capacity(usize::from(WIDTH) * usize::from(HEIGHT) * 3);
    for y in 0..usize::from(HEIGHT) {
        for x in 0..usize::from(WIDTH) {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = u8::try_from(state >> 29).unwrap_or(0);
            let r = u8::try_from(((x + 3 * t) / 3 + y / 5) % 200).unwrap_or(0) + noise;
            let g = u8::try_from((x / 4 + usize::midpoint(y, t)) % 180).unwrap_or(0) + noise;
            let b = u8::try_from((x + y + 5 * t) % 120).unwrap_or(0) + noise;
            rgb.extend_from_slice(&[r, g, b]);
        }
    }
    rgb
}

/// Why benchmark setup failed. Reported from `main` with a non-zero exit, never a silent skip.
#[derive(Debug, Error)]
enum BenchError {
    #[error("synthetic JPEG: {0}")]
    Jpeg(#[from] jpeg_encoder::EncodingError),
    #[error("decode: {0}")]
    Decode(#[from] tethercam::media::JpegError),
    #[error("frame: {0}")]
    Frame(#[from] tethercam::media::FrameError),
    #[error("encoder: {0}")]
    Encode(#[from] tethercam::media::EncodeError),
    #[error("pacer: {0}")]
    Pacer(#[from] tethercam::media::PacerError),
    /// Criterion discards the result of a timed closure, so failures are counted inside it.
    #[error("'{0}' timed iterations failed: the numbers above measure an error path")]
    TimedFailures(u64),
}

/// Failed iterations seen inside timed closures.
type Failures = Cell<u64>;

/// Count one failed timed iteration unless `ok`.
fn check(failures: &Failures, ok: bool) {
    if !ok {
        failures.set(failures.get().saturating_add(1));
    }
}

fn jpeg(rgb: &[u8], sampling: SamplingFactor) -> Result<Vec<u8>, BenchError> {
    let mut out = Vec::new();
    let mut enc = JpegEncoder::new(&mut out, 85);
    enc.set_sampling_factor(sampling);
    enc.encode(rgb, WIDTH, HEIGHT, ColorType::Rgb)?;
    Ok(out)
}

fn jpeg_decode(c: &mut Criterion, failures: &Failures) -> Result<(), BenchError> {
    let mut group = c.benchmark_group("jpeg_decode_1024x680");
    group.throughput(Throughput::Elements(1));
    let rgb = scene_rgb(0);
    for (name, sampling) in [
        ("4:2:0", SamplingFactor::F_2_2),
        ("4:2:2", SamplingFactor::F_2_1),
    ] {
        let data = jpeg(&rgb, sampling)?;
        let mut decoder = JpegDecoder::new();
        let mut frame = I420Frame::new(u32::from(WIDTH), u32::from(HEIGHT))?;
        decoder.decode_into(&data, &mut frame)?;
        group.bench_function(name, |b| {
            b.iter(|| {
                check(
                    failures,
                    decoder.decode_into(black_box(&data), &mut frame).is_ok(),
                );
            });
        });
    }
    group.finish();
    Ok(())
}

fn h264_encode(c: &mut Criterion, failures: &Failures) -> Result<(), BenchError> {
    let mut group = c.benchmark_group("h264_encode_1024x680");
    group.throughput(Throughput::Elements(1));
    let mut decoder = JpegDecoder::new();
    let frames = (0..8)
        .map(|t| {
            let mut frame = I420Frame::new(2, 2)?;
            decoder.decode_into(&jpeg(&scene_rgb(t), SamplingFactor::F_2_2)?, &mut frame)?;
            Ok(frame)
        })
        .collect::<Result<Vec<_>, BenchError>>()?;
    for (name, profile) in [("baseline", Profile::Baseline), ("main", Profile::Main)] {
        let config = EncoderConfig {
            profile,
            ..EncoderConfig::live_view()
        };
        let mut encoder = SoftwareH264Encoder::configure(config)?;
        let mut out = Vec::new();
        // Warm up and validate outside the timed loop.
        for frame in &frames {
            encoder.encode(frame, &mut out)?;
        }
        let mut next = frames.iter().cycle();
        group.bench_function(name, |b| {
            b.iter(|| {
                let ok = next
                    .next()
                    .is_some_and(|frame| encoder.encode(black_box(frame), &mut out).is_ok());
                check(failures, ok);
            });
        });
    }
    group.finish();
    Ok(())
}

/// One 15 fps slot of pacing: fresh (frame in, frame out), duplicate (no frame in) and
/// replacement (two frames in, one out), with the clock stepping one slot per iteration.
fn pacing(c: &mut Criterion, failures: &Failures) -> Result<(), BenchError> {
    let mut group = c.benchmark_group("pacer_15fps");
    group.throughput(Throughput::Elements(1));
    let period = Duration::from_nanos(1_000_000_000 / 15 + 1);
    for (name, pushes) in [("duplicate", 0_u32), ("fresh", 1), ("replacement", 2)] {
        let clock = ManualClock::new();
        let mut pacer = Pacer::new(15, clock.clone())?;
        pacer.push(0_u32);
        group.bench_function(name, |b| {
            b.iter(|| {
                clock.advance(period);
                for i in 0..pushes {
                    black_box(pacer.push(i));
                }
                check(
                    failures,
                    black_box(pacer.poll().map(|paced| paced.slot)).is_some(),
                );
            });
        });
    }
    group.finish();
    Ok(())
}

fn main() -> Result<(), BenchError> {
    let mut criterion = Criterion::default().configure_from_args();
    let failures = Failures::new(0);
    pacing(&mut criterion, &failures)?;
    jpeg_decode(&mut criterion, &failures)?;
    h264_encode(&mut criterion, &failures)?;
    criterion.final_summary();
    match failures.get() {
        0 => Ok(()),
        n => Err(BenchError::TimedFailures(n)),
    }
}
