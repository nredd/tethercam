//! Configuration diagnostics and secret-redaction regressions.
//!
//! References:
//! - <https://toml.io/>

use super::tests::assert_variant;
use super::{Config, Error, resolve_path};
use std::fs;

#[test]
fn path_resolution_isolated_from_environment() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let xdg = dir.path().join("xdg");
    assert_eq!(
        resolve_path(Some(home.clone()), Some(xdg.clone()), false).unwrap(),
        xdg.join("tethercam/config.toml")
    );
    assert_eq!(
        resolve_path(Some(home.clone()), Some("relative".into()), false).unwrap(),
        home.join(".config/tethercam/config.toml")
    );
    assert_eq!(
        resolve_path(Some(home.clone()), None, true).unwrap(),
        home.join("Library/Application Support/tethercam/config.toml")
    );
    assert!(resolve_path(None, None, false).is_err());
}

#[test]
fn nested_unknown_keys_and_secret_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    Config::generate().unwrap().create(&path).unwrap();
    let original = fs::read_to_string(&path).unwrap();
    let nested = original.replace("[streams]", "[streams]\nunexpected = true");
    fs::write(&path, nested).unwrap();
    assert_variant(
        &Config::load(&path).unwrap_err(),
        &Error::Parse(String::new()),
    );
    let malformed = original.replace(
        "[credentials]",
        "[credentials]\nunexpected = \"synthetic-secret\"",
    );
    fs::write(&path, malformed).unwrap();
    let error = Config::load(&path).unwrap_err();
    assert!(!format!("{error} {error:?}").contains("synthetic-secret"));
}

#[test]
fn bad_address_names_key_and_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    Config::generate().unwrap().create(&path).unwrap();
    let contents = fs::read_to_string(&path)
        .unwrap()
        .replace("rtsp_address = \"0.0.0.0\"", "rtsp_address = \"not-an-ip\"");
    fs::write(&path, contents).unwrap();
    let error = Config::load(&path).unwrap_err();
    assert_variant(&error, &Error::Parse(String::new()));
    let error = error.to_string();
    assert!(error.contains("rtsp_address"), "{error}");
    assert!(error.contains("not-an-ip"), "{error}");
}

#[test]
fn bad_port_names_key_and_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    Config::generate().unwrap().create(&path).unwrap();
    let contents = fs::read_to_string(&path)
        .unwrap()
        .replace("rtsp_port = 8554", "rtsp_port = 999999");
    fs::write(&path, contents).unwrap();
    let error = Config::load(&path).unwrap_err();
    assert_variant(&error, &Error::Parse(String::new()));
    let error = error.to_string();
    assert!(error.contains("rtsp_port"), "{error}");
    assert!(error.contains("999999"), "{error}");
}

#[test]
fn structural_errors_name_the_key_without_leaking_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    Config::generate().unwrap().create(&path).unwrap();
    let original = fs::read_to_string(&path).unwrap();

    // Unknown key at the root: the message names it.
    fs::write(&path, format!("mystery_key = true\n{original}")).unwrap();
    let error = Config::load(&path).unwrap_err();
    assert!(error.to_string().contains("mystery_key"), "{error}");

    // Missing required key under `credentials`: the message names it.
    let without_password = original
        .lines()
        .filter(|line| !line.trim_start().starts_with("password"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_ne!(without_password, original);
    fs::write(&path, without_password).unwrap();
    let error = Config::load(&path).unwrap_err();
    assert!(error.to_string().contains("password"), "{error}");

    // Unknown key under `credentials`: reported, but what the user typed is not echoed.
    let misplaced = original.replace("[credentials]", "[credentials]\nsynthetic-secret = true");
    assert_ne!(misplaced, original);
    fs::write(&path, misplaced).unwrap();
    let error = Config::load(&path).unwrap_err();
    let shown = format!("{error} {error:?}");
    assert!(shown.contains("unknown key"), "{shown}");
    assert!(!shown.contains("synthetic-secret"), "{shown}");

    // Wrong type for a credential: the value must not be echoed.
    let wrong_type = original
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("password") {
                "password = [\"synthetic-secret\"]"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, wrong_type).unwrap();
    let error = Config::load(&path).unwrap_err();
    assert!(!format!("{error} {error:?}").contains("synthetic-secret"));
}
