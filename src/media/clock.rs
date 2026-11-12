//! Injectable monotonic clocks.
//!
//! Time is a [`Duration`] since an arbitrary, clock-specific epoch. Only differences are
//! meaningful. Capture instants on [`crate::media::JpegFrame`] use the same representation.
//!
//! References:
//! - [`std::time::Instant`], the monotonic source behind [`SystemClock`]

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// A monotonic time source.
pub trait Clock {
    /// Time elapsed since this clock's epoch. Never decreases.
    fn now(&self) -> Duration;
}

/// The real clock: monotonic time since construction.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    epoch: Instant,
}

impl SystemClock {
    /// Start a clock whose epoch is now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.epoch.elapsed()
    }
}

/// A clock that only moves when told to. Clones share one timeline, so a test can keep a handle
/// and advance time while a [`crate::media::Pacer`] owns another.
#[derive(Debug, Clone, Default)]
pub struct ManualClock {
    now: Arc<Mutex<Duration>>,
}

impl ManualClock {
    /// A clock at its epoch.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Move time forward by `by`, saturating at the largest representable instant.
    pub fn advance(&self, by: Duration) {
        let mut now = self.lock();
        *now = now.saturating_add(by);
    }

    /// Move time forward to `to`. A target in the past is ignored: the clock never goes back.
    pub fn advance_to(&self, to: Duration) {
        let mut now = self.lock();
        *now = (*now).max(to);
    }

    fn lock(&self) -> MutexGuard<'_, Duration> {
        // A poisoned lock still holds a valid `Duration`; time keeps working.
        self.now.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        *self.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_starts_at_zero_and_advances() {
        let clock = ManualClock::new();
        assert_eq!(clock.now(), Duration::ZERO);
        clock.advance(Duration::from_millis(40));
        clock.advance(Duration::from_millis(2));
        assert_eq!(clock.now(), Duration::from_millis(42));
    }

    #[test]
    fn manual_clock_clones_share_a_timeline() {
        let a = ManualClock::new();
        let b = a.clone();
        a.advance(Duration::from_secs(1));
        assert_eq!(b.now(), Duration::from_secs(1));
    }

    #[test]
    fn manual_clock_never_goes_back() {
        let clock = ManualClock::new();
        clock.advance_to(Duration::from_secs(5));
        clock.advance_to(Duration::from_secs(2));
        assert_eq!(clock.now(), Duration::from_secs(5));
    }

    #[test]
    fn manual_clock_saturates() {
        let clock = ManualClock::new();
        clock.advance(Duration::MAX);
        clock.advance(Duration::from_secs(1));
        assert_eq!(clock.now(), Duration::MAX);
    }

    #[test]
    fn system_clock_is_monotonic() {
        let clock = SystemClock::new();
        let a = clock.now();
        let b = clock.now();
        assert!(b >= a);
    }
}
