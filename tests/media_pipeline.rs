//! The media pipeline through the public API: paced JPEG frames in, H.264 access units out.
//!
//! Everything is synthetic: a generated moving scene, JPEG-encoded in memory, no camera and no
//! external decoder. Bitstream structure is checked with `tethercam::media::annexb`.

use std::time::Duration;

use jpeg_encoder::{ColorType, Encoder as JpegEncoder, SamplingFactor};
use tethercam::media::annexb::{
    NAL_IDR, NAL_PPS, NAL_SLICE, NAL_SPS, nal_unit_type, nal_units, sps_profile_level_id,
};
use tethercam::media::{
    Clock, EncodeError, Encoder, EncoderConfig, I420Frame, JpegDecoder, JpegFrame, ManualClock,
    Pacer, Profile, SoftwareH264Encoder,
};

/// A deterministic, mildly noisy, moving test pattern as RGB.
fn scene_rgb(width: u16, height: u16, t: usize) -> Vec<u8> {
    let mut state = 0x1234_5678_u32 ^ u32::try_from(t).unwrap_or(0);
    let mut rgb = Vec::with_capacity(usize::from(width) * usize::from(height) * 3);
    for y in 0..usize::from(height) {
        for x in 0..usize::from(width) {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = u8::try_from(state >> 29).unwrap_or(0);
            let r = u8::try_from(((x + 3 * t) / 3 + y / 5) % 200).unwrap_or(0) + noise;
            let g = u8::try_from((x / 4 + usize::midpoint(y, t)) % 180).unwrap_or(0) + noise;
            let b = u8::try_from((x + y + 5 * t) % 120).unwrap_or(0) + noise;
            rgb.extend_from_slice(&[r, g, b]);
        }
    }
    rgb
}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn jpeg_of(rgb: &[u8], width: u16, height: u16) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut enc = JpegEncoder::new(&mut out, 85);
    enc.set_sampling_factor(SamplingFactor::F_2_2);
    enc.encode(rgb, width, height, ColorType::Rgb)?;
    Ok(out)
}

fn config(width: u32, height: u32, profile: Profile, keyframe_interval: u32) -> EncoderConfig {
    EncoderConfig {
        width,
        height,
        fps: 15,
        bitrate_bps: 1_000_000,
        keyframe_interval,
        profile,
    }
}

/// Decode `count` synthetic JPEG frames and return them as I420.
fn i420_frames(width: u16, height: u16, count: usize) -> Result<Vec<I420Frame>> {
    let mut decoder = JpegDecoder::new();
    (0..count)
        .map(|t| {
            let jpeg = jpeg_of(&scene_rgb(width, height, t), width, height)?;
            let mut frame = I420Frame::new(2, 2)?;
            decoder.decode_into(&jpeg, &mut frame)?;
            Ok(frame)
        })
        .collect()
}

/// NAL unit types of every NAL in an access unit.
fn nal_types(access_unit: &[u8]) -> Vec<u8> {
    nal_units(access_unit).filter_map(nal_unit_type).collect()
}

/// Encode every frame, returning the access units and their `keyframe` flags.
fn encode_all(enc: &mut SoftwareH264Encoder, frames: &[I420Frame]) -> Result<Vec<(Vec<u8>, bool)>> {
    let mut out = Vec::new();
    frames
        .iter()
        .map(|f| {
            let info = enc.encode(f, &mut out)?;
            Ok((out.clone(), info.keyframe))
        })
        .collect()
}

#[test]
fn first_access_unit_carries_sps_pps_idr() {
    let frames = i420_frames(320, 240, 1).unwrap();
    let mut enc = SoftwareH264Encoder::configure(config(320, 240, Profile::Baseline, 30)).unwrap();
    let aus = encode_all(&mut enc, &frames).unwrap();
    let (au, keyframe) = &aus[0];
    assert!(keyframe);
    assert_eq!(nal_types(au), [NAL_SPS, NAL_PPS, NAL_IDR]);

    // The advertised parameter sets are byte-identical to the in-band ones.
    let nals: Vec<&[u8]> = nal_units(au).collect();
    assert_eq!(enc.sps(), nals[0]);
    assert_eq!(enc.pps(), nals[1]);
}

#[test]
fn parameter_sets_describe_the_configured_stream() {
    for (profile, profile_idc) in [(Profile::Baseline, 0x42), (Profile::Main, 0x4D)] {
        let enc = SoftwareH264Encoder::configure(config(320, 240, profile, 30)).unwrap();
        let [idc, _constraints, level] = sps_profile_level_id(enc.sps()).unwrap();
        assert_eq!(idc, profile_idc, "{profile:?}");
        // 320x240 at 15 fps and 1 Mbps fits level 1.3 (MaxBR 768 kbps is too small for it).
        assert_eq!(level, 20, "{profile:?}");
        assert_eq!(nal_unit_type(enc.sps()), Some(NAL_SPS));
        assert_eq!(nal_unit_type(enc.pps()), Some(NAL_PPS));
    }
    let target = SoftwareH264Encoder::configure(EncoderConfig::live_view()).unwrap();
    let [idc, constraints, level] = sps_profile_level_id(target.sps()).unwrap();
    assert_eq!((idc, level), (0x42, 31), "1024x680 needs level 3.1");
    assert_ne!(constraints & 0x40, 0, "constrained baseline flag");
}

