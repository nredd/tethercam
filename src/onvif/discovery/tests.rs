//! Engine and responder tests. A manual clock and an in-memory transport stand in for time and
//! the network, so nothing here touches a socket.
//!
//! References:
//! - WS-Discovery 1.1, the protocol behavior these tests pin down

use super::message::fixtures::{PROBE_ANY, PROBE_ONVIF, RESOLVE};
use super::*;
use proptest::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::rc::Rc;

const ENDPOINT: &str = "urn:uuid:01234567-89ab-4def-8123-456789abcdef";

/// Deterministic entropy: a counter.
struct Counter(u64);

impl Entropy for Counter {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        self.0
    }
}

#[derive(Clone)]
struct ManualClock(Rc<Cell<Instant>>);

impl ManualClock {
    fn new() -> Self {
        Self(Rc::new(Cell::new(Instant::now())))
    }

    fn advance(&self, by: Duration) {
        self.0.set(self.0.get() + by);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.0.get()
    }
}

#[derive(Default)]
struct Wire {
    inbox: VecDeque<(SocketAddr, Vec<u8>)>,
    sent: Vec<(usize, SocketAddr, Vec<u8>)>,
    fail_sends: bool,
}

#[derive(Clone, Default)]
struct MemTransport(Rc<RefCell<Wire>>);

impl Transport for MemTransport {
    fn recv(&mut self, buf: &mut [u8], _timeout: Duration) -> Result<Option<Datagram>, Error> {
        let Some((source, data)) = self.0.borrow_mut().inbox.pop_front() else {
            return Ok(None);
        };
        let len = data.len().min(buf.len());
        if let (Some(dst), Some(src)) = (buf.get_mut(..len), data.get(..len)) {
            dst.copy_from_slice(src);
        }
        Ok(Some(Datagram { len, source }))
    }

    fn send(&mut self, interface: usize, dest: SocketAddr, payload: &[u8]) -> Result<(), Error> {
        let mut wire = self.0.borrow_mut();
        if wire.fail_sends {
            return Err(std::io::Error::other("unreachable").into());
        }
        wire.sent.push((interface, dest, payload.to_vec()));
        Ok(())
    }
}

fn iface(last: u8) -> Interface {
    Interface {
        addr: Ipv4Addr::new(192, 0, 2, last).into(),
        prefix_len: 24,
        index: 1,
    }
}

fn scopes() -> Vec<String> {
    vec![
        "onvif://www.onvif.org/type/video_encoder".to_owned(),
        "onvif://www.onvif.org/Profile/Streaming".to_owned(),
    ]
}

fn config_with(interfaces: Vec<Interface>) -> Config {
    Config::new(
        EndpointReference::parse(ENDPOINT).unwrap(),
        scopes(),
        interfaces,
        ServiceAddress {
            port: 8080,
            path: "/onvif/device_service".to_owned(),
        },
    )
    .unwrap()
    .with_instance_id(42)
}

fn config() -> Config {
    config_with(vec![iface(10)])
}

fn peer(last: u8) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::new(192, 0, 2, last), 40000))
}

fn engine() -> Engine<Counter> {
    Engine::new(config(), Counter(1))
}

fn with_id(doc: &str, id: &str) -> String {
    doc.replace("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", id)
}

#[test]
fn probe_gets_one_unicast_reply_with_the_arrival_interface_xaddr() {
    let mut e = Engine::new(
        config_with(vec![iface(10), {
            Interface {
                addr: Ipv4Addr::new(198, 51, 100, 10).into(),
                prefix_len: 24,
                index: 2,
            }
        }]),
        Counter(1),
    );
    let t0 = Instant::now();
    let out = e
        .handle(
            t0,
            SocketAddr::from(([198, 51, 100, 77], 5000)),
            PROBE_ONVIF.as_bytes(),
        )
        .unwrap();
    let text = String::from_utf8(out.payload).unwrap();
    assert_eq!(out.interface, 1);
    assert_eq!(out.dest, SocketAddr::from(([198, 51, 100, 77], 5000)));
    assert!(text.contains("<d:XAddrs>http://198.51.100.10:8080/onvif/device_service</d:XAddrs>"));
    assert!(!text.contains("192.0.2."));
    assert!(text.contains("<a:RelatesTo>uuid:11111111-2222-3333-4444-555555555555</a:RelatesTo>"));
    assert!(text.contains(ENDPOINT));
    assert!(text.len() <= MAX_REPLY);
}

