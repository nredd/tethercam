//! Explicit output, config persistence and sanitized diagnostics.
//!
//! References:
//! - `docs/architecture.md`, CLI and supervisor contract

use crate::transport::sim::sony::{LiveViewMode, SonySimConfig, sony_camera};

#[test]
fn path_setup_write_and_runtime_construction_failures_are_typed() {
    let mut out = Vec::new();
    assert!(
        execute_with_path(
            Options::parse(["probe".into()]),
            &mut out,
            Err(config::Error::ConfigDirectory)
        )
        .is_err()
    );
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(Command::Setup, dir.path());
    opts.no_discovery = false;
    assert!(execute(Ok(opts), &mut out).is_err());
    let path = dir.path().join("config.toml");
    let mut full: &mut [u8] = &mut [];
    assert!(execute(Ok(options(Command::Setup, &path)), &mut full).is_err());
    let cfg = Config::load(&path).unwrap();
    assert!(
        dispatch_runtime(
            &options(Command::Probe, &path),
            &cfg,
            &mut out,
            Err(std::io::Error::other("synthetic reactor failure")),
            crate::transport::usb::bus::fakes::FakeBus::with_camera(0x0c34),
        )
        .is_err()
    );
}

fn free_cfg() -> Config {
    let mut cfg = Config::generate().unwrap();
    cfg.rtsp_address = "127.0.0.1".parse().unwrap();
    cfg.onvif_address = cfg.rtsp_address;
    let a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    cfg.rtsp_port = a.local_addr().unwrap().port();
    cfg.onvif_port = b.local_addr().unwrap().port();
    cfg
}

