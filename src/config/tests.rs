//! Configuration validation and persistence regressions.
//!
//! References:
//! - <https://toml.io/>

use super::{Config, Error, Secret};
use proptest::prelude::*;
use std::fs;

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
