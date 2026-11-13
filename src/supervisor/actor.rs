//! Camera session ownership, reconnect and serialized snapshot requests.
//!
//! References:
//! - `docs/architecture.md`, Sony driver contract

#[cfg(test)]
#[path = "actor_tests.rs"]
mod tests;

use std::fmt;
use std::sync::{Arc, mpsc as blocking};
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::{mpsc, watch};

use crate::onvif::service::{SnapshotError, SnapshotSource};
use crate::ptp::session::{PtpSession, SessionConfig};
use crate::sony::{Battery, CapturedImage, SonyCamera, SonyConfig, SonyError};

/// Sanitized transport-factory failures. Each category is safe to log; device strings, USB
/// serials and operating-system error text deliberately do not cross this boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum OpenError {
    /// The interface cannot be claimed by this account.
    #[error("USB permission denied")]
    Permission,
    /// The selected body is not in remote-control mode.
    #[error("camera is not in PC Remote mode")]
    Mode,
    /// Another application owns the selected camera.
    #[error("camera is already in use")]
    Busy,
    /// The device disappeared during enumeration or opening.
    #[error("camera disconnected")]
    Disconnected,
    /// Enumeration, descriptor validation or another native setup operation failed.
    #[error("camera discovery failed")]
    Discovery,
}

/// A transport factory. Connecting must not open a PTP session; only the actor does that.
/// `pause` is a cancellation-safe retry timer, injectable for deterministic reconnect tests.
pub trait CameraFactory: Send {
    /// Acquired handle, also used by protocol tests.
    type Camera: Send;
    /// Session constructed around the acquired handle.
    type Session: PtpSession;
    /// Find and open the selected USB interface, or report that it is absent.
    fn connect(&mut self) -> impl Future<Output = Result<Option<Self::Camera>, OpenError>> + Send;
    /// Construct an unopened session with the requested limits and monotonic clock.
    fn session(
        &self, camera: Self::Camera, cfg: SessionConfig, clock: fn() -> Instant,
    ) -> Self::Session;
    /// Recover the acquired handle after the session has been closed.
    fn recover(&self, session: Self::Session) -> Self::Camera;
    /// Dispose of a closed/disconnected transport. Tests can inspect its command transcript.
    fn release(&mut self, camera: Self::Camera);
    /// Wait before another connection attempt.
    fn pause(&self, delay: Duration) -> impl Future<Output = ()> + Send;
}

/// Safe, nonidentifying camera telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraStatus {
    /// Waiting for a usable camera; no stale battery reading is exposed.
    Disconnected,
    /// Connected and warmed up.
    Ready {
        /// Battery and USB power, if available.
        battery: Option<Battery>,
    },
    /// Shutdown completed, including the session close attempt.
    Stopped,
}

/// Actor configuration errors.
#[derive(Debug, Error)]
pub enum SupervisorError {
    /// Zero or unbounded frame intervals would spin or overflow timers.
    #[error("frame interval must be positive and at most one second")]
    FrameInterval,
}

type SnapshotReply = blocking::SyncSender<Result<Vec<u8>, SnapshotError>>;

/// Handles shared with the network and media tasks. Debug never exposes image bytes.
#[derive(Clone)]
pub struct Control {
    pub(super) demand: watch::Sender<[u64; 2]>,
    pub(super) frames: watch::Receiver<Option<Arc<[u8]>>>,
    snapshots: mpsc::Sender<SnapshotReply>,
    pub(super) stop: watch::Sender<bool>,
    status: watch::Receiver<CameraStatus>,
}

impl fmt::Debug for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Control")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl Control {
    /// Request an orderly stop. The current camera transaction and any shutter release finish.
    pub fn shutdown(&self) {
        self.stop.send_replace(true);
    }

    /// Most recent battery/power state, without device identity.
    #[must_use]
    pub fn status(&self) -> CameraStatus {
        *self.status.borrow()
    }

    pub(super) fn stopping(&self) -> bool {
        *self.stop.borrow()
    }
}

impl SnapshotSource for Control {
    fn snapshot(&self, _profile_token: &str) -> Result<Vec<u8>, SnapshotError> {
        self.request_snapshot(Duration::from_secs(360))
    }
}

impl Control {
    fn request_snapshot(&self, timeout: Duration) -> Result<Vec<u8>, SnapshotError> {
        let (tx, rx) = blocking::sync_channel(1);
        self.snapshots.try_send(tx).map_err(|_| SnapshotError)?;
        // This is only a caller reply budget, not a camera deadline. Expiry abandons the
        // reply, never shutter release, a transaction or the driver's long still download.
        rx.recv_timeout(timeout).map_err(|_| SnapshotError)?
    }
}

/// Check PTP's EXIF/JPEG object type and SOI without decoding full sensor-sized images.
fn snapshot_image(image: CapturedImage) -> Result<Vec<u8>, SnapshotError> {
    if image.info.object_format != 0x3801 || !image.data.starts_with(&[0xff, 0xd8]) {
        tracing::warn!(event = "snapshot_format_rejected");
        return Err(SnapshotError);
    }
    Ok(image.data)
}

