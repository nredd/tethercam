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
    // Nothing usable yet is not fatal: discovery waits for the watch to find an interface.
    let only_unusable = vec![interface("lo", "127.0.0.1", true)];
    cfg.onvif_address = "0.0.0.0".parse().unwrap();
    let idle = discovery(&cfg, only_unusable).unwrap();
    assert_eq!(idle.interfaces().len(), 0);
}

#[test]
fn point_to_point_tunnels_are_skipped_unless_named() {
    let mut tunnel = interface("utun4", "198.51.100.2", true);
    tunnel.is_p2p = true;
    let all = vec![tunnel, interface("test0", "192.0.2.1", true)];
    let mut cfg = Config::generate().unwrap();
    let served = |cfg: &Config| discovery(cfg, all.clone()).unwrap().interfaces().len();
    assert_eq!(served(&cfg), 1);
    cfg.interface = Some("utun4".into());
    assert_eq!(served(&cfg), 1);
}

static HOST: std::sync::Mutex<Option<Vec<(&str, &str)>>> = std::sync::Mutex::new(None);

fn fake_host() -> std::io::Result<Vec<if_addrs::Interface>> {
    HOST.lock()
        .unwrap()
        .as_ref()
        .map(|list| list.iter().map(|(n, ip)| interface(n, ip, true)).collect())
        .ok_or_else(|| std::io::Error::other("synthetic enumeration failure"))
}

#[test]
fn the_watch_reapplies_the_startup_selection_to_each_enumeration() {
    let cfg = Config::generate().unwrap();
    let watch = interface_watch(&cfg, fake_host);
    *HOST.lock().unwrap() = Some(vec![("lo", "127.0.0.1"), ("test0", "192.0.2.1")]);
    assert_eq!(watch.poll().unwrap().len(), 1, "loopback is still skipped");
    *HOST.lock().unwrap() = None;
    assert!(watch.poll().is_none(), "a failed enumeration keeps the set");
}

#[test]
fn discovery_announces_and_matches_device_scopes() {
    use crate::onvif::discovery::{Engine, SystemEntropy};
    use std::time::Instant;

    let cfg = Config::generate().unwrap();
    let discovery = discovery(&cfg, vec![interface("test0", "192.0.2.1", true)]).unwrap();
    let scopes = [
        "onvif://www.onvif.org/Profile/Streaming",
        "onvif://www.onvif.org/type/video_encoder",
        "onvif://www.onvif.org/name/USB%20camera%20bridge",
        "onvif://www.onvif.org/hardware/tethercam",
    ];
    let model = crate::supervisor::model(&cfg, 640, 480).unwrap();
    let ctx = crate::onvif::service::Context {
        model: &model,
        local: "192.0.2.1:8080".parse().unwrap(),
        now: 0,
        snapshot: true,
        authenticated: true,
    };
    let op = crate::onvif::soap::Element {
        namespace: Some(crate::onvif::soap::NS_DEVICE.into()),
        name: "GetScopes".into(),
        text: String::new(),
        attributes: vec![],
        children: vec![],
    };
    let response = crate::onvif::device::handle(&op, &ctx).unwrap();
    for scope in scopes {
        assert!(response.contains(scope));
    }
    assert_eq!(response.matches("<tt:ScopeItem>").count(), scopes.len());
    let mut engine = Engine::new(discovery, SystemEntropy::default());
    let now = Instant::now();
    for hello in engine.hello(now) {
        let xml = String::from_utf8(hello.payload).unwrap();
        for scope in scopes {
            assert!(xml.contains(scope), "missing {scope} in {xml}");
        }
    }
    for (n, scope) in scopes.iter().enumerate() {
        let probe = format!(
            r#"<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope" xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery"><e:Header><w:MessageID>uuid:11111111-2222-3333-4444-{n:012}</w:MessageID><w:To>urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To><w:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action></e:Header><e:Body><d:Probe><d:Scopes>{scope}</d:Scopes></d:Probe></e:Body></e:Envelope>"#
        );
        let response = engine
            .handle(now, "192.0.2.2:1234".parse().unwrap(), probe.as_bytes())
            .unwrap();
        let xml = String::from_utf8(response.payload).unwrap();
        for expected in scopes {
            assert!(xml.contains(expected));
        }
    }
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
    let none = discovery(&cfg, interfaces).unwrap();
    assert_eq!(none.interfaces().len(), 0);
    // More addresses than the responder serves is still a startup error.
    cfg.interface = None;
    cfg.onvif_address = "0.0.0.0".parse().unwrap();
    let crowded = (1..=9)
        .map(|n| interface("test0", &format!("192.0.2.{n}"), true))
        .collect();
    assert_eq!(
        kind(&discovery(&cfg, crowded).unwrap_err()),
        kind(&Error::Interface)
    );
}