#[test]
fn reply_is_delayed_within_the_spec_bound() {
    let mut e = engine();
    let t0 = Instant::now();
    for n in 0..50_u8 {
        // Spaced out so the global budget never interferes.
        let now = t0 + Duration::from_millis(100) * u32::from(n);
        let probe = with_id(PROBE_ANY, &format!("aaaaaaaa-bbbb-cccc-dddd-{n:012}"));
        let out = e.handle(now, peer(20 + n), probe.as_bytes()).unwrap();
        assert!(out.due >= now && out.due <= now + MAX_DELAY);
    }
}

#[test]
fn drops_with_the_right_reason() {
    let mut e = engine();
    let t0 = Instant::now();
    let big = vec![b'x'; MAX_PACKET + 1];
    assert_eq!(e.handle(t0, peer(20), &big), Err(Dropped::TooLarge));
    let zero_port = SocketAddr::from(([192, 0, 2, 20], 0));
    assert_eq!(e.handle(t0, zero_port, b"x"), Err(Dropped::BadSource));
    let multicast = SocketAddr::from(([239, 255, 255, 250], 3702));
    assert_eq!(e.handle(t0, multicast, b"x"), Err(Dropped::BadSource));
    let off_link = SocketAddr::from(([198, 51, 100, 9], 4000));
    assert_eq!(
        e.handle(t0, off_link, PROBE_ANY.as_bytes()),
        Err(Dropped::OffLink)
    );
    assert!(matches!(
        e.handle(t0, peer(21), b"junk"),
        Err(Dropped::Malformed(_))
    ));
    assert!(e.handle(t0, peer(22), PROBE_ANY.as_bytes()).is_ok());
    assert_eq!(
        e.handle(t0, peer(23), PROBE_ANY.as_bytes()),
        Err(Dropped::Duplicate)
    );
}

#[test]
fn only_matching_types_and_scopes_are_answered() {
    let mut e = engine();
    let t0 = Instant::now();
    let doc = |n: u8, probe: &str| {
        with_id(PROBE_ANY, &format!("aaaaaaaa-bbbb-cccc-dddd-{n:012}")).replace("<d:Probe/>", probe)
    };
    let cases = [
        (
            "<d:Probe><d:Scopes>onvif://www.onvif.org/Profile</d:Scopes></d:Probe>",
            true,
        ),
        (
            "<d:Probe><d:Scopes>onvif://www.onvif.org/Profile/S</d:Scopes></d:Probe>",
            false,
        ),
        (
            "<d:Probe xmlns:t=\"http://www.onvif.org/ver10/device/wsdl\"><d:Types>t:Device</d:Types></d:Probe>",
            true,
        ),
        (
            "<d:Probe xmlns:t=\"http://www.onvif.org/ver10/network/wsdl\"><d:Types>t:NetworkVideoDisplay</d:Types></d:Probe>",
            false,
        ),
    ];
    for (n, (probe, want)) in cases.into_iter().enumerate() {
        let n = u8::try_from(n).unwrap();
        let got = e.handle(t0, peer(30 + n), doc(n, probe).as_bytes());
        assert_eq!(got.is_ok(), want, "{probe}");
        if !want {
            assert_eq!(got, Err(Dropped::NoMatch));
        }
    }
}

