//! A live-view acceptance probe for the real `ImageCaptureCore` path.
//!
//! The next step after [`read_only_sample`](super::read_only_sample): per cycle it runs the
//! production stack unchanged, [`IccSession`] under [`SonyCamera::run`] (the Sony handshake,
//! live-view readiness, frames, confirmed close), then decodes every frame with the production
//! JPEG decoder, and reopens on the same runtime for the next cycle. It writes nothing beyond
//! what the Sony live-view handshake already does (it takes host priority) and fires no shutter.
//! The [`LiveReceipt`] holds counts and image dimensions only: never a device identity, a frame
//! byte or a camera string, so it is safe to paste into shared logs.
//!
//! Running it against a real camera is a hardware action reserved to the hardware owner; see
//! `examples/icc_sample.rs`.
//!
//! References:
//! - `docs/macos-camera.md`, Evidence and validation

use std::io::{self, Write};
use std::time::Instant;

use thiserror::Error;

use super::{IccDeviceId, IccError, IccHandle, IccSession, IccTimeout, Selector};
use crate::media::{I420Frame, JpegDecoder};
use crate::ptp::session::SessionConfig;
use crate::sony::{SonyCamera, SonyConfig, SonyError};

/// Why a probe cycle failed. Every variant is a sanitized category.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProbeError {
    /// Discovery or selection failed.
    #[error("discovery or selection failed: {0}")]
    Discovery(#[from] IccError),
    /// The Sony handshake, live view or close failed.
    #[error("sony session failed: {0}")]
    Sony(#[from] SonyError),
    /// A live-view frame was not a decodable JPEG.
    #[error("live-view frame did not decode")]
    Decode,
}

impl ProbeError {
    /// A short static category for the receipt.
    #[must_use]
    pub const fn category(&self) -> &'static str {
        match self {
            Self::Discovery(_) => "discovery",
            Self::Sony(_) => "sony",
            Self::Decode => "decode",
        }
    }
}

/// What a probe observed. Counts and dimensions only.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LiveReceipt {
    /// Cameras discovery found.
    pub candidates: usize,
    /// Cycles attempted.
    pub cycles: usize,
    /// Live-view frames that decoded.
    pub frames: usize,
    /// Width and height of the first decoded frame.
    pub size: Option<(u32, u32)>,
    /// Decoded frames whose dimensions differed from the first.
    pub size_changes: usize,
    /// Sessions the Sony driver closed with confirmation.
    pub confirmed_closes: usize,
    /// Why the probe stopped early, if it did.
    pub failure: Option<ProbeError>,
}

impl LiveReceipt {
    /// Whether every cycle opened, decoded `frames` frames of one size and confirmed its close.
    #[must_use]
    pub fn passed(&self, cycles: usize, frames: usize) -> bool {
        self.failure.is_none()
            && self.cycles == cycles
            && self.confirmed_closes == cycles
            && self.frames == cycles.saturating_mul(frames)
            && self.size_changes == 0
    }

    /// Write one `key=value` line. Nothing identifying or frame-derived but dimensions.
    ///
    /// # Errors
    ///
    /// Any I/O error from `out`.
    pub fn write_to(&self, out: &mut dyn Write) -> io::Result<()> {
        let (width, height) = self.size.unwrap_or_default();
        write!(
            out,
            "candidates={} cycles={} frames={} size={width}x{height} size_changes={} confirmed_closes={}",
            self.candidates, self.cycles, self.frames, self.size_changes, self.confirmed_closes,
        )?;
        match &self.failure {
            None => writeln!(out, " result=ok"),
            Some(failure) => writeln!(out, " result=failed category={}", failure.category()),
        }
    }

    pub(super) fn note(&mut self, frame: &I420Frame) {
        let size = (frame.width(), frame.height());
        if *self.size.get_or_insert(size) != size {
            self.size_changes += 1;
        }
        self.frames += 1;
    }
}

/// One session: handshake, readiness, `frames` frames, confirmed close, then decode them.
async fn cycle(
    handle: IccHandle, device: &IccDeviceId, frames: usize, receipt: &mut LiveReceipt,
) -> Result<IccHandle, ProbeError> {
    let mut session = IccSession::new(
        handle,
        device.clone(),
        SessionConfig::default(),
        Instant::now,
    );
    receipt.cycles += 1;
    let jpegs = SonyCamera::run(&mut session, SonyConfig::default(), async |camera| {
        camera.wait_live_view_ready().await?;
        let mut jpegs = Vec::with_capacity(frames);
        for _ in 0..frames {
            jpegs.push(camera.next_live_view_frame().await?);
        }
        Ok(jpegs)
    })
    .await?;
    // `run` closes the session on every path and returns its error, so success is confirmation.
    receipt.confirmed_closes += 1;
    let mut decoder = JpegDecoder::new();
    let mut frame = I420Frame::default();
    for jpeg in &jpegs {
        decoder
            .decode_into(jpeg, &mut frame)
            .map_err(|_| ProbeError::Decode)?;
        receipt.note(&frame);
    }
    Ok(session.into_handle())
}

/// Run `cycles` live-view cycles of `frames` frames each on the one camera `selector` picks.
///
/// The first failure ends the probe, and the receipt records it.
pub async fn live_view_probe(
    mut handle: IccHandle, selector: &Selector, cycles: usize, frames: usize,
) -> LiveReceipt {
    let mut receipt = LiveReceipt::default();
    let found = match handle
        .discover(super::DISCOVERY_WINDOW, IccTimeout::default())
        .await
    {
        Ok(found) => found,
        Err(error) => {
            receipt.failure = Some(error.into());
            return receipt;
        }
    };
    receipt.candidates = found.len();
    let device = match selector.choose(&found) {
        Ok(device) => device,
        Err(error) => {
            receipt.failure = Some(error.into());
            return receipt;
        }
    };
    for _ in 0..cycles {
        match cycle(handle, &device, frames, &mut receipt).await {
            Ok(next) => handle = next,
            Err(error) => {
                receipt.failure = Some(error);
                break;
            }
        }
    }
    receipt
}
