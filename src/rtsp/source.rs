//! The media side of the server: the [`MediaSource`] trait and the [`Fanout`] that shares one
//! source between many clients.
//!
//! Design:
//! - The server *pulls* from the source: one pump task calls [`MediaSource::next_packet`] and
//!   publishes each packet to every subscriber
//! - Each subscriber owns a bounded queue. A full queue drops the packet for that subscriber
//!   only, so a slow client can never block the source or other clients
//! - The pump runs only while someone is subscribed. The first subscription calls
//!   [`MediaSource::on_first_subscriber`], the last unsubscribe calls
//!   [`MediaSource::on_last_unsubscribe`], so the camera pipeline can start and stop on demand
//!
//! References:
//! - [RFC 3550 section 5.1 (RTP header)](https://www.rfc-editor.org/rfc/rfc3550)
//! - `docs/architecture.md`, "RTSP server contract"

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use tokio::sync::{Notify, Semaphore, mpsc};
use tokio::task::JoinHandle;

use super::rtcp::RtpHeader;
use super::sdp::TrackInfo;

/// One ready-to-send RTP packet. Cloning is a reference-count bump, so fan-out copies nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet(Arc<[u8]>);

impl Packet {
    /// Wrap the bytes of a complete RTP packet (header and payload).
    #[must_use]
    pub fn new(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self(bytes.into())
    }

    /// The packet bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A producer of RTP packets for one track.
///
/// Implementors hand over packets that are already RTP-packetized; the server never inspects
/// payloads beyond the fixed header (needed for RTCP sender reports). Packets with a short or
/// non-version-2 header are discarded.
pub trait MediaSource: Send + Sync + 'static {
    /// The track description used to build SDP. Called per `DESCRIBE`.
    fn track(&self) -> TrackInfo;

    /// The next packet, or `None` when the source has permanently ended.
    ///
    /// Only awaited while at least one client is playing, and dropped mid-await when the last
    /// one leaves, so it must be cancel safe.
    fn next_packet(&self) -> impl Future<Output = Option<Packet>> + Send;

    /// The first client started playing. Start the camera pipeline. Must not block, and must not
    /// call back into the server.
    fn on_first_subscriber(&self) {}

    /// The last client stopped playing. Stop the camera pipeline. Same constraints as
    /// [`MediaSource::on_first_subscriber`].
    fn on_last_unsubscribe(&self) {}
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    subscribers: HashMap<u64, mpsc::Sender<Packet>>,
    /// Bumped on every first-subscribe and last-unsubscribe. A pump publishes only while its
    /// own generation is current, so nothing from an old run can reach a newer set of
    /// subscribers.
    generation: u64,
    /// The owning [`Fanout`] was dropped; the control task should wind down.
    closed: bool,
}

struct Shared<S: MediaSource> {
    source: Arc<S>,
    dropped: AtomicU64,
    inner: Mutex<Inner>,
    /// Wakes the control task. `notify_one` stores a permit, so a change made while the task is
    /// busy is never missed, and any number of changes coalesce into one wakeup: lifecycle
    /// state is O(1) however fast clients churn.
    wake: Notify,
}