#[test]
fn resolve_answers_only_for_our_endpoint() {
    let mut e = engine();
    let t0 = Instant::now();
    let out = e.handle(t0, peer(20), RESOLVE.as_bytes()).unwrap();
    let text = String::from_utf8(out.payload).unwrap();
    assert!(text.contains("<d:ResolveMatches>"));
    assert!(
        text.contains("<a:RelatesTo>urn:uuid:99999999-bbbb-cccc-dddd-eeeeeeeeeeee</a:RelatesTo>")
    );
    let other = RESOLVE
        .replace(
            "01234567-89ab-4def-8123-456789abcdef",
            "ffffffff-89ab-4def-8123-456789abcdef",
        )
        .replace("99999999", "88888888");
    assert_eq!(
        e.handle(t0, peer(21), other.as_bytes()),
        Err(Dropped::NoMatch)
    );
}

#[test]
fn duplicate_message_ids_are_answered_once_even_across_sources() {
    let mut e = engine();
    let t0 = Instant::now();
    assert!(e.handle(t0, peer(20), PROBE_ANY.as_bytes()).is_ok());
    assert_eq!(
        e.handle(t0, peer(20), PROBE_ANY.as_bytes()),
        Err(Dropped::Duplicate)
    );
    let later = t0 + Duration::from_secs(61);
    assert!(e.handle(later, peer(20), PROBE_ANY.as_bytes()).is_ok());
}

#[test]
fn a_source_flooding_probes_is_rate_limited() {
    let mut e = engine();
    let t0 = Instant::now();
    let mut answered = 0;
    for n in 0..40_u8 {
        let probe = with_id(PROBE_ANY, &format!("aaaaaaaa-bbbb-cccc-dddd-{n:012}"));
        match e.handle(t0, peer(20), probe.as_bytes()) {
            Ok(_) => answered += 1,
            Err(reason) => assert_eq!(reason, Dropped::RateLimited),
        }
    }
    assert_eq!(
        answered,
        usize::try_from(Limits::default().source_burst).unwrap()
    );
}

/// An engine on a /32 IPv6 documentation subnet, so thousands of distinct on-link sources exist.
fn v6_engine() -> Engine<Counter> {
    let iface = Interface {
        addr: "2001:db8::1".parse().unwrap(),
        prefix_len: 32,
        index: 1,
    };
    Engine::new(config_with(vec![iface]), Counter(1))
}

fn v6_peer(n: u32) -> SocketAddr {
    let ip = std::net::Ipv6Addr::new(
        0x2001,
        0x0db8,
        0,
        0,
        0,
        0,
        u16::try_from(n >> 16).unwrap(),
        u16::try_from(n & 0xffff).unwrap(),
    );
    SocketAddr::new(ip.into(), 40000)
}

fn unique_probe(n: u32) -> String {
    with_id(PROBE_ANY, &format!("aaaaaaaa-bbbb-cccc-dddd-{n:012}"))
}

#[test]
fn many_spoofed_sources_cannot_exceed_the_global_budget() {
    let limits = Limits::default();
    let mut e = v6_engine();
    let t0 = Instant::now();
    let seconds = 10_u32;
    let mut answered = 0_u64;
    let mut bytes = 0_u64;
    // 100 probes from 100 never-repeating sources every 100ms for ten simulated seconds.
    for tick in 0..seconds * 10 {
        let now = t0 + Duration::from_millis(100) * tick;
        for k in 0..100 {
            let n = tick * 100 + k;
            if let Ok(out) = e.handle(now, v6_peer(n), unique_probe(n).as_bytes()) {
                answered += 1;
                bytes += out.payload.len() as u64;
            }
        }
    }
    let budget = u64::from(limits.global_burst) + u64::from(limits.global_per_second * seconds);
    assert!(answered <= budget, "{answered} > {budget}");
    assert!(answered >= budget - 10, "budget refills: {answered}");
    assert!(
        bytes <= budget * MAX_REPLY as u64,
        "reflected {bytes} bytes, bound {}",
        budget * MAX_REPLY as u64
    );
    assert_eq!(
        limits.max_reflected_bytes_per_second(),
        10 * MAX_REPLY as u64
    );
}

