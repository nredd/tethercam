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

fn streams(h264: bool, mjpeg: bool) -> crate::config::Streams {
    crate::config::Streams {
        frame_rate: 15,
        bitrate: 100_000,
        h264,
        mjpeg,
        full_resolution_snapshot: false,
    }
}

async fn doctor(camera: crate::transport::sim::SimCamera) -> (Result<(), Error>, String) {
    let mut out = Vec::new();
    let result = diagnostics::doctor_camera(
        Factory::new(vec![camera]),
        fast(),
        &streams(true, true),
        &mut out,
    )
    .await;
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
                live_view_jpeg: vec![0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x04, 0x01, 0x02, 0xFF, 0xD9],
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
async fn doctor_rejects_the_frames_run_would_reject() {
    // Decodes, but 33x33 is not carriable as MJPEG and not an even H.264 size.
    let odd = || {
        sony_camera(SonySimConfig {
            live_view_jpeg: jpeg(33, 33),
            ..body()
        })
    };
    let check = async |h264, mjpeg| {
        let mut out = Vec::new();
        diagnostics::doctor_camera(
            Factory::new(vec![odd()]),
            fast(),
            &streams(h264, mjpeg),
            &mut out,
        )
        .await
        .map_err(|error| error.to_string())
    };
    assert_eq!(check(true, true).await, Err(Cause::Decode.to_string()));
    assert_eq!(check(false, true).await, Err(Cause::Decode.to_string()));
    assert_eq!(check(true, false).await, Err(Cause::Encoder.to_string()));
    assert!(check(false, false).await.is_ok());
}

/// How many bulk-out transfers a clean `doctor` makes; the last one is `CloseSession`.
async fn bulk_outs_in_a_clean_run() -> usize {
    let factory = Factory::new(vec![sony_camera(body())]);
    let released = factory.released.clone();
    let mut out = Vec::new();
    diagnostics::doctor_camera(factory, fast(), &streams(true, true), &mut out)
        .await
        .unwrap();
    let cameras = released.lock().unwrap();
    cameras[0].bulk_out_transfers().len()
}

#[tokio::test]
async fn a_close_failure_after_a_good_frame_is_not_blamed_on_the_frame() {
    let commands = bulk_outs_in_a_clean_run().await;
    let mut camera = sony_camera(body());
    // Everything succeeds up to the final command, `CloseSession`.
    camera.inject(Endpoint::BulkOut, commands - 1, Fault::Disconnect);
    let (result, out) = doctor(camera).await;
    assert_eq!(
        result.unwrap_err().to_string(),
        Cause::CloseFailed.to_string()
    );
    assert!(!out.contains("Camera: PC Remote"), "never ready: {out}");
    // A frame run would reject still wins over the close failure.
    let mut camera = sony_camera(SonySimConfig {
        live_view_jpeg: jpeg(33, 33),
        ..body()
    });
    camera.inject(Endpoint::BulkOut, commands - 1, Fault::Disconnect);
    assert_eq!(message(camera).await, Cause::Decode.to_string());
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

#[tokio::test]
async fn probe_reports_post_open_failures_without_frame_claims() {
    let factory = Factory::new(vec![sony_camera(body())]);
    let released = factory.released.clone();
    diagnostics::probe(factory, &mut Vec::new()).await.unwrap();
    let commands = released.lock().unwrap()[0].bulk_out_transfers().len();
    let mut disconnected = sony_camera(body());
    disconnected.inject(Endpoint::BulkOut, 0, Fault::Disconnect);
    let mut timeout = sony_camera(body());
    timeout.inject(Endpoint::BulkIn, 3, Fault::Timeout);
    let mut close = sony_camera(body());
    close.inject(Endpoint::BulkOut, commands - 1, Fault::Disconnect);
    for (camera, cause) in [
        (disconnected, Cause::Disconnected),
        (timeout, Cause::Timeout),
        (close, Cause::Disconnected),
        (
            sony_camera(SonySimConfig {
                defect: crate::transport::sim::sony::Defect::BadExtInfo,
                ..body()
            }),
            Cause::Protocol,
        ),
    ] {
        let error = diagnostics::probe(Factory::new(vec![camera]), &mut Vec::new())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Probe(actual) if actual == cause));
        assert!(!error.to_string().contains("no first frame"));
    }
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
        Cause::CleanupFailed,
        Cause::Quarantined,
        Cause::Transport,
        Cause::Busy,
        Cause::Permission,
        Cause::Rejected,
        Cause::Protocol,
        Cause::Decode,
        Cause::Unclassified,
        Cause::Encoder,
        Cause::Framing,
        Cause::CloseFailed,
    ];
    let mut seen = std::collections::HashSet::new();
    for cause in all {
        let text = cause.to_string();
        let prefix = if cause == Cause::CloseFailed {
            "frame received"
        } else {
            "no first frame: "
        };
        assert!(text.starts_with(prefix), "{text}");
        assert!(text.is_ascii() && !text.contains('{'), "{text}");
        assert!(seen.insert(text), "duplicate message for {cause:?}");
    }
}

