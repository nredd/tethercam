//! Caller-supplied description of the device and its streams.
//!
//! Nothing in the `onvif` module tree knows about any particular camera: the supervisor builds
//! these values from whatever the connected body reports and hands them in.
//!
//! References:
//! - [ONVIF Media Service Specification](https://www.onvif.org/specs/srv/media/ONVIF-Media-Service-Spec.pdf)
//! - [ONVIF Core Specification](https://www.onvif.org/specs/core/ONVIF-Core-Specification.pdf)

use thiserror::Error;

/// Upper bound on advertised profiles.
pub const MAX_PROFILES: usize = 16;

/// Video codec of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    /// H.264 at the given profile.
    H264(H264Profile),
    /// Motion JPEG.
    Jpeg,
}

/// H.264 profile advertised in the encoder configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264Profile {
    /// Baseline.
    Baseline,
    /// Main.
    Main,
    /// High.
    High,
}

impl H264Profile {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "Baseline",
            Self::Main => "Main",
            Self::High => "High",
        }
    }
}

/// One media profile: a stream a client can ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamProfile {
    /// Display name.
    pub name: String,
    /// Profile token, `[A-Za-z0-9_.-]`, unique. Used in snapshot URLs.
    pub token: String,
    /// Codec.
    pub codec: VideoCodec,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Frames per second.
    pub frame_rate: u32,
    /// Bitrate limit in kbit/s.
    pub bitrate_kbps: u32,
    /// RTSP path on this host, starting with `/`, no whitespace.
    pub rtsp_path: String,
}

/// Network interface reported by `GetNetworkInterfaces`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkInterface {
    /// Interface name and token, e.g. `eth0`.
    pub name: String,
    /// Hardware address as six colon-separated hex pairs.
    pub hw_address: String,
}

/// Identity of the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDescription {
    /// Manufacturer.
    pub manufacturer: String,
    /// Model.
    pub model: String,
    /// Firmware version.
    pub firmware_version: String,
    /// Serial number.
    pub serial_number: String,
    /// Hardware id.
    pub hardware_id: String,
    /// Hostname.
    pub hostname: String,
    /// The advertised network interface.
    pub interface: NetworkInterface,
}

/// A validated device and profile set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceModel {
    device: DeviceDescription,
    profiles: Vec<StreamProfile>,
    rtsp_port: u16,
}

/// Why a description was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ModelError {
    /// No profiles, or more than [`MAX_PROFILES`].
    #[error("between 1 and {MAX_PROFILES} profiles are required")]
    ProfileCount,
    /// Two profiles share a token.
    #[error("duplicate profile token `{0}`")]
    DuplicateToken(String),
    /// A token is empty, too long or has characters outside `[A-Za-z0-9_.-]`.
    #[error("invalid profile token `{0}`")]
    BadToken(String),
    /// A resolution, frame rate or bitrate is zero.
    #[error("profile `{0}` has a zero dimension, frame rate or bitrate")]
    ZeroValue(String),
    /// An RTSP path does not start with `/` or contains whitespace or control characters.
    #[error("profile `{0}` has an invalid RTSP path")]
    BadPath(String),
    /// A required identity field is empty.
    #[error("device field `{0}` is empty")]
    EmptyField(&'static str),
    /// The hardware address is not six colon-separated hex pairs.
    #[error("invalid hardware address")]
    BadHwAddress,
}

fn token_ok(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 64
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn mac_ok(m: &str) -> bool {
    let mut n = 0;
    for part in m.split(':') {
        n += 1;
        if part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
    }
    n == 6
}

impl DeviceModel {
    /// Validate and build.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError`] when profiles or identity fields are unusable.
    pub fn new(
        device: DeviceDescription, profiles: Vec<StreamProfile>, rtsp_port: u16,
    ) -> Result<Self, ModelError> {
        if profiles.is_empty() || profiles.len() > MAX_PROFILES {
            return Err(ModelError::ProfileCount);
        }
        for (name, value) in [
            ("manufacturer", &device.manufacturer),
            ("model", &device.model),
            ("firmware_version", &device.firmware_version),
            ("serial_number", &device.serial_number),
            ("hardware_id", &device.hardware_id),
            ("hostname", &device.hostname),
            ("interface.name", &device.interface.name),
        ] {
            if value.trim().is_empty() {
                return Err(ModelError::EmptyField(name));
            }
        }
        if !mac_ok(&device.interface.hw_address) {
            return Err(ModelError::BadHwAddress);
        }
        for (i, p) in profiles.iter().enumerate() {
            if !token_ok(&p.token) {
                return Err(ModelError::BadToken(p.token.clone()));
            }
            if profiles.iter().take(i).any(|q| q.token == p.token) {
                return Err(ModelError::DuplicateToken(p.token.clone()));
            }
            if p.width == 0 || p.height == 0 || p.frame_rate == 0 || p.bitrate_kbps == 0 {
                return Err(ModelError::ZeroValue(p.token.clone()));
            }
            if !p.rtsp_path.starts_with('/')
                || p.rtsp_path
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control())
            {
                return Err(ModelError::BadPath(p.token.clone()));
            }
        }
        Ok(Self {
            device,
            profiles,
            rtsp_port,
        })
    }

    /// Device identity.
    #[must_use]
    pub fn device(&self) -> &DeviceDescription {
        &self.device
    }

    /// Profiles in advertised order.
    #[must_use]
    pub fn profiles(&self) -> &[StreamProfile] {
        &self.profiles
    }

    /// Port of the RTSP server.
    #[must_use]
    pub fn rtsp_port(&self) -> u16 {
        self.rtsp_port
    }

    /// Profile by token.
    #[must_use]
    pub fn profile(&self, token: &str) -> Option<&StreamProfile> {
        self.profiles.iter().find(|p| p.token == token)
    }

    /// The video source resolution and frame rate: the maximum over all profiles.
    #[must_use]
    pub fn source_format(&self) -> (u32, u32, u32) {
        self.profiles.iter().fold((0, 0, 0), |(w, h, f), p| {
            (w.max(p.width), h.max(p.height), f.max(p.frame_rate))
        })
    }
}