#[test]
fn a_probe_is_answered_again_once_the_global_bucket_refills() {
    let mut e = v6_engine();
    let t0 = Instant::now();
    for n in 0..Limits::default().global_burst {
        assert!(e.handle(t0, v6_peer(n), unique_probe(n).as_bytes()).is_ok());
    }
    let legit = unique_probe(9999);
    assert_eq!(
        e.handle(t0, v6_peer(9999), legit.as_bytes()),
        Err(Dropped::RateLimited)
    );
    // The same client retries a little later with a fresh MessageID.
    let later = t0 + Duration::from_millis(100);
    let retry = unique_probe(10_000);
    assert!(e.handle(later, v6_peer(9999), retry.as_bytes()).is_ok());
}

#[test]
fn a_probe_refused_for_budget_can_be_retried_verbatim_after_refill() {
    let mut e = v6_engine();
    let t0 = Instant::now();
    for n in 0..Limits::default().global_burst {
        assert!(e.handle(t0, v6_peer(n), unique_probe(n).as_bytes()).is_ok());
    }
    let victim = unique_probe(5000);
    assert_eq!(
        e.handle(t0, v6_peer(5000), victim.as_bytes()),
        Err(Dropped::RateLimited)
    );
    let later = t0 + Duration::from_millis(100);
    assert!(e.handle(later, v6_peer(5000), victim.as_bytes()).is_ok());
}

#[test]
fn limits_are_clamped_so_the_reflection_bound_is_true() {
    let wild = Limits {
        global_burst: 0,
        global_per_second: 0,
        source_burst: 0,
        source_refill: Duration::ZERO,
        source_capacity: 0,
    };
    let l = config().with_limits(wild).limits();
    assert_eq!(
        (l.global_burst, l.global_per_second, l.source_burst),
        (1, 1, 1)
    );
    assert_eq!(l.max_reflected_bytes_per_second(), MAX_REPLY as u64);
    let fast = Limits {
        global_per_second: u32::MAX,
        ..Limits::default()
    };
    assert_eq!(
        config().with_limits(fast).limits().global_per_second,
        Limits::MAX_PER_SECOND
    );
}

#[test]
fn global_refill_interval_never_admits_more_than_the_configured_rate() {
    for per_second in [1, 3, 7, 10, 600, 999_999, Limits::MAX_PER_SECOND] {
        let limits = Limits {
            global_per_second: per_second,
            ..Limits::default()
        };
        let tokens_per_second =
            Duration::from_secs(1).as_nanos() / limits.global_refill().as_nanos();
        assert!(tokens_per_second <= u128::from(per_second), "{per_second}");
    }
}

/// The largest scope set `Config::new` accepts on one interface (grown scope by scope, then
/// byte by byte), i.e. a configuration whose worst reply sits at the size limit.
fn largest_accepted_config() -> Config {
    let build = |scopes: &[String]| {
        Config::new(
            EndpointReference::parse(ENDPOINT).unwrap(),
            scopes.to_vec(),
            vec![iface(10)],
            ServiceAddress {
                port: 8080,
                path: "/onvif/device_service".to_owned(),
            },
        )
    };
    let full = format!("a:{}", "b".repeat(126));
    let mut scopes = Vec::new();
    while build(&scopes).is_ok() && scopes.len() < MAX_SCOPES_FOR_TEST {
        scopes.push(full.clone());
    }
    scopes.pop();
    let mut best = build(&scopes).unwrap();
    for k in 0..=126 {
        let mut trial = scopes.clone();
        trial.push(format!("a:{}", "c".repeat(k)));
        match build(&trial) {
            Ok(config) => best = config,
            Err(_) => break,
        }
    }
    best
}

const MAX_SCOPES_FOR_TEST: usize = 16;

#[test]
fn worst_case_probe_and_resolve_replies_fit_at_the_size_limit() {
    // 25 x `&` escape to 125 bytes, plus 3: exactly the escaped `MessageID` bound.
    let id = format!("{}xxx", "&amp;".repeat(25));
    let probe = PROBE_ANY.replace("urn:uuid:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", &id);
    let resolve = RESOLVE.replace("urn:uuid:99999999-bbbb-cccc-dddd-eeeeeeeeeeee", &id);
    for doc in [probe, resolve] {
        let mut e = Engine::new(largest_accepted_config(), Counter(1));
        let out = e.handle(Instant::now(), peer(20), doc.as_bytes()).unwrap();
        assert!(
            out.payload.len() <= MAX_REPLY,
            "{} bytes",
            out.payload.len()
        );
        assert!(
            out.payload.len() > MAX_REPLY - 400,
            "config is near the limit"
        );
    }
}

