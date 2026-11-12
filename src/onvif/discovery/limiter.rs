//! Message-id de-duplication and per-source rate limiting.
//!
//! All are bounded in memory and none fails closed: [`Dedup`] and [`SourceLimiter`] evict their
//! oldest (least recently seen) entry when full, because refusing new entries would let any
//! on-link host lock everyone else out of discovery by flooding unique ids or rotating spoofed
//! sources. Protection against rotation comes from [`GlobalLimiter`], one shared bucket whose
//! budget does not depend on how many sources there are.
//!
//! References:
//! - WS-Discovery 1.1, section 3.1.1 "Duplicate messages" and section 3.1.3 "Message
//!   delivery" (UDP repeats and `MessageID` suppression)

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Remembers recently seen `MessageID`s so a datagram that arrives twice (once per joined
/// socket, or retransmitted) is answered once.
#[derive(Debug)]
pub(super) struct Dedup {
    seen: VecDeque<(String, Instant)>,
    ttl: Duration,
    capacity: usize,
}

impl Dedup {
    pub(super) fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            seen: VecDeque::new(),
            ttl,
            capacity,
        }
    }

    /// Whether `id` was seen within the TTL, without recording it.
    pub(super) fn contains(&mut self, id: &str, now: Instant) -> bool {
        self.expire(now);
        self.seen.iter().any(|(seen, _)| seen == id)
    }

    fn expire(&mut self, now: Instant) {
        while let Some((_, at)) = self.seen.front() {
            if now.saturating_duration_since(*at) >= self.ttl {
                self.seen.pop_front();
            } else {
                break;
            }
        }
    }

    /// Record `id`. Returns `false` if it was already seen within the TTL.
    pub(super) fn insert(&mut self, id: &str, now: Instant) -> bool {
        if self.contains(id, now) {
            return false;
        }
        if self.seen.len() >= self.capacity.max(1) {
            self.seen.pop_front();
        }
        self.seen.push_back((id.to_owned(), now));
        true
    }
}

/// A token bucket: `burst` tokens, one more every `refill`.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: u32,
    last: Instant,
}

impl Bucket {
    fn full(burst: u32, now: Instant) -> Self {
        Self {
            tokens: burst,
            last: now,
        }
    }

    fn take(&mut self, burst: u32, refill: Duration, now: Instant) -> bool {
        // A clock that goes backwards is refused outright: no credit, no spent token, no state
        // change. (`last` never exceeds a previously seen `now`.)
        if now < self.last {
            return false;
        }
        let gained = now.saturating_duration_since(self.last).as_nanos() / refill.as_nanos().max(1);
        if gained > 0 {
            // Tokens are capped at `burst`. `last` advances over *all* elapsed whole intervals,
            // credited or not, so no history is left to refill the bucket again later. Only an
            // elapsed count too large for `u32` restarts the interval at `now`.
            let (credit, advance_to) = match u32::try_from(gained) {
                Ok(credit) => (
                    credit,
                    self.last
                        .checked_add(refill.saturating_mul(credit))
                        .map_or(now, |t| t.min(now)),
                ),
                Err(_) => (u32::MAX, now),
            };
            self.tokens = self.tokens.saturating_add(credit).min(burst);
            self.last = advance_to;
        }
        match self.tokens.checked_sub(1) {
            Some(left) => {
                self.tokens = left;
                true
            }
            None => false,
        }
    }
}

/// One bucket shared by every source: caps total accepted datagrams per second.
#[derive(Debug)]
pub(super) struct GlobalLimiter {
    bucket: Option<Bucket>,
    burst: u32,
    refill: Duration,
}

impl GlobalLimiter {
    pub(super) fn new(burst: u32, refill: Duration) -> Self {
        let burst = burst.max(1);
        Self {
            bucket: None,
            burst,
            refill,
        }
    }

    /// Spend a token. Returns `false` when the shared budget is empty.
    pub(super) fn allow(&mut self, now: Instant) -> bool {
        let burst = self.burst;
        self.bucket
            .get_or_insert_with(|| Bucket::full(burst, now))
            .take(burst, self.refill, now)
    }
}

/// Token bucket per source address, least-recently-seen eviction when full.
#[derive(Debug)]
pub(super) struct SourceLimiter {
    buckets: HashMap<IpAddr, (Bucket, Instant)>,
    burst: u32,
    refill: Duration,
    capacity: usize,
}

impl SourceLimiter {
    pub(super) fn new(burst: u32, refill: Duration, capacity: usize) -> Self {
        Self {
            buckets: HashMap::new(),
            burst: burst.max(1),
            refill,
            capacity: capacity.max(1),
        }
    }

