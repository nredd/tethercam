//! Shared synthetic camera factory for actor, CLI and pipeline tests.
//!
//! References:
//! - `docs/architecture.md`, CLI and supervisor contract

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use tokio::sync::watch;

use super::{CameraFactory, OpenError};
use crate::transport::sim::{
    SimCamera,
    sony::{SonySimConfig, sony_camera},
};

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