impl<S: MediaSource> Shared<S> {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Deliver to every subscriber unless `generation` is stale. `false` means stop pumping.
    fn publish(&self, generation: u64, packet: &Packet) -> bool {
        let valid = RtpHeader::parse(packet.as_bytes()).is_some();
        let inner = self.lock();
        if inner.generation != generation {
            return false;
        }
        if valid {
            for tx in inner.subscribers.values() {
                if tx.try_send(packet.clone()).is_err() {
                    // Full (slow client) or closed (client leaving): never wait.
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        true
    }
}

/// The single owner of the pump lifecycle. It reconciles what is running with the desired
/// state (subscribers present, current generation). Stopping finishes cancelling the pump (the
/// source's `next_packet` future is dropped) *before* the stop hook runs and before any later
/// start, so source calls never overlap and hooks strictly alternate.
async fn control<S: MediaSource>(shared: Arc<Shared<S>>) {
    // `(generation, pump)` of the run whose start hook has fired, if any.
    let mut running: Option<(u64, JoinHandle<()>)> = None;
    loop {
        let (active, generation, closed) = {
            let inner = shared.lock();
            (
                !inner.subscribers.is_empty(),
                inner.generation,
                inner.closed,
            )
        };
        let stale = running
            .as_ref()
            .is_some_and(|(g, _)| closed || !active || *g != generation);
        if stale && let Some((_, pump)) = running.take() {
            pump.abort();
            // Wait for the cancellation to complete; the outcome is moot.
            let _ = pump.await;
            shared.source.on_last_unsubscribe();
            // State may have moved while cancelling: re-read before deciding.
            continue;
        }
        if closed {
            return;
        }
        if active && running.is_none() {
            // Start the pipeline before the pump first polls the source.
            shared.source.on_first_subscriber();
            let pump_shared = Arc::clone(&shared);
            let handle = tokio::spawn(async move {
                while let Some(packet) = pump_shared.source.next_packet().await {
                    if !pump_shared.publish(generation, &packet) {
                        break;
                    }
                    // A source that is always ready must still let the runtime run other
                    // tasks and honour cancellation.
                    tokio::task::yield_now().await;
                }
            });
            running = Some((generation, handle));
            continue;
        }
        shared.wake.notified().await;
    }
}

/// Shares one [`MediaSource`] between many subscribers with drop-on-full backpressure.
///
/// Source hooks and polling are driven by one background task, so
/// [`MediaSource::on_first_subscriber`] and [`MediaSource::on_last_unsubscribe`] run shortly
/// *after* the subscription that triggered them, never concurrently with each other or with a
/// poll of [`MediaSource::next_packet`]. Rapid subscribe/unsubscribe churn coalesces; hooks
/// still strictly alternate.
pub struct Fanout<S: MediaSource> {
    shared: Arc<Shared<S>>,
    queue_depth: usize,
    control_started: OnceLock<()>,
}

impl<S: MediaSource> Drop for Fanout<S> {
    fn drop(&mut self) {
        self.shared.lock().closed = true;
        self.shared.wake.notify_one();
    }
}

impl<S: MediaSource> Fanout<S> {
    /// A fan-out over `source` whose subscribers each buffer up to `queue_depth` packets
    /// (clamped to `1..=Semaphore::MAX_PERMITS`, the largest queue Tokio supports).
    #[must_use]
    pub fn new(source: Arc<S>, queue_depth: usize) -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(Shared {
                source,
                dropped: AtomicU64::new(0),
                inner: Mutex::new(Inner::default()),
                wake: Notify::new(),
            }),
            queue_depth: queue_depth.clamp(1, Semaphore::MAX_PERMITS),
            control_started: OnceLock::new(),
        })
    }

    /// The wrapped source.
    #[must_use]
    pub fn source(&self) -> &Arc<S> {
        &self.shared.source
    }

