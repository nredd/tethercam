//! Classify the native boundary's tagged advisory events.
//!
//! The boundary tags every event with a first byte. Tag 11 carries a raw PTP event container
//! (at most 32 bytes after the tag). The other tags are uncorrelated delegate observations.
//! Only PTP events are property or shutter signals, so everything that could mean a lost or
//! untrustworthy signal fails closed instead of being skipped:
//! - removal, access restriction, a native error observation, an oversized event discarded by
//!   the boundary and any malformed, truncated, oversized or unknown event are errors
//! - benign observations (browser changes, open and close notifications, capability and catalog
//!   changes, restriction lifted) are advisory and skipped by the caller
//!
//! The classifier is total over arbitrary bytes.
//!
//! References:
//! - <https://github.com/nredd/image-capture/blob/main/README.md>, Limits and cancellation
//! - USB Still Image Capture Device Definition 1.0, section 7.1 (event container)

use super::IccError;
use crate::ptp::session::Event;
use crate::ptp::{Container, ContainerKind, MAX_PARAMS};

const TAG_SELECTED_REMOVED: u8 = 5;
const TAG_NATIVE_ERROR: u8 = 6;
const TAG_ACCESS_ENABLED: u8 = 10;
const TAG_PTP: u8 = 11;
const TAG_OVERSIZED: u8 = 12;
/// Benign observations: browser add/remove, delegate open/close, capability change, catalog
/// ready, access restriction removed.
const ADVISORY: [u8; 7] = [1, 2, 3, 4, 7, 8, 9];

/// What one native event means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// A PTP event from the camera.
    Ptp(Event),
    /// A benign observation with no signal for the caller.
    Advisory,
}

/// Classify one tagged event.
///
/// # Errors
///
/// [`IccError::Removed`], [`IccError::Permission`], [`IccError::NativeFailure`],
/// [`IccError::EventOverflow`] or [`IccError::MalformedEvent`] as described in the module docs.
pub fn classify(bytes: &[u8]) -> Result<Notice, IccError> {
    let Some((tag, payload)) = bytes.split_first() else {
        return Err(IccError::MalformedEvent);
    };
    match *tag {
        TAG_SELECTED_REMOVED => Err(IccError::Removed),
        TAG_NATIVE_ERROR => Err(IccError::NativeFailure),
        TAG_ACCESS_ENABLED => Err(IccError::Permission),
        TAG_OVERSIZED => Err(IccError::EventOverflow),
        TAG_PTP => ptp_event(payload).map(Notice::Ptp),
        tag if ADVISORY.contains(&tag) => Ok(Notice::Advisory),
        _ => Err(IccError::MalformedEvent),
    }
}

/// A complete event container: exact length, event kind, whole parameters, at most five.
fn ptp_event(bytes: &[u8]) -> Result<Event, IccError> {
    let (container, rest) = Container::decode(bytes).map_err(|_| IccError::MalformedEvent)?;
    if !rest.is_empty()
        || container.kind != ContainerKind::Event
        || container.payload.len() % 4 != 0
        || container.payload.len() / 4 > MAX_PARAMS
    {
        return Err(IccError::MalformedEvent);
    }
    Ok(Event {
        code: container.code,
        transaction_id: container.transaction_id,
        params: container.params().collect(),
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn container(kind: u16, code: u16, txid: u32, params: &[u32]) -> Vec<u8> {
        let length = u32::try_from(12 + params.len() * 4).expect("small");
        let mut out = length.to_le_bytes().to_vec();
        out.extend(kind.to_le_bytes());
        out.extend(code.to_le_bytes());
        out.extend(txid.to_le_bytes());
        for param in params {
            out.extend(param.to_le_bytes());
        }
        out
    }

    fn tagged(tag: u8, rest: &[u8]) -> Vec<u8> {
        [&[tag], rest].concat()
    }

    #[test]
    fn well_formed_ptp_events_decode() {
        let wire = tagged(
            TAG_PTP,
            &container(4, 0xC201, 0xFFFF_FFFF, &[1, 2, 3, 4, 5]),
        );
        assert_eq!(
            classify(&wire),
            Ok(Notice::Ptp(Event {
                code: 0xC201,
                transaction_id: 0xFFFF_FFFF,
                params: vec![1, 2, 3, 4, 5],
            }))
        );
        let bare = tagged(TAG_PTP, &container(4, 0x4002, 0, &[]));
        assert!(matches!(classify(&bare), Ok(Notice::Ptp(e)) if e.params.is_empty()));
    }

    #[test]
    fn malformed_ptp_events_fail_closed() {
        let good = container(4, 0xC201, 0, &[7]);
        let mut trailing = good.clone();
        trailing.push(0);
        let mut short = good.clone();
        short.pop();
        let mut misaligned = container(4, 0xC201, 0, &[7]);
        misaligned.push(0);
        misaligned[0] = 17;
        let cases = [
            container(3, 0x2001, 0, &[]),
            container(4, 0xC201, 0, &[1, 2, 3, 4, 5, 6]),
            trailing,
            short,
            misaligned,
            good[..8].to_vec(),
            Vec::new(),
        ];
        for case in cases {
            assert_eq!(
                classify(&tagged(TAG_PTP, &case)),
                Err(IccError::MalformedEvent),
                "{case:?}"
            );
        }
    }

    #[test]
    fn every_tag_has_exactly_one_disposition() {
        assert_eq!(classify(&[]), Err(IccError::MalformedEvent));
        for tag in 0..=u8::MAX {
            let result = classify(&[tag]);
            match tag {
                1..=4 | 7..=9 => assert_eq!(result, Ok(Notice::Advisory), "{tag}"),
                5 => assert_eq!(result, Err(IccError::Removed)),
                6 => assert_eq!(result, Err(IccError::NativeFailure)),
                10 => assert_eq!(result, Err(IccError::Permission)),
                11 => assert_eq!(result, Err(IccError::MalformedEvent)),
                12 => assert_eq!(result, Err(IccError::EventOverflow)),
                _ => assert_eq!(result, Err(IccError::MalformedEvent), "{tag}"),
            }
        }
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
            let _ = classify(&bytes);
            let _ = classify(&tagged(TAG_PTP, &bytes));
        }

        #[test]
        fn encoded_events_round_trip(
            code in any::<u16>(), txid in any::<u32>(),
            params in proptest::collection::vec(any::<u32>(), 0..=5),
        ) {
            let wire = tagged(TAG_PTP, &container(4, code, txid, &params));
            prop_assert_eq!(
                classify(&wire),
                Ok(Notice::Ptp(Event { code, transaction_id: txid, params }))
            );
        }

        #[test]
        fn accepted_events_never_exceed_the_bounds(bytes in proptest::collection::vec(any::<u8>(), 0..40)) {
            if let Ok(Notice::Ptp(event)) = classify(&tagged(TAG_PTP, &bytes)) {
                prop_assert!(event.params.len() <= MAX_PARAMS);
            }
        }
    }
}
