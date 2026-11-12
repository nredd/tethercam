//! Explicit opt-in simulated Sony factory, never enumerating USB.
//!
//! References:
//! - `docs/architecture.md`, testing

use std::time::Duration;

use super::Error;
use crate::media::{I420Frame, JpegDecoder};
use crate::supervisor::{CameraFactory, OpenError};
use crate::transport::sim::{
    SimCamera,
    sony::{SonySimConfig, sony_camera},
};

pub(super) struct Simulator(SonySimConfig);

impl Simulator {
    pub(super) fn new(jpeg: Vec<u8>) -> Result<Self, Error> {
        let mut frame = I420Frame::default();
        JpegDecoder::new()
            .decode_into(&jpeg, &mut frame)
            .map_err(|_| Error::Simulator)?;
        Ok(Self(SonySimConfig {
            live_view_jpeg: jpeg.clone(),
            still: jpeg,
            ..SonySimConfig::default()
        }))
    }
}

impl CameraFactory for Simulator {
    type Camera = SimCamera;

    fn connect(&mut self) -> impl Future<Output = Result<Option<SimCamera>, OpenError>> + Send {
        std::future::ready(Ok(Some(sony_camera(self.0.clone()))))
    }

    fn release(&mut self, camera: SimCamera) {
        drop(camera);
    }

    fn pause(&self, delay: Duration) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(delay)
    }
}
