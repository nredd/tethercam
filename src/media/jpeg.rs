//! JPEG to I420 decode on the pure-Rust `zune-jpeg` decoder.
//!
//! The decoder emits interleaved full-resolution YCbCr. This module deinterleaves it into an
//! [`I420Frame`], averaging each 2x2 chroma neighbourhood (a 4:2:0 source is upsampled by the
//! decoder and averaged back, which is exact for flat regions and within rounding elsewhere), and
//! rescales from JPEG's full range to the limited range (Y 16..=235, `CbCr` 16..=240) that an H.264
//! stream without VUI colour description is interpreted as. Colour matrix is BT.601, as JFIF
//! mandates.
//!
//! Handled: baseline and progressive, 4:4:4 / 4:2:2 / 4:2:0 / 4:4:0 / 4:1:1 and grayscale, any
//! width and height including odd ones. Other colour models (CMYK) are rejected. A
//! [`JpegDecoder`] keeps its scratch buffer between frames and the output frame is resized in
//! place, so this module's own buffers are reused. `zune-jpeg` builds its per-image coefficient
//! state inside each decode; that allocation is the decoder's and is not pooled here.
//!
//! References:
//! - ITU-T T.81 / ISO 10918-1 (JPEG)
//! - JFIF 1.02, for the YCbCr matrix and full range
//! - ITU-T H.264 Annex E, `video_full_range_flag` default

use thiserror::Error;
use zune_jpeg::JpegDecoder as ZuneDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

use super::frame::{FrameError, I420Frame, MAX_DIMENSION};

mod structure;

#[cfg(test)]
mod fixtures;

/// Default cap on decoded pixels: 4096 x 4096. Live view is about 0.7 megapixels; the cap exists
/// so a forged header cannot make the decoder allocate gigabytes.
pub const DEFAULT_MAX_PIXELS: u64 = 16 * 1024 * 1024;

/// Why a JPEG could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum JpegError {
    /// The bytes are not a decodable JPEG, or use an unsupported feature.
    #[error("malformed or unsupported JPEG: {0}")]
    Malformed(String),
    /// The header declares more pixels than the decoder's limit.
    #[error("JPEG of '{width}x{height}' exceeds the limit of '{max_pixels}' pixels")]
    TooLarge {
        /// Declared width.
        width: u32,
        /// Declared height.
        height: u32,
        /// The configured pixel cap.
        max_pixels: u64,
    },
    /// The decoded dimensions cannot be held by an [`I420Frame`].
    #[error(transparent)]
    Frame(#[from] FrameError),
}

/// A reusable JPEG decoder.
#[derive(Debug, Clone)]
pub struct JpegDecoder {
    scratch: Vec<u8>,
    max_pixels: u64,
}

