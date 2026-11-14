//! Configuration and credential storage.
//!
//! References:
//! - [TOML](https://toml.io/)

#[cfg(test)]
mod concurrency_tests;
#[cfg(test)]
mod diagnostic_tests;
#[cfg(test)]
mod durability_tests;
#[cfg(test)]
mod listener_tests;
#[cfg(test)]
mod tests;

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
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
        constant_time_eq::constant_time_eq(self.0.as_bytes(), other.0.as_bytes())
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
    /// Serve ONVIF snapshots as full-resolution stills instead of the newest live-view frame.
    /// Each snapshot releases the shutter and writes to the card; off by default.
    #[serde(default)]
    pub full_resolution_snapshot: bool,
}

/// How the camera is reached. Selected explicitly; there is never an automatic fallback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CameraBackend {
    /// Raw USB through `nusb`: the default, and the only backend off macOS.
    #[default]
    Usb,
    /// Experimental macOS `ImageCaptureCore` (`docs/macos-camera.md`): needs a build with the
    /// `macos-icc` feature. Selects `camera_id`, else the one camera it discovers.
    Icc,
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
    /// Camera access path. Defaults to raw USB when absent.
    #[serde(default)]
    pub camera_backend: CameraBackend,
    /// Optional USB serial; absent selects a camera automatically. Raw USB only: `icc` rejects a
    /// serial rather than ignore it, and selects by `camera_id`.
    pub camera_serial: Option<String>,
    /// Optional exact `ImageCaptureCore` device identifier, as printed by `tethercam cameras`.
    /// `icc` only: with it set, other attached devices are ignored and an absent device waits,
    /// never falling back. Absent selects the sole discovered camera. Redacted when formatted.
    #[serde(default)]
    pub camera_id: Option<Secret>,
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

/// Longest `camera_id` in UTF-8 bytes, the framework identifier bound
/// (`transport::icc::MAX_ID_BYTES`, which the `icc` feature gate keeps this module from naming; a test
/// pins them together).
pub(crate) const MAX_CAMERA_ID_BYTES: usize = 1024;

/// An exact identifier: nonempty, bounded, no padding (a copy-paste slip would otherwise wait
/// forever for a camera that cannot match) and no control characters, NUL included.
fn valid_camera_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_CAMERA_ID_BYTES
        && id == id.trim()
        && !id.chars().any(char::is_control)
}

fn random_hex(
    bytes: usize, fill: &mut dyn FnMut(&mut [u8]) -> Result<(), getrandom::Error>,
) -> Result<String, Error> {
    let mut random = Zeroizing::new(vec![0_u8; bytes]);
    fill(&mut random).map_err(|_| Error::Random)?;
    let mut result = String::with_capacity(bytes * 2);
    for byte in random.iter() {
        for nibble in [byte >> 4, byte & 0x0f] {
            result.push(char::from(if nibble < 10 {
                b'0' + nibble
            } else {
                b'a' + nibble - 10
            }));
        }
    }
    Ok(result)
}

impl Config {
    /// Generate defaults with fresh credentials and identity.
    ///
    /// # Errors
    /// Returns an error if the operating system cannot provide randomness.
    pub fn generate() -> Result<Self, Error> {
        Self::generate_with(&mut getrandom::fill)
    }

