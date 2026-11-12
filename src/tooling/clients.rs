//! Owner-only loopback client inputs. Credentials never appear in process arguments or logs.
//!
//! References:
//! - <https://curl.se/docs/manpage.html>
//! - <https://ffmpeg.org/ffmpeg-formats.html#concat>

use super::Error;
use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use tethercam::config::Config;

fn inputs(cfg: &Config) -> Result<Vec<(&'static str, String)>, Error> {
    if !cfg.rtsp_address.is_loopback() || !cfg.onvif_address.is_loopback() {
        return Err(Error::Invalid("verification requires loopback listeners"));
    }
    let username = cfg.credentials.username.expose();
    let password = cfg.credentials.password.expose();
    if [username, password].iter().any(|value| {
        value.is_empty()
            || !value
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    }) {
        return Err(Error::Invalid(
            "verification requires generated hexadecimal credentials",
        ));
    }
    let authority = SocketAddr::new(cfg.rtsp_address, cfg.rtsp_port);
    let mut result = Vec::new();
    for (mount, url_name, concat_name) in [
        ("main", "rtsp-main.url", "rtsp-main.ffconcat"),
        ("mjpeg", "rtsp-mjpeg.url", "rtsp-mjpeg.ffconcat"),
    ] {
        let url = format!("rtsp://{username}:{password}@{authority}/live/{mount}");
        result.push((url_name, url.clone()));
        result.push((concat_name, format!("ffconcat version 1.0\nfile '{url}'\noption rtsp_transport tcp\noption timeout 15000000\n")));
    }
    result.push(("http-client.conf", format!("silent\nshow-error\ndigest\nuser = \"{username}:{password}\"\nconnect-timeout = 5\nmax-time = 360\n")));
    result.push((
        "http-base.url",
        format!(
            "http://{}",
            SocketAddr::new(cfg.onvif_address, cfg.onvif_port)
        ),
    ));
    Ok(result)
}

/// Publish an entirely new protected directory. Existing files/directories are never replaced.
pub(super) fn prepare(cfg: &Path, output: &Path) -> Result<String, Error> {
    prepare_with(cfg, output, Files::default())
}

/// Same-handle file operations; tests inject failures without replacing privileged host files.
#[derive(Clone, Copy)]
struct Files {
    open: fn(&Path) -> std::io::Result<fs::File>,
    write: fn(&mut fs::File, &[u8]) -> std::io::Result<()>,
    sync: fn(&fs::File) -> std::io::Result<()>,
}
impl Default for Files {
    fn default() -> Self {
        Self {
            open,
            write: Write::write_all,
            sync: fs::File::sync_all,
        }
    }
}
fn open(path: &Path) -> std::io::Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

fn prepare_with(cfg: &Path, output: &Path, files: Files) -> Result<String, Error> {
    let cfg = Config::load(cfg)
        .map_err(|_| Error::Invalid("cannot load protected verification configuration"))?;
    let inputs = inputs(&cfg)?;
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(output)?;
    for (name, contents) in inputs {
        let mut file = (files.open)(&output.join(name))?;
        (files.write)(&mut file, contents.as_bytes())?;
        (files.sync)(&file)?;
    }
    Ok(format!(
        "http://{}",
        SocketAddr::new(cfg.onvif_address, cfg.onvif_port)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loopback_types_secrets_and_atomic_no_replace_guards() {
        let mut cfg = Config::generate().expect("cfg");
        cfg.rtsp_address = "::1".parse().expect("ip");
        cfg.onvif_address = "::1".parse().expect("ip");
        let values = inputs(&cfg).expect("inputs");
        assert!(
            values
                .iter()
                .any(|(name, value)| *name == "rtsp-main.url" && value.contains("@[::1]:"))
        );
        cfg.rtsp_address = "192.0.2.1".parse().expect("ip");
        assert!(inputs(&cfg).is_err());
        cfg.rtsp_address = "127.0.0.1".parse().expect("ip");
        cfg.onvif_address = "192.0.2.1".parse().expect("ip");
        assert!(inputs(&cfg).is_err());
        cfg.onvif_address = "127.0.0.1".parse().expect("ip");
        for value in ["", "ABC", "not-a-generated-secret", "a\n"] {
            cfg.credentials.username = tethercam::config::Secret::new(value.into());
            assert!(inputs(&cfg).is_err());
        }
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("config.toml");
        let output = temp.path().join("clients");
        assert!(prepare(&path, &output).is_err());
        let mut valid = Config::generate().expect("generate");
        valid.rtsp_address = "127.0.0.1".parse().expect("ip");
        valid.onvif_address = "127.0.0.1".parse().expect("ip");
        valid.create(&path).expect("cfg");
        prepare(&path, &output).expect("prepare");
        assert!(prepare(&path, &output).is_err());
        assert!(prepare(&path, &temp.path().join("missing/clients")).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&output).expect("mode").permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(output.join("http-client.conf"))
                    .expect("mode")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn publication_failure_boundaries_never_replace_existing_inputs() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("cfg");
        let mut cfg = Config::generate().expect("cfg");
        cfg.create(&path).expect("wildcard cfg");
        assert!(prepare(&path, &temp.path().join("wildcard")).is_err());
        fs::remove_file(&path).expect("remove cfg");
        cfg.rtsp_address = "127.0.0.1".parse().expect("ip");
        cfg.onvif_address = "127.0.0.1".parse().expect("ip");
        cfg.create(&path).expect("loopback cfg");
        for (index, files) in [
            Files {
                open: |_| Err(std::io::Error::other("open")),
                ..Files::default()
            },
            Files {
                write: |_, _| Err(std::io::Error::other("write")),
                ..Files::default()
            },
            Files {
                sync: |_| Err(std::io::Error::other("sync")),
                ..Files::default()
            },
        ]
        .into_iter()
        .enumerate()
        {
            assert!(prepare_with(&path, &temp.path().join(index.to_string()), files).is_err());
        }
    }
}
