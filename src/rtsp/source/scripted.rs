//! Deterministic packet source for tests and examples.
//!
//! References:
//! - `docs/architecture.md`, "RTSP server contract"

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::Semaphore;

use super::{MediaSource, Packet, TrackInfo};

/// A finite or repeating packet script, optionally gated one packet at a time.
///
/// When the script is exhausted, a non-repeating source returns `None` permanently.
/// A gate permit is consumed for every poll, including the poll that observes the end.
pub struct ScriptedSource {
    track: TrackInfo,
    packets: Vec<Packet>,
    repeat: bool,
    position: Mutex<usize>,
    gate: Option<Arc<Semaphore>>,
    input: Option<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Packet>>>,
    starts: AtomicUsize,
    stops: AtomicUsize,
    polls: AtomicUsize,
    polls_without_subscriber: AtomicUsize,
    poll_started: tokio::sync::Notify,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    hooks_during_poll: AtomicUsize,
    events: Mutex<Vec<&'static str>>,
}

impl ScriptedSource {
    /// Build a packet source with the supplied track, script and optional gate.
    #[must_use]
    pub fn new(
        track: TrackInfo, packets: Vec<Packet>, repeat: bool, gate: Option<Arc<Semaphore>>,
    ) -> Self {
        Self {
            track,
            packets,
            repeat,
            position: Mutex::new(0),
            gate,
            input: None,
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
            polls: AtomicUsize::new(0),
            polls_without_subscriber: AtomicUsize::new(0),
            poll_started: tokio::sync::Notify::new(),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            hooks_during_poll: AtomicUsize::new(0),
            events: Mutex::new(Vec::new()),
        }
    }

    /// Build a channel-fed source; closing the sender ends the stream.
    #[must_use]
    pub fn channel(track: TrackInfo) -> (Self, tokio::sync::mpsc::UnboundedSender<Packet>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut source = Self::new(track, Vec::new(), false, None);
        source.input = Some(tokio::sync::Mutex::new(receiver));
        (source, sender)
    }

    /// Number of packet polls started, including polls still blocked on the gate.
    #[must_use]
    pub fn polls(&self) -> usize {
        self.polls.load(Ordering::SeqCst)
    }

    /// Number of polls started while no subscriber lifecycle was active.
    #[must_use]
    pub fn polls_without_subscriber(&self) -> usize {
        self.polls_without_subscriber.load(Ordering::SeqCst)
    }

    /// Wait until at least `count` packet polls have started.
    pub async fn wait_for_polls(&self, count: usize) {
        loop {
            let notified = self.poll_started.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.polls() >= count {
                return;
            }
            notified.await;
        }
    }

    /// Current number of in-flight polls.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
    }

    /// Maximum number of concurrent polls observed.
    #[must_use]
    pub fn max_in_flight(&self) -> usize {
        self.max_in_flight.load(Ordering::SeqCst)
    }

    /// Number of lifecycle notifications received during a poll.
    #[must_use]
    pub fn hooks_during_poll(&self) -> usize {
        self.hooks_during_poll.load(Ordering::SeqCst)
    }

    /// Lifecycle notifications in order, as `start` and `stop`.
    #[must_use]
    pub fn events(&self) -> Vec<&'static str> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn record_hook(&self, event: &'static str) {
        if self.in_flight() != 0 {
            self.hooks_during_poll.fetch_add(1, Ordering::SeqCst);
        }
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
    }

    /// Number of first-subscriber notifications received.
    #[must_use]
    pub fn starts(&self) -> usize {
        self.starts.load(Ordering::SeqCst)
    }

    /// Number of last-unsubscribe notifications received.
    #[must_use]
    pub fn stops(&self) -> usize {
        self.stops.load(Ordering::SeqCst)
    }
}

struct PollGuard<'a>(&'a ScriptedSource);

impl Drop for PollGuard<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl MediaSource for ScriptedSource {
    fn track(&self) -> TrackInfo {
        self.track.clone()
    }

