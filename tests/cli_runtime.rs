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
                let mut line = String::new();
                BufReader::new(stdout).read_line(&mut line).unwrap();
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
