//! Owner-only loopback client inputs. Credentials never appear in process arguments or logs.
//!
//! References:
//! - <https://curl.se/docs/manpage.html>
//! - <https://ffmpeg.org/ffmpeg-formats.html#concat>

use super::Error;
use std::fs;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use tethercam::config::Config;

/// Which listener addresses a client may be pointed at.
#[derive(Clone, Copy)]
pub(super) enum Access {
    /// Only loopback listeners: the verifier owns the server it talks to.
    Loopback,
    /// A service already running on this host. A wildcard listener is reached through the
    /// loopback address of its family; any other address is used as configured.
    Host,
}

/// The address a client on this host dials for a listener, or `None` when `access` refuses it.
fn target(listener: IpAddr, access: Access) -> Option<IpAddr> {
    match access {
        Access::Loopback => listener.is_loopback().then_some(listener),
        Access::Host if listener.is_unspecified() => Some(match listener {
            IpAddr::V4(_) => Ipv4Addr::LOCALHOST.into(),
            IpAddr::V6(_) => Ipv6Addr::LOCALHOST.into(),
        }),
        Access::Host => Some(listener),
    }
}

/// Client input files, plus the ONVIF HTTP base URL they point at.
struct Inputs {
    files: Vec<(&'static str, String)>,
    base: String,
}

fn inputs(cfg: &Config, access: Access) -> Result<Inputs, Error> {
    let (Some(rtsp_ip), Some(onvif_ip)) = (
        target(cfg.rtsp_address, access),
        target(cfg.onvif_address, access),
    ) else {
        return Err(Error::Invalid("verification requires loopback listeners"));
    };
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
    let authority = SocketAddr::new(rtsp_ip, cfg.rtsp_port);
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
    let base = format!("http://{}", SocketAddr::new(onvif_ip, cfg.onvif_port));
    result.push(("http-base.url", base.clone()));
    Ok(Inputs {
        files: result,
        base,
    })
}

/// Publish an entirely new protected directory. Existing files/directories are never replaced.
pub(super) fn prepare(cfg: &Path, output: &Path) -> Result<String, Error> {
    prepare_with(cfg, output, Files::default(), Access::Loopback)
}

/// Same, for a service already running on this host, see [`Access::Host`].
pub(super) fn prepare_host(cfg: &Path, output: &Path) -> Result<String, Error> {
    prepare_with(cfg, output, Files::default(), Access::Host)
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

fn prepare_with(cfg: &Path, output: &Path, files: Files, access: Access) -> Result<String, Error> {
    let cfg = Config::load(cfg)
        .map_err(|_| Error::Invalid("cannot load protected verification configuration"))?;
    let inputs = inputs(&cfg, access)?;
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(output)?;
    for (name, contents) in inputs.files {
        let mut file = (files.open)(&output.join(name))?;
        (files.write)(&mut file, contents.as_bytes())?;
        (files.sync)(&file)?;
    }
    Ok(inputs.base)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loopback_types_secrets_and_atomic_no_replace_guards() {
        let mut cfg = Config::generate().expect("cfg");
        cfg.rtsp_address = "::1".parse().expect("ip");
        cfg.onvif_address = "::1".parse().expect("ip");
        let values = inputs(&cfg, Access::Loopback).expect("inputs");
        assert!(
            values
                .files
                .iter()
                .any(|(name, value)| *name == "rtsp-main.url" && value.contains("@[::1]:"))
        );
        cfg.rtsp_address = "192.0.2.1".parse().expect("ip");
        assert!(inputs(&cfg, Access::Loopback).is_err());
        cfg.rtsp_address = "127.0.0.1".parse().expect("ip");
        cfg.onvif_address = "192.0.2.1".parse().expect("ip");
        assert!(inputs(&cfg, Access::Loopback).is_err());
        cfg.onvif_address = "127.0.0.1".parse().expect("ip");
        for value in ["", "ABC", "not-a-generated-secret", "a\n"] {
            cfg.credentials.username = tethercam::config::Secret::new(value.into());
            assert!(inputs(&cfg, Access::Loopback).is_err());
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
    fn host_access_reaches_wildcards_through_loopback_and_keeps_other_addresses() {
        let mut cfg = Config::generate().expect("cfg");
        for (rtsp, onvif, base) in [
            ("0.0.0.0", "0.0.0.0", "http://127.0.0.1:"),
            ("::", "::", "http://[::1]:"),
            ("192.0.2.7", "192.0.2.8", "http://192.0.2.8:"),
            ("127.0.0.1", "127.0.0.1", "http://127.0.0.1:"),
        ] {
            cfg.rtsp_address = rtsp.parse().expect("ip");
            cfg.onvif_address = onvif.parse().expect("ip");
            let host = inputs(&cfg, Access::Host).expect("host inputs");
            assert!(host.base.starts_with(base), "{}", host.base);
            let url = host
                .files
                .iter()
                .find(|(name, _)| *name == "rtsp-main.url")
                .expect("url");
            let want = if rtsp == "0.0.0.0" {
                "@127.0.0.1:"
            } else if rtsp == "::" {
                "@[::1]:"
            } else {
                &format!("@{rtsp}:")
            };
            assert!(url.1.contains(want), "{}", url.1);
            // The verifier's own mode never accepts a non-loopback listener.
            assert_eq!(inputs(&cfg, Access::Loopback).is_ok(), rtsp == "127.0.0.1");
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
            assert!(
                prepare_with(
                    &path,
                    &temp.path().join(index.to_string()),
                    files,
                    Access::Loopback
                )
                .is_err()
            );
        }
    }
}
