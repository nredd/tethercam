//! Synthetic host diagnostics; no physical USB access or daemon installation.
//!
//! References:
//! - `docs/architecture.md`, CLI and supervisor contract

#[cfg(unix)]
#[tokio::test]
async fn signal_registration_failures_are_reported_before_serving() {
    use super::diagnostics::Signals;
    use tokio::signal::unix::{SignalKind, signal};
    assert!(
        Signals::from_signals(
            Err(std::io::Error::other("synthetic")),
            signal(SignalKind::terminate())
        )
        .is_err()
    );
    assert!(
        Signals::from_signals(
            signal(SignalKind::interrupt()),
            Err(std::io::Error::other("synthetic"))
        )
        .is_err()
    );
}

use super::Error;
use super::diagnostics::*;
use crate::config::Config;
use std::mem::discriminant as kind;
use std::net::TcpListener;

#[test]
fn levels_and_clock_fail_closed() {
    let cfg = Config::generate().unwrap();
    assert!(
        local_discovery(
            &cfg,
            Err(std::io::Error::other(
                "synthetic interface enumeration failure"
            ))
        )
        .is_err()
    );
    for (name, level) in [
        ("error", tracing::Level::ERROR),
        ("warn", tracing::Level::WARN),
        ("info", tracing::Level::INFO),
        ("debug", tracing::Level::DEBUG),
        ("trace", tracing::Level::TRACE),
    ] {
        assert_eq!(super::diagnostics::level(name), level);
    }
    let mut cfg = Config::generate().unwrap();
    cfg.rtsp_address = "127.0.0.1".parse().unwrap();
    cfg.onvif_address = cfg.rtsp_address;
    assert_eq!(kind(&check_host(&cfg, 0).unwrap_err()), kind(&Error::Clock));
    assert_eq!(
        kind(&check_host(&cfg, i64::MAX).unwrap_err()),
        kind(&Error::Clock)
    );
    let first = TcpListener::bind("127.0.0.1:0").unwrap();
    let second = TcpListener::bind("127.0.0.1:0").unwrap();
    cfg.rtsp_port = first.local_addr().unwrap().port();
    cfg.onvif_port = second.local_addr().unwrap().port();
    assert_eq!(
        kind(&check_host(&cfg, 1_900_000_000).unwrap_err()),
        kind(&Error::Ports)
    );
    drop(first);
    assert_eq!(
        kind(&check_host(&cfg, 1_900_000_000).unwrap_err()),
        kind(&Error::Ports)
    );
    drop(second);
    check_host(&cfg, 1_900_000_000).unwrap();
}

fn interface(name: &str, ip: &str, up: bool) -> if_addrs::Interface {
    let address: std::net::IpAddr = ip.parse().unwrap();
    let addr = match address {
        std::net::IpAddr::V4(ip) => if_addrs::IfAddr::V4(if_addrs::Ifv4Addr {
            ip,
            prefixlen: 24,
            netmask: "255.255.255.0".parse().unwrap(),
            broadcast: None,
        }),
        std::net::IpAddr::V6(ip) => if_addrs::IfAddr::V6(if_addrs::Ifv6Addr {
            ip,
            prefixlen: 64,
            netmask: "ffff:ffff:ffff:ffff::".parse().unwrap(),
            broadcast: None,
        }),
    };
    if_addrs::Interface {
        name: name.into(),
        addr,
        index: Some(1),
        oper_status: if up {
            if_addrs::IfOperStatus::Up
        } else {
            if_addrs::IfOperStatus::Down
        },
        is_p2p: false,
        #[cfg(windows)]
        adapter_name: "synthetic".into(),
    }
}

#[test]
fn unnamed_discovery_skips_loopback_and_ipv4_link_local_unless_asked_for() {
    let mut cfg = Config::generate().unwrap();
    let interfaces = vec![
        interface("lo", "127.0.0.1", true),
        interface("ll", "169.254.7.7", true),
        interface("test0", "192.0.2.1", true),
        interface("lo6", "::1", true),
    ];
    let addrs = |cfg: &Config| {
        discovery(cfg, interfaces.clone())
            .unwrap()
            .interfaces()
            .iter()
            .map(|i| i.addr.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(addrs(&cfg), ["192.0.2.1"]);
    cfg.interface = Some("lo".into());
    assert_eq!(addrs(&cfg), ["127.0.0.1"]);
    cfg.interface = Some("ll".into());
    assert_eq!(addrs(&cfg), ["169.254.7.7"]);
    cfg.interface = None;
    cfg.onvif_address = "127.0.0.1".parse().unwrap();
    assert_eq!(addrs(&cfg), ["127.0.0.1"]);
    cfg.onvif_address = "::1".parse().unwrap();
    assert_eq!(addrs(&cfg), ["::1"]);
    cfg.onvif_address = "::".parse().unwrap();
    let mut v6 = interfaces.clone();
    v6.push(interface("v6", "2001:db8::1", true));
    assert_eq!(
        discovery(&cfg, v6)
            .unwrap()
            .interfaces()
            .iter()
            .map(|i| i.addr.to_string())
            .collect::<Vec<_>>(),
        ["2001:db8::1"]
    );
    let only_unusable = vec![interface("lo", "127.0.0.1", true)];
    cfg.onvif_address = "0.0.0.0".parse().unwrap();
    assert_eq!(
        kind(&discovery(&cfg, only_unusable).unwrap_err()),
        kind(&Error::Interface)
    );
}

#[test]
fn discovery_uses_only_selected_local_interfaces_and_stored_identity() {
    let mut cfg = Config::generate().unwrap();
    let interfaces = vec![
        interface("test0", "192.0.2.1", true),
        interface("test1", "192.0.2.2", true),
        interface("lo0", "127.0.0.1", true),
        interface("down", "192.0.2.3", false),
        interface("v6", "2001:db8::1", true),
    ];
    let result = discovery(&cfg, interfaces.clone()).unwrap();
    assert_eq!(result.interfaces().len(), 2);
    cfg.interface = Some("test1".into());
    assert_eq!(
        discovery(&cfg, interfaces.clone()).unwrap().interfaces()[0]
            .addr
            .to_string(),
        "192.0.2.2"
    );
    cfg.interface = None;
    cfg.onvif_address = "192.0.2.1".parse().unwrap();
    assert_eq!(
        discovery(&cfg, interfaces.clone())
            .unwrap()
            .interfaces()
            .len(),
        1
    );
    cfg.onvif_address = "2001:db8::1".parse().unwrap();
    assert_eq!(
        discovery(&cfg, interfaces.clone())
            .unwrap()
            .interfaces()
            .len(),
        1
    );
    cfg.interface = Some("absent".into());
    assert_eq!(
        kind(&discovery(&cfg, interfaces).unwrap_err()),
        kind(&Error::Interface)
    );
}