#[test]
fn configuration_check_covers_resolve_matches() {
    let config = largest_accepted_config();
    let mut scopes = config.scopes.clone();
    scopes.push("a:b".to_owned());
    let bigger = Config::new(
        EndpointReference::parse(ENDPOINT).unwrap(),
        scopes,
        vec![iface(10)],
        config.service.clone(),
    );
    // One more byte-scale scope pushes the longer `ResolveMatches` over the limit.
    assert!(matches!(bigger, Err(ConfigError::DescriptionTooLarge(n)) if n > MAX_REPLY));
}

#[test]
fn per_source_table_rotation_does_not_lock_out_new_clients() {
    let limits = Limits {
        source_capacity: 8,
        global_burst: 10_000,
        global_per_second: 10_000,
        ..Limits::default()
    };
    let mut e = Engine::new(
        config_with(vec![Interface {
            addr: "2001:db8::1".parse().unwrap(),
            prefix_len: 32,
            index: 1,
        }])
        .with_limits(limits),
        Counter(1),
    );
    let t0 = Instant::now();
    for n in 0..100 {
        assert!(e.handle(t0, v6_peer(n), unique_probe(n).as_bytes()).is_ok());
    }
}

#[test]
fn identical_copies_of_a_datagram_do_not_spend_budget() {
    let mut e = engine();
    let t0 = Instant::now();
    let first = unique_probe(1);
    assert!(e.handle(t0, peer(20), first.as_bytes()).is_ok());
    for _ in 0..10 {
        assert_eq!(
            e.handle(t0, peer(20), first.as_bytes()),
            Err(Dropped::Duplicate)
        );
    }
    // The source still has three of its four tokens.
    for n in 2..5 {
        assert!(e.handle(t0, peer(20), unique_probe(n).as_bytes()).is_ok());
    }
}

#[test]
fn unsupported_match_rule_never_matches_even_without_scopes() {
    let mut e = engine();
    let probe = PROBE_ANY.replace(
        "<d:Probe/>",
        "<d:Probe><d:Scopes MatchBy=\"urn:unknown\"></d:Scopes></d:Probe>",
    );
    assert_eq!(
        e.handle(Instant::now(), peer(20), probe.as_bytes()),
        Err(Dropped::NoMatch)
    );
}

#[test]
fn garbage_costs_the_sender_its_rate_budget() {
    let mut e = engine();
    let t0 = Instant::now();
    for n in 0..Limits::default().source_burst {
        assert!(matches!(
            e.handle(t0, peer(20), format!("junk{n}").as_bytes()),
            Err(Dropped::Malformed(_))
        ));
    }
    assert_eq!(
        e.handle(t0, peer(20), PROBE_ANY.as_bytes()),
        Err(Dropped::RateLimited)
    );
}

#[test]
fn reply_never_exceeds_the_cap_for_the_worst_case_message_id() {
    let mut e = engine();
    let long_id = "x".repeat(MAX_MESSAGE_ID);
    let probe = PROBE_ANY.replace("urn:uuid:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", &long_id);
    let out = e
        .handle(Instant::now(), peer(20), probe.as_bytes())
        .unwrap();
    assert!(out.payload.len() <= MAX_REPLY);
}