struct FailFinalDiagnostic;
impl Write for FailFinalDiagnostic {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.starts_with(b"Clock,") {
            Err(std::io::Error::other("synthetic pipe failure"))
        } else {
            Ok(bytes.len())
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn injected_shutdown_drives_normal_commands_and_all_diagnostic_failures() {
    let cfg = free_cfg();
    let mut out = Vec::new();
    let mut opts = options(Command::Probe, std::path::Path::new("synthetic.toml"));
    assert!(
        super::command(
            Factory::new(Vec::new()),
            &cfg,
            &opts,
            &mut out,
            Err(std::io::Error::other("synthetic signal registration"))
        )
        .await
        .is_err()
    );
    super::command_until(
        Factory::new(vec![camera()]),
        &cfg,
        &opts,
        &mut out,
        Box::pin(std::future::ready(())),
    )
    .await
    .unwrap();
    opts.command = Command::Doctor;
    super::command_until(
        Factory::new(vec![camera()]),
        &cfg,
        &opts,
        &mut out,
        Box::pin(std::future::ready(())),
    )
    .await
    .unwrap();
    assert!(
        super::command_until(
            Factory::new(Vec::new()),
            &cfg,
            &opts,
            &mut out,
            Box::pin(std::future::ready(()))
        )
        .await
        .is_err()
    );
    assert!(
        super::command_until(
            Factory::new(vec![camera()]),
            &cfg,
            &opts,
            &mut FailFinalDiagnostic,
            Box::pin(std::future::ready(()))
        )
        .await
        .is_err()
    );
    FailFinalDiagnostic.flush().unwrap();
    let occupied = std::net::TcpListener::bind((cfg.rtsp_address, cfg.rtsp_port)).unwrap();
    assert!(
        super::command_until(
            Factory::new(Vec::new()),
            &cfg,
            &opts,
            &mut out,
            Box::pin(std::future::ready(()))
        )
        .await
        .is_err()
    );
    drop(occupied);
}

#[tokio::test]
async fn injected_run_shutdown_startup_failure_and_discovery_diagnostics() {
    let mut cfg = free_cfg();
    let mut out = Vec::new();
    let mut opts = options(Command::Run, std::path::Path::new("synthetic.toml"));
    let occupied = std::net::TcpListener::bind((cfg.rtsp_address, cfg.rtsp_port)).unwrap();
    assert!(
        super::command_until(
            Factory::new(Vec::new()),
            &cfg,
            &opts,
            &mut out,
            Box::pin(std::future::ready(()))
        )
        .await
        .is_err()
    );
    drop(occupied);
    super::command_until(
        Factory::new(vec![camera()]),
        &cfg,
        &opts,
        &mut out,
        Box::pin(std::future::ready(())),
    )
    .await
    .unwrap();
    assert!(
        super::command_until(
            Factory::new(vec![sony_camera(SonySimConfig::default())]),
            &cfg,
            &opts,
            &mut out,
            Box::pin(std::future::pending())
        )
        .await
        .is_err()
    );
    opts.no_discovery = false;
    // A host whose interfaces cannot be listed fails the run before any listener is served.
    assert!(
        super::command_with(
            Factory::new(Vec::new()),
            &cfg,
            &opts,
            &mut out,
            Box::pin(std::future::ready(())),
            || Err(std::io::Error::other("synthetic interface enumeration")),
        )
        .await
        .is_err()
    );
    // An interface that is absent is waited for, not a startup failure.
    cfg.interface = Some("synthetic-absent-interface".into());
    super::command_until(
        Factory::new(Vec::new()),
        &cfg,
        &opts,
        &mut out,
        Box::pin(std::future::ready(())),
    )
    .await
    .unwrap();
    assert!(
        diagnostics::probe(
            Factory::new(vec![sony_camera(SonySimConfig {
                live_view: LiveViewMode::Unsupported,
                ..SonySimConfig::default()
            })]),
            &mut out
        )
        .await
        .is_err()
    );
}

use super::*;
use crate::supervisor::OpenError;
use crate::supervisor::testing::{Factory, camera, jpeg};
use std::mem::discriminant as kind;

fn options(command: Command, path: &std::path::Path) -> Options {
    Options {
        command,
        config: Some(path.to_owned()),
        simulate: None,
        no_discovery: true,
        report: false,
    }
}

#[test]
fn version_help_argument_and_output_failures() {
    let mut out = Vec::new();
    execute(Options::parse([]), &mut out).unwrap();
    assert_eq!(out, BANNER.as_bytes());
    out.clear();
    execute(Options::parse(["--help".into()]), &mut out).unwrap();
    assert_eq!(out, HELP.as_bytes());
    assert_eq!(finish(Ok(()), &mut out), ExitCode::SUCCESS);
    assert_eq!(
        finish(Err(ParseError::Invalid.into()), &mut out),
        ExitCode::FAILURE
    );
    let mut full: &mut [u8] = &mut [];
    assert!(execute(Options::parse([]), &mut full).is_err());
    assert!(execute(Options::parse(["--help".into()]), &mut full).is_err());
    assert_eq!(
        finish(Err(Error::Probe(FirstFrameCause::Timeout)), &mut full),
        ExitCode::FAILURE
    );
    assert!(execute(Err(ParseError::MissingPath), &mut out).is_err());
}

#[test]
fn setup_persists_identity_and_explicitly_presents_generated_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut options = options(Command::Setup, &path);
    options.no_discovery = false;
    let mut out = Vec::new();
    execute(Ok(options.clone()), &mut out).unwrap();
    let cfg = Config::load(&path).unwrap();
    let text = String::from_utf8(out.clone()).unwrap();
    assert!(text.contains(cfg.credentials.username.expose()));
    assert!(text.contains(cfg.credentials.password.expose()));
    out.clear();
    execute(Ok(options), &mut out).unwrap();
    assert_eq!(out, text.as_bytes());
    assert_eq!(
        Config::load(&path).unwrap().endpoint_uuid,
        cfg.endpoint_uuid
    );
    assert!(
        execute(
            Ok(super::tests::options(
                Command::Probe,
                &dir.path().join("missing")
            )),
            &mut out
        )
        .is_err()
    );
    std::fs::write(&path, "password = 'synthetic-do-not-leak'").unwrap();
    let error = execute(Ok(super::tests::options(Command::Probe, &path)), &mut out).unwrap_err();
    assert!(!error.to_string().contains("synthetic-do-not-leak"));
    let explicit = dir.path().join("ipv6.toml");
    let mut cfg = Config::generate().unwrap();
    cfg.onvif_address = "::1".parse().unwrap();
    cfg.create(&explicit).unwrap();
    out.clear();
    execute(
        Ok(super::tests::options(Command::Setup, &explicit)),
        &mut out,
    )
    .unwrap();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("http://[::1]:8080/onvif/device_service")
    );
}

