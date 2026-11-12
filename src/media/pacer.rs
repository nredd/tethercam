//! Constant-frame-rate pacing for an irregular frame source.
//!
//! The camera's live view arrives when it arrives: 14.5 fps on average, with jitter, stalls and
//! the occasional burst. RTP timestamps and NVR recorders want a fixed rate. [`Pacer`] sits
//! between the two:
//!
//! - Output slot `n` is due at `start + n / fps`, where `start` is the instant the first frame
//!   was pushed. Slots are computed from the slot index, so there is no accumulated rounding
//!   drift
//! - Underrun: when a slot is due and no new frame arrived since the last output, the latest
//!   frame is emitted again (a duplicate)
//! - Overrun: when several frames arrive within one slot, only the newest is kept; the older ones
//!   are dropped and handed back for buffer reuse
//! - Late consumer: [`Pacer::poll`] emits the slot that contains "now". Slots that passed
//!   entirely before it was called are counted as skipped, never emitted in a burst, so the
//!   output timeline stays tied to the clock
//!
//! The pacer is a pull-style state machine over an injected [`Clock`]: it never sleeps or
//! spawns. The caller sleeps for [`Pacer::time_until_due`] and then calls [`Pacer::poll`].
//! Tests drive it with a [`crate::media::ManualClock`] and are fully deterministic.
//!
//! References:
//! - [RFC 3550 section 5.1](https://www.rfc-editor.org/rfc/rfc3550#section-5.1), RTP timestamps
//!   advance at a constant rate for constant-rate sources

use std::time::Duration;

use thiserror::Error;

use super::clock::Clock;

const NANOS_PER_SEC: u64 = 1_000_000_000;

/// Highest accepted output rate, frames per second.
pub const MAX_FPS: u32 = 1000;

/// Why a [`Pacer`] could not be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PacerError {
    /// The frame rate is zero or above [`MAX_FPS`].
    #[error("frame rate '{0}' must be 1..={MAX_FPS}")]
    InvalidFrameRate(u32),
}

/// Counters since the pacer was created.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PacerStats {
    /// Frames emitted, fresh and duplicate.
    pub emitted: u64,
    /// Of those, emissions that repeated the previous frame (underrun).
    pub duplicated: u64,
    /// Frames replaced before they were emitted (overrun).
    pub dropped: u64,
    /// Output slots that elapsed without an emission because the consumer polled late.
    pub skipped: u64,
}

/// One paced output frame.
#[derive(Debug)]
pub struct Paced<'a, F> {
    /// The frame to encode.
    pub frame: &'a F,
    /// The output slot number, counted from 0 at the first frame. The RTP timestamp is
    /// `slot * clock_rate / fps`.
    pub slot: u64,
    /// When the slot was due, on the pacer's clock.
    pub due: Duration,
    /// `false` when this frame was already emitted for an earlier slot.
    pub fresh: bool,
}

/// The first frame establishes the epoch and every push retains one latest frame.
#[derive(Debug)]
struct Latest<F> {
    start: Duration,
    frame: F,
}

/// Turns irregular input frames into a fixed-rate sequence. See the [module docs](self).
#[derive(Debug)]
pub struct Pacer<F, C> {
    clock: C,
    fps: u32,
    /// First push establishes the epoch and the first frame together.
    latest: Option<Latest<F>>,
    /// The next slot to emit.
    next_slot: u64,
    /// `latest` has not been emitted yet.
    fresh: bool,
    stats: PacerStats,
}

impl<F, C: Clock> Pacer<F, C> {
    /// A pacer emitting `fps` frames per second on `clock`.
    ///
    /// # Errors
    /// [`PacerError::InvalidFrameRate`] unless `1 <= fps <= MAX_FPS`.
    pub fn new(fps: u32, clock: C) -> Result<Self, PacerError> {
        if !(1..=MAX_FPS).contains(&fps) {
            return Err(PacerError::InvalidFrameRate(fps));
        }
        Ok(Self {
            clock,
            fps,
            latest: None,
            next_slot: 0,
            fresh: false,
            stats: PacerStats::default(),
        })
    }

