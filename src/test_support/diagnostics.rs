//! Bounded source profiling, property inspection and still-capture diagnostics.
//!
//! Callers supply a transport/session. These routines never enumerate a camera, change host
//! services, or shorten transaction cleanup to a measurement deadline.
//!
//! References:
//! - <https://www.sony.net/Products/CameraSystem/CameraRemoteCommand/>
//! - <https://www.iso.org/standard/63602.html>

use sha2::{Digest, Sha256};
use std::io::Write;
use std::time::{Duration, Instant};

use crate::ptp::session::Session;
use crate::ptp::{DevicePropCode, Value};
use crate::sony::{SonyCamera, SonyConfig, SonyError, prop};
use crate::transport::Transport;

/// Requested diagnostic. Writes are explicit, never inferred from a measured result.
#[derive(Debug, Clone, Copy)]
pub enum Operation {
    /// Read advertised resolution/focus properties without changes.
    Properties,
    /// Fetch a bounded number of live-view frames and emit acquisition CSV.
    Source {
        /// Number of frames, from one to 4096.
        frames: u16,
    },
    /// Configure only these explicitly requested advertised properties.
    Configure {
        /// Live-view size selector, one through three.
        size: u8,
        /// Also select single capture drive mode.
        single_drive: bool,
        /// Explicitly select advertised compression/quality property `0x5004` value `4`.
        jpeg_quality: bool,
    },
    /// Focus, capture and release both shutter stages, including on capture failure.
    Capture {
        /// Number of stills, one or two.
        count: u8,
    },
}

impl Operation {
    /// Validate measurement bounds before opening a transport or creating artifacts.
    ///
    /// # Errors
    /// Returns [`Error::Limits`] for unsupported sizes or unbounded/empty counts.
    pub fn validate(self) -> Result<(), Error> {
        match self {
            Operation::Source { frames: 0 | 4097.. }
            | Operation::Configure { size: 0 | 4.., .. }
            | Operation::Capture { count: 0 | 3.. } => Err(Error::Limits),
            _ => Ok(()),
        }
    }
}

/// Diagnostic errors contain no configuration, image, URL or native output.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Invalid bounded operation parameters.
    #[error("invalid diagnostic limits")]
    Limits,
    /// Camera operation failed, with the driver's typed failure preserved.
    #[error("diagnostic camera operation failed")]
    Camera(#[from] SonyError),
    /// Output failed after the session was closed.
    #[error("diagnostic output failed")]
    Output(#[from] std::io::Error),
}

/// Named synthetic or physical measurement artifact, returned only to the explicit caller.
pub struct Artifact {
    /// Generated safe basename below the caller's chosen output directory.
    pub name: String,
    /// Image bytes. Not included in debug/log output.
    pub bytes: Vec<u8>,
}

/// Run a measurement with complete `SonyCamera::run` session cleanup before writing output.
///
/// # Errors
/// Returns invalid limits, typed camera errors, or output errors. Session cleanup is attempted
/// on every camera failure; output failures occur only after cleanup.
pub async fn run<T: Transport>(
    session: &mut Session<T>, cfg: SonyConfig, operation: Operation, out: &mut dyn Write,
) -> Result<Vec<Artifact>, Error> {
    operation.validate()?;
    let (text, artifacts) = SonyCamera::run(session, cfg, async |camera| {
        camera.wait_live_view_ready().await?;
        let mut text = Vec::new();
        let mut artifacts = Vec::new();
        match operation {
            Operation::Properties => {
                for code in [
                    0x5003, 0x5004, 0x500e, 0x5013, 0xd203, 0xd211, 0xd213, 0xd25a, 0xd267, 0xd268,
                    0xd26a, 0xd6aa,
                ] {
                    let property = camera.properties().get(DevicePropCode(code));
                    text.push(format!("{code:#06x}: {property:?}"));
                }
            }
            Operation::Configure {
                size,
                single_drive,
                jpeg_quality,
            } => {
                camera
                    .set_property(DevicePropCode(0xd203), &Value::UInt8(size))
                    .await?;
                if single_drive {
                    camera
                        .set_property(DevicePropCode(0x5013), &Value::UInt16(1))
                        .await?;
                }
                if jpeg_quality {
                    camera
                        .set_property(DevicePropCode(0x5004), &Value::UInt8(4))
                        .await?;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                camera.pump_events().await?;
                camera.refresh_properties().await?;
            }
            Operation::Source { frames } => {
                text.push("elapsed_s,fetch_s,jpeg_bytes,fingerprint".into());
                let started = Instant::now();
                for _ in 0..frames {
                    let fetch = Instant::now();
                    let image = camera.next_live_view_frame().await?;
                    text.push(format!(
                        "{:.9},{:.9},{},{:x}",
                        started.elapsed().as_secs_f64(),
                        fetch.elapsed().as_secs_f64(),
                        image.len(),
                        Sha256::digest(&image)
                    ));
                }
            }
            Operation::Capture { count } => {
                capture(camera, count, &mut text, &mut artifacts).await?;
            }
        }
        Ok((text, artifacts))
    })
    .await?;
    for line in text {
        writeln!(out, "{line}")?;
    }
    Ok(artifacts)
}

/// Release both shutter stages even when preparation or capture fails.
async fn capture<T: Transport>(
    camera: &mut SonyCamera<'_, T>, count: u8, text: &mut Vec<String>,
    artifacts: &mut Vec<Artifact>,
) -> Result<(), SonyError> {
    let prepared = camera
        .control(prop::SHUTTER_HALF_PRESS, &Value::UInt16(2))
        .await;
    let measured: Result<(), SonyError> = async {
        prepared?;
        let focused = tokio::time::Instant::now();
        while focused.elapsed() < Duration::from_secs(1) {
            camera.pump_events().await?;
            camera.refresh_properties().await?;
            if camera.properties().integer(DevicePropCode(0xd213)) == Some(2) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let before = camera.properties().integer(prop::OBJECT_IN_MEMORY);
        text.push(format!(
            "Focus state: {:?}; exposure property: {:?}",
            camera.properties().integer(DevicePropCode(0xd213)),
            camera
                .properties()
                .get(DevicePropCode(0x500d))
                .map(|property| &property.current)
        ));
        let live = camera.next_live_view_frame().await?;
        artifacts.push(Artifact {
            name: "live.jpg".into(),
            bytes: live,
        });
        for index in 0..count {
            let started = Instant::now();
            let image = camera.capture_still().await?;
            text.push(format!(
                "Capture {}: {:.6} seconds, {} bytes, format={:#06x}",
                index + 1,
                started.elapsed().as_secs_f64(),
                image.data.len(),
                image.info.object_format
            ));
            artifacts.push(Artifact {
                name: format!("still-{}.bin", index + 1),
                bytes: image.data,
            });
        }
        text.push(format!(
            "Image-memory before={before:?}, after={:?}",
            camera.properties().integer(prop::OBJECT_IN_MEMORY)
        ));
        Ok(())
    }
    .await;
    let full = camera
        .control(prop::SHUTTER_FULL_PRESS, &Value::UInt16(1))
        .await;
    let half = camera
        .control(prop::SHUTTER_HALF_PRESS, &Value::UInt16(1))
        .await;
    measured?;
    full?;
    half?;
    Ok(())
}

#[cfg(test)]
mod tests;
