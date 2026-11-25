//! Foreground simulator CLI, real OS signals and sanitized failure diagnostics.
//!
//! References:
//! - `docs/architecture.md`, CLI and supervisor contract

#[cfg(test)]
mod suite {
    use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;
    use tethercam::config::Config;

    struct ChildGuard(Option<Child>);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if let Some(child) = &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn fixture(path: &std::path::Path) {
        let mut jpeg = Vec::new();
        let mut encoder = Encoder::new(&mut jpeg, 80);
        encoder.set_sampling_factor(SamplingFactor::F_2_2);
        encoder
            .encode(&vec![127; 1024 * 680 * 3], 1024, 680, ColorType::Rgb)
            .unwrap();
        std::fs::write(path, jpeg).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn simulator_run_closes_listeners_on_interrupt_and_terminate() {
        let dir = tempfile::tempdir().unwrap();
        let jpeg = dir.path().join("synthetic.jpg");
        fixture(&jpeg);
        for signal in ["-INT", "-TERM"] {
            let mut cfg = Config::generate().unwrap();
            cfg.rtsp_address = "127.0.0.1".parse().unwrap();
            cfg.onvif_address = cfg.rtsp_address;
            let rtsp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let http = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            cfg.rtsp_port = rtsp.local_addr().unwrap().port();
            cfg.onvif_port = http.local_addr().unwrap().port();
            let path = dir.path().join(format!("{signal}.toml"));
            cfg.create(&path).unwrap();
            drop((rtsp, http));
            let mut child = ChildGuard(Some(
                Command::new(env!("CARGO_BIN_EXE_tethercam"))
                    .arg("run")
                    .arg("--config")
                    .arg(&path)
                    .arg("--simulate")
                    .arg(&jpeg)
                    .arg("--no-discovery")
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            ));
            let stdout = child.0.as_mut().unwrap().stdout.take().unwrap();
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let reader = std::thread::spawn(move || {
                // The waiting line comes first; the serving line follows the first frame.
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                while !line.contains("Serving") {
                    line.clear();
                    if reader.read_line(&mut line).unwrap() == 0 {
                        break;
                    }
                }
                tx.send(line).unwrap();
            });
            assert!(
                rx.recv_timeout(Duration::from_secs(15))
                    .unwrap()
                    .contains("Serving authenticated")
            );
            reader.join().unwrap();
            let pid = child.0.as_ref().unwrap().id();
            assert!(
                Command::new("/bin/kill")
                    .arg(signal)
                    .arg(pid.to_string())
                    .status()
                    .unwrap()
                    .success()
            );
            let child = child.0.take().unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let logs = String::from_utf8(output.stderr).unwrap();
            assert!(logs.contains("runtime_ready"));
            assert!(logs.contains("runtime_stopped"));
            assert!(!logs.contains(cfg.credentials.password.expose()));
            assert!(!logs.contains(cfg.credentials.username.expose()));
            assert!(!logs.contains(&cfg.endpoint_uuid.to_string()));
            assert!(std::net::TcpListener::bind((cfg.rtsp_address, cfg.rtsp_port)).is_ok());
            assert!(std::net::TcpListener::bind((cfg.onvif_address, cfg.onvif_port)).is_ok());
        }
    }

    fn assert_report_schema(text: &str) {
        let fields: Vec<_> = text
            .lines()
            .map(|line| line.split_once(": ").unwrap().0)
            .collect();
        assert_eq!(
            fields,
            [
                "tethercam",
                "os",
                "arch",
                "backend",
                "protocol",
                "capability_live_view",
                "capability_still_capture",
                "capability_set_property",
                "capability_battery",
                "capability_priority_mode",
                "frame",
                "battery_percent",
                "power"
            ]
        );
        for line in text.lines() {
            let (key, value) = line.split_once(": ").unwrap();
            match key {
                "tethercam" => assert_eq!(value, env!("CARGO_PKG_VERSION")),
                "os" => assert_eq!(value, std::env::consts::OS),
                "arch" => assert_eq!(value, std::env::consts::ARCH),
                "backend" => assert_eq!(value, "usb"),
                "protocol" => assert!(matches!(value, "V200" | "V300")),
                "frame" => assert_eq!(value, "32 x 32"),
                "power" => assert!(matches!(
                    value,
                    "usb" | "battery" | "no_battery" | "unknown"
                )),
                "battery_percent" => {
                    assert!(value == "unknown" || value.parse::<u8>().is_ok_and(|n| n <= 100));
                }
                _ => assert!(matches!(value, "true" | "false")),
            }
        }
    }