/// A synthetic description for tests, benches and examples. Values are documentation-range.
#[must_use]
pub fn example_model() -> DeviceModel {
    let profile = |token: &str, codec, width, height, path: &str| StreamProfile {
        name: token.to_owned(),
        token: token.to_owned(),
        codec,
        width,
        height,
        frame_rate: 15,
        bitrate_kbps: 2048,
        rtsp_path: path.to_owned(),
    };
    let device = DeviceDescription {
        manufacturer: "Example Maker".to_owned(),
        model: "EX-1".to_owned(),
        firmware_version: "1.0.0".to_owned(),
        serial_number: "SN0000".to_owned(),
        hardware_id: "HW-1".to_owned(),
        hostname: "example-cam".to_owned(),
        interface: NetworkInterface {
            name: "eth0".to_owned(),
            hw_address: "02:00:00:00:00:01".to_owned(),
        },
    };
    let profiles = vec![
        profile(
            "main",
            VideoCodec::H264(H264Profile::Main),
            1024,
            680,
            "/live/main",
        ),
        profile("mjpeg", VideoCodec::Jpeg, 1024, 680, "/live/mjpeg"),
    ];
    // Validity is asserted by the `example_is_valid` test.
    DeviceModel {
        device,
        profiles,
        rtsp_port: 8554,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn h264_profile_names() {
        use super::H264Profile;
        assert_eq!(H264Profile::Baseline.as_str(), "Baseline");
        assert_eq!(H264Profile::Main.as_str(), "Main");
        assert_eq!(H264Profile::High.as_str(), "High");
    }

    use super::*;

    #[test]
    fn validation_covers_independent_fields() {
        let m = example_model();
        for field in 0..3 {
            let mut profiles = m.profiles().to_vec();
            match field {
                0 => profiles[0].height = 0,
                1 => profiles[0].frame_rate = 0,
                _ => profiles[0].bitrate_kbps = 0,
            }
            assert_eq!(
                DeviceModel::new(m.device().clone(), profiles, 1),
                Err(ModelError::ZeroValue("main".into()))
            );
        }
        for path in ["/white space", "/control\u{7f}"] {
            let mut profiles = m.profiles().to_vec();
            profiles[0].rtsp_path = path.into();
            assert_eq!(
                DeviceModel::new(m.device().clone(), profiles, 1),
                Err(ModelError::BadPath("main".into()))
            );
        }
        assert!(token_ok("a_-."));
    }

    #[test]
    fn example_is_valid() {
        let m = example_model();
        assert_eq!(
            DeviceModel::new(m.device().clone(), m.profiles().to_vec(), 8554).as_ref(),
            Ok(&m)
        );
        assert_eq!(m.source_format(), (1024, 680, 15));
    }

    #[test]
    fn rejects_bad_descriptions() {
        let m = example_model();
        let build = |f: &dyn Fn(&mut DeviceDescription, &mut Vec<StreamProfile>)| {
            let mut d = m.device().clone();
            let mut p = m.profiles().to_vec();
            f(&mut d, &mut p);
            DeviceModel::new(d, p, 1)
        };
        assert_eq!(build(&|_, p| p.clear()), Err(ModelError::ProfileCount));
        assert_eq!(
            build(&|_, p| p[1].token = p[0].token.clone()),
            Err(ModelError::DuplicateToken("main".into()))
        );
        assert_eq!(
            build(&|_, p| p[0].token = "a b".into()),
            Err(ModelError::BadToken("a b".into()))
        );
        assert_eq!(
            build(&|_, p| p[0].width = 0),
            Err(ModelError::ZeroValue("main".into()))
        );
        assert_eq!(
            build(&|_, p| p[0].rtsp_path = "live".into()),
            Err(ModelError::BadPath("main".into()))
        );
        assert_eq!(
            build(&|d, _| d.manufacturer = " ".into()),
            Err(ModelError::EmptyField("manufacturer"))
        );
        assert_eq!(
            build(&|d, _| d.interface.hw_address = "zz".into()),
            Err(ModelError::BadHwAddress)
        );
    }
}