    async fn next_packet(&self) -> Option<Packet> {
        let in_flight = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(in_flight, Ordering::SeqCst);
        let _guard = PollGuard(self);
        if self.starts() == self.stops() {
            self.polls_without_subscriber.fetch_add(1, Ordering::SeqCst);
        }
        self.polls.fetch_add(1, Ordering::SeqCst);
        self.poll_started.notify_waiters();
        if let Some(gate) = &self.gate {
            gate.acquire().await.ok()?.forget();
        }
        if let Some(input) = &self.input {
            return input.lock().await.recv().await;
        }
        let mut position = self.position.lock().unwrap_or_else(PoisonError::into_inner);
        let packet = self.packets.get(*position)?.clone();
        *position += 1;
        if self.repeat && *position == self.packets.len() {
            *position = 0;
        }
        Some(packet)
    }

    fn on_first_subscriber(&self) {
        self.record_hook("start");
        self.starts.fetch_add(1, Ordering::SeqCst);
    }

    fn on_last_unsubscribe(&self) {
        self.record_hook("stop");
        self.stops.fetch_add(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::rtsp::sdp::MediaKind;

    fn track(codec: &str) -> TrackInfo {
        TrackInfo {
            kind: MediaKind::Video,
            payload_type: 96,
            codec: codec.into(),
            clock_rate: 90_000,
            fmtp: None,
            control: "trackID=0".into(),
        }
    }

    #[tokio::test]
    async fn finite_and_repeating_scripts() {
        let packet = Packet::new(vec![1, 2, 3]);
        let finite = ScriptedSource::new(track("H264"), vec![packet.clone()], false, None);
        assert_eq!(finite.track().codec, "H264");
        assert_eq!((finite.starts(), finite.stops()), (0, 0));
        finite.on_first_subscriber();
        finite.on_last_unsubscribe();
        assert_eq!((finite.starts(), finite.stops()), (1, 1));
        assert_eq!(finite.next_packet().await, Some(packet.clone()));
        assert_eq!(finite.next_packet().await, None);
        let repeating = ScriptedSource::new(track("JPEG"), vec![packet.clone()], true, None);
        assert_eq!(repeating.track().codec, "JPEG");
        assert_eq!(repeating.next_packet().await, Some(packet.clone()));
        assert_eq!(repeating.next_packet().await, Some(packet));
        assert_eq!(repeating.polls(), 2);
        assert_eq!(repeating.polls_without_subscriber(), 2);
        repeating.on_first_subscriber();
        assert!(repeating.next_packet().await.is_some());
        assert_eq!(repeating.polls_without_subscriber(), 2);
        repeating.wait_for_polls(2).await;
        let empty = ScriptedSource::new(track("JPEG"), vec![], true, None);
        assert_eq!(empty.next_packet().await, None);
    }

    #[tokio::test]
    async fn channel_delivers_packets_and_closes() {
        let (source, sender) = ScriptedSource::channel(track("H264"));
        let packet = Packet::new(vec![1]);
        sender.send(packet.clone()).unwrap();
        assert_eq!(source.next_packet().await, Some(packet));
        drop(sender);
        assert_eq!(source.next_packet().await, None);
    }

    #[tokio::test]
    async fn records_in_flight_hooks_and_cancellation() {
        let gate = Arc::new(Semaphore::new(0));
        let source =
            ScriptedSource::new(track("H264"), vec![Packet::new(vec![1])], false, Some(gate));
        let observer = async {
            source.wait_for_polls(1).await;
            assert_eq!(source.in_flight(), 1);
            source.on_first_subscriber();
        };
        let ((), result) = tokio::join!(
            observer,
            tokio::time::timeout(Duration::from_millis(10), source.next_packet())
        );
        assert!(result.is_err());
        assert_eq!(source.in_flight(), 0);
        assert_eq!(source.max_in_flight(), 1);
        assert_eq!(source.hooks_during_poll(), 1);
        assert_eq!(source.events(), ["start"]);
    }

    #[tokio::test]
    async fn gate_waits_for_release_and_closed_gate_ends() {
        let gate = Arc::new(Semaphore::new(0));
        let packet = Packet::new(vec![1]);
        let source = ScriptedSource::new(
            track("H264"),
            vec![packet.clone()],
            false,
            Some(Arc::clone(&gate)),
        );
        let wait = source.wait_for_polls(1);
        let poll = source.next_packet();
        let ((), result) =
            tokio::join!(wait, tokio::time::timeout(Duration::from_millis(10), poll));
        assert!(result.is_err());
        gate.add_permits(1);
        assert_eq!(source.next_packet().await, Some(packet));
        gate.close();
        assert_eq!(source.next_packet().await, None);
    }
}
