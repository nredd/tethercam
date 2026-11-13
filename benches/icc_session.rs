//! Benchmark for one live-view-sized transaction through the ICC bridge and session.
//!
//! Every live-view frame is a transaction, so the single-flight hand-off to the native owner
//! thread is on the per-frame path. The boundary here answers instantly, so this measures the
//! bridge, validation and copies, not `ImageCaptureCore`.
//!
//! References:
//! - `docs/macos-camera.md`

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tethercam::ptp::session::{PtpSession, SessionConfig};
use tethercam::transport::icc::{
    Boundary, IccDeviceId, IccError, IccReply, IccSession, IccTimeout, Lease, run_with,
};

/// `GetObject` operation code.
const GET_OBJECT: u16 = 0x1009;

/// Size of a typical live-view JPEG.
const PAYLOAD: usize = 48 * 1024;

struct Canned;

struct CannedLease;

impl Lease for CannedLease {
    fn transact(
        &mut self, _operation: u16, _params: &[u32], _outgoing: Vec<u8>, _timeout: IccTimeout,
    ) -> Result<IccReply, IccError> {
        Ok(IccReply {
            code: 0x2001,
            params: Vec::new(),
            data: vec![0xA5; PAYLOAD],
        })
    }

    fn close(self, _timeout: IccTimeout) -> Result<(), IccError> {
        Ok(())
    }

    fn poll_event(&mut self) -> Result<Option<Vec<u8>>, IccError> {
        Ok(None)
    }
}

impl Boundary for Canned {
    type Lease<'a> = CannedLease;

    fn discover(
        &mut self, _window: Duration, _timeout: IccTimeout,
    ) -> Result<Vec<IccDeviceId>, IccError> {
        Ok(Vec::new())
    }

    fn open(&mut self, _id: &IccDeviceId, _timeout: IccTimeout) -> Result<CannedLease, IccError> {
        Ok(CannedLease)
    }

    fn poll_event(&mut self) -> Result<Option<Vec<u8>>, IccError> {
        Ok(None)
    }
}

/// Time `iterations` transactions on a freshly opened session, excluding setup and close.
fn measure(iterations: u64) -> Duration {
    let device = IccDeviceId::new("synthetic-camera".into());
    assert!(device.is_ok(), "benchmark identifier must be valid");
    let Ok(device) = device else {
        return Duration::ZERO;
    };
    let measured = run_with(Canned, move |handle| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build();
        assert!(runtime.is_ok(), "benchmark runtime must build");
        let Ok(runtime) = runtime else {
            return Duration::ZERO;
        };
        let mut session = IccSession::new(handle, device, SessionConfig::default(), Instant::now);
        assert!(
            runtime.block_on(session.open()).is_ok(),
            "benchmark session must open"
        );
        let started = Instant::now();
        for _ in 0..iterations {
            let outcome = runtime.block_on(session.transaction(GET_OBJECT, &[1], None));
            assert!(outcome.is_ok(), "GetObject must succeed");
            let _ = black_box(outcome);
        }
        let elapsed = started.elapsed();
        assert!(
            runtime.block_on(session.close()).is_ok(),
            "benchmark session must close"
        );
        elapsed
    });
    assert!(measured.is_ok(), "benchmark owner must finish");
    measured.unwrap_or(Duration::ZERO)
}

fn get_object(c: &mut Criterion) {
    let mut group = c.benchmark_group("icc_session");
    group.throughput(Throughput::Bytes(PAYLOAD as u64));
    group.bench_function("get_object_48k", |b| b.iter_custom(measure));
    group.finish();
}

criterion_group!(benches, get_object);
criterion_main!(benches);
