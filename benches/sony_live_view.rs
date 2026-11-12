//! Benchmark for the live-view fetch loop against the simulated Sony body.
//!
//! One iteration is one `next_live_view_frame`: a `GetObject` transaction of about 46 KB (the
//! size measured on an a7 III) plus the header strip. This is the per-frame path of the whole
//! pipeline, so it is what the 15 fps budget (about 66 ms per frame) is spent on first.
//!
//! References:
//! - `docs/architecture.md`, "What the prototype established"

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tethercam::ptp::session::{Session, SessionConfig};
use tethercam::sony::{SonyCamera, SonyConfig};
use tethercam::transport::sim::sony::{SonySimConfig, sony_camera};

/// Size of a measured live-view JPEG.
const JPEG_LEN: usize = 46 * 1024;

fn live_view_fetch(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build();
    assert!(runtime.is_ok(), "benchmark runtime must build");
    let Ok(runtime) = runtime else { return };

    let mut jpeg = vec![0xFF, 0xD8];
    jpeg.resize(JPEG_LEN - 2, 0xA5);
    jpeg.extend_from_slice(&[0xFF, 0xD9]);
    let sim = SonySimConfig {
        live_view_jpeg: jpeg,
        warmup_reads: 0,
        ..SonySimConfig::default()
    };
    let mut session = Session::new(sony_camera(sim), SessionConfig::default());
    let config = SonyConfig {
        poll_interval: Duration::ZERO,
        ..SonyConfig::default()
    };

    let mut group = c.benchmark_group("sony_live_view");
    group.throughput(Throughput::Bytes(JPEG_LEN as u64));
    group.bench_function("next_live_view_frame_46k", |b| {
        b.iter_custom(|iterations| {
            let elapsed = runtime.block_on(SonyCamera::run(
                &mut session,
                config.clone(),
                async |camera| {
                    let start = Instant::now();
                    for _ in 0..iterations {
                        black_box(camera.next_live_view_frame().await?);
                    }
                    Ok(start.elapsed())
                },
            ));
            assert!(elapsed.is_ok(), "live-view loop must succeed");
            elapsed.unwrap_or_default()
        });
    });
    group.finish();
}

criterion_group!(benches, live_view_fetch);
criterion_main!(benches);