    /// Packets dropped so far because a subscriber's queue was full.
    #[must_use]
    pub fn dropped_packets(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// Number of live subscriptions.
    #[must_use]
    pub fn subscribers(&self) -> usize {
        self.shared.lock().subscribers.len()
    }

    /// Subscribe. The first subscriber starts the pump and notifies the source.
    ///
    /// Must be called inside a Tokio runtime.
    #[must_use]
    pub fn subscribe(self: &Arc<Self>) -> Subscription<S> {
        let (tx, rx) = mpsc::channel(self.queue_depth);
        let mut inner = self.shared.lock();
        let id = inner.next_id;
        inner.next_id += 1;
        let first = inner.subscribers.is_empty();
        inner.subscribers.insert(id, tx);
        if first {
            inner.generation += 1;
        }
        drop(inner);
        self.control_started.get_or_init(|| {
            tokio::spawn(control(Arc::clone(&self.shared)));
        });
        if first {
            self.shared.wake.notify_one();
        }
        Subscription {
            id,
            fanout: Arc::clone(self),
            rx,
        }
    }

    fn unsubscribe(&self, id: u64) {
        let mut inner = self.shared.lock();
        if inner.subscribers.remove(&id).is_some() && inner.subscribers.is_empty() {
            inner.generation += 1;
            drop(inner);
            self.shared.wake.notify_one();
        }
    }
}

/// One subscriber's queue. Dropping it unsubscribes.
pub struct Subscription<S: MediaSource> {
    id: u64,
    fanout: Arc<Fanout<S>>,
    rx: mpsc::Receiver<Packet>,
}

impl<S: MediaSource> Subscription<S> {
    /// The next packet, waiting if the queue is empty. `None` only if the fan-out was torn down.
    pub async fn recv(&mut self) -> Option<Packet> {
        self.rx.recv().await
    }
}

impl<S: MediaSource> Drop for Subscription<S> {
    fn drop(&mut self) {
        self.fanout.unsubscribe(self.id);
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::atomic::AtomicUsize;

    use tokio::sync::Mutex as AsyncMutex;

    use super::*;
    use crate::rtsp::sdp::MediaKind;

    /// A source fed by the test through [`TestSource::push`].
    pub(crate) struct TestSource {
        pub(crate) tx: mpsc::UnboundedSender<Packet>,
        rx: AsyncMutex<mpsc::UnboundedReceiver<Packet>>,
        pub(crate) starts: AtomicUsize,
        pub(crate) stops: AtomicUsize,
    }

    impl TestSource {
        pub(crate) fn new() -> Arc<Self> {
            let (tx, rx) = mpsc::unbounded_channel();
            Arc::new(Self {
                tx,
                rx: AsyncMutex::new(rx),
                starts: AtomicUsize::new(0),
                stops: AtomicUsize::new(0),
            })
        }

        pub(crate) fn push(&self, seq: u16) {
            let mut p = vec![0x80, 96, 0, 0, 0, 0, 0, 0, 0xca, 0xfe, 0xba, 0xbe, 0xaa];
            p[2..4].copy_from_slice(&seq.to_be_bytes());
            let _ = self.tx.send(Packet::new(p));
        }
    }

    impl MediaSource for TestSource {
        fn track(&self) -> TrackInfo {
            TrackInfo {
                kind: MediaKind::Video,
                payload_type: 96,
                codec: "H264".into(),
                clock_rate: 90_000,
                fmtp: None,
                control: "trackID=0".into(),
            }
        }

        async fn next_packet(&self) -> Option<Packet> {
            self.rx.lock().await.recv().await
        }

        fn on_first_subscriber(&self) {
            self.starts.fetch_add(1, Ordering::SeqCst);
        }

        fn on_last_unsubscribe(&self) {
            self.stops.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::testing::TestSource;
    use super::*;

    async fn recv_seq(sub: &mut Subscription<TestSource>) -> u16 {
        let p = timeout(Duration::from_secs(5), sub.recv())
            .await
            .unwrap()
            .unwrap();
        RtpHeader::parse(p.as_bytes()).unwrap().sequence
    }

    /// Hooks run on the control task, so tests wait for them instead of assuming a timing.
    async fn eventually(what: &str, cond: impl Fn() -> bool) {
        for _ in 0..400 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    #[tokio::test]
    async fn fans_out_to_every_subscriber_in_order() {
        let src = TestSource::new();
        let fan = Fanout::new(Arc::clone(&src), 8);
        let mut a = fan.subscribe();
        let mut b = fan.subscribe();
        for seq in 1..=3 {
            src.push(seq);
        }
        for seq in 1..=3 {
            assert_eq!(recv_seq(&mut a).await, seq);
            assert_eq!(recv_seq(&mut b).await, seq);
        }
        assert_eq!(fan.subscribers(), 2);
    }

    #[tokio::test]
    async fn reports_first_subscriber_and_last_unsubscribe_once() {
        let src = TestSource::new();
        let fan = Fanout::new(Arc::clone(&src), 8);
        let a = fan.subscribe();
        let b = fan.subscribe();
        eventually("start", || src.starts.load(Ordering::SeqCst) == 1).await;
        drop(a);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(src.stops.load(Ordering::SeqCst), 0);
        drop(b);
        eventually("stop", || src.stops.load(Ordering::SeqCst) == 1).await;
        let _again = fan.subscribe();
        eventually("restart", || src.starts.load(Ordering::SeqCst) == 2).await;
        assert_eq!(src.starts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn slow_subscriber_drops_without_blocking_others() {
        let src = TestSource::new();
        let fan = Fanout::new(Arc::clone(&src), 2);
        let mut fast = fan.subscribe();
        let _slow = fan.subscribe();
        for seq in 0..10 {
            src.push(seq);
            // The fast subscriber keeps up; the slow one never reads.
            assert_eq!(recv_seq(&mut fast).await, seq);
        }
        assert_eq!(fan.dropped_packets(), 8);
    }

    /// Source that records whether the start hook ran before the first poll.
    struct OrderedSource {
        started: std::sync::atomic::AtomicBool,
        polled_before_start: std::sync::atomic::AtomicBool,
    }

    impl MediaSource for OrderedSource {
        fn track(&self) -> TrackInfo {
            TrackInfo {
                kind: crate::rtsp::sdp::MediaKind::Video,
                payload_type: 96,
                codec: "H264".into(),
                clock_rate: 90_000,
                fmtp: None,
                control: "trackID=0".into(),
            }
        }

        fn next_packet(&self) -> impl Future<Output = Option<Packet>> + Send {
            if !self.started.load(Ordering::SeqCst) {
                self.polled_before_start.store(true, Ordering::SeqCst);
            }
            std::future::ready(None)
        }

        fn on_first_subscriber(&self) {
            // Give a racing pump every chance to poll first.
            std::thread::sleep(Duration::from_millis(20));
            self.started.store(true, Ordering::SeqCst);
        }
    }

    /// Source that is always ready, to prove the pump yields and can be stopped.
    struct FirehoseSource {
        stops: std::sync::atomic::AtomicUsize,
    }

    impl MediaSource for FirehoseSource {
        fn track(&self) -> TrackInfo {
            OrderedSource {
                started: false.into(),
                polled_before_start: false.into(),
            }
            .track()
        }

        fn next_packet(&self) -> impl Future<Output = Option<Packet>> + Send {
            std::future::ready(Some(Packet::new(vec![
                0x80, 96, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ])))
        }

        fn on_last_unsubscribe(&self) {
            self.stops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn always_ready_source_does_not_starve_the_runtime_and_stops() {
        let source = Arc::new(FirehoseSource { stops: 0.into() });
        let fan = Fanout::new(Arc::clone(&source), 4);
        let sub = fan.subscribe();
        // Without a yield in the pump this sleep would never return on a current-thread runtime.
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(sub);
        eventually("stop", || source.stops.load(Ordering::SeqCst) == 1).await;
        let dropped = fan.dropped_packets();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            fan.dropped_packets(),
            dropped,
            "pump kept running after unsubscribe"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_hook_runs_before_the_source_is_polled() {
        let source = Arc::new(OrderedSource {
            started: false.into(),
            polled_before_start: false.into(),
        });
        let fan = Fanout::new(Arc::clone(&source), 4);
        let _sub = fan.subscribe();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!source.polled_before_start.load(Ordering::SeqCst));
    }

    /// Source whose `next_packet` blocks until released, recording overlap and hook order.
    struct GatedSource {
        gate: tokio::sync::Semaphore,
        in_flight: std::sync::atomic::AtomicUsize,
        max_in_flight: std::sync::atomic::AtomicUsize,
        hooks_during_poll: std::sync::atomic::AtomicUsize,
        events: Mutex<Vec<&'static str>>,
    }

    struct InFlight<'a>(&'a GatedSource);

    impl Drop for InFlight<'_> {
        fn drop(&mut self) {
            self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl GatedSource {
        fn event(&self, what: &'static str) {
            if self.in_flight.load(Ordering::SeqCst) != 0 {
                self.hooks_during_poll.fetch_add(1, Ordering::SeqCst);
            }
            self.events.lock().unwrap().push(what);
        }
    }

    impl MediaSource for GatedSource {
        fn track(&self) -> TrackInfo {
            OrderedSource {
                started: false.into(),
                polled_before_start: false.into(),
            }
            .track()
        }

        async fn next_packet(&self) -> Option<Packet> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            let _guard = InFlight(self);
            self.gate.acquire().await.ok()?.forget();
            Some(Packet::new(vec![0x80, 96, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]))
        }

        fn on_first_subscriber(&self) {
            self.event("start");
        }

        fn on_last_unsubscribe(&self) {
            self.event("stop");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rapid_resubscribe_never_overlaps_source_calls_or_hooks() {
        let source = Arc::new(GatedSource {
            gate: tokio::sync::Semaphore::new(0),
            in_flight: 0.into(),
            max_in_flight: 0.into(),
            hooks_during_poll: 0.into(),
            events: Mutex::new(Vec::new()),
        });
        let fan = Fanout::new(Arc::clone(&source), 4);
        for round in 0..60 {
            let sub = fan.subscribe();
            if round % 3 == 0 {
                tokio::task::yield_now().await;
            }
            drop(sub);
        }
        let mut last = fan.subscribe();
        eventually("final start", || {
            source.events.lock().unwrap().last() == Some(&"start")
        })
        .await;
        // A packet released now must reach the live subscriber, exactly one poll in flight.
        source.gate.add_permits(1);
        let packet = timeout(Duration::from_secs(5), last.recv()).await.unwrap();
        assert!(packet.is_some());
        drop(last);
        eventually("final stop", || {
            source.events.lock().unwrap().last() == Some(&"stop")
        })
        .await;
        eventually("poll cancelled", || {
            source.in_flight.load(Ordering::SeqCst) == 0
        })
        .await;

        let events = source.events.lock().unwrap().clone();
        let expected: Vec<&str> = (0..events.len())
            .map(|i| if i % 2 == 0 { "start" } else { "stop" })
            .collect();
        assert_eq!(events, expected, "hooks must strictly alternate");
        assert_eq!(
            source.max_in_flight.load(Ordering::SeqCst),
            1,
            "overlapping polls"
        );
        assert_eq!(
            source.hooks_during_poll.load(Ordering::SeqCst),
            0,
            "hook ran during a poll"
        );
    }

    #[tokio::test]
    async fn churn_while_control_is_stalled_coalesces() {
        // A current-thread runtime never schedules the control task during the loop, so every
        // transition happens while it is "stalled". Lifecycle state must stay O(1).
        let src = TestSource::new();
        let fan = Fanout::new(Arc::clone(&src), 4);
        for _ in 0..10_000 {
            drop(fan.subscribe());
        }
        let keep = fan.subscribe();
        eventually("start", || src.starts.load(Ordering::SeqCst) >= 1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (starts, stops) = (
            src.starts.load(Ordering::SeqCst),
            src.stops.load(Ordering::SeqCst),
        );
        assert!(
            starts <= 2 && starts == stops + 1,
            "starts {starts} stops {stops}"
        );
        drop(keep);
        eventually("stop", || src.stops.load(Ordering::SeqCst) == starts).await;
    }

    #[tokio::test]
    async fn absurd_queue_depths_are_clamped_not_panics() {
        let src = TestSource::new();
        for depth in [0, usize::MAX] {
            let fan = Fanout::new(Arc::clone(&src), depth);
            drop(fan.subscribe());
        }
    }

    #[tokio::test]
    async fn discards_malformed_packets() {
        let src = TestSource::new();
        let fan = Fanout::new(Arc::clone(&src), 4);
        let mut sub = fan.subscribe();
        let _ = src.tx.send(Packet::new(vec![1, 2, 3]));
        src.push(9);
        assert_eq!(recv_seq(&mut sub).await, 9);
        assert_eq!(fan.dropped_packets(), 0);
    }
}
