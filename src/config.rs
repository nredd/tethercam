//! Configuration and credential storage.
//!
//! References:
//! - [TOML](https://toml.io/)

#[cfg(test)]
mod concurrency_tests;
#[cfg(test)]
mod diagnostic_tests;
#[cfg(test)]
mod tests;

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

/// Configuration failures.
#[derive(Debug, Error)]
pub enum Error {
    /// An I/O operation failed.
    #[error("configuration I/O: {0}")]
    Io(#[from] io::Error),
    /// The configuration cannot be parsed.
    #[error("invalid configuration: {0}")]
    Parse(String),
    /// A field failed validation.
    #[error("invalid `{key}`: '{value}'")]
    Validation {
        /// Name of the field.
        key: &'static str,
        /// Invalid value, redacted for credentials.
        value: String,
    },
    /// Owner-only file permissions are required.
    #[error("configuration file must not be accessible by group or others")]
    Permissions,
    /// The platform does not provide a usable configuration directory.
    #[error("cannot determine configuration directory")]
    ConfigDirectory,
    /// The system random number generator failed.
    #[error("system random number generator failed")]
    Random,
    /// Serializing the configuration failed.
    #[error("configuration serialization failed")]
    Serialize,
}

/// A secret string, redacted when formatted and cleared on drop.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Construct a secret from an owned string.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(value)
    }
    /// Expose the secret to an authentication implementation.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
    /// Compare two secrets in constant time with respect to their contents.
    #[must_use]
    pub fn constant_time_eq(&self, other: &Self) -> bool {
        bool::from(self.0.as_bytes().ct_eq(other.0.as_bytes()))
    }
}
impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}
impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Credentials shared by both network protocols.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    /// Generated username.
    pub username: Secret,
    /// Generated password.
    pub password: Secret,
}

/// Stream settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Streams {
    /// Frames per second.
    pub frame_rate: u32,
    /// H.264 bitrate in bits per second.
    pub bitrate: u32,
    /// Enable the H.264 stream.
    pub h264: bool,
    /// Enable the JPEG stream.
    pub mjpeg: bool,
}

/// Complete validated configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// RTSP bind address.
    pub rtsp_address: IpAddr,
    /// RTSP bind port.
    pub rtsp_port: u16,
    /// ONVIF bind address.
    pub onvif_address: IpAddr,
    /// ONVIF bind port.
    pub onvif_port: u16,
    /// Optional network interface name.
    pub interface: Option<String>,
    /// Optional USB serial; absent selects a camera automatically.
    pub camera_serial: Option<String>,
    /// Device name advertised to clients.
    pub device_name: String,
    /// Logging level.
    pub log_level: String,
    /// Persisted ONVIF endpoint identity.
    pub endpoint_uuid: uuid::Uuid,
    /// Stream configuration.
    pub streams: Streams,
    /// Shared protocol credentials.
    pub credentials: Credentials,
}

fn random_hex(bytes: usize) -> Result<String, Error> {
    use std::fmt::Write as _;
    let mut random = Zeroizing::new(vec![0_u8; bytes]);
    getrandom::fill(&mut random).map_err(|_| Error::Random)?;
    let mut result = String::with_capacity(bytes * 2);
    for byte in random.iter() {
        write!(&mut result, "{byte:02x}").map_err(|_| Error::Random)?;
    }
    Ok(result)
}

