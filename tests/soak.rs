//! The soak sampler against the real simulator service, with the real `curl` and, when
//! installed, the real `ffprobe`. No camera, USB or daemon is involved.
//!
//! References:
//! - `docs/service.md`, run counters and soak
#![cfg(feature = "repo-tools")]
#![cfg(unix)]

use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use tethercam::config::Config;

struct Service {
    child: Option<Child>,
    config: PathBuf,
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Start `tethercam run --simulate` on loopback and return once it serves video.
fn service(dir: &Path) -> Service {
    let mut jpeg = Vec::new();
    let mut encoder = Encoder::new(&mut jpeg, 80);
    encoder.set_sampling_factor(SamplingFactor::F_2_2);
    encoder
        .encode(&vec![127; 320 * 240 * 3], 320, 240, ColorType::Rgb)
        .unwrap();
    let image = dir.join("synthetic.jpg");
    std::fs::write(&image, jpeg).unwrap();
    let mut cfg = Config::generate().unwrap();
    cfg.rtsp_address = "127.0.0.1".parse().unwrap();
    cfg.onvif_address = cfg.rtsp_address;
    let rtsp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let http = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    cfg.rtsp_port = rtsp.local_addr().unwrap().port();
    cfg.onvif_port = http.local_addr().unwrap().port();
    let config = dir.join("service.toml");
    cfg.create(&config).unwrap();
    drop((rtsp, http));
    let mut child = Command::new(env!("CARGO_BIN_EXE_tethercam"))
        .args(["run", "--no-discovery", "--config"])
        .arg(&config)
        .arg("--simulate")
        .arg(&image)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let service = Service {
        child: Some(child),
        config,
    };
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    while !line.contains("Serving") {
        line.clear();
        assert!(reader.read_line(&mut line).unwrap() > 0, "service exited");
    }
    service
}

fn soak(
    dir: &Path, ffprobe: &Path, service: &Service, seconds: &str, segment: &str,
) -> serde_json::Value {
    let output = dir.join("soak");
    let result = Command::new(env!("CARGO_BIN_EXE_tethercam-tools"))
        .arg("soak")
        .arg(&service.config)
        .arg(&output)
        .arg(ffprobe)
        .args(["/usr/bin/curl", seconds, segment, "500"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let text = std::fs::read_to_string(output.join("report.json")).unwrap();
    // Nothing that identifies the service or lets a reader into it.
    let cfg = Config::load(&service.config).unwrap();
    assert!(!text.contains(cfg.credentials.password.expose()));
    assert!(!text.contains(cfg.credentials.username.expose()));
    assert!(!text.contains(&cfg.endpoint_uuid.to_string()));
    assert!(!output.join("clients").exists());
    serde_json::from_str(&text).unwrap()
}

#[test]
fn status_sampling_reads_the_counters_of_a_running_simulator() {
    let dir = tempfile::tempdir().unwrap();
    let service = service(dir.path());
    let fixture = dir.path().join("ffprobe-fixture");
    assert!(
        Command::new("rustc")
            .args(["--edition=2024", "-D", "warnings"])
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/verification_client.rs")
            )
            .arg("-o")
            .arg(&fixture)
            .status()
            .unwrap()
            .success()
    );
    let report = soak(dir.path(), &fixture, &service, "3", "1");
    let status = &report["status"];
    assert!(status["samples"].as_u64().unwrap() >= 2, "{report}");
    assert_eq!(status["failures"], 0, "{report}");
    assert_eq!(
        status["camera_ready_samples"], status["samples"],
        "{report}"
    );
    assert_eq!(status["process_restarts_seen"], 0, "{report}");
    // Nobody streams, so the simulator's one first frame is all that was fetched.
    assert_eq!(status["totals"]["frames_fetched"], 1, "{report}");
    assert_eq!(status["totals"]["session_restarts"], 0, "{report}");
    assert_eq!(report["battery"]["points_dropped"], 0, "{report}");
}

#[test]
fn a_real_ffprobe_segment_counts_the_streamed_frames() {
    let Some(ffprobe) = [
        "/usr/bin/ffprobe",
        "/opt/homebrew/bin/ffprobe",
        "/usr/local/bin/ffprobe",
    ]
    .map(Path::new)
    .into_iter()
    .find(|path| path.is_file()) else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let service = service(dir.path());
    let report = soak(dir.path(), ffprobe, &service, "8", "3");
    let stream = &report["stream"];
    assert!(stream["segments"].as_u64().unwrap() >= 2, "{report}");
    assert_eq!(stream["segments_failed"], 0, "{report}");
    assert!(stream["packets"].as_u64().unwrap() > 10, "{report}");
    let totals = &report["status"]["totals"];
    assert!(totals["frames_fetched"].as_u64().unwrap() > 10, "{report}");
    assert!(totals["frames_served"].as_u64().unwrap() > 10, "{report}");
}