impl Default for JpegDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl JpegDecoder {
    /// A decoder with [`DEFAULT_MAX_PIXELS`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_max_pixels(DEFAULT_MAX_PIXELS)
    }

    /// A decoder that rejects images with more than `max_pixels` pixels (for example stills).
    #[must_use]
    pub fn with_max_pixels(max_pixels: u64) -> Self {
        Self {
            scratch: Vec::new(),
            max_pixels,
        }
    }

    /// Decode `jpeg` into `out`, resizing `out` to the image dimensions.
    ///
    /// # Errors
    /// [`JpegError::Malformed`] for anything that is not a supported JPEG. That includes a stream
    /// that ends before its `EOI` marker (the normal result of a truncated transfer, however
    /// deep into the scan the cut falls), undefined marker codes inside the scan, and whatever
    /// the decoder itself rejects in strict mode. The caller drops such a frame and the pacer
    /// repeats the last good one. [`JpegError::TooLarge`] past the pixel cap;
    /// [`JpegError::Frame`] if the dimensions are not representable. Never panics, whatever the
    /// input. `out` is unspecified on error. On `Ok`, `out` has exactly the dimensions declared
    /// in the JPEG header.
    ///
    /// Known limit: a scan whose compressed data was cut and then closed with a forged `EOI`
    /// is structurally complete and is not detected; the decoder fills the missing blocks. The
    /// decoder does not report how many MCUs it actually decoded, so this cannot be checked
    /// without a decoder of our own.
    pub fn decode_into(&mut self, jpeg: &[u8], out: &mut I420Frame) -> Result<(), JpegError> {
        let max_side = usize::try_from(MAX_DIMENSION).unwrap_or(usize::MAX);
        let options = DecoderOptions::default()
            .jpeg_set_out_colorspace(ColorSpace::YCbCr)
            .set_max_width(max_side)
            .set_max_height(max_side)
            // A truncated or corrupt scan is an error, not a frame with gray or missing blocks.
            .set_strict_mode(true);
        // Strict mode alone lets a stream cut inside the last MCUs through with wrong pixels, so
        // the marker structure is checked first: the scan must reach `EOI`.
        structure::validate(jpeg).map_err(|e| JpegError::Malformed(e.to_string()))?;
        let mut decoder = ZuneDecoder::new_with_options(ZCursor::new(jpeg), options);
        decoder.decode_headers().map_err(|e| malformed(&e))?;
        // The decoder cannot widen grayscale to YCbCr; take luma as is and synthesize chroma.
        let gray = decoder.input_colorspace() == Some(ColorSpace::Luma);
        if gray {
            decoder.set_options(options.jpeg_set_out_colorspace(ColorSpace::Luma));
        }

        required(decoder.dimensions(), "no frame header").and_then(|(w, h)| {
            let width = u32::try_from(w).unwrap_or(u32::MAX);
            let height = u32::try_from(h).unwrap_or(u32::MAX);
            if u64::from(width) * u64::from(height) > self.max_pixels {
                return Err(JpegError::TooLarge {
                    width,
                    height,
                    max_pixels: self.max_pixels,
                });
            }
            required(decoder.output_buffer_size(), "no output size").and_then(|needed| {
                self.scratch.resize(needed, 0);
                decoder
                    .decode_into(&mut self.scratch)
                    .map_err(|e| malformed(&e))
                    .and_then(|()| {
                        out.resize(width, height)
                            .map_err(JpegError::from)
                            .map(|()| {
                                if gray {
                                    pack_gray(&self.scratch, out);
                                } else {
                                    pack_i420(&self.scratch, out);
                                }
                            })
                    })
            })
        })
    }
}

fn required<T>(value: Option<T>, message: &str) -> Result<T, JpegError> {
    value.ok_or_else(|| JpegError::Malformed(message.into()))
}

fn malformed(e: &zune_jpeg::errors::DecodeErrors) -> JpegError {
    JpegError::Malformed(e.to_string())
}

/// Full-range luma to limited range: `16 + round(y * 219 / 255)`.
fn limited_luma(y: u8) -> u8 {
    u8::try_from((u32::from(y) * 219 + 127) / 255 + 16).unwrap_or(u8::MAX)
}

/// Full-range chroma to limited range: `128 + round((c - 128) * 224 / 255)`, in integers.
fn limited_chroma(c: u32) -> u8 {
    u8::try_from((c * 448 + 8160) / 510).unwrap_or(u8::MAX)
}

/// Convert interleaved full-range `YCbCr` (`3 * width * height` bytes) into limited-range I420.
fn pack_i420(ycbcr: &[u8], out: &mut I420Frame) {
    let width = to_usize(out.width());
    let height = to_usize(out.height());
    let (stride_y, stride_c) = (to_usize(out.stride_y()), to_usize(out.stride_c()));
    let chroma_width = to_usize(out.chroma_width());
    let row_bytes = width * 3;
    let (y_plane, u_plane, v_plane) = out.planes_mut();

    for (y_row, src) in y_plane
        .chunks_mut(stride_y)
        .zip(ycbcr.chunks_exact(row_bytes))
        .take(height)
    {
        for (dst, [y, _, _]) in y_row.iter_mut().zip(src.as_chunks::<3>().0) {
            *dst = limited_luma(*y);
        }
    }

    let mut src_rows = ycbcr.chunks_exact(row_bytes);
    for (u_row, v_row) in u_plane
        .chunks_mut(stride_c)
        .zip(v_plane.chunks_mut(stride_c))
    {
        let Some(top) = src_rows.next() else { break };
        // An odd height leaves the last chroma row with one source row; it is counted twice.
        let bottom = src_rows.next().unwrap_or(top);
        let pairs = top.chunks(6).zip(bottom.chunks(6)).take(chroma_width);
        for ((cb, cr), (t, b)) in u_row.iter_mut().zip(v_row.iter_mut()).zip(pairs) {
            let (blue, red, n) = chroma_sums(t, b);
            *cb = limited_chroma(rounded_mean(blue, n));
            *cr = limited_chroma(rounded_mean(red, n));
        }
    }
}

