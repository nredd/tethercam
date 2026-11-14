//! Counts-only run observability for an unattended run.
//!
//! Every counter is a relaxed atomic, so the frame hot path never takes a lock. Nothing here
//! identifies a device, an address or a payload: only totals since the process started.
//!
//! References:
//! - `docs/service.md`, run counters

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

/// Process-lifetime totals, shared by the camera actor, the media pipeline and snapshot callers.
#[derive(Debug)]
pub(crate) struct Counters {
    started: SystemTime,
    frames_fetched: AtomicU64,
    frames_served: AtomicU64,
    frames_dropped: AtomicU64,
    reconnects: AtomicU64,
    ready_sessions: AtomicU64,
    session_restarts: AtomicU64,
    connect_misses: AtomicU64,
    snapshots_served: AtomicU64,
    snapshots_refused: AtomicU64,
}

impl Counters {
    pub(crate) fn new() -> Self {
        Self {
            started: SystemTime::now(),
            frames_fetched: AtomicU64::new(0),
            frames_served: AtomicU64::new(0),
            frames_dropped: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            ready_sessions: AtomicU64::new(0),
            session_restarts: AtomicU64::new(0),
            connect_misses: AtomicU64::new(0),
            snapshots_served: AtomicU64::new(0),
            snapshots_refused: AtomicU64::new(0),
        }
    }

    fn add(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// One live-view frame fetched from the camera.
    pub(crate) fn frame_fetched(&self) {
        Self::add(&self.frames_fetched);
    }

    /// One paced frame handed to the RTP queues of at least one subscriber.
    pub(crate) fn frame_served(&self) {
        Self::add(&self.frames_served);
    }

    /// Frames dropped before a subscriber saw them: replaced in the pacer before they were
    /// emitted, or discarded because a client queue was full.
    pub(crate) fn frames_dropped(&self, count: u64) {
        self.frames_dropped.fetch_add(count, Ordering::Relaxed);
    }

    /// A camera session reached live view. Every one after the first is a reconnect.
    pub(crate) fn session_ready(&self) {
        if self.ready_sessions.fetch_add(1, Ordering::Relaxed) > 0 {
            Self::add(&self.reconnects);
        }
    }

    /// A camera session ended with an error.
    pub(crate) fn session_restarted(&self) {
        Self::add(&self.session_restarts);
    }

    /// A connection attempt found no usable camera.
    pub(crate) fn connect_missed(&self) {
        Self::add(&self.connect_misses);
    }

    /// A snapshot request was answered with an image.
    pub(crate) fn snapshot_served(&self) {
        Self::add(&self.snapshots_served);
    }

    /// A snapshot request was refused or failed.
    pub(crate) fn snapshot_refused(&self) {
        Self::add(&self.snapshots_refused);
    }

    pub(crate) fn snapshot(&self) -> CounterSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        CounterSnapshot {
            // Wall clock on purpose: a monotonic clock stops while the host sleeps, and sleep
            // is one of the cases an unattended run has to account for.
            uptime_secs: self
                .started
                .elapsed()
                .map_or(0, |elapsed| elapsed.as_secs()),
            frames_fetched: load(&self.frames_fetched),
            frames_served: load(&self.frames_served),
            frames_dropped: load(&self.frames_dropped),
            reconnects: load(&self.reconnects),
            session_restarts: load(&self.session_restarts),
            connect_misses: load(&self.connect_misses),
            snapshots_served: load(&self.snapshots_served),
            snapshots_refused: load(&self.snapshots_refused),
        }
    }
}

/// A point-in-time copy of the run counters. Counts only, never a device identity, an address
/// or image bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CounterSnapshot {
    /// Wall-clock seconds since the process started, host sleep included.
    pub uptime_secs: u64,
    /// Live-view frames fetched from the camera.
    pub frames_fetched: u64,
    /// Paced frames handed to subscribers' RTP queues, duplicates included.
    pub frames_served: u64,
    /// Fetched frames replaced before they were emitted, plus frames whose packets met a full
    /// client queue.
    pub frames_dropped: u64,
    /// Camera sessions that reached live view again after an earlier one.
    pub reconnects: u64,
    /// Camera sessions that ended with an error.
    pub session_restarts: u64,
    /// Connection attempts that found no usable camera.
    pub connect_misses: u64,
    /// Snapshot requests answered with an image.
    pub snapshots_served: u64,
    /// Snapshot requests refused or failed.
    pub snapshots_refused: u64,
}

impl CounterSnapshot {
    /// Comma-separated JSON members, appended to the status document after the camera state.
    #[must_use]
    pub fn json_members(&self) -> String {
        format!(
            "\"uptime_secs\":{},\"frames_fetched\":{},\"frames_served\":{},\"frames_dropped\":{},\
             \"reconnects\":{},\"session_restarts\":{},\"connect_misses\":{},\
             \"snapshots_served\":{},\"snapshots_refused\":{}",
            self.uptime_secs,
            self.frames_fetched,
            self.frames_served,
            self.frames_dropped,
            self.reconnects,
            self.session_restarts,
            self.connect_misses,
            self.snapshots_served,
            self.snapshots_refused,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_counter_increments_independently_and_reads_back() {
        let counters = Counters::new();
        assert_eq!(counters.snapshot().frames_fetched, 0);
        counters.frame_fetched();
        counters.frame_served();
        counters.frame_served();
        counters.frames_dropped(3);
        counters.frames_dropped(1);
        counters.session_restarted();
        counters.connect_missed();
        counters.snapshot_served();
        counters.snapshot_refused();
        counters.snapshot_refused();
        let snapshot = counters.snapshot();
        assert_eq!(
            (
                snapshot.frames_fetched,
                snapshot.frames_served,
                snapshot.frames_dropped,
                snapshot.reconnects,
                snapshot.session_restarts,
                snapshot.connect_misses,
                snapshot.snapshots_served,
                snapshot.snapshots_refused,
            ),
            (1, 2, 4, 0, 1, 1, 1, 2)
        );
    }

    #[test]
    fn only_the_second_ready_session_onward_is_a_reconnect() {
        let counters = Counters::new();
        counters.session_ready();
        assert_eq!(counters.snapshot().reconnects, 0);
        counters.session_ready();
        counters.session_ready();
        assert_eq!(counters.snapshot().reconnects, 2);
    }

    #[test]
    fn a_start_time_in_the_future_reads_as_zero_uptime() {
        let mut counters = Counters::new();
        counters.started = SystemTime::now() + std::time::Duration::from_secs(3600);
        assert_eq!(counters.snapshot().uptime_secs, 0);
        counters.started = SystemTime::now() - std::time::Duration::from_secs(90);
        assert!(counters.snapshot().uptime_secs >= 90);
    }

    #[test]
    fn json_members_name_every_count_and_nothing_else() {
        let snapshot = CounterSnapshot {
            uptime_secs: 1,
            frames_fetched: 2,
            frames_served: 3,
            frames_dropped: 4,
            reconnects: 5,
            session_restarts: 6,
            connect_misses: 7,
            snapshots_served: 8,
            snapshots_refused: 9,
        };
        assert_eq!(
            snapshot.json_members(),
            "\"uptime_secs\":1,\"frames_fetched\":2,\"frames_served\":3,\"frames_dropped\":4,\
             \"reconnects\":5,\"session_restarts\":6,\"connect_misses\":7,\
             \"snapshots_served\":8,\"snapshots_refused\":9"
        );
    }
}
