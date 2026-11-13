//! Shared synthetic camera factory for actor, CLI and pipeline tests.
//!
//! References:
//! - `docs/architecture.md`, CLI and supervisor contract

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use tokio::sync::watch;
use tracing::Dispatch;
use tracing::subscriber::NoSubscriber;

use super::{CameraFactory, OpenError};
use crate::transport::sim::{
    SimCamera,
    sony::{SonySimConfig, sony_camera},
};

/// Two process-lifetime dispatchers. `tracing-core` only consults every registered dispatcher
/// when callsite interest is computed while at least two exist; with exactly one, a callsite
/// first reached on a thread without a scoped default caches "never" and silences the scoped
/// capture of a concurrently running test. Keeping two alive makes capture independent of
/// test scheduling.
static ANCHORS: OnceLock<[Dispatch; 2]> = OnceLock::new();

/// Scoped capture of `tracing` output for one test, safe next to concurrently running tests.
pub(crate) struct LogCapture {
    file: tempfile::NamedTempFile,
    dispatch: Dispatch,
}

impl LogCapture {
    pub(crate) fn new() -> Self {
        ANCHORS.get_or_init(|| {
            [
                Dispatch::new(NoSubscriber::default()),
                Dispatch::new(NoSubscriber::default()),
            ]
        });
        let file = tempfile::NamedTempFile::new().unwrap();
        let dispatch = Dispatch::new(
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(file.reopen().unwrap())
                .finish(),
        );
        Self { file, dispatch }
    }

    /// Dispatcher to install with `with_subscriber` or `tracing::dispatcher::with_default`.
    pub(crate) fn dispatch(&self) -> Dispatch {
        self.dispatch.clone()
    }

    /// Everything captured so far.
    pub(crate) fn text(&self) -> String {
        std::fs::read_to_string(self.file.path()).unwrap()
    }
}

pub(crate) fn jpeg(width: u16, height: u16) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut encoder = Encoder::new(&mut bytes, 80);
    encoder.set_sampling_factor(SamplingFactor::F_2_2);
    encoder
        .encode(
            &vec![127; usize::from(width) * usize::from(height) * 3],
            width,
            height,
            ColorType::Rgb,
        )
        .unwrap();
    bytes
}

pub(crate) fn camera() -> SimCamera {
    sony_camera(SonySimConfig {
        warmup_reads: 0,
        live_view_jpeg: jpeg(32, 32),
        still: jpeg(64, 64),
        ..SonySimConfig::default()
    })
}

pub(crate) struct Factory {
    pub(crate) steps: VecDeque<Result<Option<SimCamera>, OpenError>>,
    pub(crate) released: Arc<Mutex<Vec<SimCamera>>>,
    pub(crate) stop_on_connect: Option<watch::Sender<bool>>,
    pub(crate) pauses: Arc<Mutex<Vec<Duration>>>,
}

impl Factory {
    pub(crate) fn new(cameras: Vec<SimCamera>) -> Self {
        Self {
            steps: cameras.into_iter().map(|camera| Ok(Some(camera))).collect(),
            released: Arc::default(),
            stop_on_connect: None,
            pauses: Arc::default(),
        }
    }
}

impl CameraFactory for Factory {
    type Camera = SimCamera;
    type Session = crate::ptp::session::Session<Self::Camera>;

    fn session(
        &self, camera: Self::Camera, mut cfg: crate::ptp::session::SessionConfig,
        clock: fn() -> std::time::Instant,
    ) -> Self::Session {
        let _ = &mut cfg;
        Self::Session::new(camera, cfg).with_clock(clock)
    }
    fn recover(&self, session: Self::Session) -> Self::Camera {
        session.into_transport()
    }

    fn connect(&mut self) -> impl Future<Output = Result<Option<SimCamera>, OpenError>> + Send {
        if let Some(stop) = &self.stop_on_connect {
            stop.send_replace(true);
        }
        std::future::ready(self.steps.pop_front().unwrap_or(Ok(None)))
    }
    fn release(&mut self, camera: SimCamera) {
        self.released.lock().unwrap().push(camera);
    }
    fn pause(&self, delay: Duration) -> impl Future<Output = ()> + Send {
        self.pauses.lock().unwrap().push(delay);
        tokio::time::sleep(delay)
    }
}

#[cfg(test)]
mod tests {
    use super::LogCapture;

    fn emit() {
        tracing::info!(event = "capture_probe");
    }

    /// A callsite first reached on a thread with no scoped dispatcher must still be captured.
    /// Run alone (e.g. under nextest) this is red without the process-lifetime anchors.
    #[test]
    fn capture_survives_a_callsite_first_registered_on_another_thread() {
        let logs = LogCapture::new();
        std::thread::spawn(emit).join().unwrap();
        tracing::dispatcher::with_default(&logs.dispatch(), emit);
        assert_eq!(logs.text().matches("capture_probe").count(), 1);
    }
}
