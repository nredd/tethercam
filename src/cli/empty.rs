//! Bounded recovery from an empty `ImageCaptureCore` discovery snapshot, for one-shot commands.
//!
//! The owner saw one saved `camera_id` list as selected, then `doctor` twice report no selected
//! camera, then succeed, with the camera attached throughout. That an empty snapshot can happen
//! is proven. Why it happens is not (framework settle time after a replug, a trust or permission
//! prompt, something else), and nothing here claims to fix it.
//!
//! What a one-shot command (`cameras`, `probe`, `doctor`) does about it: when discovery answers
//! with *no devices at all* it discovers again, at most [`EmptyRecovery::retries`] more times with
//! [`EmptyRecovery::pause`] between attempts, and uses the first non-empty snapshot. Nothing else
//! is retried:
//! - any error (busy, a deadline, a quarantine, a native failure) is returned at once, because
//!   native work may be pending and those answers have their own policy
//! - a non-empty snapshot is an answer, even one without the selected camera. The caller then
//!   selects exactly as before, so an absent camera never falls back to another device and no
//!   other device is ever opened
//!
//! The continuous service (`run`) is not changed: it keeps its supervisor loop, which already
//! waits and discovers again, so [`EmptyRecovery::for_command`] gives it [`EmptyRecovery::OFF`].
//!
//! The wall time is bounded: each attempt is one discovery window of
//! [`DISCOVERY_WINDOW`](crate::transport::icc::DISCOVERY_WINDOW), so [`EmptyRecovery::ONE_SHOT`]
//! spends at most three windows and two pauses, about 8 s, on empty snapshots when native answers
//! every window on time. A native call that does not answer is an error and ends the loop with
//! it, on the existing per-call deadline.
//!
//! References:
//! - `docs/macos-camera.md`, Listing and selecting cameras

use std::time::Duration;

use super::Command;
use crate::transport::icc::{DISCOVERY_WINDOW, IccDeviceId, IccError, IccHandle, IccTimeout};

#[cfg(test)]
#[path = "empty_tests.rs"]
mod tests;

/// How many extra discoveries follow an empty snapshot, and how long to wait before each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct EmptyRecovery {
    /// Discoveries after the first. Zero is the single-shot behaviour.
    pub(super) retries: u8,
    /// Wait before each of them.
    pub(super) pause: Duration,
}

impl EmptyRecovery {
    /// One discovery, as the service does.
    pub(super) const OFF: Self = Self {
        retries: 0,
        pause: Duration::ZERO,
    };

    /// Two more discoveries a second apart: three observation windows in all, about 8 s.
    pub(super) const ONE_SHOT: Self = Self {
        retries: 2,
        pause: Duration::from_secs(1),
    };

    /// The policy of `command`: one-shot commands recover, the long-running service does not.
    pub(super) fn for_command(command: Command) -> Self {
        if command == Command::Run {
            Self::OFF
        } else {
            Self::ONE_SHOT
        }
    }

    /// Discoveries made when every one of them is empty.
    pub(super) fn attempts(self) -> u8 {
        self.retries.saturating_add(1)
    }
}

/// A discovery snapshot and how many discoveries it took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Snapshot {
    /// What the last discovery returned.
    pub(super) found: Vec<IccDeviceId>,
    /// Discoveries made, at least one.
    pub(super) attempts: u8,
}

impl Snapshot {
    /// Whether the snapshot is empty after every attempt the policy allowed it.
    pub(super) fn exhausted_empty(&self) -> bool {
        self.found.is_empty() && self.attempts > 1
    }
}

/// Discover, and again after an empty snapshot while `policy` allows.
///
/// # Errors
/// The first discovery error, unretried: pending native work must not be probed again.
pub(super) async fn discover(
    handle: &mut IccHandle, policy: EmptyRecovery,
) -> Result<Snapshot, IccError> {
    let mut attempts: u8 = 0;
    loop {
        let found = handle
            .discover(DISCOVERY_WINDOW, IccTimeout::default())
            .await?;
        attempts = attempts.saturating_add(1);
        if !found.is_empty() || attempts >= policy.attempts() {
            return Ok(Snapshot { found, attempts });
        }
        tokio::time::sleep(policy.pause).await;
    }
}