#[test]
fn hello_and_bye_go_to_the_group_from_each_interface() {
    let second = Interface {
        addr: "2001:db8::10".parse().unwrap(),
        prefix_len: 64,
        index: 3,
    };
    let mut e = Engine::new(config_with(vec![iface(10), second]), Counter(7));
    let t0 = Instant::now();
    let hello = e.hello(t0);
    assert_eq!(hello.len(), 4, "two interfaces, two dialects");
    assert_eq!(hello[0].dest, SocketAddr::new(GROUP_V4.into(), PORT));
    assert_eq!(hello[1].dest, SocketAddr::new(GROUP_V4.into(), PORT));
    assert_eq!(hello[2].dest, SocketAddr::new(GROUP_V6.into(), PORT));
    let first = String::from_utf8(hello[0].payload.clone()).unwrap();
    assert!(first.contains("schemas.xmlsoap.org/ws/2005/04/discovery/Hello"));
    let second = String::from_utf8(hello[1].payload.clone()).unwrap();
    assert!(second.contains("ws-dd/ns/discovery/2009/01/Hello"));
    assert!(hello.iter().all(|h| h.due <= t0 + MAX_DELAY));
    let text = String::from_utf8(hello[3].payload.clone()).unwrap();
    assert!(text.contains("http://[2001:db8::10]:8080/onvif/device_service"));
    assert!(text.contains("InstanceId=\"42\" MessageNumber=\"4\""));

    let bye = e.bye(t0);
    assert!(bye.iter().all(|b| b.due == t0));
    let text = String::from_utf8(bye[0].payload.clone()).unwrap();
    assert!(text.contains("<d:Bye>") && !text.contains("XAddrs") && !text.contains("Scopes"));
}

#[test]
fn overlapping_subnets_pick_the_most_specific_interface() {
    let narrow = Interface {
        addr: Ipv4Addr::new(192, 0, 2, 11).into(),
        prefix_len: 28,
        index: 2,
    };
    let e = Engine::new(config_with(vec![iface(10), narrow]), Counter(1));
    let at = |ip: &str| e.arrival_interface(SocketAddr::new(ip.parse().unwrap(), 4000));
    assert_eq!(at("192.0.2.5"), Some(1));
    assert_eq!(at("192.0.2.200"), Some(0));
    assert_eq!(at("198.51.100.5"), None);
}

#[test]
fn scoped_link_local_senders_pick_the_interface_with_that_index() {
    let ll = |index| Interface {
        addr: "fe80::10".parse().unwrap(),
        prefix_len: 64,
        index,
    };
    let e = Engine::new(config_with(vec![ll(4), ll(5)]), Counter(1));
    let from = |scope| {
        SocketAddr::V6(std::net::SocketAddrV6::new(
            "fe80::99".parse().unwrap(),
            4000,
            0,
            scope,
        ))
    };
    assert_eq!(e.arrival_interface(from(5)), Some(1));
    assert_eq!(e.arrival_interface(from(4)), Some(0));
    assert_eq!(e.arrival_interface(from(9)), None);
    assert_eq!(e.arrival_interface(from(0)), Some(0));
}

#[test]
fn config_validation() {
    let ep = || EndpointReference::parse(ENDPOINT).unwrap();
    let svc = || ServiceAddress {
        port: 80,
        path: "/d".to_owned(),
    };
    let new = |scopes: Vec<String>, ifs: Vec<Interface>, svc: ServiceAddress| {
        Config::new(ep(), scopes, ifs, svc).unwrap_err()
    };
    assert_eq!(new(vec![], vec![], svc()), ConfigError::NoInterfaces);
    assert_eq!(
        new(vec![], vec![iface(1); 9], svc()),
        ConfigError::TooManyInterfaces
    );
    let bad = Interface {
        prefix_len: 33,
        ..iface(1)
    };
    assert!(matches!(
        new(vec![], vec![bad], svc()),
        ConfigError::InvalidInterface(_)
    ));
    let mcast = Interface {
        addr: GROUP_V4.into(),
        ..iface(1)
    };
    assert!(matches!(
        new(vec![], vec![mcast], svc()),
        ConfigError::InvalidInterface(_)
    ));
    for scope in [
        "",
        "no-scheme",
        "has space:x",
        "a:b\n",
        "a:\u{fffe}",
        "a:caf\u{e9}",
    ] {
        assert!(matches!(
            new(vec![scope.to_owned()], vec![iface(1)], svc()),
            ConfigError::InvalidScope(_)
        ));
    }
    assert_eq!(
        new(vec!["a:b".to_owned(); 17], vec![iface(1)], svc()),
        ConfigError::TooManyScopes
    );
    for path in ["no-slash", "/a b", "/<x>"] {
        let s = ServiceAddress {
            port: 80,
            path: path.to_owned(),
        };
        assert!(matches!(
            new(vec![], vec![iface(1)], s),
            ConfigError::InvalidServicePath(_)
        ));
    }
    let huge = (0..16)
        .map(|n| format!("onvif://www.onvif.org/{n}/{}", "a".repeat(100)))
        .collect();
    assert!(matches!(
        new(huge, vec![iface(1)], svc()),
        ConfigError::DescriptionTooLarge(_)
    ));
}

