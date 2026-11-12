//! The [`Encoder`] trait: planar I420 in, H.264 Annex B access units out.
//!
//! A backend is configured once with an [`EncoderConfig`], then fed frames. Software and
//! hardware encoders implement the same trait, so the RTP layer never knows which one runs.
//!
//! Output contract:
//! - One call to [`Encoder::encode`] produces exactly one access unit, with no lookahead delay
//! - Every IDR access unit starts with the SPS and PPS, so a client that joins late can decode
//!   from the next keyframe
//! - [`Encoder::sps`] and [`Encoder::pps`] are single NAL units without a start code, ready for
//!   `sprop-parameter-sets` in an SDP
//!
//! References:
//! - ITU-T H.264, Table A-1 "Level limits" and Table A-2
//! - [RFC 6184](https://www.rfc-editor.org/rfc/rfc6184), RTP payload format for H.264

use thiserror::Error;

use super::frame::I420Frame;

/// Highest frame rate accepted, frames per second.
pub const MAX_FPS: u32 = 120;

/// H.264 profile. Both are decodable by every NVR and browser; Baseline is the cheapest to
/// encode and has no B-frames or CABAC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// Constrained Baseline: CAVLC entropy coding, no B-frames.
    #[default]
    Baseline,
    /// Main: CABAC entropy coding, about 3% smaller at equal quality, slower to encode.
    Main,
}

/// Encoder parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Coded width in pixels. Even, because 4:2:0 chroma is subsampled by two.
    pub width: u32,
    /// Coded height in pixels. Even.
    pub height: u32,
    /// Nominal frame rate. Drives rate control and the level; the pacer drives the real timing.
    pub fps: u32,
    /// Target average bit rate in bits per second.
    pub bitrate_bps: u32,
    /// Frames between IDR frames, at least 1 (1 is all-intra).
    pub keyframe_interval: u32,
    /// H.264 profile.
    pub profile: Profile,
}

impl EncoderConfig {
    /// The target stream: a7 III live view, 1024x680 at 15 fps, about 3 Mbps, an IDR every two
    /// seconds, Baseline.
    #[must_use]
    pub fn live_view() -> Self {
        Self {
            width: 1024,
            height: 680,
            fps: 15,
            bitrate_bps: 3_000_000,
            keyframe_interval: 30,
            profile: Profile::Baseline,
        }
    }

    /// Check the parameters and pick the lowest H.264 `level_idc` that holds them.
    ///
    /// # Errors
    /// [`EncodeError::InvalidDimensions`], [`EncodeError::InvalidFrameRate`],
    /// [`EncodeError::InvalidBitrate`], [`EncodeError::InvalidKeyframeInterval`] or
    /// [`EncodeError::LevelExceeded`].
    pub fn level_idc(&self) -> Result<u8, EncodeError> {
        if self.width < MIN_DIMENSION
            || self.height < MIN_DIMENSION
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
        {
            return Err(EncodeError::InvalidDimensions {
                width: self.width,
                height: self.height,
            });
        }
        if !(1..=MAX_FPS).contains(&self.fps) {
            return Err(EncodeError::InvalidFrameRate(self.fps));
        }
        if self.bitrate_bps == 0 {
            return Err(EncodeError::InvalidBitrate(self.bitrate_bps));
        }
        if self.keyframe_interval == 0 {
            return Err(EncodeError::InvalidKeyframeInterval(self.keyframe_interval));
        }
        let mbs = u64::from(self.width.div_ceil(16)) * u64::from(self.height.div_ceil(16));
        let mb_rate = mbs * u64::from(self.fps);
        let bitrate = u64::from(self.bitrate_bps);
        LEVELS
            .iter()
            .find(|level| {
                mbs <= level.max_frame_mbs
                    && mb_rate <= level.max_mb_rate
                    && bitrate <= level.max_bitrate_bps
                    && fits_side_limit(self.width, level.max_frame_mbs)
                    && fits_side_limit(self.height, level.max_frame_mbs)
            })
            .map(|level| level.idc)
            .ok_or(EncodeError::LevelExceeded {
                width: self.width,
                height: self.height,
                fps: self.fps,
                bitrate_bps: self.bitrate_bps,
            })
    }
}