#[test]
fn simulator_cli_probe_and_doctor_use_real_driver_transactions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut cfg = Config::generate().unwrap();
    cfg.rtsp_address = "127.0.0.1".parse().unwrap();
    cfg.onvif_address = cfg.rtsp_address;
    let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let second = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    cfg.rtsp_port = first.local_addr().unwrap().port();
    cfg.onvif_port = second.local_addr().unwrap().port();
    drop((first, second));
    cfg.create(&path).unwrap();
    let jpeg_path = dir.path().join("synthetic.jpg");
    std::fs::write(&jpeg_path, jpeg(32, 32)).unwrap();
    let mut options = options(Command::Probe, &path);
    options.simulate = Some(jpeg_path.clone());
    let mut out = Vec::new();
    execute(Ok(options.clone()), &mut out).unwrap();
    let text = String::from_utf8(out.clone()).unwrap();
    assert!(text.contains("PC Remote"));
    assert!(text.contains("Battery/power"));
    assert!(!text.contains(cfg.credentials.password.expose()));
    options.command = Command::Doctor;
    execute(Ok(options.clone()), &mut out).unwrap();
    std::fs::write(&jpeg_path, b"not a jpeg").unwrap();
    assert_eq!(
        kind(&execute(Ok(options.clone()), &mut out).unwrap_err()),
        kind(&Error::Simulator)
    );
    std::fs::remove_file(jpeg_path).unwrap();
    assert_eq!(
        kind(&execute(Ok(options), &mut out).unwrap_err()),
        kind(&Error::Io(std::io::Error::other("synthetic")))
    );
}

#[test]
fn service_dispatch_is_explicit_and_never_loads_runtime_config() {
    let temp = tempfile::tempdir().unwrap();
    let opts = || Options::parse(["service", "status", "--user"].map(std::ffi::OsString::from));
    let mut out = Vec::new();
    let mut factory = |user: bool| {
        assert!(user);
        Ok(crate::service::Manager::test_status(
            temp.path().into(),
            false,
        ))
    };
    super::execute_with_service(
        opts(),
        &mut out,
        Err(config::Error::ConfigDirectory),
        &mut factory,
    )
    .unwrap();
    assert_eq!(out, b"Service: absent\n");
    assert!(
        super::execute_with_service(
            opts(),
            &mut out,
            Err(config::Error::ConfigDirectory),
            &mut |_| Ok(crate::service::Manager::test_status(
                temp.path().into(),
                true
            ))
        )
        .is_err()
    );
    let mut full: &mut [u8] = &mut [];
    assert!(
        super::execute_with_service(
            opts(),
            &mut full,
            Err(config::Error::ConfigDirectory),
            &mut factory
        )
        .is_err()
    );
    let err = super::execute_with_service(
        opts(),
        &mut out,
        Err(config::Error::ConfigDirectory),
        &mut |_| Err(crate::service::Error::Manager),
    )
    .unwrap_err();
    assert_eq!(
        kind(&err),
        kind(&Error::Service(crate::service::Error::Manager))
    );
}

