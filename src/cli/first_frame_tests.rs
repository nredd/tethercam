//! `doctor` first-frame diagnostics: every failure is synthetic, nothing needs a camera.

use std::time::Duration;

use super::FirstFrameCause as Cause;
use crate::cli::{Error, diagnostics};
use crate::ptp::ContainerError;
use crate::ptp::session::SessionError;
use crate::sony::{LiveViewError, SonyConfig, SonyError};
use crate::supervisor::testing::{Factory, jpeg};
use crate::transport::sim::Fault;
use crate::transport::sim::sony::{LiveViewMode, SonySimConfig, Version300, sony_camera};
use crate::transport::{Endpoint, TransportError};

/// Short deadlines so an absent frame fails in milliseconds.
fn fast() -> SonyConfig {
    SonyConfig {
        poll_interval: Duration::from_millis(1),
        event_poll_timeout: Duration::from_millis(1),
        live_view_ready_timeout: Duration::from_millis(40),
        live_view_timeout: Duration::from_millis(40),
        ..SonyConfig::default()
    }
}

fn body() -> SonySimConfig {
    SonySimConfig {
        warmup_reads: 0,
        live_view_jpeg: jpeg(32, 32),
        ..SonySimConfig::default()
    }
}

async fn doctor(camera: crate::transport::sim::SimCamera) -> (Result<(), Error>, String) {
    let mut out = Vec::new();
    let result =
        diagnostics::doctor_camera_with(Factory::new(vec![camera]), fast(), &mut out).await;
    (result, String::from_utf8(out).unwrap())
}

/// The failure's message. Every cause has a distinct one, so equality names the variant too.
async fn message(camera: crate::transport::sim::SimCamera) -> String {
    doctor(camera).await.0.unwrap_err().to_string()
}

#[tokio::test]
async fn a_working_camera_gets_a_frame_and_reports() {
    let (result, out) = doctor(sony_camera(body())).await;
    result.unwrap();
    assert!(out.contains("Camera: PC Remote"), "{out}");
}

#[tokio::test]
async fn synthetic_bodies_map_to_their_likely_cause() {
    let cases = [
        (
            SonySimConfig {
                advertise_sdio: false,
                ..body()
            },
            Cause::NotRemote,
        ),
        (
            SonySimConfig {
                max_version: 300,
                version_300: Version300::RefusesAll(0x201D),
                ..body()
            },
            Cause::Refused,
        ),
        (
            SonySimConfig {
                live_view: LiveViewMode::Unsupported,
                ..body()
            },
            Cause::LiveViewUnavailable,
        ),
        (
            SonySimConfig {
                warmup_reads: u32::MAX,
                ..body()
            },
            Cause::NotReady,
        ),
        (
            SonySimConfig {
                live_view_denied: u32::MAX,
                ..body()
            },
            Cause::NoFrame,
        ),
        (
            // Valid container, but not a JPEG the decoder accepts.
            SonySimConfig {
                live_view_jpeg: SonySimConfig::default().live_view_jpeg,
                ..body()
            },
            Cause::Decode,
        ),
        (
            SonySimConfig {
                defect: crate::transport::sim::sony::Defect::BadExtInfo,
                ..body()
            },
            Cause::Protocol,
        ),
    ];
    for (config, expected) in cases {
        assert_eq!(
            message(sony_camera(config)).await,
            expected.to_string(),
            "{expected:?}"
        );
    }
}

#[tokio::test]
async fn injected_transport_faults_name_timeout_and_disconnect() {
    let mut camera = sony_camera(body());
    camera.inject(Endpoint::BulkOut, 0, Fault::Disconnect);
    assert_eq!(message(camera).await, Cause::Disconnected.to_string());
    let mut camera = sony_camera(body());
    camera.inject(Endpoint::BulkIn, 3, Fault::Timeout);
    assert_eq!(message(camera).await, Cause::Timeout.to_string());
}

