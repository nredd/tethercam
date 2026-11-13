//! Live-view soak: the same code runs against the simulated body in CI and, by hand, against a
//! real camera.
//!
//! The hardware test is `#[ignore]`d and skips cleanly when no camera is selected. It needs a
//! Sony body in PC Remote mode on USB. The session is always closed, because a session
//! abandoned without `CloseSession` wedges the camera until it is power-cycled.
//!
//! Run it with `TETHERCAM_HARDWARE=1 cargo test --test sony_hardware -- --ignored --nocapture`.
//!
//! NOTE(redd): `hardware_transport` has no USB transport to open yet. Replace its body with the
//! `transport::usb` open call once that module lands.
//!
//! References:
//! - <https://github.com/nredd/tethercam/blob/main/docs/architecture.md#testing>
//! - <https://github.com/nredd/tethercam/blob/main/src/ptp/session.rs>

use std::io::Write;
use std::time::{Duration, Instant};

use tethercam::ptp::session::{Session, SessionConfig};
use tethercam::sony::{SonyCamera, SonyConfig, SonyError};
use tethercam::transport::Transport;
use tethercam::transport::sim::SimCamera;
use tethercam::transport::sim::sony::{SonySimConfig, sony_camera};

/// What a soak run saw.
#[derive(Debug, Default, PartialEq, Eq)]
struct Soak {
    frames: u64,
    bytes: u64,
    largest: usize,
}

/// Hands out live-view frames for `duration`, then closes the session.
async fn soak<T: Transport + Send>(
    session: &mut Session<T>, config: SonyConfig, duration: Duration,
) -> Result<Soak, SonyError> {
    SonyCamera::run(session, config, async |camera| {
        camera.wait_live_view_ready().await?;
        let mut soak = Soak::default();
        let start = Instant::now();
        while start.elapsed() < duration {
            let frame = camera.next_live_view_frame().await?;
            soak.frames += 1;
            soak.bytes += frame.len() as u64;
            soak.largest = soak.largest.max(frame.len());
        }
        Ok(soak)
    })
    .await
}

/// Writes a line to stdout through an explicit `Write`.
fn report(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
}

/// The camera selected for the hardware test: `Ok(None)` to skip, `Err` when one was asked for
/// but cannot be opened.
fn hardware_transport() -> Result<Option<SimCamera>, String> {
    if std::env::var_os("TETHERCAM_HARDWARE").is_none() {
        return Ok(None);
    }
    Err("TETHERCAM_HARDWARE is set but this build has no USB transport yet".to_owned())
}

#[tokio::test]
async fn soak_against_the_simulated_body() {
    for version in [200, 300] {
        let sim = SonySimConfig {
            max_version: version,
            ..SonySimConfig::default()
        };
        let mut session = Session::new(sony_camera(sim), SessionConfig::default());
        let config = SonyConfig {
            poll_interval: Duration::ZERO,
            ..SonyConfig::default()
        };
        let soak = soak(&mut session, config, Duration::from_millis(50))
            .await
            .unwrap();
        assert!(soak.frames > 0);
        assert_eq!(soak.bytes, soak.frames * 10);
        assert!(!session.is_open());
        assert!(!session.transport().session_open());
    }
}

#[test]
fn hardware_is_skipped_unless_selected() {
    if std::env::var_os("TETHERCAM_HARDWARE").is_none() {
        assert_eq!(hardware_transport().map(|t| t.is_some()), Ok(false));
    }
}

#[tokio::test]
#[ignore = "needs a Sony body in PC Remote mode on USB"]
async fn thirty_seconds_of_live_view_from_a_real_body() {
    let Some(transport) = hardware_transport().unwrap() else {
        report("skipped: TETHERCAM_HARDWARE is not set");
        return;
    };
    let mut session = Session::new(transport, SessionConfig::default());
    let started = Instant::now();
    let soak = soak(&mut session, SonyConfig::default(), Duration::from_secs(30))
        .await
        .unwrap();
    let seconds = started.elapsed().as_secs_f64();
    report(&format!(
        "{} frames, {} bytes, largest {} bytes, {:.1} fps",
        soak.frames,
        soak.bytes,
        soak.largest,
        f64::from(u32::try_from(soak.frames).unwrap_or(u32::MAX)) / seconds
    ));
    assert!(!session.is_open());
    assert!(soak.frames > 0);
}