#[test]
fn classification_covers_every_driver_error_family() {
    let session = |error: SessionError| SonyError::Session(error);
    let transport = |error: TransportError| session(SessionError::Transport(error));
    let cases = [
        (SonyError::UnsupportedProtocol(150), Cause::Protocol),
        (SonyError::ExtInfoUnavailable { attempts: 3 }, Cause::Busy),
        (
            // A handshake that stalled or timed out is not a refusal.
            SonyError::ProtocolRefused {
                requested: crate::sony::Protocol::V300,
                requested_error: Box::new(SonyError::SdioUnsupported),
                fallback: crate::sony::Protocol::V200,
                fallback_error: Box::new(transport(TransportError::Timeout)),
            },
            Cause::Timeout,
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
            Cause::Framing,
        ),
        (session(SessionError::UnsupportedLength), Cause::Framing),
        (
            session(SessionError::TransactionMismatch {
                expected: 1,
                actual: 2,
            }),
            Cause::Framing,
        ),
        (
            session(SessionError::DataTooLarge {
                announced: 2,
                limit: 1,
            }),
            Cause::Framing,
        ),
        (
            session(SessionError::Response {
                code: 0x2002,
                params: Vec::new(),
            }),
            Cause::Rejected,
        ),
        (
            session(SessionError::Response {
                code: crate::ptp::session::code::DEVICE_BUSY,
                params: Vec::new(),
            }),
            Cause::Busy,
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

#[test]
fn pipeline_failures_are_classified_by_stage() {
    use crate::supervisor::PipelineError;
    assert_eq!(
        Cause::of_pipeline(&PipelineError::Jpeg(crate::media::JpegError::Malformed(
            String::new()
        ))),
        Cause::Decode
    );
    assert_eq!(
        Cause::of_pipeline(&PipelineError::Encode(
            crate::media::EncodeError::InvalidDimensions {
                width: 1,
                height: 1
            }
        )),
        Cause::Encoder
    );
    assert_eq!(
        Cause::of_pipeline(&PipelineError::Worker),
        Cause::Unclassified
    );
}

#[cfg(feature = "macos-icc")]
#[test]
fn icc_categories_map_to_their_likely_cause() {
    use crate::transport::icc::IccError;
    let cause = Cause::Native(IccError::MalformedCompletion);
    assert!(
        Error::FirstFrame(cause)
            .to_string()
            .contains("malformed native completion")
    );
    let probe = Error::Probe(cause).to_string();
    assert!(probe.contains("malformed native completion"));
    assert!(!probe.contains("no first frame"));
    for (error, expected) in [
        (IccError::Permission, Cause::Permission),
        (IccError::Busy, Cause::Busy),
        (IccError::AdmissionExpired, Cause::Busy),
        (IccError::DeadlineExceeded, Cause::Timeout),
        (IccError::Removed, Cause::Disconnected),
        (IccError::Disconnected, Cause::Disconnected),
        (IccError::CleanupFailure, Cause::CleanupFailed),
        (IccError::Quarantined, Cause::Quarantined),
        (
            IccError::MalformedCompletion,
            Cause::Native(IccError::MalformedCompletion),
        ),
        (
            IccError::NativeFailure,
            Cause::Native(IccError::NativeFailure),
        ),
        (IccError::Runtime, Cause::Native(IccError::Runtime)),
    ] {
        let error = SonyError::Session(SessionError::Icc(error));
        assert_eq!(Cause::of(&error, true), expected, "{error:?}");
    }
}
