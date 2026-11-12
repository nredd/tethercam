//! Per-model exceptions to the driver's capability-based behavior.
//!
//! Nothing in the driver branches on a model name: behavior follows what the camera advertises.
//! The few differences that cannot be detected live here, keyed by the model string from
//! `GetDeviceInfo`, and default to "no exception".
//!
//! References:
//! - libgphoto2 `camlibs/ptp2/library.c`, `ptp.c` (Sony sections)
//! - `watsonmw/alphawire` `src/aw/aw-control.c`
//! - `docs/architecture.md`

use std::time::Duration;

use super::protocol::Protocol;

/// Exceptions for one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Quirks {
    /// Extra time to keep retrying the live-view fetch after the camera reports live view
    /// ready. The first frames of some bodies still fail for about a second.
    pub extra_live_view_wait: Duration,
    /// The protocol generation to request first, overriding
    /// [`SonyConfig::protocol`](super::SonyConfig::protocol), for a model known to mishandle a newer request.
    pub start_protocol: Option<Protocol>,
}

/// Known exceptions: `(model, quirks)`. Models compare case-insensitively.
const REGISTRY: &[(&str, Quirks)] = &[
    (
        // libgphoto2 `library.c`: "This state can persist for up to 1 second on the ZV-1".
        "ZV-1",
        Quirks {
            extra_live_view_wait: Duration::from_secs(1),
            start_protocol: None,
        },
    ),
    (
        // Measured: a 3.00 connect against this body, from another host library, ended in a
        // stalled bulk pipe instead of an error response. It is a 2.00 body, so start there.
        "ILCE-7M3",
        Quirks {
            extra_live_view_wait: Duration::ZERO,
            start_protocol: Some(Protocol::V200),
        },
    ),
];

impl Quirks {
    /// The exceptions for `model`, or none when the model is not listed.
    #[must_use]
    pub fn for_model(model: &str) -> Self {
        let model = model.trim();
        REGISTRY
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(model))
            .map(|(_, quirks)| *quirks)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_exceptions_and_defaults_to_none() {
        let zv1 = Quirks::for_model("ZV-1");
        assert_eq!(zv1.extra_live_view_wait, Duration::from_secs(1));
        assert_eq!(zv1.start_protocol, None);
        assert_eq!(Quirks::for_model("  zv-1 "), zv1);
        assert_eq!(
            Quirks::for_model("ILCE-7M3").start_protocol,
            Some(Protocol::V200)
        );
        assert_eq!(Quirks::for_model("ILCE-9999"), Quirks::default());
        assert_eq!(Quirks::for_model(""), Quirks::default());
    }
}