#[test]
fn endpoint_reference_parse_and_generate() {
    assert_eq!(
        EndpointReference::parse("URN:UUID:01234567-89AB-4DEF-8123-456789ABCDEF")
            .unwrap()
            .as_str(),
        ENDPOINT
    );
    for bad in [
        "",
        "urn:uuid:",
        "uuid:01234567-89ab-4def-8123-456789abcdef",
        "urn:uuid:0123-89ab",
        "urn:uuid:0123456z-89ab-4def-8123-456789abcdef",
        "ürn:uuid:x",
    ] {
        assert!(EndpointReference::parse(bad).is_err(), "{bad}");
    }
    let generated = EndpointReference::generate(&mut Counter(3));
    assert_eq!(
        EndpointReference::parse(generated.as_str()).unwrap(),
        generated
    );
    let s = generated.as_str();
    assert_eq!(s.get(23..24), Some("4"), "version nibble");
    assert!(
        matches!(s.get(28..29), Some("8" | "9" | "a" | "b")),
        "variant"
    );
}

#[test]
fn system_entropy_varies() {
    let mut e = SystemEntropy::default();
    assert_ne!(e.next_u64(), e.next_u64());
}

#[test]
fn subnet_membership() {
    let v4 = iface(10);
    assert!(v4.contains("192.0.2.200".parse().unwrap()));
    assert!(!v4.contains("198.51.100.1".parse().unwrap()));
    assert!(!v4.contains("2001:db8::1".parse().unwrap()));
    let any = Interface {
        prefix_len: 0,
        ..v4
    };
    assert!(any.contains("198.51.100.1".parse().unwrap()));
    let v6 = Interface {
        addr: "2001:db8::10".parse().unwrap(),
        prefix_len: 64,
        index: 1,
    };
    assert!(v6.contains("2001:db8::99".parse().unwrap()));
    assert!(!v6.contains("2001:db8:1::99".parse().unwrap()));
}

struct Rig {
    responder: Responder<MemTransport, Counter, ManualClock>,
    wire: MemTransport,
    clock: ManualClock,
}

fn rig() -> Rig {
    rig_with(config())
}

fn rig_with(config: Config) -> Rig {
    let wire = MemTransport::default();
    let clock = ManualClock::new();
    let responder = Responder::new(config, Counter(1), wire.clone(), clock.clone());
    Rig {
        responder,
        wire,
        clock,
    }
}

#[test]
fn responder_holds_replies_until_due_then_sends_once() {
    let mut r = rig();
    r.wire
        .0
        .borrow_mut()
        .inbox
        .push_back((peer(20), PROBE_ONVIF.as_bytes().to_vec()));
    r.responder.step(POLL).unwrap();
    r.clock.advance(MAX_DELAY);
    r.responder.step(POLL).unwrap();
    r.responder.step(POLL).unwrap();
    let wire = r.wire.0.borrow();
    assert_eq!(wire.sent.len(), 1);
    assert_eq!(wire.sent[0].1, peer(20));
    assert_eq!(r.responder.stats().received, 1);
}

