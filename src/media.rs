//! Live-view frame handling: pacing, JPEG decode and H.264 encode.
//!
//! The camera delivers JPEG frames at an irregular rate. The main RTSP stream needs constant
//! frame rate H.264. The pipeline between the two, all pure Rust and all driven without hardware:
//!
//! ```text
//! JpegFrame --> Pacer (fixed rate) --> JpegDecoder --> I420Frame --> Encoder --> Annex B
//! ```
//!
//! - [`frame`]: [`JpegFrame`] and [`I420Frame`], with reusable buffers
//! - [`clock`]: the [`Clock`] trait, [`SystemClock`] and the deterministic [`ManualClock`]
//! - [`pacer`]: [`Pacer`], irregular frames in, fixed rate out
//! - [`jpeg`]: [`JpegDecoder`], baseline JPEG (4:2:0, 4:2:2, 4:4:4, grayscale) to I420
//! - [`encoder`]: the [`Encoder`] trait, [`EncoderConfig`] and H.264 level selection
//! - [`h264`]: [`SoftwareH264Encoder`], the pure-Rust backend
//! - [`annexb`]: a total Annex B NAL splitter and SPS header reader
//!
//! The choice of JPEG decoder and H.264 backend, with measurements, is recorded in
//! `docs/encoder-evaluation.md`. Hardware encoders are a later, optional feature behind the same
//! [`Encoder`] trait.
//!
//! References:
//! - ITU-T H.264, Annex B (byte stream format) and Annex A (levels)
//! - ITU-T T.81 / ISO 10918-1 (JPEG) and JFIF 1.02
//! - [RTP payload for H.264, RFC 6184](https://www.rfc-editor.org/rfc/rfc6184), for `profile-level-id`

pub mod annexb;
pub mod clock;
pub mod encoder;
pub mod frame;
pub mod h264;
pub mod jpeg;
pub mod pacer;

pub use clock::{Clock, ManualClock, SystemClock};
pub use encoder::{EncodeError, EncodedFrame, Encoder, EncoderConfig, Profile};
pub use frame::{FrameError, I420Frame, JpegFrame};
pub use h264::SoftwareH264Encoder;
pub use jpeg::{JpegDecoder, JpegError};
pub use pacer::{Paced, Pacer, PacerError, PacerStats};