/// The sole owner of PTP I/O. Run to completion; do not abort its task or drop its future.
pub struct Supervisor<F: CameraFactory> {
    factory: F,
    sony: SonyConfig,
    session: SessionConfig,
    clock: fn() -> Instant,
    frame_interval: Duration,
    demand: watch::Receiver<[u64; 2]>,
    snapshots: mpsc::Receiver<SnapshotReply>,
    frames: watch::Sender<Option<Arc<[u8]>>>,
    status: watch::Sender<CameraStatus>,
    stop: watch::Receiver<bool>,
}

impl<F: CameraFactory> Supervisor<F> {
    /// Construct bounded actor channels. Nothing is opened until [`Self::run`].
    ///
    /// # Errors
    /// Rejects a zero or greater-than-one-second frame interval.
    pub fn new(
        factory: F, sony: SonyConfig, session: SessionConfig, frame_interval: Duration,
    ) -> Result<(Self, Control), SupervisorError> {
        if frame_interval.is_zero() || frame_interval > Duration::from_secs(1) {
            return Err(SupervisorError::FrameInterval);
        }
        let (demand, demand_rx) = watch::channel([0; 2]);
        let (stop, stop_rx) = watch::channel(false);
        let (frames, frames_rx) = watch::channel(None);
        let (status, status_rx) = watch::channel(CameraStatus::Disconnected);
        let (snapshots, snapshots_rx) = mpsc::channel(4);
        Ok((
            Self {
                factory,
                sony,
                session,
                clock: Instant::now,
                frame_interval,
                demand: demand_rx,
                snapshots: snapshots_rx,
                frames,
                status,
                stop: stop_rx,
            },
            Control {
                demand,
                stop,
                frames: frames_rx,
                status: status_rx,
                snapshots,
            },
        ))
    }

    /// Inject the session's monotonic transaction clock; wait budgets still gate only starts.
    #[must_use]
    pub fn with_clock(mut self, clock: fn() -> Instant) -> Self {
        self.clock = clock;
        self
    }

    /// Wait for a camera and reconnect with capped exponential backoff until shutdown.
    /// Camera operations are deliberately outside `select!`: no in-flight PTP future is dropped.
    pub async fn run(mut self) -> F {
        let mut delay = Duration::from_millis(100);
        while !*self.stop.borrow() && self.stop.has_changed().is_ok() {
            let connected = self.factory.connect().await;
            if let Err(error) = &connected {
                tracing::warn!(event = "camera_open_failed", reason = %error);
            }
            if let Ok(Some(camera)) = connected {
                let mut session = self
                    .factory
                    .session(camera, self.session.clone(), self.clock);
                if !*self.stop.borrow() {
                    let mut was_ready = false;
                    let result = SonyCamera::run(&mut session, self.sony.clone(), async |camera| {
                        camera.wait_live_view_ready().await?;
                        if *self.stop.borrow() {
                            return Ok(());
                        }
                        let first = camera.next_live_view_frame().await?;
                        self.frames.send_replace(Some(Arc::from(first)));
                        self.status.send_replace(CameraStatus::Ready {
                            battery: camera.battery(),
                        });
                        was_ready = true;
                        self.connected(camera).await
                    })
                    .await;
                    if was_ready {
                        delay = Duration::from_millis(100);
                    }
                    if result.is_err() {
                        tracing::warn!(event = "camera_disconnected");
                    }
                }
                let camera = self.factory.recover(session);
                self.factory.release(camera);
            }
            self.frames.send_replace(None);
            self.status.send_replace(CameraStatus::Disconnected);
            if *self.stop.borrow() {
                break;
            }
            tokio::select! {
                _ = self.stop.changed() => break,
                () = self.factory.pause(delay) => {},
            }
            delay = (delay * 2).min(Duration::from_secs(5));
        }
        self.snapshots.close();
        while let Ok(reply) = self.snapshots.try_recv() {
            let _ = reply.send(Err(SnapshotError));
        }
        self.status.send_replace(CameraStatus::Stopped);
        self.factory
    }

    async fn connected(
        &mut self, camera: &mut SonyCamera<'_, F::Session>,
    ) -> Result<(), SonyError> {
        let mut next_frame = tokio::time::Instant::now();
        let mut telemetry = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if *self.stop.borrow() {
                return Ok(());
            }
            let active = self.demand.borrow().iter().any(|epoch| *epoch != 0);
            tokio::select! {
                biased;
                _ = self.stop.changed() => return Ok(()),
                reply = self.snapshots.recv() => {
                    let Some(reply) = reply else { return Ok(()); };
                    match camera.capture_still().await {
                        Ok(image) => { let _ = reply.send(snapshot_image(image)); },
                        Err(error) => {
                            let _ = reply.send(Err(SnapshotError));
                            return Err(error);
                        },
                    }
                },
                _ = self.demand.changed() => {},
                () = tokio::time::sleep_until(next_frame), if active => {
                    let frame = camera.next_live_view_frame().await?;
                    self.frames.send_replace(Some(Arc::from(frame)));
                    next_frame = tokio::time::Instant::now() + self.frame_interval;
                },
                () = tokio::time::sleep_until(telemetry) => {
                    camera.pump_events().await?;
                    camera.refresh_properties().await?;
                    self.status.send_replace(CameraStatus::Ready { battery: camera.battery() });
                    telemetry = tokio::time::Instant::now() + Duration::from_secs(5);
                },
            }
        }
    }
}
