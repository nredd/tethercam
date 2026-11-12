use super::{Config, Error, Secret};
use proptest::prelude::*;
use std::fs;

#[test]
fn round_trip_and_no_overwrite() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("config.toml");
    let original = Config::load_or_create(&path)?;
    let loaded = Config::load_or_create(&path)?;
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
    Ok(())
}

#[test]
fn redaction() {
    let secret = Secret::new("synthetic-secret".to_owned());
    assert!(!format!("{secret} {secret:?}").contains("synthetic-secret"));
    assert!(!secret.constant_time_eq(&Secret::new("different".into())));
}

#[test]
fn invalid_values() -> Result<(), Box<dyn std::error::Error>> {
    let base = Config::generate()?;
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
        assert!(
            matches!(config.validate(), Err(Error::Validation { key: actual, .. }) if actual == key)
        );
    }
    Ok(())
}

#[test]
fn invalid_streams_and_credentials() -> Result<(), Box<dyn std::error::Error>> {
    let base = Config::generate()?;
    let mut config = base.clone();
    config.streams.frame_rate = 0;
    assert!(matches!(
        config.validate(),
        Err(Error::Validation {
            key: "streams.frame_rate",
            ..
        })
    ));
    config = base.clone();
    config.streams.bitrate = 0;
    assert!(matches!(
        config.validate(),
        Err(Error::Validation {
            key: "streams.bitrate",
            ..
        })
    ));
    config = base.clone();
    config.streams.h264 = false;
    config.streams.mjpeg = false;
    assert!(matches!(
        config.validate(),
        Err(Error::Validation { key: "streams", .. })
    ));
    config = base.clone();
    config.credentials.username = Secret::new(String::new());
    assert!(matches!(
        config.validate(),
        Err(Error::Validation {
            key: "credentials.username",
            ..
        })
    ));
    config = base;
    config.credentials.password = Secret::new(String::new());
    assert!(matches!(
        config.validate(),
        Err(Error::Validation {
            key: "credentials.password",
            ..
        })
    ));
    Ok(())
}

#[test]
fn unknown_key_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("config.toml");
    let config = Config::generate()?;
    config.create(&path)?;
    let content = fs::read_to_string(&path)?;
    fs::write(&path, format!("unknown = true\n{content}"))?;
    assert!(matches!(Config::load(&path), Err(Error::Parse(_))));
    Ok(())
}

#[cfg(unix)]
#[test]
fn refuses_insecure_permissions() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("config.toml");
    Config::generate()?.create(&path)?;
    assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640))?;
    assert!(matches!(Config::load(&path), Err(Error::Permissions)));
    Ok(())
}

proptest! {
    #[test]
    fn arbitrary_config_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        Config::generate().map_err(|e| TestCaseError::fail(e.to_string()))?.create(&path).map_err(|e| TestCaseError::fail(e.to_string()))?;
        fs::write(&path, bytes)?;
        let _ = Config::load(&path);
    }
}