#[tokio::test]
async fn command_dispatch_and_sanitized_probe_failures() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unused.toml");
    let cfg = Config::generate().unwrap();
    let mut out = Vec::new();
    for command in [
        Command::Setup,
        Command::Version,
        Command::Help,
        Command::Service {
            action: crate::service::Action::Status,
            user: true,
        },
    ] {
        super::command(
            Factory::new(Vec::new()),
            &cfg,
            &options(command, &path),
            &mut out,
            diagnostics::Signals::install(),
        )
        .await
        .unwrap();
    }
    assert_eq!(
        kind(
            &diagnostics::probe(Factory::new(Vec::new()), &mut out)
                .await
                .unwrap_err()
        ),
        kind(&Error::NoCamera)
    );
    for failure in [
        OpenError::Permission,
        OpenError::Mode,
        OpenError::Busy,
        OpenError::Disconnected,
        OpenError::Discovery,
        OpenError::Ambiguous,
    ] {
        let mut factory = Factory::new(Vec::new());
        factory.steps.push_back(Err(failure));
        let error = diagnostics::probe(factory, &mut out).await.unwrap_err();
        assert_eq!(error.to_string(), failure.to_string());
    }
    assert_eq!(
        kind(
            &diagnostics::probe(
                Factory::new(vec![crate::transport::sim::SimCamera::new()]),
                &mut out
            )
            .await
            .unwrap_err()
        ),
        kind(&Error::Probe(FirstFrameCause::Timeout))
    );
    let mut full: &mut [u8] = &mut [];
    assert_eq!(
        kind(
            &diagnostics::probe(Factory::new(vec![camera()]), &mut full)
                .await
                .unwrap_err()
        ),
        kind(&Error::Io(std::io::Error::other("synthetic")))
    );
    let simulator = simulator::Simulator::new(jpeg(32, 32)).unwrap();
    simulator.pause(std::time::Duration::from_millis(1)).await;
}

#[cfg(feature = "macos-icc")]
#[test]
fn settled_icc_report_writes_original_error_and_flush_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("icc.toml");
    let mut cfg = free_cfg();
    cfg.camera_backend = config::CameraBackend::Icc;
    cfg.create(&path).unwrap();
    let mut opts = options(Command::Doctor, &path);
    opts.report = true;
    let mut out = Vec::new();
    let result = super::execute_using(
        Ok(opts),
        &mut out,
        Ok(path),
        &mut crate::service::Manager::new,
        &|_, _, _, out| {
            writeln!(out, "Error: no_camera")
                .and(Err(std::io::Error::other("synthetic flush")))
                .map_err(Error::Io)
        },
    );
    assert!(matches!(result, Err(Error::Report("io"))));
    assert!(out.ends_with(b"backend: icc\nError: no_camera\nError: io\n"));
    let mut full: &mut [u8] = &mut [];
    assert!(matches!(
        super::finish_report(&mut full, &Err(Error::NoCamera)),
        Err(Error::Report("io"))
    ));
    let mut invalid = options(Command::Setup, std::path::Path::new("synthetic.toml"));
    invalid.report = true;
    assert!(matches!(
        super::execute_using(
            Ok(invalid),
            &mut out,
            config::default_path(),
            &mut crate::service::Manager::new,
            &super::icc::launch
        ),
        Err(Error::Arguments(_))
    ));
}

#[test]
fn finish_does_not_duplicate_a_report_failure() {
    let dir = tempfile::tempdir().unwrap();
    let opts = options(Command::Doctor, &dir.path().join("missing.toml"));
    let mut out = Vec::new();
    let mut report = opts;
    report.report = true;
    let failed = super::execute_using(
        Ok(report),
        &mut out,
        config::default_path(),
        &mut crate::service::Manager::new,
        &super::icc::launch,
    );
    assert!(matches!(failed, Err(Error::Report("configuration"))));
    let path = dir.path().join("config.toml");
    free_cfg().create(&path).unwrap();
    let mut opts = options(Command::Doctor, &path);
    opts.report = true;
    let header_len = String::from_utf8_lossy(&out).find("Error:").unwrap();
    let mut header = vec![0; header_len];
    for mut sink in [&mut [][..], header.as_mut_slice()] {
        let result = super::execute_using(
            Ok(opts.clone()),
            &mut sink,
            config::default_path(),
            &mut crate::service::Manager::new,
            &super::icc::launch,
        );
        assert!(matches!(result, Err(Error::Report("io"))));
    }
    assert!(out.ends_with(b"Error: configuration\n"));
    out.clear();
    assert_eq!(
        finish(Err(Error::Report("configuration")), &mut out),
        ExitCode::FAILURE
    );
    assert_eq!(out, [] as [u8; 0]);
}

