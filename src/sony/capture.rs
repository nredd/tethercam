//! Still capture: press, release, wait for the image, download.
//!
//! The sequence is the one `libgphoto2` uses for pre-2020 bodies: half-press (`0xD2C1`),
//! full-press (`0xD2C2`), one look for the image while held, release full then half, wait for the
//! image if it has not appeared, then fetch the object at handle `0xFFFFC001`. The wait watches
//! two signals because either can be missed: the object-added event, and the object-in-memory
//! property (`0xD215`, `>= 0x8000` means an image waits). The camera crashes if the handle is
//! read before one of them says the image exists. Events queued before the press are drained
//! first so they cannot be mistaken for the new image.
//!
//! References:
//! - libgphoto2 `camlibs/ptp2/library.c`, Sony capture
//! - `watsonmw/alphawire` `src/aw/aw-control.c`, `AwControl_SetControlToggle`

use std::time::{Duration, Instant};

use super::camera::{SonyCamera, SonyEvent};
use super::capability::Capability;
use super::error::SonyError;
use super::{handle, op, prop};
use crate::ptp::session::PtpSession;
use crate::ptp::{ObjectInfo, Value};

/// Value of `0xD215` from which an image waits in camera memory.
const IMAGE_IN_MEMORY: i128 = 0x8000;

/// Control value that presses a button.
const PRESS: u16 = 2;

/// Control value that releases a button.
const RELEASE: u16 = 1;

/// A downloaded still.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedImage {
    /// The camera's description of the object (format, size, file name).
    pub info: ObjectInfo,
    /// The image bytes.
    pub data: Vec<u8>,
}

impl<T: PtpSession> SonyCamera<'_, T> {
    /// Captures a still and downloads it.
    ///
    /// Any image left in camera memory by an earlier capture is discarded first, since a
    /// stale one would be downloaded in place of the new one. The shutter is released on every
    /// path once it was pressed, even when waiting for the image fails.
    ///
    /// Right after connecting, property values read as zero for a few seconds, which hides a
    /// stale image. Call [`SonyCamera::wait_live_view_ready`] first on a body with live view.
    ///
    /// # Errors
    ///
    /// [`SonyError::CaptureUnsupported`] when the camera does not advertise remote capture,
    /// [`SonyError::CaptureTimeout`] when no image arrives, or a session or decoding error.
    pub async fn capture_still(&mut self) -> Result<CapturedImage, SonyError> {
        if !self.capabilities().supports(Capability::StillCapture) {
            return Err(SonyError::CaptureUnsupported);
        }
        self.discard_stale_image().await?;
        let held = self.press_and_look().await;
        let released = self.release_shutter().await;
        let appeared = held?;
        released?;
        if !appeared {
            self.wait_for_image().await?;
        }
        let info = self
            .session
            .transaction(op::GET_OBJECT_INFO, &[handle::CAPTURED_IMAGE], None)
            .await?;
        let info = ObjectInfo::decode(&info.data)?;
        let data = self.download_captured().await?;
        Ok(CapturedImage { info, data })
    }

    async fn download_captured(&mut self) -> Result<Vec<u8>, SonyError> {
        let outcome = self
            .session
            .transaction_within(
                self.config.download_timeout,
                op::GET_OBJECT,
                &[handle::CAPTURED_IMAGE],
                None,
            )
            .await?;
        Ok(outcome.data)
    }

    fn image_in_memory(&self) -> bool {
        self.properties
            .integer(prop::OBJECT_IN_MEMORY)
            .is_some_and(|value| value >= IMAGE_IN_MEMORY)
    }

    async fn discard_stale_image(&mut self) -> Result<(), SonyError> {
        self.refresh_properties().await?;
        if self.image_in_memory() {
            self.download_captured().await?;
        }
        // The stale image's notifications must not satisfy the wait for the new one.
        self.pump_events().await?;
        Ok(())
    }

    /// Presses half then full and checks once whether the image already appeared.
    async fn press_and_look(&mut self) -> Result<bool, SonyError> {
        self.control(prop::SHUTTER_HALF_PRESS, &Value::UInt16(PRESS))
            .await?;
        self.control(prop::SHUTTER_FULL_PRESS, &Value::UInt16(PRESS))
            .await?;
        self.check_image().await
    }

    /// Releases full then half press. Both are attempted so a failed first release does not
    /// leave the half-press held.
    async fn release_shutter(&mut self) -> Result<(), SonyError> {
        let full = self
            .control(prop::SHUTTER_FULL_PRESS, &Value::UInt16(RELEASE))
            .await;
        let half = self
            .control(prop::SHUTTER_HALF_PRESS, &Value::UInt16(RELEASE))
            .await;
        full.and(half)
    }

    /// One look: drains events, then re-reads the properties.
    async fn check_image(&mut self) -> Result<bool, SonyError> {
        self.check_image_within(None).await
    }

    async fn check_image_within(
        &mut self, budget: Option<(Instant, Duration)>,
    ) -> Result<bool, SonyError> {
        let (events, refreshed) = self.pump_events_inner(budget).await?;
        if events
            .iter()
            .any(|event| matches!(event, SonyEvent::ObjectAdded { .. }))
        {
            return Ok(true);
        }
        if !refreshed {
            if let Some((started, limit)) = budget
                && self.remaining(started, limit).is_zero()
            {
                return Err(SonyError::CaptureTimeout { timeout: limit });
            }
            self.refresh_properties().await?;
        }
        Ok(self.image_in_memory())
    }

    async fn wait_for_image(&mut self) -> Result<(), SonyError> {
        let started = self.session.now();
        let limit = self.config.capture_timeout;
        while !self.remaining(started, limit).is_zero() {
            match self.check_image_within(Some((started, limit))).await {
                Ok(true) => return Ok(()),
                Ok(false) => self.pause_within(started, limit).await,
                Err(err) => return Err(err),
            }
        }
        Err(SonyError::CaptureTimeout {
            timeout: self.config.capture_timeout,
        })
    }
}