    fn report_cfg() -> Config {
        let mut cfg = Config::generate().unwrap();
        cfg.rtsp_address = "127.0.0.1".parse().unwrap();
        cfg.onvif_address = cfg.rtsp_address;
        let rtsp = std::net::TcpListener::bind((cfg.rtsp_address, 0)).unwrap();
        let onvif = std::net::TcpListener::bind((cfg.onvif_address, 0)).unwrap();
        cfg.rtsp_port = rtsp.local_addr().unwrap().port();
        cfg.onvif_port = onvif.local_addr().unwrap().port();
        drop((rtsp, onvif));
        cfg.camera_serial = Some("synthetic-private-serial".into());
        cfg.interface = Some("synthetic-private-host".into());
        cfg
    }

    #[test]
    fn doctor_report_is_allowlisted_on_success_and_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic-private-config.toml");
        let cfg = report_cfg();
        cfg.create(&path).unwrap();
        let jpeg_path = dir.path().join("synthetic-private-frame.jpg");
        let mut jpeg = Vec::new();
        let mut encoder = Encoder::new(&mut jpeg, 80);
        encoder.set_sampling_factor(SamplingFactor::F_2_2);
        encoder
            .encode(&[127; 32 * 32 * 3], 32, 32, ColorType::Rgb)
            .unwrap();
        std::fs::write(&jpeg_path, jpeg).unwrap();
        let report = |path: &std::path::Path| {
            Command::new(env!("CARGO_BIN_EXE_tethercam"))
                .args(["doctor", "--report", "--config"])
                .arg(path)
                .arg("--simulate")
                .arg(&jpeg_path)
                .output()
                .unwrap()
        };
        let output = report(&path);
        assert!(output.status.success());
        assert_eq!(output.stderr, [] as [u8; 0]);
        let text = String::from_utf8(output.stdout).unwrap();
        assert_report_schema(&text);
        for value in [
            "synthetic-private",
            cfg.credentials.username.expose(),
            cfg.credentials.password.expose(),
            &cfg.endpoint_uuid.to_string(),
            "127.0.0.1",
            "indicator",
        ] {
            assert!(!text.contains(value), "sensitive data in report");
        }
        std::fs::remove_file(jpeg_path.as_path()).unwrap();
        let failed = report(&path);
        assert_eq!(failed.status.code(), Some(1));
        assert_eq!(failed.stderr, [] as [u8; 0]);
        assert!(failed.stdout.ends_with(b"Error: io\n"));
        let failed = report(&dir.path().join("synthetic-private-missing"));
        assert_eq!(failed.status.code(), Some(1));
        let partial = String::from_utf8(failed.stdout).unwrap();
        assert!(partial.ends_with("Error: configuration\n"), "{partial}");
        assert_eq!(partial.matches("Error:").count(), 1, "{partial}");
        assert!(!partial.contains("synthetic-private"));
    }

    #[test]
    fn invalid_report_never_dispatches_setup() {
        use tethercam::cli::{Command, Options, execute};
        let mut opts = Options::parse(["doctor".into(), "--report".into()]).unwrap();
        opts.command = Command::Setup;
        let mut out = Vec::new();
        assert!(matches!(
            execute(Ok(opts), &mut out),
            Err(tethercam::cli::Error::Arguments(_))
        ));
        assert_eq!(out, [] as [u8; 0]);
    }

    #[test]
    fn borrowed_run_dispatches_into_its_writer() {
        const CHILD: &str = "TETHERCAM_TEST_BORROWED_RUN";
        if std::env::var_os(CHILD).is_some() {
            assert_eq!(std::env::args().nth(1).as_deref(), Some("--exact"));
            let mut out = Vec::new();
            assert_eq!(
                tethercam::cli::run(&mut out),
                std::process::ExitCode::FAILURE
            );
            assert!(out.starts_with(b"Error:"));
            return;
        }
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "suite::borrowed_run_dispatches_into_its_writer"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn unavailable_icc_report_is_finalized_on_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic-private.toml");
        let mut cfg = Config::generate().unwrap();
        cfg.camera_backend = tethercam::config::CameraBackend::Icc;
        cfg.create(&path).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_tethercam"))
            .args(["doctor", "--report", "--config"])
            .arg(&path)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(output.stderr, [] as [u8; 0]);
        let report = String::from_utf8(output.stdout).unwrap();
        assert!(report.starts_with("tethercam: "), "{report}");
        assert!(
            report.ends_with("backend: icc\nError: backend\n"),
            "{report}"
        );
        assert!(!report.contains("synthetic-private"));
    }

    #[test]
    fn malformed_inputs_produce_redacted_nonzero_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = Config::generate().unwrap();
        cfg.create(&path).unwrap();
        for args in [
            vec!["unknown"],
            vec!["probe", "--simulate"],
            vec![
                "doctor",
                "--config",
                path.to_str().unwrap(),
                "--simulate",
                "/synthetic/missing.jpg",
            ],
        ] {
            let output = Command::new(env!("CARGO_BIN_EXE_tethercam"))
                .args(args)
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert!(
                String::from_utf8(output.stdout)
                    .unwrap()
                    .starts_with("Error:")
            );
            assert_eq!(output.stderr, [] as [u8; 0]);
        }
    }
}
