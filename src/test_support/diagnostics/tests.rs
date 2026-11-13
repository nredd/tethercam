//! Synthetic success/failure measurements through the real Sony driver.
//!
//! References:
//! - <https://www.iso.org/standard/63602.html>
use super::*;
use crate::ptp::session::SessionConfig;
use crate::transport::sim::sony::{Defect, Shutter, SonySimConfig, sony_camera};

#[tokio::test(start_paused = true)]
async fn all_operations_close_before_output_and_limits_precede_camera_work() {
    let mut session = Session::new(
        sony_camera(SonySimConfig {
            resolution_controls: crate::transport::sim::sony::ResolutionControls::Present,
            ..SonySimConfig::default()
        }),
        SessionConfig::default(),
    );
    for operation in [
        Operation::Properties,
        Operation::Source { frames: 2 },
        Operation::Configure {
            size: 1,
            single_drive: true,
            jpeg_quality: true,
        },
        Operation::Configure {
            size: 2,
            single_drive: false,
            jpeg_quality: false,
        },
        Operation::Capture { count: 2 },
    ] {
        let mut output = Vec::new();
        let artifacts = run(&mut session, SonyConfig::default(), operation, &mut output)
            .await
            .expect("diagnostic");
        assert!(!session.is_open());
        if matches!(operation, Operation::Capture { .. }) {
            assert_eq!(artifacts.len(), 3);
            assert_eq!(artifacts[0].name, "live.jpg");
            assert_eq!(artifacts[1].name, "still-1.bin");
            assert_eq!(artifacts[2].name, "still-2.bin");
            for artifact in &artifacts {
                assert!(artifact.bytes.starts_with(&[255, 216]));
            }
        }
    }
    for operation in [
        Operation::Source { frames: 0 },
        Operation::Source { frames: 4097 },
        Operation::Capture { count: 0 },
        Operation::Capture { count: 3 },
        Operation::Configure {
            size: 0,
            single_drive: false,
            jpeg_quality: false,
        },
        Operation::Configure {
            size: 4,
            single_drive: false,
            jpeg_quality: false,
        },
    ] {
        let error = run(
            &mut session,
            SonyConfig::default(),
            operation,
            &mut Vec::new(),
        )
        .await
        .err()
        .expect("invalid limits");
        assert_eq!(error.to_string(), "invalid diagnostic limits");
    }
    assert!(
        run(
            &mut session,
            SonyConfig::default(),
            Operation::Properties,
            &mut Broken
        )
        .await
        .is_err()
    );
    assert!(!session.is_open());
}

struct Broken;
impl Write for Broken {
    fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("injected"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn capture_failures_release_shutters_and_close_sessions() {
    for cfg in [
        SonySimConfig::default(),
        SonySimConfig {
            shutter: Shutter::NoControls,
            ..SonySimConfig::default()
        },
        SonySimConfig {
            defect: Defect::BadObjectInfo,
            ..SonySimConfig::default()
        },
    ] {
        let mut session = Session::new(sony_camera(cfg), SessionConfig::default());
        let _ = run(
            &mut session,
            SonyConfig::default(),
            Operation::Capture { count: 1 },
            &mut Vec::new(),
        )
        .await;
        assert!(!session.is_open());
    }
    assert!(Broken.flush().is_ok());
}

#[tokio::test(start_paused = true)]
async fn every_transaction_boundary_preserves_cleanup_on_disconnect_and_timeout() {
    use crate::transport::{Endpoint, sim::Fault};
    for operation in [
        Operation::Properties,
        Operation::Source { frames: 2 },
        Operation::Configure {
            size: 1,
            single_drive: true,
            jpeg_quality: true,
        },
        Operation::Capture { count: 2 },
    ] {
        for endpoint in [Endpoint::BulkOut, Endpoint::InterruptIn] {
            for fault in [Fault::Disconnect, Fault::Timeout] {
                for after in 0..100 {
                    let mut camera = sony_camera(SonySimConfig {
                        resolution_controls:
                            crate::transport::sim::sony::ResolutionControls::Present,
                        ..SonySimConfig::default()
                    });
                    camera.inject(endpoint, after, fault);
                    let mut session = Session::new(camera, SessionConfig::default());
                    let _result = run(
                        &mut session,
                        SonyConfig::default(),
                        operation,
                        &mut Vec::new(),
                    )
                    .await;
                    assert!(!session.is_open());
                }
            }
        }
    }
}