#[tokio::test]
async fn report_camera_failure_and_output_paths_are_bounded() {
    let cfg = free_cfg();
    let mut out = Vec::new();
    let mut opts = options(Command::Doctor, std::path::Path::new("synthetic.toml"));
    opts.report = true;
    let private_camera = sony_camera(SonySimConfig {
        model: "synthetic-private-model".into(),
        live_view_jpeg: jpeg(32, 32),
        ..SonySimConfig::default()
    });
    super::command_until(
        Factory::new(vec![private_camera]),
        &cfg,
        &opts,
        &mut out,
        Box::pin(std::future::ready(())),
    )
    .await
    .unwrap();
    assert!(!String::from_utf8_lossy(&out).contains("synthetic-private-model"));
    out.clear();
    diagnostics::doctor_report(Factory::new(vec![camera()]), &cfg.streams, &mut out)
        .await
        .unwrap();
    for length in 0..out.len() {
        let mut sink = vec![0; length];
        let mut writer = sink.as_mut_slice();
        assert!(
            diagnostics::doctor_report(Factory::new(vec![camera()]), &cfg.streams, &mut writer)
                .await
                .is_err()
        );
    }
    let bad = sony_camera(SonySimConfig {
        advertise_sdio: false,
        ..SonySimConfig::default()
    });
    assert!(
        diagnostics::doctor_report(Factory::new(vec![bad]), &cfg.streams, &mut out)
            .await
            .is_err()
    );
    out.clear();
    let invalid = sony_camera(SonySimConfig::default());
    assert!(
        diagnostics::doctor_report(Factory::new(vec![invalid]), &cfg.streams, &mut out)
            .await
            .is_err()
    );
    let malformed = String::from_utf8(std::mem::take(&mut out)).unwrap();
    assert!(malformed.contains("protocol: V"), "{malformed}");
    assert!(malformed.contains("power:"), "{malformed}");
    assert!(!malformed.contains("frame:"), "{malformed}");
    let odd = sony_camera(SonySimConfig {
        live_view_jpeg: jpeg(33, 33),
        ..SonySimConfig::default()
    });
    assert!(
        diagnostics::doctor_report(Factory::new(vec![odd]), &cfg.streams, &mut out)
            .await
            .is_err()
    );
    let partial = String::from_utf8(std::mem::take(&mut out)).unwrap();
    assert!(partial.contains("frame: 33 x 33\n"), "{partial}");
    assert!(partial.contains("capability_live_view: true"), "{partial}");
    assert!(
        diagnostics::doctor_report(Factory::new(Vec::new()), &cfg.streams, &mut out)
            .await
            .is_err()
    );
}

#[test]
fn report_power_is_coarse_and_never_displays_the_indicator() {
    use crate::sony::{Battery, PowerSource};
    assert_eq!(diagnostics::report_power(None), "unknown");
    let mut out = Vec::new();
    diagnostics::report_battery(&mut out, None).unwrap();
    assert_eq!(out, b"battery_percent: unknown\npower: unknown\n");
    let mut full: &mut [u8] = &mut [];
    assert!(diagnostics::report_battery(&mut full, None).is_err());
    for (source, expected) in [
        (PowerSource::Battery, "battery"),
        (PowerSource::Usb, "usb"),
        (PowerSource::NoBattery, "no_battery"),
        (PowerSource::Unknown, "unknown"),
    ] {
        assert_eq!(
            diagnostics::report_power(Some(Battery {
                percent: None,
                indicator: Some(255),
                source
            })),
            expected
        );
    }
}

