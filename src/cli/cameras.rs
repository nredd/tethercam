//! `tethercam cameras` on the `ImageCaptureCore` backend: enumerate, then choose.
//!
//! The listing is the one place a device identifier is printed, to the stdout of the person who
//! asked for it, so they can paste it into `camera_id`. It never reaches a log, the tracing
//! dispatcher, status output or an error: errors here are the same sanitized categories every
//! other command uses.
//!
//! Discovery returns identifiers only. Nothing else (name, vendor, type) is available without
//! opening a session, so telling devices apart is an explicit opt-in, `--describe`: each listed
//! device is opened read-only, asked the standard PTP `GetDeviceInfo` (`0x1001`) and closed with
//! confirmation. Only the manufacturer and model strings are printed, never the serial number.
//! Opening a phone can raise a trust prompt on it, which is why this is not the default. A device
//! that refuses or fails is reported as "not a supported camera" with a category and the listing
//! goes on. The work is bounded: at most [`MAX_DEVICES`] devices, [`DESCRIBE_WAIT`] per step.
//! After a failure that leaves native cleanup unsettled the remaining devices are listed but not
//! described, because a replacement session would only be refused as busy. A quarantine is the
//! same terminal condition as everywhere: the process reports it and exits for a restart.
//!
//! References:
//! - `docs/macos-camera.md`, Listing and selecting cameras
//! - ISO 15740 / PIMA 15740:2000 section 5.5.1 "`DeviceInfo`"

use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::time::{Duration, Instant};

use super::Error;
use super::icc::open_error;
use crate::config::Config;
use crate::ptp::DeviceInfo;
use crate::ptp::session::{PtpSession, SessionConfig, SessionError};
use crate::transport::icc::{
    DISCOVERY_WINDOW, Effect, IccDeviceId, IccError, IccHandle, IccSession, IccTimeout,
};

#[cfg(test)]
#[path = "cameras_tests.rs"]
mod tests;

/// Most devices the native layer ever returns from one discovery.
pub(super) const MAX_DEVICES: usize = 64;
/// Longest wait for one step (open, `GetDeviceInfo`, close) of describing one device.
const DESCRIBE_WAIT: Duration = Duration::from_secs(10);
/// A `DeviceInfo` dataset is a few hundred bytes; anything past this is not one.
const MAX_DEVICE_INFO: usize = 64 * 1024;
/// Longest make or model printed, in characters.
const MAX_LABEL: usize = 64;

/// Whether native cleanup allows describing the next device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    /// Every session this device used is closed with confirmation.
    Settled,
    /// A session was retired or failed to close; a new one would only report busy.
    Pending,
    /// Ownership is unresolvable for the life of the process.
    Quarantined,
}

/// What describing one device found: a label to print, never a serial number.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Description {
    Camera(String),
    Unsupported(&'static str),
}

/// A short static category for a failed session step.
fn category(error: &SessionError) -> &'static str {
    match error {
        SessionError::Icc(IccError::Permission) => "access restricted",
        SessionError::Icc(IccError::DeadlineExceeded | IccError::AdmissionExpired)
        | SessionError::DeadlineExceeded(_) => "timed out",
        SessionError::Response { .. } => "refused",
        _ => "failed",
    }
}

/// Make or model text from a device, safe to print: control characters (terminal escapes) become
/// `?`, padding goes and the length is bounded.
fn clean(text: &str) -> String {
    let text: String = text
        .chars()
        .take(MAX_LABEL)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    text.trim().to_owned()
}

/// `Manufacturer Model` from a `DeviceInfo` data phase. The serial number is parsed and dropped.
fn label(data: &[u8]) -> Description {
    let Ok(info) = DeviceInfo::decode(data) else {
        return Description::Unsupported("no device information");
    };
    let label = [&info.manufacturer, &info.model]
        .map(|part| clean(part))
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if label.is_empty() {
        Description::Unsupported("no device information")
    } else {
        Description::Camera(label)
    }
}

