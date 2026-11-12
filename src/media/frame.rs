//! Frame types with reusable buffers.
//!
//! - [`JpegFrame`]: one compressed live-view frame and the instant it was captured
//! - [`I420Frame`]: a planar 4:2:0 frame (Y, then Cb, then Cr) with explicit strides
//!
//! Both are plain owned buffers. Steady-state code recycles them: [`JpegFrame::data`] is cleared
//! and refilled, [`I420Frame::resize`] keeps capacity when the dimensions do not grow.
//!
//! References:
//! - [FourCC `I420`](https://fourcc.org/pixel-format/yuv-i420/), planar YUV 4:2:0

use std::time::Duration;

use thiserror::Error;

/// Largest accepted width or height in pixels. Bounds allocation from untrusted dimensions.
pub const MAX_DIMENSION: u32 = 16_384;

/// Largest accepted size of one plane in bytes (512 MiB). Bounds allocation from caller-supplied
/// strides, which the dimension limit alone does not.
pub const MAX_PLANE_BYTES: u64 = 1 << 29;

/// Why an [`I420Frame`] could not be created or resized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FrameError {
    /// Width or height is zero, or above [`MAX_DIMENSION`].
    #[error("frame dimensions '{width}x{height}' must be 1..={MAX_DIMENSION} in both axes")]
    InvalidDimensions {
        /// Requested width in pixels.
        width: u32,
        /// Requested height in pixels.
        height: u32,
    },
    /// A plane would exceed [`MAX_PLANE_BYTES`].
    #[error("plane of '{stride}' bytes x '{rows}' rows exceeds {MAX_PLANE_BYTES} bytes")]
    PlaneTooLarge {
        /// Requested stride in bytes.
        stride: u32,
        /// Rows in the plane.
        rows: u32,
    },
    /// A stride is shorter than the row it has to hold.
    #[error("stride '{stride}' is shorter than the '{row}'-byte row it must hold")]
    StrideTooSmall {
        /// Requested stride in bytes.
        stride: u32,
        /// Bytes needed per row.
        row: u32,
    },
}

/// One live-view JPEG frame as received from the camera.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JpegFrame {
    /// The compressed bytes, a complete JFIF / baseline JPEG stream.
    pub data: Vec<u8>,
    /// When the frame was captured, on the pipeline's [`crate::media::Clock`].
    pub captured_at: Duration,
}

impl JpegFrame {
    /// Wrap compressed bytes captured at `captured_at`.
    #[must_use]
    pub fn new(data: Vec<u8>, captured_at: Duration) -> Self {
        Self { data, captured_at }
    }
}

