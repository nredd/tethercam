//! Shared RTSP burst workload for Criterion and its multi-iteration regression test.
//!
//! References:
//! - `docs/architecture.md`, "RTSP server contract"

use std::hint::black_box;
use std::sync::Arc;

use tethercam::rtsp::source::{ScriptedSource, Subscription};
use tethercam::rtsp::{Fanout, MediaKind, Packet, TrackInfo};
use tokio::sync::Semaphore;

/// Packets delivered to each subscriber per measured iteration.
pub(crate) const BURST: usize = 64;

/// A steady-state fan-out workload with persistent subscribers.
pub(crate) struct FanoutBurst {
    gate: Arc<Semaphore>,
    subscribers: Vec<Subscription<ScriptedSource>>,
}

impl FanoutBurst {
    /// Create the source and subscribe inside the caller's Tokio runtime.
    pub(crate) fn new(clients: usize) -> Self {
        let gate = Arc::new(Semaphore::new(0));
        let packet = Packet::new(
            std::iter::once(0x80_u8)
                .chain(std::iter::repeat_n(0, 1199))
                .collect::<Vec<u8>>(),
        );
        let track = TrackInfo {
            kind: MediaKind::Video,
            payload_type: 96,
            codec: "H264".into(),
            clock_rate: 90_000,
            fmtp: None,
            control: "trackID=0".into(),
        };
        let source = ScriptedSource::new(track, vec![packet], true, Some(Arc::clone(&gate)));
        let fanout = Fanout::new(Arc::new(source), BURST * 2);
        let subscribers = (0..clients).map(|_| fanout.subscribe()).collect();
        Self { gate, subscribers }
    }

    /// Publish and drain one complete burst without restarting the source lifecycle.
    pub(crate) async fn run(&mut self) {
        self.gate.add_permits(BURST);
        for subscriber in &mut self.subscribers {
            for _ in 0..BURST {
                let packet = subscriber.recv().await;
                assert!(packet.is_some(), "benchmark source must remain live");
                black_box(packet);
            }
        }
    }
}
