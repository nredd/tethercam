//! Pure-Rust software H.264 backend, built on `rusty_h264-encoder`.
//!
//! Chosen over the alternatives in `docs/encoder-evaluation.md`. Settings that matter here:
//!
//! - One access unit per frame, no lookahead, no B-frames, one reference frame: lowest latency
//!   and memory, and the output is decodable by every Baseline / Main decoder
//! - Scene-cut detection off: the keyframe interval is exactly what the caller configured
//! - Average-bit-rate control targeting [`EncoderConfig::bitrate_bps`]
//! - The encoder writes into a buffer owned by this struct, so this crate adds no per-frame allocation
//!
//! References:
//! - [`rusty_h264-encoder`](https://docs.rs/rusty_h264-encoder)
//! - ITU-T H.264

use rusty_h264_common::{Profile as RustyProfile, YuvPlanes};
use rusty_h264_encoder::{Encoder as RustyEncoder, EncoderConfig as RustyConfig, Pps, Sps};

use super::annexb::{NAL_PPS, NAL_SPS, contains_idr, nal_unit_type, nal_units};
use super::encoder::{EncodeError, EncodedFrame, Encoder, EncoderConfig, Profile};
use super::frame::I420Frame;

/// Slack on top of the raw frame size for the output buffer: an IDR at very low QP approaches
/// `width * height * 3 / 2`, plus parameter sets and headers.
const OUTPUT_SLACK: usize = 4096;

/// The pure-Rust software encoder. See the [module docs](self).
pub struct SoftwareH264Encoder {
    config: EncoderConfig,
    inner: RustyEncoder,
    sps: Vec<u8>,
    pps: Vec<u8>,
    scratch: Vec<u8>,
}

impl std::fmt::Debug for SoftwareH264Encoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoftwareH264Encoder")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Encoder for SoftwareH264Encoder {
    fn configure(config: EncoderConfig) -> Result<Self, EncodeError> {
        let level = config.level_idc()?;
        let (width, height) = (to_usize(config.width)?, to_usize(config.height)?);

        let mut cfg = RustyConfig::baseline(width, height);
        if config.profile == Profile::Main {
            cfg.profile = RustyProfile::Main;
            cfg.cabac = true;
        }
        cfg.level_idc = level;
        cfg.gop_size = config.keyframe_interval;
        cfg.bitrate = config.bitrate_bps;
        // Precision loss is irrelevant: `fps` is at most 120.
        cfg.framerate = f32::from(u16::try_from(config.fps).unwrap_or(u16::MAX));

        let sps = parameter_set(&Sps::from_config(&cfg).to_nal().to_annex_b(), NAL_SPS)?;
        let pps = parameter_set(&Pps::from_config(&cfg).to_nal().to_annex_b(), NAL_PPS)?;
        let inner = RustyEncoder::new(cfg).map_err(|e| EncodeError::Backend(e.to_string()))?;
        let scratch = vec![0; width * height * 3 / 2 + OUTPUT_SLACK];
        Ok(Self {
            config,
            inner,
            sps,
            pps,
            scratch,
        })
    }

    fn config(&self) -> &EncoderConfig {
        &self.config
    }

    fn sps(&self) -> &[u8] {
        &self.sps
    }

    fn pps(&self) -> &[u8] {
        &self.pps
    }

    fn force_keyframe(&mut self) {
        self.inner.request_keyframe();
    }

    fn encode(
        &mut self, frame: &I420Frame, out: &mut Vec<u8>,
    ) -> Result<EncodedFrame, EncodeError> {
        out.clear();
        if (frame.width(), frame.height()) != (self.config.width, self.config.height) {
            return Err(EncodeError::FrameMismatch {
                width: self.config.width,
                height: self.config.height,
                actual_width: frame.width(),
                actual_height: frame.height(),
            });
        }
        let planes = YuvPlanes::new(
            to_usize(frame.width())?,
            to_usize(frame.height())?,
            frame.y(),
            frame.u(),
            frame.v(),
            to_usize(frame.stride_y())?,
            to_usize(frame.stride_c())?,
        )
        .ok_or_else(|| EncodeError::Backend("frame planes rejected by backend".into()))?;

        let len = self
            .inner
            .encode_into(&planes, &mut self.scratch)
            .map_err(|e| EncodeError::Backend(e.to_string()))?;
        let access_unit = self
            .scratch
            .get(..len)
            .filter(|au| !au.is_empty())
            .ok_or_else(|| EncodeError::Backend("backend produced no access unit".into()))?;
        out.extend_from_slice(access_unit);
        Ok(EncodedFrame {
            keyframe: contains_idr(out),
        })
    }
}

fn to_usize(value: u32) -> Result<usize, EncodeError> {
    usize::try_from(value).map_err(|_| EncodeError::Backend(format!("'{value}' exceeds usize")))
}

/// The single NAL unit of type `expected` in an Annex B serialization of one parameter set.
fn parameter_set(annex_b: &[u8], expected: u8) -> Result<Vec<u8>, EncodeError> {
    nal_units(annex_b)
        .find(|nal| nal_unit_type(nal) == Some(expected))
        .map(<[u8]>::to_vec)
        .ok_or_else(|| EncodeError::Backend(format!("backend emitted no NAL of type '{expected}'")))
}