#[test]
fn keyframe_interval_is_honored() {
    let frames = i420_frames(160, 96, 12).unwrap();
    for profile in [Profile::Baseline, Profile::Main] {
        let mut enc = SoftwareH264Encoder::configure(config(160, 96, profile, 5)).unwrap();
        let aus = encode_all(&mut enc, &frames).unwrap();
        let keyframes: Vec<usize> = aus
            .iter()
            .enumerate()
            .filter_map(|(i, (_, k))| k.then_some(i))
            .collect();
        assert_eq!(keyframes, [0, 5, 10], "{profile:?}");
        for (i, (au, keyframe)) in aus.iter().enumerate() {
            if *keyframe {
                assert_eq!(nal_types(au), [NAL_SPS, NAL_PPS, NAL_IDR], "frame {i}");
            } else {
                assert_eq!(nal_types(au), [NAL_SLICE], "frame {i}");
            }
        }
    }
}

#[test]
fn all_intra_when_interval_is_one() {
    let frames = i420_frames(160, 96, 3).unwrap();
    let mut enc = SoftwareH264Encoder::configure(config(160, 96, Profile::Baseline, 1)).unwrap();
    assert!(
        encode_all(&mut enc, &frames)
            .unwrap()
            .iter()
            .all(|(_, k)| *k)
    );
}

#[test]
fn forced_keyframe_lands_on_the_next_frame_and_restarts_the_interval() {
    let frames = i420_frames(160, 96, 12).unwrap();
    let mut enc = SoftwareH264Encoder::configure(config(160, 96, Profile::Baseline, 5)).unwrap();
    let mut out = Vec::new();
    let mut keyframes = Vec::new();
    for (i, frame) in frames.iter().enumerate() {
        if i == 3 {
            enc.force_keyframe();
        }
        if enc.encode(frame, &mut out).unwrap().keyframe {
            keyframes.push(i);
        }
    }
    assert_eq!(keyframes, [0, 3, 8]);
}

#[test]
fn bitrate_tracks_the_target() {
    let frames = i420_frames(320, 240, 30).unwrap();
    let mut enc = SoftwareH264Encoder::configure(config(320, 240, Profile::Baseline, 15)).unwrap();
    let total: usize = encode_all(&mut enc, &frames)
        .unwrap()
        .iter()
        .map(|(au, _)| au.len())
        .sum();
    let bps = total * 8 * 15 / frames.len();
    assert!(
        bps < 1_300_000,
        "{bps} bps overshoots a 1 Mbps target by more than 30%"
    );
    assert!(
        bps > 100_000,
        "{bps} bps: encoder is producing almost nothing"
    );
}

#[test]
fn rejects_mismatched_frames_and_bad_configs() {
    let mut enc = SoftwareH264Encoder::configure(config(160, 96, Profile::Baseline, 5)).unwrap();
    let wrong = I420Frame::new(176, 96).unwrap();
    let mut out = vec![1, 2, 3];
    assert_eq!(
        enc.encode(&wrong, &mut out),
        Err(EncodeError::FrameMismatch {
            width: 160,
            height: 96,
            actual_width: 176,
            actual_height: 96,
        })
    );
    assert!(out.is_empty(), "output is cleared on error");

    for bad in [
        config(161, 96, Profile::Baseline, 5),
        config(160, 96, Profile::Baseline, 0),
        EncoderConfig {
            fps: 0,
            ..config(160, 96, Profile::Baseline, 5)
        },
        EncoderConfig {
            bitrate_bps: 0,
            ..config(160, 96, Profile::Baseline, 5)
        },
    ] {
        assert!(SoftwareH264Encoder::configure(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn paced_jpeg_pipeline_encodes_at_the_constant_rate() {
    let (width, height) = (160_u16, 96_u16);
    let clock = ManualClock::new();
    let mut pacer = Pacer::new(15, clock.clone()).unwrap();
    let mut decoder = JpegDecoder::new();
    let mut enc = SoftwareH264Encoder::configure(config(160, 96, Profile::Baseline, 10)).unwrap();
    let mut frame = I420Frame::new(2, 2).unwrap();
    let mut out = Vec::new();

    // The camera delivers on an irregular schedule: 40..=110 ms gaps, one 400 ms stall.
    let gaps_ms = [
        0_u64, 40, 110, 55, 90, 400, 60, 70, 45, 100, 66, 66, 80, 52, 71, 66,
    ];
    let mut arrivals = Vec::new();
    let mut at = Duration::ZERO;
    for gap in gaps_ms {
        at += Duration::from_millis(gap);
        arrivals.push(at);
    }

    let mut access_units = 0_u64;
    let mut keyframes = Vec::new();
    let mut next = arrivals.iter().enumerate().peekable();
    let mut slots = Vec::new();
    while next.peek().is_some() || pacer.time_until_due().is_some_and(|d| d.is_zero()) {
        let due = pacer.next_due();
        let arrival = next.peek().map(|(_, t)| **t);
        let Some(step_to) = arrival.into_iter().chain(due).min() else {
            break;
        };
        clock.advance_to(step_to);
        while let Some((i, t)) = next.next_if(|(_, t)| **t <= clock.now()) {
            let rgb = scene_rgb(width, height, i);
            let jpeg = JpegFrame::new(jpeg_of(&rgb, width, height).unwrap(), *t);
            pacer.push(jpeg);
        }
        if let Some(paced) = pacer.poll() {
            decoder.decode_into(&paced.frame.data, &mut frame).unwrap();
            let info = enc.encode(&frame, &mut out).unwrap();
            access_units += 1;
            slots.push(paced.slot);
            if info.keyframe {
                keyframes.push(paced.slot);
            }
        }
    }

    assert!(
        slots.windows(2).all(|w| w[1] == w[0] + 1),
        "no gaps: {slots:?}"
    );
    assert_eq!(access_units, u64::try_from(slots.len()).unwrap());
    assert!(
        pacer.stats().duplicated > 0,
        "the 400 ms stall must be bridged by duplicates"
    );
    assert_eq!(keyframes.first(), Some(&0));
    assert!(
        keyframes.windows(2).all(|w| w[1] - w[0] == 10),
        "{keyframes:?}"
    );
}