    /// Spend a token for `source`. Returns `false` when it is over its rate. A new source
    /// always gets an entry, evicting the least recently seen one if the table is full.
    pub(super) fn allow(&mut self, source: IpAddr, now: Instant) -> bool {
        if !self.buckets.contains_key(&source) && self.buckets.len() >= self.capacity {
            let oldest = self
                .buckets
                .iter()
                .min_by_key(|(_, (_, seen))| *seen)
                .map(|(ip, _)| *ip);
            if let Some(ip) = oldest {
                self.buckets.remove(&ip);
            }
        }
        let burst = self.burst;
        let (bucket, seen) = self
            .buckets
            .entry(source)
            .or_insert((Bucket::full(burst, now), now));
        *seen = now;
        bucket.take(burst, self.refill, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([192, 0, 2, last])
    }

    #[test]
    fn dedup_suppresses_within_ttl_and_forgets_after() {
        let t0 = Instant::now();
        let mut d = Dedup::new(Duration::from_secs(10), 8);
        assert!(d.insert("a", t0));
        assert!(!d.insert("a", t0 + Duration::from_secs(9)));
        assert!(d.insert("a", t0 + Duration::from_secs(10)));
    }

    #[test]
    fn dedup_is_bounded() {
        let t0 = Instant::now();
        let mut d = Dedup::new(Duration::from_secs(60), 2);
        assert!(d.insert("a", t0));
        assert!(d.insert("b", t0));
        assert!(d.insert("c", t0));
        assert_eq!(d.seen.len(), 2);
        assert!(d.insert("a", t0), "oldest was evicted");
    }

    #[test]
    fn limiter_allows_burst_then_refills() {
        let t0 = Instant::now();
        let mut l = SourceLimiter::new(3, Duration::from_secs(1), 8);
        assert!((0..3).all(|_| l.allow(ip(1), t0)));
        assert!(!l.allow(ip(1), t0));
        assert!(l.allow(ip(2), t0), "other sources are independent");
        assert!(l.allow(ip(1), t0 + Duration::from_secs(1)));
        assert!(!l.allow(ip(1), t0 + Duration::from_secs(1)));
    }

    #[test]
    fn limiter_evicts_the_least_recently_seen_source_when_full() {
        let t0 = Instant::now();
        let mut l = SourceLimiter::new(1, Duration::from_secs(60), 2);
        assert!(l.allow(ip(1), t0));
        assert!(l.allow(ip(2), t0 + Duration::from_secs(1)));
        assert!(
            !l.allow(ip(1), t0 + Duration::from_secs(2)),
            "ip(1) is now freshest"
        );
        // Table is full: a new source is admitted by evicting ip(2), the least recently seen.
        assert!(l.allow(ip(3), t0 + Duration::from_secs(3)));
        assert_eq!(l.buckets.len(), 2);
        assert!(l.buckets.contains_key(&ip(1)) && l.buckets.contains_key(&ip(3)));
    }

    #[test]
    fn rotating_sources_never_lock_out_a_new_client() {
        let t0 = Instant::now();
        let mut l = SourceLimiter::new(1, Duration::from_secs(60), 4);
        for n in 0..100 {
            assert!(l.allow(ip(n), t0));
        }
        assert!(l.allow(ip(200), t0));
        assert_eq!(l.buckets.len(), 4);
    }

    #[test]
    fn global_limiter_rate_is_exact_for_sub_millisecond_intervals() {
        let t0 = Instant::now();
        let mut g = GlobalLimiter::new(1, Duration::from_secs(1) / 600);
        let mut allowed = 0;
        // Constant demand every 100us for one simulated second.
        for step in 0..10_000 {
            if g.allow(t0 + Duration::from_micros(100) * step) {
                allowed += 1;
            }
        }
        assert!((598..=601).contains(&allowed), "{allowed}");
    }

    #[test]
    fn a_long_idle_period_credits_at_most_the_burst() {
        let t0 = Instant::now();
        let mut g = GlobalLimiter::new(5, Duration::from_micros(1));
        assert!(g.allow(t0));
        // 5000 s at 1,000,000 tokens/s is 5e9 intervals, more than `u32::MAX`.
        let later = t0 + Duration::from_secs(5000);
        let admitted = (0..100).filter(|_| g.allow(later)).count();
        assert_eq!(admitted, 5);
        // No leftover history: nothing more at the same timestamp, one token a microsecond on.
        assert!(!g.allow(later));
        assert!(g.allow(later + Duration::from_micros(1)));
    }

    #[test]
    fn a_clock_going_backwards_is_a_no_op() {
        let base = Instant::now();
        let t0 = base + Duration::from_secs(100);
        let mut g = GlobalLimiter::new(2, Duration::from_secs(1));
        assert!(g.allow(t0));
        // One token left. Backwards timestamps are refused and do not spend it.
        let earlier = base + Duration::from_secs(50);
        assert!((0..10).all(|_| !g.allow(earlier)));
        assert!(g.allow(t0), "the remaining token is intact");
        assert!(!g.allow(t0));
        assert!(g.allow(t0 + Duration::from_secs(1)));
    }

    #[test]
    fn global_limiter_caps_the_total_and_refills() {
        let t0 = Instant::now();
        let mut g = GlobalLimiter::new(3, Duration::from_millis(100));
        assert!((0..3).all(|_| g.allow(t0)));
        assert!(!g.allow(t0));
        assert!(!g.allow(t0 + Duration::from_millis(99)));
        assert!(g.allow(t0 + Duration::from_millis(100)));
        assert!(!g.allow(t0 + Duration::from_millis(100)));
    }
}
