//! Configuration validation and persistence regressions.
//!
//! References:
//! - <https://toml.io/>

use super::{CameraBackend, Config, Error, Secret};
use proptest::prelude::*;
use std::fs;

#[test]
fn credential_syntax_is_rejected_before_camera_or_listener_startup() {
    for (username, password) in [
        ("bad:name", "synthetic-password"),
        ("bad\"name", "synthetic-password"),
        ("bad\\name", "synthetic-password"),
        ("bad\nname", "synthetic-password"),
        ("synthetic-operator", "bad\npassword"),
    ] {
        let mut cfg = Config::generate().unwrap();
        cfg.credentials.username = Secret::new(username.into());
        cfg.credentials.password = Secret::new(password.into());
        let (key, value) = validation(cfg.validate().unwrap_err());
        assert_eq!(key, "credentials");
        assert_eq!(value, "[REDACTED]");
    }
}

/// Check an error variant without introducing an unreachable `matches!` fallback per test.
pub(super) fn assert_variant(actual: &Error, expected: &Error) {
    assert_eq!(
        std::mem::discriminant(actual),
        std::mem::discriminant(expected)
    );
}

/// Extract validation details; callers still assert the exact key and value.
pub(super) fn validation(error: Error) -> (&'static str, String) {
    match error {
        Error::Validation { key, value } => (key, value),
        other => panic!("expected validation error, got {other:?}"),
    }
}

#[test]
#[should_panic(expected = "expected validation error")]
fn validation_assertion_rejects_other_errors() {
    validation(Error::Random);
}

#[test]
fn round_trip_and_no_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let original = Config::load_or_create(&path).unwrap();
    let loaded = Config::load_or_create(&path).unwrap();
    assert_eq!(original.endpoint_uuid, loaded.endpoint_uuid);
    assert!(
        original
            .credentials
            .username
            .constant_time_eq(&loaded.credentials.username)
    );
    assert!(
        original
            .credentials
            .password
            .constant_time_eq(&loaded.credentials.password)
    );
    assert!(original.create(&path).is_err());
    assert_eq!(original.streams.frame_rate, 15);
    assert_eq!(original.rtsp_port, 8554);
}

#[test]
fn redaction() {
    let secret = Secret::new("synthetic-secret".to_owned());
    assert!(!format!("{secret} {secret:?}").contains("synthetic-secret"));
    assert!(!secret.constant_time_eq(&Secret::new("different".into())));
}

#[test]
fn invalid_values() {
    let base = Config::generate().unwrap();
    let cases: Vec<(&str, Config)> = vec![
        (
            "rtsp_port",
            Config {
                rtsp_port: 0,
                ..base.clone()
            },
        ),
        (
            "onvif_port",
            Config {
                onvif_port: 0,
                ..base.clone()
            },
        ),
        (
            "device_name",
            Config {
                device_name: String::new(),
                ..base.clone()
            },
        ),
        (
            "log_level",
            Config {
                log_level: "bad".into(),
                ..base.clone()
            },
        ),
        (
            "interface",
            Config {
                interface: Some(String::new()),
                ..base.clone()
            },
        ),
        (
            "camera_serial",
            Config {
                camera_serial: Some(String::new()),
                ..base.clone()
            },
        ),
        (
            "endpoint_uuid",
            Config {
                endpoint_uuid: uuid::Uuid::nil(),
                ..base.clone()
            },
        ),
    ];
    for (key, config) in cases {
        assert_eq!(validation(config.validate().unwrap_err()).0, key);
    }
}

#[test]
fn invalid_streams_and_credentials() {
    let base = Config::generate().unwrap();
    let mut config = base.clone();
    config.streams.frame_rate = 0;
    assert_eq!(
        validation(config.validate().unwrap_err()).0,
        "streams.frame_rate"
    );
    config = base.clone();
    config.streams.bitrate = 0;
    assert_eq!(
        validation(config.validate().unwrap_err()).0,
        "streams.bitrate"
    );
    config = base.clone();
    config.streams.h264 = false;
    config.streams.mjpeg = false;
    assert_eq!(validation(config.validate().unwrap_err()).0, "streams");
    config = base.clone();
    config.credentials.username = Secret::new(String::new());
    assert_eq!(
        validation(config.validate().unwrap_err()).0,
        "credentials.username"
    );
    config = base;
    config.credentials.password = Secret::new(String::new());
    assert_eq!(
        validation(config.validate().unwrap_err()).0,
        "credentials.password"
    );
}

#[test]
fn unknown_key_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let config = Config::generate().unwrap();
    config.create(&path).unwrap();
    let content = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("unknown = true\n{content}")).unwrap();
    assert_variant(
        &Config::load(&path).unwrap_err(),
        &Error::Parse(String::new()),
    );
}

#[cfg(unix)]
#[test]
fn refuses_insecure_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    Config::generate().unwrap().create(&path).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    assert_variant(&Config::load(&path).unwrap_err(), &Error::Permissions);
}

proptest! {
    #[test]
    fn arbitrary_config_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        Config::generate().unwrap().create(&path).unwrap();
        fs::write(&path, bytes).unwrap();
        let _ = Config::load(&path);
    }
}