#[test]
fn every_cause_is_actionable_and_free_of_identifiers() {
    let all = [
        Cause::NotRemote,
        Cause::Refused,
        Cause::LiveViewUnavailable,
        Cause::NotReady,
        Cause::NoFrame,
        Cause::Timeout,
        Cause::Disconnected,
        Cause::Transport,
        Cause::Busy,
        Cause::Permission,
        Cause::Rejected,
        Cause::Protocol,
        Cause::Decode,
        Cause::Unclassified,
    ];
    let mut seen = std::collections::HashSet::new();
    for cause in all {
        let text = cause.to_string();
        assert!(text.starts_with("no first frame: "), "{text}");
        assert!(text.is_ascii() && !text.contains('{'), "{text}");
        assert!(seen.insert(text), "duplicate message for {cause:?}");
    }
}

#[test]
fn classification_covers_every_driver_error_family() {
    let session = |error: SessionError| SonyError::Session(error);
    let transport = |error: TransportError| session(SessionError::Transport(error));
    let cases = [
        (SonyError::UnsupportedProtocol(150), Cause::Refused),
        (
            SonyError::ExtInfoUnavailable { attempts: 3 },
            Cause::Refused,
        ),
        (SonyError::LiveView(LiveViewError::NotJpeg), Cause::Decode),
        (
            SonyError::Codec(crate::ptp::CodecError::Truncated {
                needed: 4,
                available: 0,
            }),
            Cause::Protocol,
        ),
        (transport(TransportError::Timeout), Cause::Timeout),
        (transport(TransportError::Disconnected), Cause::Disconnected),
        (transport(TransportError::Stall), Cause::Transport),
        (
            transport(TransportError::Other("synthetic".into())),
            Cause::Transport,
        ),
        (
            session(SessionError::Unrecoverable(TransportError::Stall)),
            Cause::Transport,
        ),
        (
            session(SessionError::DeadlineExceeded(Duration::from_secs(1))),
            Cause::Timeout,
        ),
        (session(SessionError::TooManyEmptyReads), Cause::Timeout),
        (
            session(SessionError::Container(ContainerError::TruncatedHeader(0))),
            Cause::Protocol,
        ),
        (session(SessionError::UnsupportedLength), Cause::Protocol),
        (
            session(SessionError::TransactionMismatch {
                expected: 1,
                actual: 2,
            }),
            Cause::Protocol,
        ),
        (
            session(SessionError::DataTooLarge {
                announced: 2,
                limit: 1,
            }),
            Cause::Protocol,
        ),
        (
            session(SessionError::Response {
                code: 0x2002,
                params: Vec::new(),
            }),
            Cause::Rejected,
        ),
        (session(SessionError::NotOpen), Cause::Unclassified),
        (
            SonyError::UnknownProperty(crate::ptp::DevicePropCode(0xD21C)),
            Cause::Unclassified,
        ),
    ];
    for (error, expected) in cases {
        assert_eq!(Cause::of(&error, false), expected, "{error:?}");
    }
    assert_eq!(
        Cause::of(&SonyError::LiveViewNotReady, true),
        Cause::NoFrame
    );
}

#[cfg(feature = "macos-icc")]
#[test]
fn icc_categories_map_to_their_likely_cause() {
    use crate::transport::icc::IccError;
    for (error, expected) in [
        (IccError::Permission, Cause::Permission),
        (IccError::Busy, Cause::Busy),
        (IccError::AdmissionExpired, Cause::Busy),
        (IccError::DeadlineExceeded, Cause::Timeout),
        (IccError::Removed, Cause::Disconnected),
        (IccError::Disconnected, Cause::Disconnected),
        (IccError::Runtime, Cause::Unclassified),
    ] {
        let error = SonyError::Session(SessionError::Icc(error));
        assert_eq!(Cause::of(&error, true), expected, "{error:?}");
    }
}