/// Smallest accepted side length in pixels.
const MIN_DIMENSION: u32 = 16;

/// One row of H.264 Table A-1, for Baseline and Main.
struct Level {
    idc: u8,
    max_mb_rate: u64,
    max_frame_mbs: u64,
    max_bitrate_bps: u64,
}

/// Table A-1 for levels 1 to 5.2 (1b omitted), ascending. Bit rates are `MaxBR * 1000`
/// (`cpbBrVclFactor` for Baseline and Main).
const LEVELS: [Level; 16] = [
    Level::new(10, 1_485, 99, 64_000),
    Level::new(11, 3_000, 396, 192_000),
    Level::new(12, 6_000, 396, 384_000),
    Level::new(13, 11_880, 396, 768_000),
    Level::new(20, 11_880, 396, 2_000_000),
    Level::new(21, 19_800, 792, 4_000_000),
    Level::new(22, 20_250, 1_620, 4_000_000),
    Level::new(30, 40_500, 1_620, 10_000_000),
    Level::new(31, 108_000, 3_600, 14_000_000),
    Level::new(32, 216_000, 5_120, 20_000_000),
    Level::new(40, 245_760, 8_192, 20_000_000),
    Level::new(41, 245_760, 8_192, 50_000_000),
    Level::new(42, 522_240, 8_704, 50_000_000),
    Level::new(50, 589_824, 22_080, 135_000_000),
    Level::new(51, 983_040, 36_864, 240_000_000),
    Level::new(52, 2_073_600, 36_864, 240_000_000),
];

impl Level {
    const fn new(idc: u8, max_mb_rate: u64, max_frame_mbs: u64, max_bitrate_bps: u64) -> Self {
        Self {
            idc,
            max_mb_rate,
            max_frame_mbs,
            max_bitrate_bps,
        }
    }
}

/// H.264 A.3.1: neither side may exceed `sqrt(MaxFS * 8)` macroblocks.
fn fits_side_limit(side_px: u32, max_frame_mbs: u64) -> bool {
    let mbs = u64::from(side_px.div_ceil(16));
    mbs * mbs <= max_frame_mbs * 8
}

/// Why configuring or running an encoder failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EncodeError {
    /// Width or height is odd or below 16 pixels.
    #[error("dimensions '{width}x{height}' must be even and at least 16 pixels per side")]
    InvalidDimensions {
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
    },
    /// Frame rate is zero or above [`MAX_FPS`].
    #[error("frame rate '{0}' must be 1..={MAX_FPS}")]
    InvalidFrameRate(u32),
    /// Bit rate is zero.
    #[error("bit rate '{0}' bps must be positive")]
    InvalidBitrate(u32),
    /// Keyframe interval is zero.
    #[error("keyframe interval '{0}' must be at least 1 frame")]
    InvalidKeyframeInterval(u32),
    /// No H.264 level up to 5.2 can carry the stream.
    #[error("no H.264 level fits '{width}x{height}' at '{fps}' fps and '{bitrate_bps}' bps")]
    LevelExceeded {
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
        /// Requested frame rate.
        fps: u32,
        /// Requested bit rate.
        bitrate_bps: u32,
    },
    /// The frame does not have the configured dimensions.
    #[error("frame is '{actual_width}x{actual_height}', encoder expects '{width}x{height}'")]
    FrameMismatch {
        /// Configured width.
        width: u32,
        /// Configured height.
        height: u32,
        /// Width of the offending frame.
        actual_width: u32,
        /// Height of the offending frame.
        actual_height: u32,
    },
    /// The backend failed or produced no access unit.
    #[error("encoder backend failed: {0}")]
    Backend(String),
}

/// What [`Encoder::encode`] reports about the access unit it wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodedFrame {
    /// The access unit starts with SPS and PPS and contains an IDR slice.
    pub keyframe: bool,
}