/// Convert 8-bit grayscale (`width * height` bytes) into limited-range I420 with neutral chroma.
fn pack_gray(luma: &[u8], out: &mut I420Frame) {
    let width = to_usize(out.width());
    let stride_y = to_usize(out.stride_y());
    let (y_plane, u_plane, v_plane) = out.planes_mut();
    for (y_row, src) in y_plane.chunks_mut(stride_y).zip(luma.chunks_exact(width)) {
        for (dst, &y) in y_row.iter_mut().zip(src) {
            *dst = limited_luma(y);
        }
    }
    u_plane.fill(128);
    v_plane.fill(128);
}

/// Sum Cb and Cr and count the pixels in two interleaved pixel groups (each 6 bytes, or 3 at an
/// odd right edge).
fn chroma_sums(top: &[u8], bottom: &[u8]) -> (u32, u32, u32) {
    let pixels = top
        .as_chunks::<3>()
        .0
        .iter()
        .chain(bottom.as_chunks::<3>().0);
    pixels.fold((0, 0, 0), |(cb, cr, n), [_, b, r]| {
        (cb + u32::from(*b), cr + u32::from(*r), n + 1)
    })
}

/// `round(sum / n)`, with neutral chroma (128) when there were no samples.
fn rounded_mean(sum: u32, n: u32) -> u32 {
    (sum + n / 2).checked_div(n).unwrap_or(128)
}