    fn generate_with(
        fill: &mut dyn FnMut(&mut [u8]) -> Result<(), getrandom::Error>,
    ) -> Result<Self, Error> {
        let mut id = [0_u8; 16];
        fill(&mut id).map_err(|_| Error::Random)?;
        let id = uuid::Builder::from_random_bytes(id).into_uuid();
        Ok(Self {
            rtsp_address: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            rtsp_port: 8554,
            onvif_address: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            onvif_port: 8080,
            interface: None,
            camera_backend: CameraBackend::Usb,
            camera_serial: None,
            camera_id: None,
            device_name: "tethercam".into(),
            log_level: "info".into(),
            endpoint_uuid: id,
            streams: Streams {
                frame_rate: 15,
                bitrate: 2_000_000,
                h264: true,
                mjpeg: true,
                full_resolution_snapshot: false,
            },
            credentials: Credentials {
                username: Secret::new(random_hex(16, fill)?),
                password: Secret::new(random_hex(32, fill)?),
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
        let same_family = self.rtsp_address.is_ipv4() == self.onvif_address.is_ipv4();
        let addresses_overlap = same_family
            && (self.rtsp_address == self.onvif_address
                || self.rtsp_address.is_unspecified()
                || self.onvif_address.is_unspecified());
        check(
            self.rtsp_port != self.onvif_port || !addresses_overlap,
            "rtsp_address, rtsp_port, onvif_address, onvif_port",
            &format!(
                "{}:{} and {}:{}",
                self.rtsp_address, self.rtsp_port, self.onvif_address, self.onvif_port
            ),
        )?;
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
            self.camera_backend == CameraBackend::Usb || self.camera_serial.is_none(),
            "camera_serial",
            &"unused with camera_backend = \"icc\"",
        )?;
        if let Some(id) = &self.camera_id {
            // Never echo the identifier: it names a private device.
            check(
                valid_camera_id(id.expose()),
                "camera_id",
                &"empty, padded, over 1024 bytes or containing control characters",
            )?;
            check(
                self.camera_backend == CameraBackend::Icc,
                "camera_id",
                &"unused with camera_backend = \"usb\"",
            )?;
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
        crate::rtsp::digest::Credentials::validate(
            self.credentials.username.expose(),
            self.credentials.password.expose(),
            "tethercam",
        )
        .map_err(|_| Error::Validation {
            key: "credentials",
            value: "[REDACTED]".into(),
        })?;
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
                // "missing field" names a key from our own schema. "unknown field" echoes what
                // the user typed, which under `credentials` may be a misplaced secret, so it
                // is reported there without the name. Anything else may quote a value.
                let message = error.inner().message();
                if message.starts_with("unknown field") && key.starts_with("credentials") {
                    // The path ends in the typed name too, so report the table only.
                    return Error::Parse(
                        "invalid `credentials`: unknown key; expected `username` and `password`"
                            .into(),
                    );
                }
                let structural =
                    message.starts_with("missing field") || message.starts_with("unknown field");
                let detail = if structural { message } else { "[REDACTED]" };
                Error::Parse(format!("invalid `{key}`: {detail}"))
            } else {
                let value = error
                    .inner()
                    .span()
                    .and_then(|span| contents.get(span))
                    .unwrap_or("[unavailable]");
                Error::Parse(format!(
                    "invalid `{key}`: {value}: {}",
                    error.inner().message()
                ))
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
        Self::load_or_create_with(
            path,
            &mut Self::load,
            &mut Self::generate,
            &mut Self::create,
            &mut sync_directory,
        )
    }

    fn load_or_create_with(
        path: &Path, load: &mut dyn FnMut(&Path) -> Result<Self, Error>,
        generate: &mut dyn FnMut() -> Result<Self, Error>,
        create: &mut dyn FnMut(&Self, &Path) -> Result<(), Error>,
        sync: &mut dyn FnMut(&Path) -> io::Result<()>,
    ) -> Result<Self, Error> {
        match load(path) {
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                let config = generate()?;
                match create(&config, path) {
                    Ok(()) => Ok(config),
                    Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
                        let config = load(path)?;
                        sync(config_parent(path))?;
                        Ok(config)
                    }
                    Err(error) => Err(error),
                }
            }
            Ok(config) => {
                sync(config_parent(path))?;
                Ok(config)
            }
            Err(error) => Err(error),
        }
    }
    /// Atomically create a new configuration without overwriting an existing one.
    ///
    /// # Errors
    /// Returns an error if validation or file creation fails.
    pub fn create(&self, path: &Path) -> Result<(), Error> {
        self.create_with_sync(path, &mut sync_directory)
    }

    fn create_with_sync(
        &self, path: &Path, sync: &mut dyn FnMut(&Path) -> io::Result<()>,
    ) -> Result<(), Error> {
        self.create_with_storage(path, sync, cfg!(unix), &mut FileStorage)
    }

    fn create_with_storage(
        &self, path: &Path, sync: &mut dyn FnMut(&Path) -> io::Result<()>, unix: bool,
        storage: &mut dyn Storage,
    ) -> Result<(), Error> {
        self.validate()?;
        check_supported_permissions(unix)?;
        let parent = config_parent(path);
        create_directories(parent, sync)?;
        let mut temp = storage.new_file(parent)?;
        let contents = Zeroizing::new(storage.serialize(self).map_err(|_| Error::Serialize)?);
        storage.write(&mut temp, contents.as_bytes())?;
        storage.sync(&temp)?;
        temp.persist_noclobber(path)
            .map_err(|e| Error::Io(e.error))?;
        sync(parent)?;
        Ok(())
    }
}

trait Storage {
    fn new_file(&mut self, parent: &Path) -> io::Result<tempfile::NamedTempFile>;
    fn serialize(&mut self, config: &Config) -> Result<String, toml::ser::Error>;
    fn write(&mut self, file: &mut tempfile::NamedTempFile, bytes: &[u8]) -> io::Result<()>;
    fn sync(&mut self, file: &tempfile::NamedTempFile) -> io::Result<()>;
}

struct FileStorage;

impl Storage for FileStorage {
    fn new_file(&mut self, parent: &Path) -> io::Result<tempfile::NamedTempFile> {
        tempfile::NamedTempFile::new_in(parent)
    }
    fn serialize(&mut self, config: &Config) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(config)
    }
    fn write(&mut self, file: &mut tempfile::NamedTempFile, bytes: &[u8]) -> io::Result<()> {
        file.write_all(bytes)
    }
    fn sync(&mut self, file: &tempfile::NamedTempFile) -> io::Result<()> {
        file.as_file().sync_all()
    }
}

fn check_supported_permissions(unix: bool) -> Result<(), Error> {
    if unix {
        Ok(())
    } else {
        Err(Error::Permissions)
    }
}

fn config_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn create_directories(
    path: &Path, sync: &mut dyn FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if path.is_dir() {
        return sync(parent);
    }
    create_directories(parent, sync)?;
    match fs::create_dir(path) {
        Ok(()) => sync(parent),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => sync(parent),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn check_permissions(file: &File) -> Result<(), Error> {
    check_permission_metadata(&mut || file.metadata())
}
#[cfg(unix)]
fn check_permission_metadata(
    metadata: &mut dyn FnMut() -> io::Result<fs::Metadata>,
) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    if metadata()?.permissions().mode() & 0o077 != 0 {
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