#[test]
fn start_announces_hello_and_shutdown_says_bye_immediately() {
    let mut r = rig();
    r.responder.start();
    r.clock.advance(MAX_DELAY);
    r.responder.step(POLL).unwrap();
    assert_eq!(r.wire.0.borrow().sent.len(), 2, "one Hello per dialect");
    assert!(String::from_utf8_lossy(&r.wire.0.borrow().sent[0].2).contains("<d:Hello>"));

    r.responder.shutdown();
    let wire = r.wire.0.borrow();
    assert_eq!(wire.sent.len(), 4);
    assert!(String::from_utf8_lossy(&wire.sent[2].2).contains("<d:Bye>"));
}

#[test]
fn shutdown_discards_queued_replies() {
    let mut r = rig();
    r.wire
        .0
        .borrow_mut()
        .inbox
        .push_back((peer(20), PROBE_ANY.as_bytes().to_vec()));
    r.responder.step(POLL).unwrap();
    r.responder.shutdown();
    r.clock.advance(MAX_DELAY);
    r.responder.step(POLL).unwrap();
    let wire = r.wire.0.borrow();
    assert_eq!(wire.sent.len(), 2, "only the Byes");
}

#[test]
fn send_failures_are_counted_not_fatal() {
    let mut r = rig();
    r.wire.0.borrow_mut().fail_sends = true;
    r.responder.start();
    r.clock.advance(MAX_DELAY);
    r.responder.step(POLL).unwrap();
    assert_eq!(r.responder.stats().send_errors, 2);
}

#[test]
fn pending_queue_is_bounded() {
    let generous = Limits {
        global_burst: 1000,
        global_per_second: 1000,
        source_burst: 1000,
        ..Limits::default()
    };
    let mut r = rig_with(config().with_limits(generous));
    for n in 0..200_u32 {
        let src = SocketAddr::from(([192, 0, 2, 100 + u8::try_from(n % 100).unwrap()], 4000));
        let probe = with_id(
            PROBE_ANY,
            &format!("aaaaaaaa-bbbb-cccc-{:04x}-{n:012}", n / 100),
        );
        r.wire
            .0
            .borrow_mut()
            .inbox
            .push_back((src, probe.into_bytes()));
    }
    for _ in 0..200 {
        r.responder.step(Duration::ZERO).unwrap();
    }
    let stats = r.responder.stats();
    assert_eq!(stats.received, 200);
    assert!(r.responder.pending.len() <= MAX_PENDING);
    assert!(stats.dropped > 0);
}

#[test]
fn run_announces_then_says_bye_when_stopped() {
    let mut r = rig();
    let stop = AtomicBool::new(false);
    // The first step lets the manual clock move past the Hello delay, then stop.
    r.clock.advance(MAX_DELAY);
    stop.store(true, Ordering::Relaxed);
    r.responder.run(&stop).unwrap();
    let wire = r.wire.0.borrow();
    assert_eq!(
        wire.sent.len(),
        2,
        "stopped before Hello was due; Byes still sent"
    );
    assert!(String::from_utf8_lossy(&wire.sent[0].2).contains("<d:Bye>"));
}

#[test]
fn dropped_has_readable_text() {
    assert_eq!(Dropped::Duplicate.to_string(), "duplicate message id");
    assert!(
        Dropped::Malformed(ParseError::NotEnvelope)
            .to_string()
            .contains("Envelope")
    );
}

proptest! {
    #[test]
    fn engine_never_panics_on_arbitrary_datagrams(
        data in proptest::collection::vec(any::<u8>(), 0..MAX_PACKET + 64),
        octet in any::<u8>(),
        port in any::<u16>(),
    ) {
        let mut e = engine();
        let src = SocketAddr::from(([192, 0, 2, octet], port));
        let _ = e.handle(Instant::now(), src, &data);
    }

    #[test]
    fn replies_are_always_small_and_unicast_to_the_source(
        id in "[a-zA-Z0-9:-]{1,128}",
        octet in 1u8..255,
    ) {
        let mut e = engine();
        let probe = PROBE_ANY.replace("urn:uuid:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", &id);
        if let Ok(out) = e.handle(Instant::now(), peer(octet), probe.as_bytes()) {
            prop_assert!(out.payload.len() <= MAX_REPLY);
            prop_assert_eq!(out.dest, peer(octet));
        }
    }
}
