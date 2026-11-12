//! Benchmark for a full `GetObject`-style transaction through the simulated camera.
//!
//! Live view fetches one JPEG per frame, so the whole transaction is on the per-frame path.
//!
//! References:
//! - USB Still Image Capture Device Definition 1.0, sections 5-7

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tethercam::ptp::session::{Session, SessionConfig};
use tethercam::transport::sim::{Reply, SimCamera};

/// `GetObject` operation code.
const GET_OBJECT: u16 = 0x1009;

/// Size of a typical live-view JPEG.
const PAYLOAD: usize = 48 * 1024;

fn get_object(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread().build();
    assert!(runtime.is_ok(), "benchmark runtime must build");
    let Ok(runtime) = runtime else { return };
    let camera = SimCamera::new().with_operation(GET_OBJECT, Reply::data(vec![0xA5; PAYLOAD]));
    let mut session = Session::new(camera, SessionConfig::default());
    assert!(
        runtime.block_on(session.open()).is_ok(),
        "benchmark session must open"
    );

    let mut group = c.benchmark_group("ptp_session");
    group.throughput(Throughput::Bytes(PAYLOAD as u64));
    group.bench_function("get_object_48k", |b| {
        b.iter(|| {
            let outcome = runtime.block_on(session.transaction(GET_OBJECT, &[1], None));
            assert!(outcome.is_ok(), "GetObject must succeed");
            black_box(outcome)
        });
    });
    group.finish();
    assert!(
        runtime.block_on(session.close()).is_ok(),
        "benchmark session must close"
    );
}

criterion_group!(benches, get_object);
criterion_main!(benches);