    /// Offer a newly arrived frame. It becomes the frame for the next due slot.
    ///
    /// Returns the frame it replaced, if any, so the caller can recycle its buffer. When that
    /// frame was never emitted it counts as an overrun drop in [`PacerStats::dropped`]; a
    /// replaced frame that was already emitted is returned too but is not a drop.
    pub fn push(&mut self, frame: F) -> Option<F> {
        let previous = if let Some(latest) = &mut self.latest {
            Some(std::mem::replace(&mut latest.frame, frame))
        } else {
            self.latest = Some(Latest {
                start: self.clock.now(),
                frame,
            });
            None
        };
        if std::mem::replace(&mut self.fresh, true) {
            self.stats.dropped += 1;
        }
        previous
    }

    /// When the next slot is due, or `None` before the first frame.
    #[must_use]
    pub fn next_due(&self) -> Option<Duration> {
        self.latest.as_ref().map(|latest| {
            latest
                .start
                .saturating_add(slot_offset(self.next_slot, self.fps))
        })
    }

    /// How long until the next slot is due: zero if it already is, `None` before the first frame.
    #[must_use]
    pub fn time_until_due(&self) -> Option<Duration> {
        self.next_due()
            .map(|due| due.saturating_sub(self.clock.now()))
    }

    /// The frame for the current slot, if one is due.
    ///
    /// Returns `None` before the first frame and while the next slot is still in the future.
    /// Call again after the returned [`Paced`] is dropped; at most one slot is emitted per call.
    pub fn poll(&mut self) -> Option<Paced<'_, F>> {
        let latest = self.latest.as_ref()?;
        let start = latest.start;
        let elapsed = self.clock.now().saturating_sub(start);
        let current = slot_at(elapsed, self.fps);
        if current < self.next_slot {
            return None;
        }
        let slot = current;
        self.stats.skipped += slot - self.next_slot;
        self.next_slot = slot.saturating_add(1);

        let fresh = std::mem::take(&mut self.fresh);
        self.stats.emitted += 1;
        if !fresh {
            self.stats.duplicated += 1;
        }
        Some(Paced {
            frame: &latest.frame,
            slot,
            due: start.saturating_add(slot_offset(slot, self.fps)),
            fresh,
        })
    }

    /// Counters since creation.
    #[must_use]
    pub fn stats(&self) -> PacerStats {
        self.stats
    }

    /// The configured output rate.
    #[must_use]
    pub fn fps(&self) -> u32 {
        self.fps
    }
}

/// Time from the first frame to the start of `slot`: `slot / fps` seconds, exact to the nanosecond
/// per slot (no per-slot rounding accumulates).
fn slot_offset(slot: u64, fps: u32) -> Duration {
    let fps = u64::from(fps);
    let nanos = (slot % fps) * NANOS_PER_SEC / fps;
    // `nanos < 1e9`, so it always fits `u32`; saturate rather than panic if that ever changed.
    Duration::new(slot / fps, u32::try_from(nanos).unwrap_or(u32::MAX))
}

