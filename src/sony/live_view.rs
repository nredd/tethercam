//! The live-view object: a small Sony header in front of a baseline JPEG.
//!
//! `GetObject` on handle `0xFFFFC002` returns an object that starts with two little-endian
//! `u32`s, the offset and size of the JPEG inside the object. 3.00 bodies append the offset and
//! size of a focus-frame block to the header; the JPEG fields come first in both generations, so
//! one parser serves both. The frame is never decoded here.
//!
//! References:
//! - `watsonmw/alphawire` `src/aw/aw-control.c`, `PTP_GetLiveViewImage`
//! - libgphoto2 `camlibs/ptp2/library.c`, Sony live view (uses the offset, checks for `FFD8`)

use super::error::LiveViewError;
use crate::ptp::Reader;

/// Bytes of the header both generations share: JPEG offset and JPEG size.
const HEADER_LEN: usize = 8;

/// The JPEG start-of-image marker.
const SOI: [u8; 2] = [0xFF, 0xD8];

/// Turns a live-view object into its JPEG, reusing the object's allocation.
///
/// # Errors
///
/// [`LiveViewError`] when the header is missing, inconsistent with the object, or the payload is
/// not a JPEG. Never panics.
pub fn extract_jpeg(mut object: Vec<u8>) -> Result<Vec<u8>, LiveViewError> {
    let len = object.len();
    let mut reader = Reader::new(&object);
    let (Ok(offset), Ok(size)) = (reader.u32(), reader.u32()) else {
        return Err(LiveViewError::Truncated(len));
    };
    let out_of_bounds = LiveViewError::OutOfBounds { offset, size, len };
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    let length = usize::try_from(size).unwrap_or(usize::MAX);
    if start < HEADER_LEN {
        return Err(LiveViewError::OffsetInsideHeader(offset));
    }
    let end = start.saturating_add(length);
    let jpeg = object.get(start..end).ok_or(out_of_bounds)?;
    if !jpeg.starts_with(&SOI) {
        return Err(LiveViewError::NotJpeg);
    }
    object.truncate(end);
    object.drain(..start);
    Ok(object)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn object(offset: u32, size: u32, body: &[u8]) -> Vec<u8> {
        let mut out = offset.to_le_bytes().to_vec();
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn strips_a_minimal_header() {
        let jpeg = [0xFF, 0xD8, 1, 2, 3, 0xFF, 0xD9];
        let bytes = object(8, 7, &jpeg);
        assert_eq!(extract_jpeg(bytes).unwrap(), jpeg);
    }

    #[test]
    fn skips_a_longer_header_and_trailing_data() {
        let jpeg = [0xFF, 0xD8, 9, 0xFF, 0xD9];
        let mut body = vec![0xEE; 8];
        body.extend_from_slice(&jpeg);
        body.extend_from_slice(&[0xAA; 6]);
        assert_eq!(extract_jpeg(object(16, 5, &body)).unwrap(), jpeg);
    }

    #[test]
    fn rejects_malformed_objects() {
        assert_eq!(
            extract_jpeg(vec![1, 2, 3]),
            Err(LiveViewError::Truncated(3))
        );
        assert_eq!(
            extract_jpeg(object(4, 2, &[0xFF, 0xD8])),
            Err(LiveViewError::OffsetInsideHeader(4))
        );
        assert_eq!(
            extract_jpeg(object(8, 9, &[0xFF, 0xD8])),
            Err(LiveViewError::OutOfBounds {
                offset: 8,
                size: 9,
                len: 10
            })
        );
        assert_eq!(
            extract_jpeg(object(u32::MAX, u32::MAX, &[0xFF, 0xD8])),
            Err(LiveViewError::OutOfBounds {
                offset: u32::MAX,
                size: u32::MAX,
                len: 10
            })
        );
        assert_eq!(
            extract_jpeg(object(8, 2, &[0x00, 0x01])),
            Err(LiveViewError::NotJpeg)
        );
        assert_eq!(extract_jpeg(object(8, 0, &[])), Err(LiveViewError::NotJpeg));
    }

    proptest! {
        #[test]
        fn never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
            let _ = extract_jpeg(bytes);
        }
    }
}
