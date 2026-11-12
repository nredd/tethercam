//! Session description (SDP, RFC 4566) for the single media track the server publishes.
//!
//! References:
//! - [RFC 4566](https://www.rfc-editor.org/rfc/rfc4566)
//! - [RFC 2326 appendix C](https://www.rfc-editor.org/rfc/rfc2326#appendix-C)

use std::fmt::Write as _;
use std::net::IpAddr;

use thiserror::Error;

/// Kind of media carried by a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    /// An `m=video` section.
    Video,
    /// An `m=audio` section.
    Audio,
}

/// Description of the one track a [`MediaSource`](super::MediaSource) produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInfo {
    /// Media kind.
    pub kind: MediaKind,
    /// RTP payload type, 0 to 127.
    pub payload_type: u8,
    /// Encoding name for `a=rtpmap`, e.g. `H264` or `JPEG`.
    pub codec: String,
    /// RTP clock rate in Hz, e.g. 90000.
    pub clock_rate: u32,
    /// Format parameters for `a=fmtp`, e.g. `packetization-mode=1;sprop-parameter-sets=...`.
    pub fmtp: Option<String>,
    /// Relative control URI segment used by `SETUP`, e.g. `trackID=0`.
    pub control: String,
}

/// A [`TrackInfo`] that cannot be rendered into valid SDP or a valid `SETUP` URI.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid track: {0}")]
pub struct SdpError(pub &'static str);

impl TrackInfo {
    /// Check every field that ends up in SDP or a URI.
    ///
    /// # Errors
    ///
    /// [`SdpError`] naming the offending field.
    pub fn validate(&self) -> Result<(), SdpError> {
        if self.payload_type > 127 {
            return Err(SdpError("payload type above 127"));
        }
        if self.clock_rate == 0 {
            return Err(SdpError("clock rate is zero"));
        }
        let codec_ok = !self.codec.is_empty()
            && self
                .codec
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
        if !codec_ok {
            return Err(SdpError("codec must be alphanumeric"));
        }
        if self
            .fmtp
            .as_deref()
            .is_some_and(|f| f.chars().any(char::is_control))
        {
            return Err(SdpError("fmtp contains control characters"));
        }
        let control_ok = !self.control.is_empty()
            && self
                .control
                .bytes()
                .all(|b| b.is_ascii_graphic() && !matches!(b, b'/' | b'?' | b'#'));
        if !control_ok {
            return Err(SdpError("control must be a single URI segment"));
        }
        Ok(())
    }

    /// Render the SDP body. `origin` is the server's own address for the `o=` line.
    ///
    /// # Errors
    ///
    /// [`SdpError`] when [`TrackInfo::validate`] fails.
    pub fn to_sdp(&self, origin: IpAddr, session_name: &str) -> Result<String, SdpError> {
        self.validate()?;
        let (family, any) = match origin {
            IpAddr::V4(_) => ("IP4", "0.0.0.0"),
            IpAddr::V6(_) => ("IP6", "::"),
        };
        let media = match self.kind {
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
        };
        let name: String = session_name.chars().filter(|c| !c.is_control()).collect();
        let name = if name.is_empty() { "-" } else { &name };
        let pt = self.payload_type;
        let mut sdp = String::new();
        // Writing to a `String` cannot fail.
        let _ = write!(
            sdp,
            "v=0\r\no=- 0 0 IN {family} {origin}\r\ns={name}\r\nc=IN {family} {any}\r\nt=0 0\r\n\
             a=control:*\r\nm={media} 0 RTP/AVP {pt}\r\na=rtpmap:{pt} {}/{}\r\n",
            self.codec, self.clock_rate
        );
        if let Some(fmtp) = &self.fmtp {
            let _ = write!(sdp, "a=fmtp:{pt} {fmtp}\r\n");
        }
        let _ = write!(sdp, "a=control:{}\r\n", self.control);
        Ok(sdp)
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    fn track() -> TrackInfo {
        TrackInfo {
            kind: MediaKind::Video,
            payload_type: 96,
            codec: "H264".into(),
            clock_rate: 90_000,
            fmtp: Some("packetization-mode=1;profile-level-id=42e01f".into()),
            control: "trackID=0".into(),
        }
    }

    #[test]
    fn renders_h264_sdp() {
        let sdp = track()
            .to_sdp(Ipv4Addr::new(192, 0, 2, 1).into(), "tethercam")
            .unwrap();
        assert_eq!(
            sdp,
            "v=0\r\no=- 0 0 IN IP4 192.0.2.1\r\ns=tethercam\r\nc=IN IP4 0.0.0.0\r\nt=0 0\r\n\
             a=control:*\r\nm=video 0 RTP/AVP 96\r\na=rtpmap:96 H264/90000\r\n\
             a=fmtp:96 packetization-mode=1;profile-level-id=42e01f\r\na=control:trackID=0\r\n"
        );
    }

    #[test]
    fn renders_ipv6_and_no_fmtp() {
        let mut t = track();
        t.fmtp = None;
        t.kind = MediaKind::Audio;
        let sdp = t.to_sdp(Ipv6Addr::LOCALHOST.into(), "").unwrap();
        assert!(sdp.contains("IN IP6 ::1") && sdp.contains("c=IN IP6 ::\r\n"));
        assert!(sdp.contains("s=-\r\n") && sdp.contains("m=audio") && !sdp.contains("fmtp"));
    }

    #[test]
    fn rejects_unrenderable_tracks() {
        let cases: [fn(&mut TrackInfo); 6] = [
            |t| t.payload_type = 128,
            |t| t.clock_rate = 0,
            |t| t.codec = "H 264\r\n".into(),
            |t| t.codec.clear(),
            |t| t.fmtp = Some("a\r\nb".into()),
            |t| t.control = "a/b".into(),
        ];
        for mutate in cases {
            let mut t = track();
            mutate(&mut t);
            assert!(t.validate().is_err(), "{t:?}");
            assert!(t.to_sdp(Ipv4Addr::LOCALHOST.into(), "x").is_err());
        }
        let mut t = track();
        t.control.clear();
        assert!(t.validate().is_err());
    }
}