impl Config {
    /// Generate defaults with fresh credentials and identity.
    ///
    /// # Errors
    /// Returns an error if the operating system cannot provide randomness.
    pub fn generate() -> Result<Self, Error> {
        let mut id = [0_u8; 16];
        getrandom::fill(&mut id).map_err(|_| Error::Random)?;
        let id = uuid::Builder::from_random_bytes(id).into_uuid();
        Ok(Self {
            rtsp_address: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            rtsp_port: 8554,
            onvif_address: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            onvif_port: 8080,
            interface: None,
            camera_serial: None,
            device_name: "tethercam".into(),
            log_level: "info".into(),
            endpoint_uuid: id,
            streams: Streams {
                frame_rate: 15,
                bitrate: 2_000_000,
                h264: true,
                mjpeg: true,
            },
            credentials: Credentials {
                username: Secret::new(random_hex(16)?),
                password: Secret::new(random_hex(32)?),
            },
        })
    }
    /// Validate every configurable value.
    ///
    /// # Errors
    /// Returns the invalid key and value, with credentials redacted.
    pub fn validate(&self) -> Result<(), Error> {
        fn check(ok: bool, key: &'static str, value: &dyn ToString) -> Result<(), Error> {
            if ok {
                Ok(())
            } else {
                Err(Error::Validation {
                    key,
                    value: value.to_string(),
                })
            }
        }
        check(self.rtsp_port != 0, "rtsp_port", &self.rtsp_port)?;
        check(self.onvif_port != 0, "onvif_port", &self.onvif_port)?;
        check(
            self.streams.frame_rate > 0 && self.streams.frame_rate <= 120,
            "streams.frame_rate",
            &self.streams.frame_rate,
        )?;
        check(
            self.streams.bitrate > 0,
            "streams.bitrate",
            &self.streams.bitrate,
        )?;
        check(
            self.streams.h264 || self.streams.mjpeg,
            "streams",
            &"no streams enabled",
        )?;
        check(
            !self.device_name.trim().is_empty(),
            "device_name",
            &self.device_name,
        )?;
        check(
            ["error", "warn", "info", "debug", "trace"].contains(&self.log_level.as_str()),
            "log_level",
            &self.log_level,
        )?;
        for (key, value) in [
            ("interface", &self.interface),
            ("camera_serial", &self.camera_serial),
        ] {
            if let Some(value) = value {
                check(!value.trim().is_empty(), key, value)?;
            }
        }
        check(
            !self.credentials.username.expose().is_empty(),
            "credentials.username",
            &"[REDACTED]",
        )?;
        check(
            !self.credentials.password.expose().is_empty(),
            "credentials.password",
            &"[REDACTED]",
        )?;
        check(
            !self.endpoint_uuid.is_nil(),
            "endpoint_uuid",
            &self.endpoint_uuid,
        )?;
        Ok(())
    }
    /// Load and validate a protected file.
    ///
    /// # Errors
    /// Returns an error for I/O, unsafe permissions, parse errors or invalid values.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let mut file = File::open(path)?;
        check_permissions(&file)?;
        let mut contents = Zeroizing::new(String::new());
        file.read_to_string(&mut contents)?;
        // Do not return TOML's source excerpt: it can include credentials.
        let deserializer = toml::Deserializer::parse(&contents)
            .map_err(|_| Error::Parse("invalid TOML".into()))?;
        let config: Self = serde_path_to_error::deserialize(deserializer).map_err(|error| {
            let key = error.path().to_string();
            if key.starts_with("credentials") || key == "." {
                Error::Parse(format!("invalid `{key}`: [REDACTED]"))
            } else {
                Error::Parse(format!("invalid `{key}`: {}", error.inner().message()))
            }
        })?;
        config.validate()?;
        Ok(config)
    }
    /// Load an existing configuration or create one with fresh credentials.
    ///
    /// # Errors
    /// Returns an error if generation, creation, loading, or validation fails.
    pub fn load_or_create(path: &Path) -> Result<Self, Error> {
        match Self::load(path) {
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                let config = Self::generate()?;
                match config.create(path) {
                    Ok(()) => Ok(config),
                    Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
                        Self::load(path)
                    }
                    Err(error) => Err(error),
                }
            }
            result => result,
        }
    }
    /// Atomically create a new configuration without overwriting an existing one.
    ///
    /// # Errors
    /// Returns an error if validation or file creation fails.
    pub fn create(&self, path: &Path) -> Result<(), Error> {
        self.validate()?;
        if !cfg!(unix) {
            return Err(Error::Permissions);
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        let contents = Zeroizing::new(toml::to_string_pretty(self).map_err(|_| Error::Serialize)?);
        temp.write_all(contents.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist_noclobber(path)
            .map_err(|e| Error::Io(e.error))?;
        Ok(())
    }
}

#[cfg(unix)]
fn check_permissions(file: &File) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    if file.metadata()?.permissions().mode() & 0o077 != 0 {
        return Err(Error::Permissions);
    }
    Ok(())
}
#[cfg(not(unix))]
fn check_permissions(_file: &File) -> Result<(), Error> {
    Err(Error::Permissions)
}

/// Resolve the platform's default path without accessing the file.
///
/// # Errors
/// Returns an error if the required environment variable is absent or not absolute.
pub fn default_path() -> Result<PathBuf, Error> {
    resolve_path(
        std::env::var_os("HOME").map(PathBuf::from),
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        cfg!(target_os = "macos"),
    )
}

fn resolve_path(
    home: Option<PathBuf>, xdg: Option<PathBuf>, macos: bool,
) -> Result<PathBuf, Error> {
    let base = if macos {
        home.map(|p| p.join("Library/Application Support"))
    } else {
        xdg.filter(|p| p.is_absolute())
            .or_else(|| home.map(|p| p.join(".config")))
    };
    base.filter(|p| p.is_absolute())
        .map(|p| p.join("tethercam/config.toml"))
        .ok_or(Error::ConfigDirectory)
}
