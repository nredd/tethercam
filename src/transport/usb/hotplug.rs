//! Camera arrival and removal events from raw bus hot-plug events.
//!
//! The bus reports every USB device; [`Tracker`] keeps only Sony cameras, and reports a removal
//! only for a camera it reported as arrived, because the bus names a removed device by an
//! opaque id and nothing else.
//!
//! References:
//! - nusb 0.2 `watch_devices`: <https://docs.rs/nusb/0.2>

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

use super::camera::{CameraInfo, DeviceSummary, classify};

/// A raw bus event, keyed by the bus's own opaque device id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawEvent<I> {
    /// A device appeared.
    Connected(I, DeviceSummary),
    /// A device disappeared.
    Disconnected(I),
}

/// A camera arrived or was removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CameraEvent {
    /// A camera was plugged in.
    Arrived(CameraInfo),
    /// A camera that had arrived was unplugged.
    Removed(CameraInfo),
}

/// A source of raw bus events, ending (`None`) when the watch is closed.
pub trait EventSource: Send {
    /// The bus's opaque device id.
    type Id: Clone + Eq + Hash + Send;

    /// Wait for the next raw event.
    fn next_event(&mut self) -> impl Future<Output = Option<RawEvent<Self::Id>>> + Send;
}

/// An [`EventSource`] that first replays a backlog, then continues with a live source.
///
/// The bus only reports changes from the moment the watch starts, so the cameras already
/// attached are replayed as `Connected` events; a later removal of one of them is then
/// reported.
#[derive(Debug)]
pub struct Seeded<S: EventSource> {
    backlog: VecDeque<RawEvent<S::Id>>,
    source: S,
}

impl<S: EventSource> Seeded<S> {
    /// Replay `backlog`, then `source`.
    pub fn new(source: S, backlog: impl IntoIterator<Item = RawEvent<S::Id>>) -> Self {
        Self {
            backlog: backlog.into_iter().collect(),
            source,
        }
    }
}

impl<S: EventSource> EventSource for Seeded<S> {
    type Id = S::Id;

    async fn next_event(&mut self) -> Option<RawEvent<S::Id>> {
        if let Some(event) = self.backlog.pop_front() {
            return Some(event);
        }
        self.source.next_event().await
    }
}

/// Filters raw events down to camera events.
#[derive(Debug)]
pub struct Tracker<I> {
    present: HashMap<I, CameraInfo>,
}

impl<I> Default for Tracker<I> {
    fn default() -> Self {
        Self {
            present: HashMap::new(),
        }
    }
}

impl<I: Clone + Eq + Hash> Tracker<I> {
    /// Fold one raw event; `None` when it does not concern a camera.
    pub fn apply(&mut self, event: RawEvent<I>) -> Option<CameraEvent> {
        match event {
            RawEvent::Connected(id, summary) => {
                let camera = classify(&summary)?;
                if self.present.contains_key(&id) {
                    return None;
                }
                self.present.insert(id, camera.clone());
                Some(CameraEvent::Arrived(camera))
            }
            RawEvent::Disconnected(id) => self.present.remove(&id).map(CameraEvent::Removed),
        }
    }
}

/// A stream of [`CameraEvent`]s over an [`EventSource`].
#[derive(Debug)]
pub struct Watcher<S: EventSource> {
    source: S,
    tracker: Tracker<S::Id>,
}

impl<S: EventSource> Watcher<S> {
    /// Watch `source`.
    pub fn new(source: S) -> Self {
        Self {
            source,
            tracker: Tracker::default(),
        }
    }

    /// Wait for the next camera event; `None` once the source ends.
    pub async fn next(&mut self) -> Option<CameraEvent> {
        loop {
            let raw = self.source.next_event().await?;
            if let Some(event) = self.tracker.apply(raw) {
                return Some(event);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::super::camera::{STILL_IMAGE, fixtures::summary};
    use super::*;

    struct Script(VecDeque<RawEvent<u32>>);

    impl EventSource for Script {
        type Id = u32;

        fn next_event(&mut self) -> impl Future<Output = Option<RawEvent<u32>>> + Send {
            std::future::ready(self.0.pop_front())
        }
    }

    #[test]
    fn tracker_reports_a_camera_once() {
        let mut tracker = Tracker::default();
        let device = summary(0x0c34, &[STILL_IMAGE]);
        assert!(
            tracker
                .apply(RawEvent::Connected(1, device.clone()))
                .is_some()
        );
        assert_eq!(tracker.apply(RawEvent::Connected(1, device)), None);
    }

    #[tokio::test]
    async fn seeded_replays_the_backlog_before_the_live_source() {
        let device = summary(0x0c34, &[STILL_IMAGE]);
        let live = Script(VecDeque::from([RawEvent::Disconnected(4)]));
        let mut seeded = Seeded::new(live, [RawEvent::Connected(4, device.clone())]);
        assert_eq!(
            seeded.next_event().await,
            Some(RawEvent::Connected(4, device))
        );
        assert_eq!(seeded.next_event().await, Some(RawEvent::Disconnected(4)));
        assert_eq!(seeded.next_event().await, None);
    }

    #[test]
    fn tracker_reports_cameras_only() {
        let mut tracker = Tracker::default();
        let mut other = summary(0x0c34, &[STILL_IMAGE]);
        other.vendor_id = 1;
        assert_eq!(tracker.apply(RawEvent::Connected(1, other)), None);
        assert_eq!(tracker.apply(RawEvent::Disconnected(1)), None);

        let device = summary(0x0c34, &[STILL_IMAGE]);
        let camera = classify(&device).expect("camera");
        assert_eq!(
            tracker.apply(RawEvent::Connected(2, device)),
            Some(CameraEvent::Arrived(camera.clone()))
        );
        assert_eq!(
            tracker.apply(RawEvent::Disconnected(2)),
            Some(CameraEvent::Removed(camera))
        );
        assert_eq!(tracker.apply(RawEvent::Disconnected(2)), None);
    }

    #[tokio::test]
    async fn watcher_skips_noise_and_ends_with_the_source() {
        let device = summary(0x0c34, &[STILL_IMAGE]);
        let camera = classify(&device).expect("camera");
        let mut noise = summary(0x0c34, &[STILL_IMAGE]);
        noise.vendor_id = 1;
        let mut watcher = Watcher::new(Script(VecDeque::from([
            RawEvent::Connected(9, noise),
            RawEvent::Connected(3, device),
            RawEvent::Disconnected(3),
        ])));
        assert_eq!(
            watcher.next().await,
            Some(CameraEvent::Arrived(camera.clone()))
        );
        assert_eq!(watcher.next().await, Some(CameraEvent::Removed(camera)));
        assert_eq!(watcher.next().await, None);
    }
}
