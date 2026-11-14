//! Counts-only run observability for an unattended run.
//!
//! Every counter is a relaxed atomic, so the frame hot path never takes a lock. Nothing here
//! identifies a device, an address or a payload: only totals since the process started.
//!
//! References:
//! - `docs/service.md`, run counters

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime};

/// Process-lifetime totals, shared by the camera actor, the media pipeline and snapshot callers.
#[derive(Debug)]
pub(crate) struct Counters {
    started: Instant,
    run_id: u64,
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
            started: Instant::now(),
            run_id: run_id(&mut getrandom::fill),
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

    /// One paced frame processed and offered to the RTP queues of the subscribed streams.
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
            // Monotonic, so a wall-clock step cannot move it. It does not advance while the host
            // sleeps; `run_id` is what tells two processes apart.
            uptime_secs: self.started.elapsed().as_secs(),
            run_id: self.run_id,
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

/// A random identifier for this process run, so a reader can tell a restart from a clock step.
/// It names nothing about the device or the host. If the system source fails, the wall clock in
/// microseconds still differs between two starts.
fn run_id(fill: &mut dyn FnMut(&mut [u8]) -> Result<(), getrandom::Error>) -> u64 {
    let mut bytes = [0; 8];
    match fill(&mut bytes) {
        Ok(()) => u64::from_be_bytes(bytes),
        Err(_) => SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since| u64::try_from(since.as_micros()).unwrap_or(0)),
    }
}

/// A point-in-time copy of the run counters. Counts only, never a device identity, an address
/// or image bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CounterSnapshot {
    /// Monotonic seconds since the process started. Does not advance during host sleep.
    pub uptime_secs: u64,
    /// Live-view frames fetched from the camera.
    pub frames_fetched: u64,
    /// Paced frames processed and offered to subscribers' RTP queues, duplicates included. A
    /// frame whose queue was full counts here and in `frames_dropped`.
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
    /// Random identifier of this process run. Differs after every restart.
    pub run_id: u64,
}

impl CounterSnapshot {
    /// Comma-separated JSON members, appended to the status document after the camera state.
    #[must_use]
    pub fn json_members(&self) -> String {
        format!(
            "\"uptime_secs\":{},\"frames_fetched\":{},\"frames_served\":{},\"frames_dropped\":{},\
             \"reconnects\":{},\"session_restarts\":{},\"connect_misses\":{},\
             \"snapshots_served\":{},\"snapshots_refused\":{},\"run_id\":\"{:016x}\"",
            self.uptime_secs,
            self.frames_fetched,
            self.frames_served,
            self.frames_dropped,
            self.reconnects,
            self.session_restarts,
            self.connect_misses,
            self.snapshots_served,
            self.snapshots_refused,
            self.run_id,
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
    fn uptime_counts_from_the_start() {
        let mut counters = Counters::new();
        counters.started = Instant::now()
            .checked_sub(std::time::Duration::from_secs(90))
            .expect("monotonic clock far enough from its origin");
        assert!(counters.snapshot().uptime_secs >= 90);
    }

    #[test]
    fn the_run_id_is_random_and_falls_back_to_the_clock() {
        let a = run_id(&mut |bytes| {
            bytes.copy_from_slice(&[0, 0, 0, 0, 0, 0, 1, 2]);
            Ok(())
        });
        assert_eq!(a, 0x0102);
        assert_ne!(Counters::new().run_id, Counters::new().run_id);
        let fallback = run_id(&mut |_| Err(getrandom::Error::UNSUPPORTED));
        assert!(fallback > 1_000_000_000_000_000);
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
            run_id: 0xabc,
        };
        assert_eq!(
            snapshot.json_members(),
            "\"uptime_secs\":1,\"frames_fetched\":2,\"frames_served\":3,\"frames_dropped\":4,\
             \"reconnects\":5,\"session_restarts\":6,\"connect_misses\":7,\
             \"snapshots_served\":8,\"snapshots_refused\":9,\"run_id\":\"0000000000000abc\""
        );
    }
}