/// An H.264 encoder.
pub trait Encoder {
    /// Create an encoder for `config`.
    ///
    /// # Errors
    /// Any validation error from [`EncoderConfig::level_idc`], or [`EncodeError::Backend`] if the
    /// backend rejects the configuration.
    fn configure(config: EncoderConfig) -> Result<Self, EncodeError>
    where
        Self: Sized;

    /// The configuration in force.
    fn config(&self) -> &EncoderConfig;

    /// The sequence parameter set as one NAL unit, without a start code.
    fn sps(&self) -> &[u8];

    /// The picture parameter set as one NAL unit, without a start code.
    fn pps(&self) -> &[u8];

    /// Make the next frame an IDR. The keyframe interval restarts from it.
    fn force_keyframe(&mut self);

    /// Encode `frame`. `out` is cleared and then holds one Annex B access unit.
    ///
    /// # Errors
    /// [`EncodeError::FrameMismatch`] when `frame` is not the configured size,
    /// [`EncodeError::Backend`] on a backend failure. `out` is empty on error.
    fn encode(&mut self, frame: &I420Frame, out: &mut Vec<u8>)
    -> Result<EncodedFrame, EncodeError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> EncoderConfig {
        EncoderConfig {
            width,
            height,
            fps,
            bitrate_bps,
            keyframe_interval: 30,
            profile: Profile::Baseline,
        }
    }

    #[test]
    fn live_view_target_needs_level_3_1() {
        // 64 x 43 = 2752 MBs exceeds level 3.0's 1620; 41280 MB/s fits level 3.1.
        assert_eq!(EncoderConfig::live_view().level_idc(), Ok(31));
    }

    #[test]
    fn level_follows_resolution_rate_and_bitrate() {
        assert_eq!(config(320, 240, 15, 200_000).level_idc(), Ok(12));
        assert_eq!(config(640, 480, 30, 1_000_000).level_idc(), Ok(30));
        assert_eq!(config(1280, 720, 30, 4_000_000).level_idc(), Ok(31));
        assert_eq!(config(1920, 1080, 30, 8_000_000).level_idc(), Ok(40));
        // Same picture, more bits than level 3.1 allows.
        assert_eq!(config(1024, 680, 15, 15_000_000).level_idc(), Ok(32));
    }

    #[test]
    fn rejects_bad_parameters() {
        assert_eq!(
            config(1023, 680, 15, 1).level_idc(),
            Err(EncodeError::InvalidDimensions {
                width: 1023,
                height: 680
            })
        );
        assert!(matches!(
            config(8, 680, 15, 1).level_idc(),
            Err(EncodeError::InvalidDimensions { .. })
        ));
        assert_eq!(
            config(640, 480, 0, 1).level_idc(),
            Err(EncodeError::InvalidFrameRate(0))
        );
        assert_eq!(
            config(640, 480, MAX_FPS + 1, 1).level_idc(),
            Err(EncodeError::InvalidFrameRate(MAX_FPS + 1))
        );
        assert_eq!(
            config(640, 480, 15, 0).level_idc(),
            Err(EncodeError::InvalidBitrate(0))
        );
        let mut c = config(640, 480, 15, 1_000_000);
        c.keyframe_interval = 0;
        assert_eq!(c.level_idc(), Err(EncodeError::InvalidKeyframeInterval(0)));
    }

    #[test]
    fn impossible_streams_exceed_every_level() {
        assert!(matches!(
            config(16_384, 16_384, 30, 1_000_000).level_idc(),
            Err(EncodeError::LevelExceeded { .. })
        ));
        assert!(matches!(
            config(640, 480, 15, u32::MAX).level_idc(),
            Err(EncodeError::LevelExceeded { .. })
        ));
    }

    #[test]
    fn very_wide_pictures_hit_the_side_limit() {
        // 4096 x 16: 256 MBs wide, but level 3.1 allows sqrt(3600 * 8) = 169 MBs per side.
        let level = config(4096, 16, 1, 100_000).level_idc();
        assert!(matches!(level, Ok(idc) if idc > 31));
    }
}
