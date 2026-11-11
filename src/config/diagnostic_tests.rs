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
fn nested_unknown_keys_and_secret_errors() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("config.toml");
    Config::generate()?.create(&path)?;
    let original = fs::read_to_string(&path)?;
    let nested = original.replace("[streams]", "[streams]\nunexpected = true");
    fs::write(&path, nested)?;
    assert!(matches!(Config::load(&path), Err(Error::Parse(_))));
    let malformed = original.replace(
        "[credentials]",
        "[credentials]\nunexpected = \"synthetic-secret\"",
    );
    fs::write(&path, malformed)?;
    let error = Config::load(&path).err().ok_or("expected parse error")?;
    assert!(!format!("{error} {error:?}").contains("synthetic-secret"));
    Ok(())
}

#[test]
fn bad_address_names_key_and_value() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("config.toml");
    Config::generate()?.create(&path)?;
    let contents = fs::read_to_string(&path)?
        .replace("rtsp_address = \"0.0.0.0\"", "rtsp_address = \"not-an-ip\"");
    fs::write(&path, contents)?;
    let Err(Error::Parse(error)) = Config::load(&path) else {
        return Err("expected parse error".into());
    };
    assert!(error.contains("rtsp_address"), "{error}");
    assert!(error.contains("not-an-ip"), "{error}");
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

#[test]
fn structural_errors_name_the_key_without_leaking_values() -> Result<(), Box<dyn std::error::Error>>
{
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("config.toml");
    Config::generate()?.create(&path)?;
    let original = fs::read_to_string(&path)?;

    // Unknown key at the root: the message names it.
    fs::write(&path, format!("mystery_key = true\n{original}"))?;
    let error = Config::load(&path).err().ok_or("expected parse error")?;
    assert!(error.to_string().contains("mystery_key"), "{error}");

    // Missing required key under `credentials`: the message names it.
    let without_password = original
        .lines()
        .filter(|line| !line.trim_start().starts_with("password"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_ne!(without_password, original);
    fs::write(&path, without_password)?;
    let error = Config::load(&path).err().ok_or("expected parse error")?;
    assert!(error.to_string().contains("password"), "{error}");

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
    fs::write(&path, wrong_type)?;
    let error = Config::load(&path).err().ok_or("expected parse error")?;
    assert!(!format!("{error} {error:?}").contains("synthetic-secret"));
    Ok(())
}
