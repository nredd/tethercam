//! Listener overlap validation regressions.
//!
//! References:
//! - <https://doc.rust-lang.org/std/net/enum.IpAddr.html>

use super::Config;
use super::tests::validation;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[test]
fn listener_overlaps_are_rejected_per_address_family() {
    let base = Config::generate().unwrap();
    let a = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
    let b = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2));
    let wild4 = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    let wild6 = IpAddr::V6(Ipv6Addr::UNSPECIFIED);
    let a6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
    for (rtsp, onvif) in [(a, a), (wild4, a), (a, wild4), (wild6, a6), (a6, wild6)] {
        let config = Config {
            rtsp_address: rtsp,
            onvif_address: onvif,
            rtsp_port: 1234,
            onvif_port: 1234,
            ..base.clone()
        };
        let (key, value) = validation(config.validate().unwrap_err());
        for expected in ["rtsp_address", "rtsp_port", "onvif_address", "onvif_port"] {
            assert!(key.contains(expected));
        }
        assert!(value.contains(&rtsp.to_string()));
        assert!(value.contains(&onvif.to_string()));
        assert!(value.contains("1234"));
    }
    for (rtsp, onvif) in [(a, b), (wild4, wild6)] {
        let config = Config {
            rtsp_address: rtsp,
            onvif_address: onvif,
            rtsp_port: 1234,
            onvif_port: 1234,
            ..base.clone()
        };
        config.validate().unwrap();
    }
}