/// A planar 4:2:0 frame. Chroma planes are `ceil(width / 2)` by `ceil(height / 2)`, so odd
/// dimensions are representable. Each plane holds `stride * rows` bytes; bytes between the end
/// of a row and its stride are padding and unspecified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct I420Frame {
    width: u32,
    height: u32,
    stride_y: u32,
    stride_c: u32,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl I420Frame {
    /// A black frame (limited range: Y 16, Cb and Cr 128) with tight strides.
    ///
    /// # Errors
    /// [`FrameError::InvalidDimensions`] when either dimension is zero or above
    /// [`MAX_DIMENSION`].
    pub fn new(width: u32, height: u32) -> Result<Self, FrameError> {
        let mut frame = Self {
            width: 0,
            height: 0,
            stride_y: 0,
            stride_c: 0,
            y: Vec::new(),
            u: Vec::new(),
            v: Vec::new(),
        };
        frame.resize(width, height)?;
        frame.fill_black();
        Ok(frame)
    }

    /// Change the dimensions, using tight strides and reusing the existing allocations when they
    /// are large enough. Pixel contents afterwards are unspecified; callers overwrite them.
    ///
    /// # Errors
    /// [`FrameError::InvalidDimensions`] as for [`I420Frame::new`]. The frame is unchanged on error.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), FrameError> {
        self.resize_with_strides(width, height, width, width.div_ceil(2))
    }

    /// Like [`I420Frame::resize`] with explicit strides, e.g. rows padded to a SIMD multiple.
    ///
    /// # Errors
    /// [`FrameError::InvalidDimensions`] for bad dimensions, [`FrameError::StrideTooSmall`] when
    /// `stride_y < width` or `stride_c < ceil(width / 2)`.
    pub fn resize_with_strides(
        &mut self, width: u32, height: u32, stride_y: u32, stride_c: u32,
    ) -> Result<(), FrameError> {
        if !(1..=MAX_DIMENSION).contains(&width) || !(1..=MAX_DIMENSION).contains(&height) {
            return Err(FrameError::InvalidDimensions { width, height });
        }
        let chroma_width = width.div_ceil(2);
        if stride_y < width {
            return Err(FrameError::StrideTooSmall {
                stride: stride_y,
                row: width,
            });
        }
        if stride_c < chroma_width {
            return Err(FrameError::StrideTooSmall {
                stride: stride_c,
                row: chroma_width,
            });
        }
        let chroma_height = height.div_ceil(2);
        // Strides are caller-supplied: bound the byte size before any allocation.
        let y_len = plane_len(stride_y, height)?;
        let c_len = plane_len(stride_c, chroma_height)?;
        self.y.resize(y_len, 0);
        self.u.resize(c_len, 128);
        self.v.resize(c_len, 128);
        self.width = width;
        self.height = height;
        self.stride_y = stride_y;
        self.stride_c = stride_c;
        Ok(())
    }

    /// Set every pixel to limited-range black.
    pub fn fill_black(&mut self) {
        self.y.fill(16);
        self.u.fill(128);
        self.v.fill(128);
    }

    /// Luma width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Luma height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Chroma plane width in pixels, `ceil(width / 2)`.
    #[must_use]
    pub fn chroma_width(&self) -> u32 {
        self.width.div_ceil(2)
    }

    /// Chroma plane height in rows, `ceil(height / 2)`.
    #[must_use]
    pub fn chroma_height(&self) -> u32 {
        self.height.div_ceil(2)
    }

    /// Bytes between the starts of consecutive luma rows.
    #[must_use]
    pub fn stride_y(&self) -> u32 {
        self.stride_y
    }

    /// Bytes between the starts of consecutive chroma rows.
    #[must_use]
    pub fn stride_c(&self) -> u32 {
        self.stride_c
    }

    /// The luma plane, `stride_y * height` bytes.
    #[must_use]
    pub fn y(&self) -> &[u8] {
        &self.y
    }

    /// The Cb plane, `stride_c * chroma_height` bytes.
    #[must_use]
    pub fn u(&self) -> &[u8] {
        &self.u
    }

    /// The Cr plane, `stride_c * chroma_height` bytes.
    #[must_use]
    pub fn v(&self) -> &[u8] {
        &self.v
    }

    /// All three planes mutably at once, as `(y, u, v)`.
    pub fn planes_mut(&mut self) -> (&mut [u8], &mut [u8], &mut [u8]) {
        (&mut self.y, &mut self.u, &mut self.v)
    }

    /// The valid bytes of luma row `row`, or `None` past the last row.
    #[must_use]
    pub fn y_row(&self, row: u32) -> Option<&[u8]> {
        plane_row(&self.y, self.stride_y, self.width, row, self.height)
    }

    /// The valid bytes of Cb row `row`, or `None` past the last chroma row.
    #[must_use]
    pub fn u_row(&self, row: u32) -> Option<&[u8]> {
        plane_row(
            &self.u,
            self.stride_c,
            self.chroma_width(),
            row,
            self.chroma_height(),
        )
    }

    /// The valid bytes of Cr row `row`, or `None` past the last chroma row.
    #[must_use]
    pub fn v_row(&self, row: u32) -> Option<&[u8]> {
        plane_row(
            &self.v,
            self.stride_c,
            self.chroma_width(),
            row,
            self.chroma_height(),
        )
    }
}

/// `stride * rows` as a `usize`, or [`FrameError::PlaneTooLarge`] past [`MAX_PLANE_BYTES`].
fn plane_len(stride: u32, rows: u32) -> Result<usize, FrameError> {
    u64::from(stride)
        .checked_mul(u64::from(rows))
        .filter(|&len| len <= MAX_PLANE_BYTES)
        .and_then(|len| usize::try_from(len).ok())
        .ok_or(FrameError::PlaneTooLarge { stride, rows })
}