/// Open `device` read-only, read `GetDeviceInfo` and close with confirmation.
async fn describe_one(handle: IccHandle, device: &IccDeviceId) -> (Description, Health, IccHandle) {
    let config = SessionConfig {
        transaction_timeout: DESCRIBE_WAIT,
        max_data_len: MAX_DEVICE_INFO,
        ..SessionConfig::default()
    };
    let mut session = IccSession::new(handle, device.clone(), config, Instant::now);
    let (description, cleaned) = if let Err(error) = session.open().await {
        // A failed open holds nothing, unless it retired or quarantined the session.
        let cleaned = !matches!(&error, SessionError::Icc(e) if e.retires_lease());
        (Description::Unsupported(category(&error)), cleaned)
    } else {
        let info = session.get_device_info().await;
        // Always close an opened session, even after a refused or failed command.
        let closed = session.close().await;
        let description = match &info {
            Ok(data) => label(data),
            Err(error) => Description::Unsupported(category(error)),
        };
        (description, closed.is_ok())
    };
    let health = if session.is_quarantined() {
        Health::Quarantined
    } else if cleaned {
        Health::Settled
    } else {
        Health::Pending
    };
    (description, health, session.into_handle())
}

/// Whether `stop` has already resolved. Polled once, never awaited: a stop is honoured between
/// devices, so a session is never abandoned mid-command.
async fn stopped(stop: &mut Pin<Box<dyn Future<Output = ()> + '_>>) -> bool {
    std::future::poll_fn(|cx| std::task::Poll::Ready(stop.as_mut().poll(cx).is_ready())).await
}

/// One output line per device: `index  identifier[  description][  (selected by camera_id)]`.
fn write_line(
    out: &mut dyn Write, index: usize, device: &IccDeviceId, description: Option<&str>,
    selected: bool,
) -> Result<(), Error> {
    write!(out, "{index}  {}", device.as_str())?;
    if let Some(description) = description {
        write!(out, "  {description}")?;
    }
    if selected {
        write!(out, "  (selected by camera_id)")?;
    }
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

/// What to print beside a device that was not opened: nothing without `--describe`.
fn skipped(describe: bool, reason: &'static str) -> Option<String> {
    describe.then(|| format!("not described ({reason})"))
}

/// Discover and print the cameras. `describe` opens each one (see the module docs).
///
/// # Errors
/// Discovery failures as [`Error::Open`], output failures as [`Error::Io`] and
/// [`Error::Quarantined`] when native ownership became unresolvable.
pub(super) async fn list(
    mut handle: IccHandle, cfg: &Config, describe: bool, out: &mut dyn Write,
    mut stop: Pin<Box<dyn Future<Output = ()> + '_>>,
) -> Result<(), Error> {
    let wanted = cfg.camera_id.as_ref().map(crate::config::Secret::expose);
    let found = match handle
        .discover(DISCOVERY_WINDOW, IccTimeout::default())
        .await
    {
        Ok(found) if found.len() <= MAX_DEVICES => found,
        Ok(_) => return Err(Error::Open(open_error(IccError::LimitExceeded))),
        Err(error) if error.effect() == Effect::Quarantine => return Err(Error::Quarantined),
        Err(error) => return Err(Error::Open(open_error(error))),
    };
    writeln!(out, "Cameras found: {}", found.len())?;
    let mut pause: Option<&'static str> = None;
    let mut matched = false;
    let mut quarantined = false;
    for (position, device) in found.iter().enumerate() {
        if pause.is_none() && describe && stopped(&mut stop).await {
            pause = Some("stopped");
        }
        let description = match pause {
            Some(reason) => skipped(describe, reason),
            None if describe => {
                let (described, health, next) = describe_one(handle, device).await;
                handle = next;
                match health {
                    Health::Settled => {}
                    Health::Pending => pause = Some("native cleanup pending"),
                    Health::Quarantined => {
                        quarantined = true;
                        pause = Some("native runtime quarantined");
                    }
                }
                Some(match described {
                    Description::Camera(label) => label,
                    Description::Unsupported(why) => format!("not a supported camera ({why})"),
                })
            }
            None => None,
        };
        let selected = wanted.is_some_and(|wanted| wanted == device.as_str());
        matched |= selected;
        write_line(
            out,
            position.saturating_add(1),
            device,
            description.as_deref(),
            selected,
        )?;
    }
    match (wanted, matched) {
        (Some(_), false) => writeln!(
            out,
            "camera_id is set but matches none of these; the service waits for it and never falls back."
        )?,
        (None, _) if !found.is_empty() => writeln!(
            out,
            "Set camera_id in the configuration to one of these identifiers{}.",
            if describe {
                ""
            } else {
                "; --describe opens each device briefly to show its make and model, and a phone can show a trust prompt"
            }
        )?,
        _ => {}
    }
    if quarantined {
        return Err(Error::Quarantined);
    }
    Ok(())
}