/// The latest slot whose [`slot_offset`] is at or before `elapsed`.
///
/// `slot_offset` truncates to whole nanoseconds, so the exact `floor(elapsed * fps)` can be one
/// slot behind a boundary that `next_due` already reports as reached. Correct for it, otherwise
/// a caller sleeping until `next_due` would wake and find nothing due.
fn slot_at(elapsed: Duration, fps: u32) -> u64 {
    let exact = elapsed.as_nanos().saturating_mul(u128::from(fps)) / u128::from(NANOS_PER_SEC);
    let slot = u64::try_from(exact).unwrap_or(u64::MAX);
    match slot.checked_add(1) {
        Some(next) if slot_offset(next, fps) <= elapsed => next,
        _ => slot,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::media::ManualClock;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    fn pacer(fps: u32) -> (Pacer<u32, ManualClock>, ManualClock) {
        let clock = ManualClock::new();
        (Pacer::new(fps, clock.clone()).unwrap(), clock)
    }

    /// `(slot, frame, fresh)` of the next emission, if any.
    fn poll(p: &mut Pacer<u32, ManualClock>) -> Option<(u64, u32, bool)> {
        p.poll().map(|o| (o.slot, *o.frame, o.fresh))
    }

    #[test]
    fn configured_rate_and_saturated_slot() {
        let (p, _) = pacer(10);
        assert_eq!(p.fps(), 10);
        assert_eq!(slot_at(Duration::MAX, 10), u64::MAX);
    }

    #[test]
    fn rejects_bad_rates() {
        for fps in [0, MAX_FPS + 1] {
            assert_eq!(
                Pacer::<u32, _>::new(fps, ManualClock::new()).unwrap_err(),
                PacerError::InvalidFrameRate(fps)
            );
        }
    }

    #[test]
    fn nothing_before_the_first_frame() {
        let (mut p, clock) = pacer(10);
        assert_eq!(p.next_due(), None);
        assert_eq!(p.time_until_due(), None);
        clock.advance(MS(500));
        assert!(poll(&mut p).is_none());
        assert_eq!(p.stats(), PacerStats::default());
    }

    #[test]
    fn first_frame_is_due_immediately_then_every_period() {
        let (mut p, clock) = pacer(10);
        clock.advance(MS(250));
        assert_eq!(p.push(1), None);
        assert_eq!(p.next_due(), Some(MS(250)));
        assert_eq!(poll(&mut p), Some((0, 1, true)));
        assert_eq!(p.next_due(), Some(MS(350)));
        assert_eq!(p.time_until_due(), Some(MS(100)));
        assert!(poll(&mut p).is_none(), "slot 1 is not due yet");
    }

    #[test]
    fn jittery_input_never_leaves_a_gap_in_the_output() {
        // 10 fps output; frame i arrives on its 100 ms grid point plus -40..=+40 ms of jitter.
        let jitter_ms: [i64; 12] = [0, 38, -40, 25, -5, 40, -38, 3, 0, 31, -29, 12];
        let arrivals: Vec<(Duration, u32)> = jitter_ms
            .iter()
            .enumerate()
            .map(|(i, j)| {
                let grid = i64::try_from(i).unwrap() * 100;
                (
                    MS(u64::try_from(grid + j).unwrap()),
                    u32::try_from(i).unwrap(),
                )
            })
            .collect();

        let (mut p, clock) = pacer(10);
        let mut pending = arrivals.iter().peekable();
        let mut outputs = Vec::new();
        let mut returned = Vec::new();
        // Event loop: jump to whichever comes first, the next arrival or the next due slot.
        for _ in 0..200 {
            let next_arrival = pending.peek().map(|a| a.0);
            let next_slot = p.next_due();
            let at = next_arrival.into_iter().chain(next_slot).min().unwrap();
            clock.advance_to(at);
            while let Some(&&(t, frame)) = pending.peek() {
                if t > clock.now() {
                    break;
                }
                returned.extend(p.push(frame));
                pending.next();
            }
            if let Some(out) = poll(&mut p) {
                outputs.push(out);
            }
            if pending.peek().is_none() && outputs.len() >= 12 {
                break;
            }
        }

        let slots: Vec<u64> = outputs.iter().map(|o| o.0).collect();
        assert_eq!(
            slots,
            (0..slots.len() as u64).collect::<Vec<_>>(),
            "gap-free slots"
        );
        let stats = p.stats();
        assert_eq!(stats.skipped, 0);
        let fresh = outputs.iter().filter(|o| o.2).count() as u64;
        // Every pushed frame was either emitted fresh, dropped by overrun, or is still pending.
        let still_pending = u64::from(p.fresh);
        assert_eq!(fresh + stats.dropped + still_pending, arrivals.len() as u64);
        // Every push after the first hands the previous buffer back, dropped or not.
        assert_eq!(returned.len(), arrivals.len() - 1);
        // Late arrivals show up as duplicates (underrun), early ones as drops (overrun).
        assert_eq!(stats.duplicated, outputs.len() as u64 - fresh);
        assert!(stats.duplicated > 0 && stats.dropped > 0);
    }

    #[test]
    fn underrun_duplicates_the_latest_frame() {
        let (mut p, clock) = pacer(10);
        p.push(7);
        assert_eq!(poll(&mut p), Some((0, 7, true)));
        for slot in 1..=3 {
            clock.advance(MS(100));
            assert_eq!(poll(&mut p), Some((slot, 7, false)), "slot {slot}");
        }
        clock.advance(MS(100));
        p.push(8);
        assert_eq!(poll(&mut p), Some((4, 8, true)));
        let stats = p.stats();
        assert_eq!((stats.emitted, stats.duplicated, stats.dropped), (5, 3, 0));
    }

    #[test]
    fn overrun_keeps_only_the_newest_and_returns_the_dropped_frame() {
        let (mut p, clock) = pacer(10);
        assert_eq!(p.push(1), None);
        assert_eq!(p.push(2), Some(1), "unemitted frame comes back for reuse");
        assert_eq!(p.push(3), Some(2));
        assert_eq!(poll(&mut p), Some((0, 3, true)));
        // Replacing an already-emitted frame is not a drop, but the buffer still comes back.
        assert_eq!(p.push(4), Some(3));
        clock.advance(MS(100));
        assert_eq!(poll(&mut p), Some((1, 4, true)));
        assert_eq!(p.stats().dropped, 2);
    }

    #[test]
    fn double_rate_input_drops_every_other_frame() {
        // 30 fps in, 15 fps out.
        let (mut p, clock) = pacer(15);
        let mut out = Vec::new();
        for i in 0..30_u32 {
            p.push(i);
            if let Some(o) = poll(&mut p) {
                out.push((o.0, o.1));
            }
            clock.advance(Duration::from_nanos(1_000_000_000 / 30));
        }
        assert_eq!(out.len(), 15);
        assert!(out.windows(2).all(|w| w[0].1 < w[1].1), "newest frame wins");
        assert_eq!(p.stats().duplicated, 0);
        assert!(p.stats().dropped >= 14);
    }

    #[test]
    fn late_poll_skips_to_the_current_slot() {
        let (mut p, clock) = pacer(10);
        p.push(1);
        assert_eq!(poll(&mut p), Some((0, 1, true)));
        clock.advance(MS(1050)); // slots 1..=9 passed, now inside slot 10
        let out = p.poll().unwrap();
        assert_eq!((out.slot, out.due, out.fresh), (10, MS(1000), false));
        assert_eq!(p.stats().skipped, 9);
        assert!(poll(&mut p).is_none(), "one emission per slot");
    }

    #[test]
    fn slot_times_do_not_drift() {
        // 15 fps has no exact nanosecond period; slot times still land exactly on n / 15 s.
        assert_eq!(slot_offset(15, 15), Duration::from_secs(1));
        assert_eq!(slot_offset(15 * 3600, 15), Duration::from_secs(3600));
        assert_eq!(slot_offset(1, 15), Duration::from_nanos(66_666_666));
        assert_eq!(slot_at(Duration::from_nanos(66_666_665), 15), 0);
        // `next_due` for slot 1 is 66_666_666 ns, so a caller sleeping until then must find it due.
        assert_eq!(slot_at(Duration::from_nanos(66_666_666), 15), 1);
    }

    /// Check slot ordering and accounting for one concrete, reproducible input sequence.
    fn check_slot_accounting(fps: u32, steps: &[(u64, bool)]) -> Result<(), TestCaseError> {
        let clock = ManualClock::new();
        let mut p = Pacer::new(fps, clock.clone()).unwrap();
        let mut frame = 0_u32;
        let mut last_slot: Option<u64> = None;
        for &(advance_ms, push) in steps {
            clock.advance(Duration::from_millis(advance_ms));
            if push {
                frame += 1;
                p.push(frame);
            }
            if let Some(out) = p.poll() {
                prop_assert!(last_slot.is_none_or(|l| out.slot > l));
                prop_assert!(out.due <= clock.now());
                last_slot = Some(out.slot);
            }
        }
        let s = p.stats();
        prop_assert!(s.duplicated <= s.emitted);
        // Every slot up to the last emitted one was either emitted or skipped.
        if let Some(l) = last_slot {
            prop_assert_eq!(s.emitted + s.skipped, l + 1);
        }
        Ok(())
    }

    #[test]
    fn slot_accounting_covers_output_cardinalities() {
        // No output, one output, then fresh/duplicate outputs with an early and a late poll.
        for steps in [
            &[(100, false)][..],
            &[(0, true)][..],
            &[(0, true), (0, true), (100, false), (350, true)][..],
        ] {
            check_slot_accounting(10, steps).unwrap();
        }
    }

    proptest! {
        #[test]
        fn waking_at_next_due_always_finds_a_slot(fps in 1_u32..=MAX_FPS) {
            let clock = ManualClock::new();
            let mut p = Pacer::new(fps, clock.clone()).unwrap();
            p.push(0_u32);
            prop_assert!(p.poll().is_some());
            for _ in 0..5 {
                clock.advance_to(p.next_due().unwrap());
                prop_assert!(p.poll().is_some());
            }
        }

        #[test]
        fn slots_strictly_increase_and_account_for_every_period(
            fps in 1_u32..=60,
            steps in proptest::collection::vec((0_u64..400, any::<bool>()), 1..80),
        ) {
            check_slot_accounting(fps, &steps)?;
        }
    }
}