/// Row `row` of a plane, trimmed to `width` bytes of real pixels.
fn plane_row(plane: &[u8], stride: u32, width: u32, row: u32, rows: u32) -> Option<&[u8]> {
    if row >= rows {
        return None;
    }
    let start = usize::try_from(u64::from(row) * u64::from(stride)).ok()?;
    let end = start.checked_add(usize::try_from(width).ok()?)?;
    plane.get(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_frame_is_black_with_tight_strides() {
        let frame = I420Frame::new(8, 4).unwrap();
        assert_eq!((frame.width(), frame.height()), (8, 4));
        assert_eq!((frame.stride_y(), frame.stride_c()), (8, 4));
        assert_eq!(frame.y().len(), 32);
        assert_eq!(frame.u().len(), 8);
        assert_eq!(frame.v().len(), 8);
        assert!(frame.y().iter().all(|&p| p == 16));
        assert!(frame.u().iter().chain(frame.v()).all(|&p| p == 128));
    }

    #[test]
    fn odd_dimensions_round_chroma_up() {
        let frame = I420Frame::new(7, 5).unwrap();
        assert_eq!((frame.chroma_width(), frame.chroma_height()), (4, 3));
        assert_eq!(frame.y().len(), 35);
        assert_eq!(frame.u().len(), 12);
    }

    #[test]
    fn rejects_zero_and_oversized_dimensions() {
        for (w, h) in [
            (0, 8),
            (8, 0),
            (MAX_DIMENSION + 1, 8),
            (8, MAX_DIMENSION + 1),
        ] {
            assert_eq!(
                I420Frame::new(w, h),
                Err(FrameError::InvalidDimensions {
                    width: w,
                    height: h
                })
            );
        }
    }

    #[test]
    fn resize_reuses_capacity_when_shrinking() {
        let mut frame = I420Frame::new(64, 48).unwrap();
        let (y_ptr, cap) = (frame.y().as_ptr(), frame.y.capacity());
        frame.resize(32, 24).unwrap();
        assert_eq!(frame.y().len(), 32 * 24);
        assert_eq!(frame.y().as_ptr(), y_ptr);
        assert_eq!(frame.y.capacity(), cap);
    }

    #[test]
    fn failed_resize_leaves_frame_unchanged() {
        let mut frame = I420Frame::new(8, 8).unwrap();
        let before = frame.clone();
        assert!(frame.resize(0, 8).is_err());
        assert!(frame.resize_with_strides(8, 8, 4, 4).is_err());
        assert_eq!(frame, before);
    }

    #[test]
    fn padded_strides_are_honoured_by_row_accessors() {
        let mut frame = I420Frame::new(2, 2).unwrap();
        frame.resize_with_strides(6, 4, 16, 8).unwrap();
        assert_eq!(frame.y().len(), 64);
        assert_eq!(frame.u().len(), 16);
        let (y, _, _) = frame.planes_mut();
        y.fill(0);
        y[16..22].fill(9);
        assert_eq!(frame.y_row(1), Some(&[9_u8; 6][..]));
        assert_eq!(frame.y_row(4), None);
        assert_eq!(frame.u_row(1).map(<[u8]>::len), Some(3));
        assert_eq!(frame.v_row(2), None);
    }

    #[test]
    fn huge_strides_are_rejected_before_allocating() {
        let mut frame = I420Frame::new(2, 2).unwrap();
        assert_eq!(
            frame.resize_with_strides(2, 16_384, u32::MAX, u32::MAX),
            Err(FrameError::PlaneTooLarge {
                stride: u32::MAX,
                rows: 16_384
            })
        );
        assert_eq!(frame.y().len(), 4);
    }

    #[test]
    fn stride_shorter_than_row_is_rejected() {
        let mut frame = I420Frame::new(2, 2).unwrap();
        assert_eq!(
            frame.resize_with_strides(10, 4, 9, 5),
            Err(FrameError::StrideTooSmall { stride: 9, row: 10 })
        );
        assert_eq!(
            frame.resize_with_strides(10, 4, 10, 4),
            Err(FrameError::StrideTooSmall { stride: 4, row: 5 })
        );
    }

    #[test]
    fn jpeg_frame_new_keeps_fields() {
        let frame = JpegFrame::new(vec![1, 2, 3], Duration::from_millis(7));
        assert_eq!(frame.data, [1, 2, 3]);
        assert_eq!(frame.captured_at, Duration::from_millis(7));
    }
}