#[test]
fn report_error_categories_do_not_interpolate_sources() {
    for (cause, expected) in [
        (FirstFrameCause::NotRemote, "not_remote"),
        (FirstFrameCause::Refused, "handshake_refused"),
        (
            FirstFrameCause::LiveViewUnavailable,
            "live_view_unavailable",
        ),
        (FirstFrameCause::NotReady, "live_view_not_ready"),
        (FirstFrameCause::NoFrame, "no_frame"),
        (FirstFrameCause::Timeout, "camera_timeout"),
        (FirstFrameCause::Disconnected, "camera_disconnected"),
        (FirstFrameCause::Transport, "transport"),
        (FirstFrameCause::Busy, "camera_busy"),
        (FirstFrameCause::Permission, "camera_permission"),
        (FirstFrameCause::Rejected, "camera_rejected"),
        (FirstFrameCause::Protocol, "protocol"),
        (FirstFrameCause::Decode, "frame_decode"),
        (FirstFrameCause::Encoder, "encoder"),
        (FirstFrameCause::CloseFailed, "camera_close"),
        (FirstFrameCause::Framing, "framing"),
        (FirstFrameCause::Unclassified, "camera_unclassified"),
    ] {
        assert_eq!(super::report_error(&Error::FirstFrame(cause)), expected);
    }
    #[cfg(feature = "macos-icc")]
    for (cause, expected) in [
        (FirstFrameCause::NativeDeadline, "camera_timeout"),
        (
            FirstFrameCause::Native(crate::transport::icc::IccError::NativeFailure),
            "backend",
        ),
        (FirstFrameCause::Quarantined, "backend"),
        (FirstFrameCause::CleanupFailed, "camera_close"),
    ] {
        assert_eq!(super::report_error(&Error::FirstFrame(cause)), expected);
    }
    for (error, category) in [
        (Error::Clock, "clock"),
        (Error::Ports, "ports"),
        (Error::NoCamera, "no_camera"),
        (Error::Open(OpenError::Busy), "camera_open"),
        (Error::FirstFrame(FirstFrameCause::Decode), "frame_decode"),
        (Error::IccUnavailable, "backend"),
        (Error::IccFailed, "backend"),
        (Error::Quarantined, "backend"),
        (Error::Io(std::io::Error::other("synthetic-private")), "io"),
        (Error::Simulator, "other"),
    ] {
        assert_eq!(super::report_error(&error), category);
    }
}

#[test]
fn simulation_bypasses_the_icc_backend_and_never_acquires_native_cameras() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut cfg = free_cfg();
    cfg.camera_backend = config::CameraBackend::Icc;
    cfg.create(&path).unwrap();
    let jpeg_path = dir.path().join("synthetic.jpg");
    std::fs::write(&jpeg_path, jpeg(32, 32)).unwrap();
    let mut options = options(Command::Probe, &path);
    options.simulate = Some(jpeg_path);
    let mut out = Vec::new();
    // Runs on every platform, including where `icc` is unavailable: nothing native is touched.
    execute(Ok(options), &mut out).unwrap();
    assert!(String::from_utf8(out).unwrap().contains("PC Remote"));
}

#[test]
fn icc_is_rejected_before_any_camera_access_where_it_cannot_run() {
    // Off macOS the native entry point is a stub. On macOS the real one needs the main thread and
    // refuses on a test thread before it touches anything native. The guard keeps that true.
    assert_ne!(std::thread::current().name(), Some("main"));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut cfg = free_cfg();
    cfg.camera_backend = config::CameraBackend::Icc;
    cfg.create(&path).unwrap();
    for command in [Command::Probe, Command::Doctor, Command::Run] {
        let mut out = Vec::new();
        let error = execute(Ok(options(command, &path)), &mut out).unwrap_err();
        assert!(matches!(error, Error::IccUnavailable), "{error:?}");
        // No USB enumeration happened and nothing was printed: no `Detected cameras` line.
        assert_eq!(String::from_utf8(out).unwrap(), "");
    }
}

#[tokio::test]
async fn doctor_and_probe_name_the_sanitized_cause() {
    use crate::cli::first_frame::FirstFrameCause;
    use crate::transport::sim::sony::{SonySimConfig, sony_camera};
    let cfg = free_cfg();
    let body = || {
        sony_camera(SonySimConfig {
            advertise_sdio: false,
            ..SonySimConfig::default()
        })
    };
    let mut out = Vec::new();
    let error = super::command_until(
        Factory::new(vec![body()]),
        &cfg,
        &options(Command::Doctor, std::path::Path::new("synthetic.toml")),
        &mut out,
        Box::pin(std::future::ready(())),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, Error::FirstFrame(FirstFrameCause::NotRemote)),
        "{error:?}"
    );
    assert!(error.to_string().contains("PC Remote"), "{error}");
    let error = diagnostics::probe(Factory::new(vec![body()]), &mut out)
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Probe(FirstFrameCause::NotRemote)),
        "{error:?}"
    );
    assert!(error.to_string().contains("PC Remote"));
    assert!(!error.to_string().contains("no first frame"));
}
