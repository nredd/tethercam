use super::{Config, Error, resolve_path};
use std::fs;

#[test]
fn path_resolution_isolated_from_environment() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let home = dir.path().join("home");
    let xdg = dir.path().join("xdg");
    assert_eq!(
        resolve_path(Some(home.clone()), Some(xdg.clone()), false)?,
        xdg.join("tethercam/config.toml")
    );
    assert_eq!(
        resolve_path(Some(home.clone()), Some("relative".into()), false)?,
        home.join(".config/tethercam/config.toml")
    );
    assert_eq!(
        resolve_path(Some(home.clone()), None, true)?,
        home.join("Library/Application Support/tethercam/config.toml")
    );
    assert!(resolve_path(None, None, false).is_err());
    Ok(())
}

#[test]
fn bad_port_names_key_and_value() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("config.toml");
    Config::generate()?.create(&path)?;
    let contents = fs::read_to_string(&path)?.replace("rtsp_port = 8554", "rtsp_port = 999999");
    fs::write(&path, contents)?;
    let Err(Error::Parse(error)) = Config::load(&path) else {
        return Err("expected parse error".into());
    };
    assert!(error.contains("rtsp_port"), "{error}");
    assert!(error.contains("999999"), "{error}");
    Ok(())
}