fn to_usize(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use jpeg_encoder::{ColorType, Encoder as JpegEncoder, SamplingFactor};
    use proptest::prelude::*;

    use super::fixtures::QUADRANTS_420;
    use super::*;

    /// Full-range `(Y, Cb, Cr)` to the limited range the decoder promises.
    fn limited(y: u8, cb: u8, cr: u8) -> [u8; 3] {
        [
            limited_luma(y),
            limited_chroma(u32::from(cb)),
            limited_chroma(u32::from(cr)),
        ]
    }

    /// BT.601 full-range conversion of an RGB triple, the JFIF matrix.
    fn rgb_to_ycbcr(red: u8, green: u8, blue: u8) -> [u8; 3] {
        // 16.16 fixed point BT.601 coefficients.
        let (r, g, b) = (i32::from(red), i32::from(green), i32::from(blue));
        let luma = (19_595 * r + 38_470 * g + 7_471 * b + 32_768) >> 16;
        let cb = 128 + ((-11_059 * r - 21_709 * g + 32_768 * b + 32_768) >> 16);
        let cr = 128 + ((32_768 * r - 27_439 * g - 5_329 * b + 32_768) >> 16);
        let byte = |v: i32| u8::try_from(v.clamp(0, 255)).unwrap();
        [byte(luma), byte(cb), byte(cr)]
    }

    /// Pixel `(Y, Cb, Cr)` at luma coordinates `(x, y)`.
    fn sample(frame: &I420Frame, x: u32, y: u32) -> [u8; 3] {
        [
            frame.y_row(y).unwrap()[x as usize],
            frame.u_row(y / 2).unwrap()[(x / 2) as usize],
            frame.v_row(y / 2).unwrap()[(x / 2) as usize],
        ]
    }

    fn assert_close(actual: [u8; 3], expected: [u8; 3], tolerance: u8, what: &str) {
        for (a, e) in actual.iter().zip(expected) {
            assert!(
                a.abs_diff(e) <= tolerance,
                "{what}: got {actual:?}, want {expected:?}"
            );
        }
    }

    const QUADRANT_COLORS: [[u8; 3]; 4] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [128, 128, 128]];

    /// Encode a `width` x `height` RGB image of four flat quadrants (one per
    /// `QUADRANT_COLORS`, quadrant edges at multiples of `block`).
    fn quadrant_jpeg(width: u16, height: u16, block: u16, sampling: SamplingFactor) -> Vec<u8> {
        let mut rgb = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let q = usize::from(x / block % 2) + 2 * usize::from(y / block % 2);
                rgb.extend_from_slice(&QUADRANT_COLORS[q]);
            }
        }
        let mut out = Vec::new();
        let mut enc = JpegEncoder::new(&mut out, 100);
        enc.set_sampling_factor(sampling);
        enc.encode(&rgb, width, height, ColorType::Rgb).unwrap();
        out
    }

    #[test]
    fn decodes_embedded_fixture_to_known_pixels() {
        let mut frame = I420Frame::new(2, 2).unwrap();
        JpegDecoder::new()
            .decode_into(&QUADRANTS_420, &mut frame)
            .unwrap();
        assert_eq!((frame.width(), frame.height()), (32, 32));
        assert_eq!((frame.chroma_width(), frame.chroma_height()), (16, 16));

        // Full-range values from an independent decoder (see the fixture docs), mapped to the
        // limited range. Tolerance covers IDCT rounding differences between decoders.
        let expected = [
            ((4, 4), limited(76, 84, 255), "red"),
            ((24, 4), limited(150, 44, 22), "green"),
            ((4, 24), limited(29, 255, 107), "blue"),
            ((24, 24), limited(128, 129, 128), "gray"),
        ];
        for ((x, y), want, name) in expected {
            assert_close(sample(&frame, x, y), want, 2, name);
        }
        // Spot-check the absolute numbers so the range mapping itself is pinned.
        assert_close(sample(&frame, 4, 4), [81, 90, 240], 2, "red, absolute");
    }

    #[test]
    fn range_mapping_hits_the_limited_range_endpoints() {
        assert_eq!((limited_luma(0), limited_luma(255)), (16, 235));
        assert_eq!((limited_chroma(0), limited_chroma(255)), (16, 240));
        assert_eq!(limited_chroma(128), 128);
    }

    #[test]
    fn decodes_each_subsampling_to_the_same_colors() {
        for (name, sampling) in [
            ("4:4:4", SamplingFactor::F_1_1),
            ("4:2:2", SamplingFactor::F_2_1),
            ("4:2:0", SamplingFactor::F_2_2),
        ] {
            let jpeg = quadrant_jpeg(64, 64, 32, sampling);
            let mut frame = I420Frame::new(2, 2).unwrap();
            JpegDecoder::new().decode_into(&jpeg, &mut frame).unwrap();
            assert_eq!((frame.width(), frame.height()), (64, 64), "{name}");
            for ((x, y), rgb) in [(16, 16), (48, 16), (16, 48), (48, 48)]
                .into_iter()
                .zip(QUADRANT_COLORS)
            {
                let [yy, cb, cr] = rgb_to_ycbcr(rgb[0], rgb[1], rgb[2]);
                assert_close(sample(&frame, x, y), limited(yy, cb, cr), 3, name);
            }
        }
    }

    #[test]
    fn odd_dimensions_decode_with_rounded_up_chroma() {
        for (w, h) in [(33_u16, 17_u16), (1, 1), (17, 33), (31, 31)] {
            let jpeg = quadrant_jpeg(w, h, 16, SamplingFactor::F_2_2);
            let mut frame = I420Frame::new(2, 2).unwrap();
            JpegDecoder::new().decode_into(&jpeg, &mut frame).unwrap();
            let (w, h) = (u32::from(w), u32::from(h));
            assert_eq!((frame.width(), frame.height()), (w, h));
            assert_eq!(frame.u().len(), (w.div_ceil(2) * h.div_ceil(2)) as usize);
            // The first pixel is red in every quadrant layout.
            let [yy, cb, cr] = rgb_to_ycbcr(255, 0, 0);
            assert_close(sample(&frame, 0, 0), limited(yy, cb, cr), 3, "origin");
        }
    }

    #[test]
    fn grayscale_has_neutral_chroma() {
        let pixels = vec![200_u8; 16 * 16];
        let mut jpeg = Vec::new();
        JpegEncoder::new(&mut jpeg, 100)
            .encode(&pixels, 16, 16, ColorType::Luma)
            .unwrap();
        let mut frame = I420Frame::new(2, 2).unwrap();
        JpegDecoder::new().decode_into(&jpeg, &mut frame).unwrap();
        assert_close(
            sample(&frame, 8, 8),
            [limited_luma(200), 128, 128],
            2,
            "gray",
        );
    }

    #[test]
    fn reuses_output_and_scratch_buffers_across_frames() {
        let jpeg = quadrant_jpeg(64, 48, 16, SamplingFactor::F_2_2);
        let mut decoder = JpegDecoder::new();
        let mut frame = I420Frame::new(2, 2).unwrap();
        decoder.decode_into(&jpeg, &mut frame).unwrap();
        let (y_ptr, scratch_ptr) = (frame.y().as_ptr(), decoder.scratch.as_ptr());
        let first = frame.clone();
        for _ in 0..3 {
            decoder.decode_into(&jpeg, &mut frame).unwrap();
        }
        assert_eq!(frame, first);
        assert_eq!(frame.y().as_ptr(), y_ptr);
        assert_eq!(decoder.scratch.as_ptr(), scratch_ptr);
    }

    #[test]
    fn honours_output_strides() {
        let jpeg = quadrant_jpeg(32, 32, 16, SamplingFactor::F_2_2);
        let mut tight = I420Frame::new(2, 2).unwrap();
        JpegDecoder::new().decode_into(&jpeg, &mut tight).unwrap();
        // `decode_into` resizes with tight strides, so a padded output is a caller concern:
        // verify the row accessors still return the same pixels after `resize_with_strides`.
        let mut padded = I420Frame::new(2, 2).unwrap();
        padded.resize_with_strides(32, 32, 48, 24).unwrap();
        pack_i420(&vec![200_u8; 32 * 32 * 3], &mut padded);
        assert_eq!(padded.y_row(31).unwrap(), &[limited_luma(200); 32][..]);
    }

    #[test]
    fn decoder_default_and_structurally_valid_invalid_jpegs() {
        let mut frame = I420Frame::new(2, 2).unwrap();
        let mut decoder = JpegDecoder::default();
        assert!(
            decoder
                .decode_into(&[0xff, 0xd8, 0xff, 0xd9], &mut frame)
                .is_err()
        );
        assert_eq!(
            required::<usize>(None, "missing"),
            Err(JpegError::Malformed("missing".into()))
        );
        assert_eq!(
            required::<(usize, usize)>(None, "missing"),
            Err(JpegError::Malformed("missing".into()))
        );
        pack_i420(&[], &mut frame);
        assert_eq!(declared_size(&QUADRANTS_420), Some((32, 32)));
        assert_eq!(declared_size(&[]), None);
        assert_eq!(declared_size(&[0xff, 0xc0]), None);
    }

    #[test]
    fn rejects_truncated_and_non_jpeg_input() {
        let mut frame = I420Frame::new(2, 2).unwrap();
        let mut decoder = JpegDecoder::new();
        assert!(decoder.decode_into(&[], &mut frame).is_err());
        assert!(
            decoder
                .decode_into(b"not a jpeg at all", &mut frame)
                .is_err()
        );
        let cut = QUADRANTS_420.len() / 2;
        assert!(
            decoder
                .decode_into(&QUADRANTS_420[..cut], &mut frame)
                .is_err()
        );
    }

    /// `(width, height)` from the first baseline / extended / progressive `SOF` marker.
    fn declared_size(jpeg: &[u8]) -> Option<(u32, u32)> {
        let sof = jpeg
            .windows(2)
            .position(|w| w[0] == 0xFF && matches!(w[1], 0xC0..=0xC2))?;
        let size = jpeg.get(sof + 5..sof + 9)?;
        Some((
            u32::from(u16::from_be_bytes([size[2], size[3]])),
            u32::from(u16::from_be_bytes([size[0], size[1]])),
        ))
    }

    /// Offset of the first entropy-coded byte of `QUADRANTS_420`: after `SOS` and its header.
    fn scan_start() -> usize {
        let sos = QUADRANTS_420
            .windows(2)
            .position(|w| w == [0xFF, 0xDA])
            .unwrap();
        sos + 2
            + usize::from(u16::from_be_bytes([
                QUADRANTS_420[sos + 2],
                QUADRANTS_420[sos + 3],
            ]))
    }

    #[test]
    fn data_cut_off_inside_the_scan_is_an_error() {
        let start = scan_start();
        let end = QUADRANTS_420.len() - 2; // before `EOI`
        assert!(start + 40 < end, "fixture scan is long enough to cut");
        let mut frame = I420Frame::new(2, 2).unwrap();
        for cut in [
            start + 1,
            start + 8,
            start + 20,
            start + 40,
            usize::midpoint(start, end),
            end - 8,
        ] {
            let result = JpegDecoder::new().decode_into(&QUADRANTS_420[..cut], &mut frame);
            assert!(result.is_err(), "cut at {cut} decoded: {result:?}");
        }
    }

    #[test]
    fn every_prefix_of_a_valid_jpeg_is_an_error() {
        let mut frame = I420Frame::new(2, 2).unwrap();
        let mut decoder = JpegDecoder::new();
        for cut in 0..QUADRANTS_420.len() {
            assert!(
                decoder
                    .decode_into(&QUADRANTS_420[..cut], &mut frame)
                    .is_err(),
                "prefix of {cut} bytes decoded"
            );
        }
        assert!(decoder.decode_into(&QUADRANTS_420, &mut frame).is_ok());
    }

    #[test]
    fn real_shaped_jpegs_cut_inside_the_scan_are_errors() {
        // Standard Huffman tables, no restart interval, 4:2:2 and 4:2:0, like the camera's output.
        for sampling in [SamplingFactor::F_2_1, SamplingFactor::F_2_2] {
            let jpeg = quadrant_jpeg(256, 128, 32, sampling);
            let start = jpeg.windows(2).position(|w| w == [0xFF, 0xDA]).unwrap() + 14;
            let mut frame = I420Frame::new(2, 2).unwrap();
            JpegDecoder::new().decode_into(&jpeg, &mut frame).unwrap();
            for numerator in 1..=7 {
                let cut = start + (jpeg.len() - 2 - start) * numerator / 8;
                assert!(
                    JpegDecoder::new()
                        .decode_into(&jpeg[..cut], &mut frame)
                        .is_err(),
                    "{sampling:?} cut at {cut}"
                );
            }
        }
    }

    #[test]
    fn malformed_entropy_is_rejected_by_decoder() {
        let start = scan_start();
        let mut data = QUADRANTS_420.to_vec();
        let end = data.len() - 2;
        data[start..end].fill(0xff);
        // Stuff FF bytes so the marker validator accepts the scan.
        for byte in data[start..end].iter_mut().skip(1).step_by(2) {
            *byte = 0;
        }
        assert_eq!(structure::validate(&data), Ok(()));
        let mut frame = I420Frame::new(2, 2).unwrap();
        assert!(JpegDecoder::new().decode_into(&data, &mut frame).is_err());
    }

    #[test]
    fn corrupt_scan_bytes_are_an_error() {
        let start = scan_start();
        // A comment marker in the middle of the scan swallows the rest of it.
        let mut data = QUADRANTS_420.to_vec();
        data[start + 30] = 0xFF;
        data[start + 31] = 0xFE;
        let mut frame = I420Frame::new(2, 2).unwrap();
        assert!(JpegDecoder::new().decode_into(&data, &mut frame).is_err());
        // An undefined `0xFF` escape inside the scan.
        let mut data = QUADRANTS_420.to_vec();
        data[start + 10] = 0xFF;
        data[start + 11] = 0x7F;
        assert!(JpegDecoder::new().decode_into(&data, &mut frame).is_err());
    }

    #[test]
    fn pixel_cap_rejects_before_allocating() {
        let mut frame = I420Frame::new(2, 2).unwrap();
        let mut decoder = JpegDecoder::with_max_pixels(1000);
        assert_eq!(
            decoder.decode_into(&QUADRANTS_420, &mut frame),
            Err(JpegError::TooLarge {
                width: 32,
                height: 32,
                max_pixels: 1000
            })
        );
        assert_eq!(decoder.scratch.len(), 0);
    }

    proptest! {
        #[test]
        fn never_panics_on_arbitrary_bytes(data in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let mut frame = I420Frame::new(2, 2).unwrap();
            let _ = JpegDecoder::new().decode_into(&data, &mut frame);
        }

        #[test]
        fn mutated_valid_jpeg_never_panics_and_ok_has_declared_size(
            flips in proptest::collection::vec((0..QUADRANTS_420.len(), any::<u8>()), 0..16),
            cut in 0..=QUADRANTS_420.len(),
        ) {
            let mut data = QUADRANTS_420.to_vec();
            for (i, b) in flips {
                data[i] = b;
            }
            data.truncate(cut);
            let mut frame = I420Frame::new(2, 2).unwrap();
            if JpegDecoder::new().decode_into(&data, &mut frame).is_ok() {
                // A mutation may legitimately change the header's size; the frame must match it.
                prop_assert_eq!(Some((frame.width(), frame.height())), declared_size(&data));
            }
        }
    }
}
