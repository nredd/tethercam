//! `Transport` header parsing and formatting for `SETUP` (RFC 2326 section 12.39).
//!
//! Two lower transports are supported, both unicast: RTP over the RTSP TCP connection
//! (`RTP/AVP/TCP;interleaved=a-b`) and RTP over UDP (`RTP/AVP[/UDP];client_port=a-b`).
//!
//! References:
//! - [RFC 2326 section 12.39](https://www.rfc-editor.org/rfc/rfc2326#section-12.39)
//! - [RFC 2326 section 10.12](https://www.rfc-editor.org/rfc/rfc2326#section-10.12)

use thiserror::Error;

/// Most comma-separated alternatives examined in one header.
const MAX_SPECS: usize = 8;

/// A transport the client asked for and the server can provide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportSpec {
    /// RTP and RTCP interleaved on the control connection.
    Tcp {
        /// Channel carrying RTP.
        rtp_channel: u8,
        /// Channel carrying RTCP.
        rtcp_channel: u8,
    },
    /// RTP and RTCP over UDP to the client's ports.
    Udp {
        /// Client RTP port.
        rtp_port: u16,
        /// Client RTCP port.
        rtcp_port: u16,
    },
}

/// No alternative in the `Transport` header is acceptable (answer `461`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("no acceptable transport")]
pub struct NoAcceptableTransport;

/// Pick the first supported alternative from a `Transport` header value.
///
/// # Errors
///
/// [`NoAcceptableTransport`] when every alternative is multicast, a recording mode, malformed
/// or missing the parameters its lower transport needs.
pub fn parse_transport(header: &str) -> Result<TransportSpec, NoAcceptableTransport> {
    header
        .split(',')
        .take(MAX_SPECS)
        .find_map(parse_spec)
        .ok_or(NoAcceptableTransport)
}

fn parse_pair<T: std::str::FromStr + Copy>(
    value: &str, next: impl Fn(T) -> Option<T>,
) -> Option<(T, T)> {
    if let Some((a, b)) = value.split_once('-') {
        return Some((a.trim().parse().ok()?, b.trim().parse().ok()?));
    }
    let a: T = value.trim().parse().ok()?;
    Some((a, next(a)?))
}

fn parse_spec(spec: &str) -> Option<TransportSpec> {
    let (protocol, params) = spec.split_once(';').unwrap_or((spec, ""));
    let protocol = protocol.trim();
    let tcp = if protocol.eq_ignore_ascii_case("RTP/AVP/TCP") {
        true
    } else if protocol.eq_ignore_ascii_case("RTP/AVP")
        || protocol.eq_ignore_ascii_case("RTP/AVP/UDP")
    {
        false
    } else {
        return None;
    };
    let (mut interleaved, mut client_port) = (None, None);
    for param in params.split(';').map(str::trim) {
        let (key, value) = param.split_once('=').unwrap_or((param, ""));
        match key.to_ascii_lowercase().as_str() {
            "multicast" => return None,
            "mode" if !value.trim_matches('"').eq_ignore_ascii_case("play") => return None,
            "interleaved" => interleaved = Some(parse_pair::<u8>(value, |a| a.checked_add(1))?),
            "client_port" => client_port = Some(parse_pair::<u16>(value, |a| a.checked_add(1))?),
            _ => {}
        }
    }
    if tcp {
        let (media, control) = interleaved.unwrap_or((0, 1));
        (media != control).then_some(TransportSpec::Tcp {
            rtp_channel: media,
            rtcp_channel: control,
        })
    } else {
        let (media, control) = client_port?;
        (media != 0 && control != 0 && media != control).then_some(TransportSpec::Udp {
            rtp_port: media,
            rtcp_port: control,
        })
    }
}

/// Format the `Transport` header of a `SETUP` response. `server_ports` is required for UDP.
#[must_use]
pub fn format_transport(spec: TransportSpec, server_ports: Option<(u16, u16)>) -> String {
    match spec {
        TransportSpec::Tcp {
            rtp_channel,
            rtcp_channel,
        } => format!("RTP/AVP/TCP;unicast;interleaved={rtp_channel}-{rtcp_channel}"),
        TransportSpec::Udp {
            rtp_port,
            rtcp_port,
        } => {
            let (data, control) = server_ports.unwrap_or((0, 0));
            format!(
                "RTP/AVP/UDP;unicast;client_port={rtp_port}-{rtcp_port};server_port={data}-{control}"
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn parses_tcp_interleaved() {
        assert_eq!(
            parse_transport("RTP/AVP/TCP;unicast;interleaved=2-3"),
            Ok(TransportSpec::Tcp {
                rtp_channel: 2,
                rtcp_channel: 3
            })
        );
        assert_eq!(
            parse_transport("RTP/AVP/TCP;interleaved=4"),
            Ok(TransportSpec::Tcp {
                rtp_channel: 4,
                rtcp_channel: 5
            })
        );
        assert_eq!(
            parse_transport("rtp/avp/tcp;unicast"),
            Ok(TransportSpec::Tcp {
                rtp_channel: 0,
                rtcp_channel: 1
            })
        );
    }

    #[test]
    fn parses_udp() {
        for h in [
            "RTP/AVP;unicast;client_port=5000-5001",
            "RTP/AVP/UDP;unicast;client_port=5000-5001;mode=\"PLAY\"",
            "RTP/AVP;client_port=5000",
        ] {
            assert_eq!(
                parse_transport(h),
                Ok(TransportSpec::Udp {
                    rtp_port: 5000,
                    rtcp_port: 5001
                }),
                "{h}"
            );
        }
    }

    #[test]
    fn picks_first_acceptable_alternative() {
        assert_eq!(
            parse_transport(
                "RTP/AVP;multicast;destination=233.252.0.1, RTP/AVP/TCP;unicast;interleaved=0-1"
            ),
            Ok(TransportSpec::Tcp {
                rtp_channel: 0,
                rtcp_channel: 1
            })
        );
    }

    #[test]
    fn rejects_unacceptable() {
        for h in [
            "",
            "RTP/AVP;multicast",
            "RTP/AVP;unicast",
            "RTP/AVP;client_port=0-1",
            "RTP/AVP;client_port=7-7",
            "RTP/AVP;client_port=65535",
            "RTP/AVP/TCP;interleaved=3-3",
            "RTP/AVP/TCP;interleaved=255",
            "RTP/AVP/TCP;interleaved=a-b",
            "RTP/AVP/TCP;interleaved=1-b",
            "RTP/AVP/TCP;interleaved=bad",
            "RTP/AVP;client_port=5000-5001;mode=RECORD",
            "RAW/RAW/UDP;unicast",
        ] {
            assert_eq!(parse_transport(h), Err(NoAcceptableTransport), "{h:?}");
        }
    }

    #[test]
    fn formats_responses() {
        assert_eq!(
            format_transport(
                TransportSpec::Tcp {
                    rtp_channel: 0,
                    rtcp_channel: 1
                },
                None
            ),
            "RTP/AVP/TCP;unicast;interleaved=0-1"
        );
        assert_eq!(
            format_transport(
                TransportSpec::Udp {
                    rtp_port: 5000,
                    rtcp_port: 5001
                },
                Some((6970, 6971))
            ),
            "RTP/AVP/UDP;unicast;client_port=5000-5001;server_port=6970-6971"
        );
    }

    proptest! {
        #[test]
        fn transport_parser_never_panics(header in ".{0,300}") {
            let _ = parse_transport(&header);
        }
    }
}
