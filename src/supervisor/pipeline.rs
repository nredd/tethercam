//! Paced media processing, isolated from the PTP actor by a single latest-frame slot.
//!
//! References:
//! - [RFC 6184](https://www.rfc-editor.org/rfc/rfc6184)
//! - [RFC 2435](https://www.rfc-editor.org/rfc/rfc2435)

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc, watch};

use super::Control;

#[cfg(test)]
#[path = "pipeline_tests.rs"]
mod tests;
use crate::config::Streams;
use crate::media::{
    Encoder, EncoderConfig, I420Frame, JpegDecoder, ManualClock, Pacer, Profile,
    SoftwareH264Encoder,
};
use crate::rtp::{PacketPool, StreamConfig, h264::H264Packetizer, jpeg::JpegPacketizer};
use crate::rtsp::sdp::{MediaKind, TrackInfo};
use crate::rtsp::source::{MediaSource, Packet};

/// A media/configuration failure, never containing camera bytes or authentication material.
#[derive(Debug, Error)]
pub enum PipelineError {
    /// Invalid JPEG or unsafe dimensions.
    #[error(transparent)]
    Jpeg(#[from] crate::media::JpegError),
    /// Encoder settings or frame rejected.
    #[error(transparent)]
    Encode(#[from] crate::media::EncodeError),
    /// Invalid pacer frame rate.
    #[error(transparent)]
    Pacer(#[from] crate::media::PacerError),
    /// Invalid H.264 packetization.
    #[error(transparent)]
    H264(#[from] crate::rtp::h264::H264Error),
    /// JPEG cannot be carried by RFC 2435.
    #[error(transparent)]
    RtpJpeg(#[from] crate::rtp::jpeg::JpegError),
    /// System entropy for per-run RTP identifiers was unavailable.
    #[error("RTP entropy unavailable")]
    Random,
    /// The bounded CPU worker terminated unexpectedly.
    #[error("media worker failed")]
    Worker,
}

type Queued = (u64, Packet);

/// A cancel-safe RTP source. Cancelling a socket's poll can never cancel camera I/O.
/// The queue is bounded, stale subscription generations are discarded, and hooks only update
/// desired state; they never wait for the camera or encoder.
pub struct RuntimeSource {
    track: TrackInfo,
    jpeg: bool,
    epoch: AtomicU64,
    rx: Mutex<mpsc::Receiver<Queued>>,
    control: Control,
}

impl MediaSource for RuntimeSource {
    fn track(&self) -> TrackInfo {
        self.track.clone()
    }

    async fn next_packet(&self) -> Option<Packet> {
        let mut rx = self.rx.lock().await;
        while let Some((epoch, packet)) = rx.recv().await {
            if epoch == self.epoch.load(Ordering::Relaxed) {
                return Some(packet);
            }
        }
        None
    }

    fn on_first_subscriber(&self) {
        let epoch = self
            .epoch
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
            .max(1);
        self.epoch.store(epoch, Ordering::Relaxed);
        self.control.demand.send_modify(|[main, jpeg]| {
            *if self.jpeg { jpeg } else { main } = epoch;
        });
    }

    fn on_last_unsubscribe(&self) {
        self.control.demand.send_modify(|[main, jpeg]| {
            *if self.jpeg { jpeg } else { main } = 0;
        });
    }
}

/// One stateful decode/encode/packetization worker. Only one job can be in flight.
struct Processor {
    decoder: JpegDecoder,
    frame: I420Frame,
    format: (u32, u32),
    encoder: Option<SoftwareH264Encoder>,
    h264: H264Packetizer,
    jpeg: JpegPacketizer,
    annex_b: Vec<u8>,
    pool: PacketPool,
    outputs: [mpsc::Sender<Queued>; 2],
    epochs: [u64; 2],
}

impl Processor {
    fn process(
        &mut self, jpeg: &[u8], timestamp: u32, epochs: [u64; 2],
    ) -> Result<(), PipelineError> {
        let [main_epoch, jpeg_epoch] = epochs;
        self.main(jpeg, timestamp, main_epoch, epochs)
            .and_then(|()| self.mjpeg(jpeg, timestamp, jpeg_epoch))
            .map(|()| self.epochs = epochs)
    }

    fn main(
        &mut self, jpeg: &[u8], timestamp: u32, epoch: u64, epochs: [u64; 2],
    ) -> Result<(), PipelineError> {
        let Some(encoder) = &mut self.encoder else {
            return Ok(());
        };
        if epoch == 0 {
            return Ok(());
        }
        if self.epochs != epochs {
            encoder.force_keyframe();
        }
        self.decoder.decode_into(jpeg, &mut self.frame)?;
        encoder.encode(&self.frame, &mut self.annex_b)?;
        let [main_tx, _] = &self.outputs;
        self.h264
            .packetize(&self.annex_b, timestamp, &mut self.pool)
            .map(|()| publish(main_tx, epoch, &self.pool))
            .map_err(PipelineError::from)
    }

    fn mjpeg(&mut self, jpeg: &[u8], timestamp: u32, epoch: u64) -> Result<(), PipelineError> {
        if epoch == 0 {
            return Ok(());
        }
        let info = crate::rtp::jpeg::JpegInfo::parse(jpeg)?;
        if (u32::from(info.width), u32::from(info.height)) != self.format {
            return Err(crate::media::EncodeError::FrameMismatch {
                width: self.format.0,
                height: self.format.1,
                actual_width: u32::from(info.width),
                actual_height: u32::from(info.height),
            }
            .into());
        }
        let [_, jpeg_tx] = &self.outputs;
        self.jpeg
            .packetize(jpeg, timestamp, &mut self.pool)
            .map(|()| publish(jpeg_tx, epoch, &self.pool))
            .map_err(PipelineError::from)
    }
}

fn publish(tx: &mpsc::Sender<Queued>, epoch: u64, pool: &PacketPool) {
    // Never wait for network clients. Once a queue fills, drop the rest of this frame.
    for packet in pool.iter() {
        if tx.try_send((epoch, Packet::new(packet.to_vec()))).is_err() {
            break;
        }
    }
}

type Processed = (Processor, Result<(), PipelineError>);
type SpawnWorker = fn(Processor, Arc<[u8]>, u32, [u64; 2]) -> tokio::task::JoinHandle<Processed>;

fn spawn_worker(
    mut processor: Processor, frame: Arc<[u8]>, timestamp: u32, epochs: [u64; 2],
) -> tokio::task::JoinHandle<Processed> {
    tokio::task::spawn_blocking(move || {
        let result = processor.process(&frame, timestamp, epochs);
        (processor, result)
    })
}

fn worker_result<T>(result: Result<T, tokio::task::JoinError>) -> Result<T, PipelineError> {
    result.map_err(|_| PipelineError::Worker)
}

/// Per-run packetizer settings and the shared random timestamp origin.
type RtpConfig = ([StreamConfig; 2], u32);

/// Draw per-run RTP identities, initial sequence numbers and the timestamp origin. Pair the
/// second SSRC by flipping one bit, keeping both local streams distinct without retries.
fn stream_configs(
    fill: &mut dyn FnMut(&mut [u8]) -> Result<(), getrandom::Error>,
) -> Result<RtpConfig, PipelineError> {
    let mut bytes = [[0; 4]; 3];
    fill(bytes.as_flattened_mut()).map_err(|_| PipelineError::Random)?;
    let [identity, sequences, timestamp] = bytes;
    let [main_hi, main_lo, jpeg_hi, jpeg_lo] = sequences;
    let ssrc = u32::from_be_bytes(identity);
    let config = |payload_type, ssrc, initial_sequence| StreamConfig {
        payload_type,
        ssrc,
        initial_sequence,
        mtu: 1200,
    };
    Ok((
        [
            config(96, ssrc, u16::from_be_bytes([main_hi, main_lo])),
            config(26, ssrc ^ 1, u16::from_be_bytes([jpeg_hi, jpeg_lo])),
        ],
        u32::from_be_bytes(timestamp),
    ))
}

pub(crate) struct Pipeline {
    processor: Processor,
    frames: watch::Receiver<Option<Arc<[u8]>>>,
    control: Control,
    fps: u32,
    timestamp_base: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl Pipeline {
    pub(crate) fn prepare(
        first: &[u8], settings: &Streams, control: Control,
    ) -> Result<(Self, [Arc<RuntimeSource>; 2]), PipelineError> {
        Self::prepare_with_streams(
            first,
            settings,
            control,
            stream_configs(&mut getrandom::fill),
        )
    }

    fn prepare_with_streams(
        first: &[u8], settings: &Streams, control: Control,
        streams: Result<RtpConfig, PipelineError>,
    ) -> Result<(Self, [Arc<RuntimeSource>; 2]), PipelineError> {
        let ([main_config, jpeg_config], timestamp_base) = streams?;
        if settings.mjpeg {
            crate::rtp::jpeg::JpegInfo::parse(first)?;
        }
        let mut decoder = JpegDecoder::new();
        let mut frame = I420Frame::default();
        decoder.decode_into(first, &mut frame)?;
        let encoder = if settings.h264 {
            Some(SoftwareH264Encoder::configure(EncoderConfig {
                width: frame.width(),
                height: frame.height(),
                fps: settings.frame_rate,
                bitrate_bps: settings.bitrate,
                keyframe_interval: settings.frame_rate.saturating_mul(2),
                profile: Profile::Baseline,
            })?)
        } else {
            None
        };
        let fmtp = encoder.as_ref().map(|encoder| {
            let b64 = base64::engine::general_purpose::STANDARD;
            format!(
                "packetization-mode=1;sprop-parameter-sets={},{}",
                b64.encode(encoder.sps()),
                b64.encode(encoder.pps())
            )
        });
        let (h264_tx, h264_rx) = mpsc::channel(512);
        let (jpeg_tx, jpeg_rx) = mpsc::channel(512);
        let source = |stream, codec: &str, payload_type, fmtp, rx| {
            Arc::new(RuntimeSource {
                track: TrackInfo {
                    kind: MediaKind::Video,
                    payload_type,
                    codec: codec.into(),
                    clock_rate: 90_000,
                    fmtp,
                    control: "trackID=0".into(),
                },
                jpeg: stream,
                epoch: AtomicU64::new(0),
                rx: Mutex::new(rx),
                control: control.clone(),
            })
        };
        let sources = [
            source(false, "H264", 96, fmtp, h264_rx),
            source(true, "JPEG", 26, None, jpeg_rx),
        ];
        H264Packetizer::new(main_config)
            .map_err(PipelineError::from)
            .and_then(|h264| {
                JpegPacketizer::new(jpeg_config)
                    .map_err(PipelineError::from)
                    .map(|jpeg| {
                        let processor = Processor {
                            format: (frame.width(), frame.height()),
                            decoder,
                            encoder,
                            frame,
                            h264,
                            jpeg,
                            annex_b: Vec::new(),
                            pool: PacketPool::new(),
                            outputs: [h264_tx, jpeg_tx],
                            epochs: [0; 2],
                        };
                        (
                            Self {
                                width: processor.frame.width(),
                                height: processor.frame.height(),
                                processor,
                                frames: control.frames.clone(),
                                control,
                                fps: settings.frame_rate,
                                timestamp_base,
                            },
                            sources,
                        )
                    })
            })
    }

    /// Reject a frame whose declared size is not the one the streams were described with, even
    /// while nobody streams: the camera changed mode, or another body replaced it. Serving it
    /// under the old SDP and ONVIF profiles would be wrong. A frame without a readable header is
    /// left to processing, which reports it.
    fn check_size(&self, frame: &[u8]) -> Result<(), PipelineError> {
        let (width, height) = self.processor.format;
        match JpegDecoder::dimensions(frame) {
            Ok(actual) if actual != (width, height) => {
                Err(crate::media::EncodeError::FrameMismatch {
                    width,
                    height,
                    actual_width: actual.0,
                    actual_height: actual.1,
                }
                .into())
            }
            _ => Ok(()),
        }
    }

    pub(crate) async fn run(self) -> Result<(), PipelineError> {
        self.run_with_worker(spawn_worker).await
    }

    async fn run_with_worker(mut self, spawn: SpawnWorker) -> Result<(), PipelineError> {
        let clock = ManualClock::new();
        let origin = tokio::time::Instant::now();
        let mut pacer = Pacer::new(self.fps, clock.clone())?;
        let mut demand = self.control.demand.subscribe();
        let mut stop = self.control.stop.subscribe();
        let mut fresh = true;
        loop {
            if self.control.stopping() {
                return Ok(());
            }
            let epochs = *demand.borrow();
            let active = epochs.iter().any(|epoch| *epoch != 0);
            clock.advance_to(origin.elapsed());
            if fresh {
                let latest = self.frames.borrow_and_update().clone();
                if let Some(frame) = latest {
                    self.check_size(&frame)?;
                    pacer.push(frame);
                } else {
                    // Never repeat a stale frame after unplug, even while clients stay subscribed.
                    pacer.clear();
                    self.processor.epochs = [0; 2];
                }
                fresh = false;
            }
            if active && let Some(paced) = pacer.poll() {
                let frame = Arc::clone(paced.frame);
                let timestamp = u32::try_from(
                    (paced.due.as_nanos() * 90_000 / 1_000_000_000) & u128::from(u32::MAX),
                )
                .unwrap_or(0)
                .wrapping_add(self.timestamp_base);
                // Exactly one bounded CPU job, always joined, including during shutdown.
                // The camera actor stays runnable while decoding/encoding consumes a core.
                let result = spawn(self.processor, frame, timestamp, epochs).await;
                let result = worker_result(result)?;
                self.processor = result.0;
                result.1?;
            }
            clock.advance_to(origin.elapsed());
            let wait = pacer.time_until_due().unwrap_or(Duration::from_secs(1));
            tokio::select! {
                _ = stop.changed() => {},
                _ = demand.changed() => {},
                changed = self.frames.changed() => {
                    if changed.is_err() { return Ok(()); }
                    fresh = true;
                },
                () = tokio::time::sleep(wait), if active && pacer.next_due().is_some() => {},
            }
        }
    }
}