#[test]
fn full_resolution_snapshot_defaults_off_and_older_configs_still_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let config = Config::generate().unwrap();
    assert!(!config.streams.full_resolution_snapshot);
    config.create(&path).unwrap();
    let content = fs::read_to_string(&path).unwrap();
    let older = content
        .lines()
        .filter(|line| !line.starts_with("full_resolution_snapshot"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_ne!(older, content);
    fs::write(&path, older).unwrap();
    assert!(
        !Config::load(&path)
            .unwrap()
            .streams
            .full_resolution_snapshot
    );
}

#[test]
fn camera_backend_defaults_to_usb_and_selects_icc_only_when_named() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let config = Config::generate().unwrap();
    assert_eq!(config.camera_backend, CameraBackend::Usb);
    config.create(&path).unwrap();
    let content = fs::read_to_string(&path).unwrap();
    assert!(content.contains("camera_backend = \"usb\""));
    // A config written before the key existed still loads, as USB.
    let older = content
        .lines()
        .filter(|line| !line.starts_with("camera_backend"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, &older).unwrap();
    assert_eq!(
        Config::load(&path).unwrap().camera_backend,
        CameraBackend::Usb
    );
    fs::write(
        &path,
        older.replacen('\n', "\ncamera_backend = \"icc\"\n", 1),
    )
    .unwrap();
    assert_eq!(
        Config::load(&path).unwrap().camera_backend,
        CameraBackend::Icc
    );
    // No spelling variants and no fallback value: anything else is a parse error.
    for bad in ["ICC", "auto", "libusb", ""] {
        let text = older.replacen('\n', &format!("\ncamera_backend = \"{bad}\"\n"), 1);
        fs::write(&path, text).unwrap();
        assert!(Config::load(&path).is_err(), "{bad:?}");
    }
}

#[test]
fn icc_rejects_a_usb_serial_instead_of_ignoring_it() {
    let mut config = Config::generate().unwrap();
    config.camera_serial = Some("SYNTHETIC-SERIAL".into());
    assert!(config.validate().is_ok());
    config.camera_backend = CameraBackend::Icc;
    let (key, value) = validation(config.validate().unwrap_err());
    assert_eq!(key, "camera_serial");
    assert!(!value.contains("SYNTHETIC-SERIAL"));
    config.camera_serial = None;
    assert!(config.validate().is_ok());
}

#[test]
fn camera_id_is_optional_persisted_exactly_and_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut config = Config::generate().unwrap();
    assert!(config.camera_id.is_none());
    config.create(&path).unwrap();
    let content = fs::read_to_string(&path).unwrap();
    assert!(!content.contains("camera_id"));
    // A config written before the key existed still loads, with no selection.
    assert!(Config::load(&path).unwrap().camera_id.is_none());
    fs::remove_file(&path).unwrap();
    config.camera_backend = CameraBackend::Icc;
    config.camera_id = Some(Secret::new("SYNTHETIC-ID-0001".into()));
    config.create(&path).unwrap();
    let loaded = Config::load(&path).unwrap();
    assert_eq!(
        loaded.camera_id.as_ref().unwrap().expose(),
        "SYNTHETIC-ID-0001"
    );
    // It names a private device: never formatted.
    assert!(!format!("{loaded:?}").contains("SYNTHETIC-ID-0001"));
}

#[test]
fn camera_id_is_icc_only_and_validated_without_echoing_it() {
    let mut config = Config::generate().unwrap();
    config.camera_id = Some(Secret::new("SYNTHETIC-ID-0001".into()));
    // The raw-USB backend selects by `camera_serial`: a `camera_id` is rejected, not ignored.
    let (key, value) = validation(config.validate().unwrap_err());
    assert_eq!(key, "camera_id");
    assert!(value.contains("camera_backend"), "{value}");
    assert!(!value.contains("SYNTHETIC-ID-0001"));
    config.camera_backend = CameraBackend::Icc;
    assert!(config.validate().is_ok());
    // And the reverse is still true.
    config.camera_serial = Some("SYNTHETIC-SERIAL".into());
    assert_eq!(
        validation(config.validate().unwrap_err()).0,
        "camera_serial"
    );
    config.camera_serial = None;
    for bad in [
        String::new(),
        " ".into(),
        " SYNTHETIC-ID-0001".into(),
        "SYNTHETIC-ID-0001 ".into(),
        "SYNTHETIC\0ID".into(),
        "SYNTHETIC\nID".into(),
        // Format and separator characters hide or forge text in a terminal.
        "SYNTHETIC\u{202E}ID".into(),
        "SYNTHETIC\u{200B}ID".into(),
        "SYNTHETIC\u{2028}ID".into(),
        "S".repeat(1025),
    ] {
        config.camera_id = Some(Secret::new(bad.clone()));
        let error = config.validate().unwrap_err();
        let shown = format!("{error} {error:?}");
        let (key, value) = validation(error);
        assert_eq!(key, "camera_id", "{bad:?}");
        assert!(
            !value.contains("SYNTHETIC") && !shown.contains("SYNTHETIC"),
            "{shown}"
        );
    }
    config.camera_id = Some(Secret::new("S".repeat(1024)));
    assert!(config.validate().is_ok());
}
