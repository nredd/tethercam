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
    fail_receives: bool,
    stop_on_receive: Option<std::sync::Arc<AtomicBool>>,
    rebinds: Vec<Vec<Interface>>,
    fail_rebind: bool,
    unbindable: Vec<Interface>,
    /// The interfaces the sockets are open on, in socket order (what `UdpTransport` keeps).
    bound: Vec<Interface>,
    /// The interface each datagram actually left from.
    left_from: Vec<Option<Interface>>,
}

#[derive(Clone, Default)]
struct MemTransport(Rc<RefCell<Wire>>);

impl Transport for MemTransport {
    fn recv(&mut self, buf: &mut [u8], _timeout: Duration) -> Result<Option<Datagram>, Error> {
        if let Some(stop) = &self.0.borrow().stop_on_receive {
            stop.store(true, Ordering::Relaxed);
        }
        if self.0.borrow().fail_receives {
            return Err(std::io::Error::other("receive failed").into());
        }
        let Some((source, data)) = self.0.borrow_mut().inbox.pop_front() else {
            return Ok(None);
        };
        let len = data.len().min(buf.len());
        buf[..len].copy_from_slice(&data[..len]);
        Ok(Some(Datagram { len, source }))
    }

    fn send(&mut self, interface: usize, dest: SocketAddr, payload: &[u8]) -> Result<(), Error> {
        let mut wire = self.0.borrow_mut();
        if wire.fail_sends {
            return Err(std::io::Error::other("unreachable").into());
        }
        let socket = wire.bound.get(interface).copied();
        wire.left_from.push(socket);
        wire.sent.push((interface, dest, payload.to_vec()));
        Ok(())
    }

    fn rebind(&mut self, interfaces: &[Interface]) -> Result<Vec<bool>, Error> {
        let mut wire = self.0.borrow_mut();
        if wire.fail_rebind {
            return Err(std::io::Error::other("rebind failed").into());
        }
        wire.rebinds.push(interfaces.to_vec());
        let usable: Vec<bool> = interfaces
            .iter()
            .map(|i| !wire.unbindable.contains(i))
            .collect();
        wire.bound = interfaces
            .iter()
            .zip(&usable)
            .filter(|(_, usable)| **usable)
            .map(|(i, _)| *i)
            .collect();
        Ok(usable)
    }
}

/// A transport that keeps the trait's default `rebind`.
struct Bare;

impl Transport for Bare {
    fn recv(&mut self, _buf: &mut [u8], _timeout: Duration) -> Result<Option<Datagram>, Error> {
        Ok(None)
    }

    fn send(&mut self, _: usize, _: SocketAddr, _: &[u8]) -> Result<(), Error> {
        Ok(())
    }
}

/// Recognize malformed input while allowing both assertion outcomes to be tested.
fn is_malformed(error: &Dropped) -> bool {
    matches!(error, Dropped::Malformed(_))
}

/// Inspect the oversized description while keeping the unexpected variant testable.
fn description_size(error: &ConfigError) -> Option<usize> {
    match error {
        ConfigError::DescriptionTooLarge(size) => Some(*size),
        _ => None,
    }
}

#[test]
fn error_assertion_helpers_reject_other_variants() {
    assert!(!is_malformed(&Dropped::NoMatch));
    assert_eq!(description_size(&ConfigError::NoInterfaces), None);
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
    assert!(is_malformed(&e.handle(t0, peer(21), b"junk").unwrap_err()));
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

/// The text of the first `<a:MessageID>` in an outgoing envelope.
fn message_id(payload: &[u8]) -> String {
    let text = String::from_utf8(payload.to_vec()).unwrap();
    let (_, rest) = text.split_once("<a:MessageID>").unwrap();
    rest.split_once("</a:MessageID>").unwrap().0.to_owned()
}

#[test]
fn every_announcement_and_match_carries_one_stable_endpoint_and_a_fresh_message_id() {
    // What a capture analysis compares (#112): the endpoint reference is the identity and must
    // not vary between advertisements; the WS-Addressing `MessageID` is per message and must.
    let mut e = engine();
    let t0 = Instant::now();
    let mut out = e.hello(t0);
    for (n, probe) in [
        with_id(PROBE_ANY, "aaaaaaaa-bbbb-cccc-dddd-000000000001"),
        with_id(PROBE_ANY, "aaaaaaaa-bbbb-cccc-dddd-000000000002"),
        PROBE_ONVIF.to_owned(),
        RESOLVE.to_owned(),
    ]
    .iter()
    .enumerate()
    {
        let peer = peer(20 + u8::try_from(n).unwrap());
        out.push(e.handle(t0, peer, probe.as_bytes()).unwrap());
    }
    assert_eq!(out.len(), 6, "a `Hello` per dialect plus four answers");
    let mut ids = Vec::new();
    for message in &out {
        let text = String::from_utf8(message.payload.clone()).unwrap();
        assert_eq!(
            text.matches(ENDPOINT).count(),
            1,
            "one endpoint reference: {text}"
        );
        ids.push(message_id(&message.payload));
    }
    let distinct: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(distinct.len(), ids.len(), "no `MessageID` repeats: {ids:?}");
    // Every answer echoes its own request's `MessageID`, never another's or its own.
    for (message, request) in out[2..].iter().zip([
        "urn:uuid:aaaaaaaa-bbbb-cccc-dddd-000000000001",
        "urn:uuid:aaaaaaaa-bbbb-cccc-dddd-000000000002",
        "uuid:11111111-2222-3333-4444-555555555555",
        "urn:uuid:99999999-bbbb-cccc-dddd-eeeeeeeeeeee",
    ]) {
        let text = String::from_utf8(message.payload.clone()).unwrap();
        assert!(text.contains(&format!("<a:RelatesTo>{request}</a:RelatesTo>")));
    }
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
    let byte_limit = budget * MAX_REPLY as u64;
    assert!(
        bytes <= byte_limit,
        "reflected {bytes} bytes, bound {byte_limit}"
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
    while build(&scopes).is_ok() {
        scopes.push(full.clone());
    }
    scopes.pop();
    let mut best = build(&scopes).unwrap();
    for k in 0..=MAX_REPLY {
        let mut trial = scopes.clone();
        trial.push(format!("a:{}", "c".repeat(k)));
        match build(&trial) {
            Ok(config) => best = config,
            Err(_) => break,
        }
    }
    best
}

#[test]
fn worst_case_probe_and_resolve_replies_fit_at_the_size_limit() {
    // 25 x `&` escape to 125 bytes, plus 3: exactly the escaped `MessageID` bound.
    let id = format!("{}xxx", "&amp;".repeat(25));
    let probe = PROBE_ANY.replace("urn:uuid:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", &id);
    let resolve = RESOLVE.replace("urn:uuid:99999999-bbbb-cccc-dddd-eeeeeeeeeeee", &id);
    for doc in [probe, resolve] {
        let mut e = Engine::new(largest_accepted_config(), Counter(1));
        let out = e.handle(Instant::now(), peer(20), doc.as_bytes()).unwrap();
        let len = out.payload.len();
        assert!(len <= MAX_REPLY, "{len} bytes");
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
    assert!(description_size(&bigger.unwrap_err()).unwrap() > MAX_REPLY);
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
        assert!(is_malformed(
            &e.handle(t0, peer(20), format!("junk{n}").as_bytes())
                .unwrap_err()
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
    let ula = |index| Interface {
        addr: "fd00::10".parse().unwrap(),
        prefix_len: 64,
        index,
    };
    let e = Engine::new(config_with(vec![ll(4), ll(5), ula(4), ula(5)]), Counter(1));
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
fn link_local_ipv6_requires_an_advertisable_address_on_the_same_interface() {
    super::test_log::install();
    let link = Interface {
        addr: "fe80::10".parse().unwrap(),
        prefix_len: 64,
        index: 4,
    };
    let ula = Interface {
        addr: "fd00::10".parse().unwrap(),
        prefix_len: 64,
        index: 4,
    };
    let service = ServiceAddress {
        port: 8080,
        path: "/onvif/device_service".to_owned(),
    };
    assert_eq!(
        Config::new(
            EndpointReference::parse(ENDPOINT).unwrap(),
            vec![],
            vec![link],
            service
        )
        .unwrap_err(),
        ConfigError::NoInterfaces,
    );
    let mut e = Engine::new(config_with(vec![link, ula]), Counter(1));
    let source = SocketAddr::V6(std::net::SocketAddrV6::new(
        "fe80::99".parse().unwrap(),
        4000,
        0,
        4,
    ));
    let out = e
        .handle(Instant::now(), source, PROBE_ANY.as_bytes())
        .unwrap();
    let text = String::from_utf8(out.payload).unwrap();
    assert!(text.contains("http://[fd00::10]:8080/"));
    assert!(!text.contains("fe80"));
    let cfg = config_with(vec![link, iface(10)]);
    assert_eq!(cfg.interfaces(), &[iface(10)]);
    assert!(
        super::test_log::lines()
            .iter()
            .any(|s| s.contains("interface index '4'")
                && s.contains("no non-link-local IPv6 address"))
    );
    let mut e = Engine::new(cfg, Counter(1));
    assert_eq!(
        e.handle(Instant::now(), source, PROBE_ANY.as_bytes()),
        Err(Dropped::OffLink)
    );
    assert!(
        e.handle(Instant::now(), peer(20), PROBE_ANY.as_bytes())
            .is_ok()
    );
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
    assert_eq!(
        new(vec![], vec![bad], svc()),
        ConfigError::InvalidInterface(bad.addr)
    );
    let mcast = Interface {
        addr: GROUP_V4.into(),
        ..iface(1)
    };
    assert_eq!(
        new(vec![], vec![mcast], svc()),
        ConfigError::InvalidInterface(mcast.addr)
    );
    for scope in [
        "",
        "no-scheme",
        "has space:x",
        "a:b\n",
        "a:\u{fffe}",
        "a:caf\u{e9}",
    ] {
        assert_eq!(
            new(vec![scope.to_owned()], vec![iface(1)], svc()),
            ConfigError::InvalidScope(scope.to_owned())
        );
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
        assert_eq!(
            new(vec![], vec![iface(1)], s),
            ConfigError::InvalidServicePath(path.to_owned())
        );
    }
    let huge = (0..16)
        .map(|n| format!("onvif://www.onvif.org/{n}/{}", "a".repeat(100)))
        .collect();
    assert!(description_size(&new(huge, vec![iface(1)], svc())).unwrap() > MAX_REPLY);
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
    assert!("89ab".contains(s.get(28..29).unwrap()), "variant");
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
fn replies_discarded_by_shutdown_or_run_do_not_suppress_the_retransmit() {
    for restart in [false, true] {
        let mut r = rig();
        r.wire
            .0
            .borrow_mut()
            .inbox
            .push_back((peer(20), PROBE_ONVIF.as_bytes().to_vec()));
        r.responder.step(POLL).unwrap();
        if restart {
            // `run` starts clean: a reply queued before it is discarded.
            r.responder.run(&AtomicBool::new(true)).unwrap();
        } else {
            r.responder.shutdown();
        }
        r.wire.0.borrow_mut().sent.clear();
        assert_eq!(deliver(&mut r, peer(20), PROBE_ONVIF), 1, "answered");
    }
}

#[test]
fn an_unbound_interface_does_not_claim_its_subnet() {
    // Wired and wireless on one /24; the first one could not join the group.
    let both = config_with(vec![iface(10), iface(11)]);
    assert_eq!(both.interfaces().len(), 2);
    let healthy = both.with_usable_interfaces(&[false, true]);
    assert_eq!(healthy.interfaces()[0].addr, iface(11).addr);
    let mut r = rig_with(healthy);
    r.wire
        .0
        .borrow_mut()
        .inbox
        .push_back((peer(20), PROBE_ONVIF.as_bytes().to_vec()));
    r.responder.step(POLL).unwrap();
    r.clock.advance(MAX_DELAY);
    r.responder.step(POLL).unwrap();
    let wire = r.wire.0.borrow();
    assert_eq!(wire.sent.len(), 1);
    assert_eq!(wire.sent[0].0, 0);
    assert_eq!(wire.sent[0].1, peer(20));
    let reply = String::from_utf8_lossy(&wire.sent[0].2);
    assert!(reply.contains("ProbeMatches"));
    assert!(reply.contains("192.0.2.11:8080"));
    assert_eq!(r.responder.stats().send_errors, 0);
}

#[test]
fn a_short_usable_mask_drops_the_unlisted_interfaces() {
    let kept = config_with(vec![iface(10), iface(11)]).with_usable_interfaces(&[true]);
    assert_eq!(kept.interfaces().len(), 1);
    assert_eq!(kept.interfaces()[0].addr, iface(10).addr);
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
fn run_processes_a_successful_step_before_stopping() {
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let mut r = rig();
    r.wire.0.borrow_mut().stop_on_receive = Some(stop.clone());
    r.responder.run(&stop).unwrap();
    assert_eq!(r.responder.stats().sent, 2);
}

#[test]
fn run_reports_receive_errors_and_still_sends_bye() {
    let mut r = rig();
    r.wire.0.borrow_mut().fail_receives = true;
    assert!(r.responder.run(&AtomicBool::new(false)).is_err());
    assert_eq!(r.wire.0.borrow().sent.len(), 2);
}

#[test]
fn metadata_version_is_advertised() {
    let mut e = Engine::new(config().with_metadata_version(123), Counter(1));
    let out = e
        .handle(Instant::now(), peer(20), PROBE_ANY.as_bytes())
        .unwrap();
    assert!(
        String::from_utf8(out.payload)
            .unwrap()
            .contains("<d:MetadataVersion>123</d:MetadataVersion>")
    );
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

fn svc() -> ServiceAddress {
    ServiceAddress {
        port: 8080,
        path: "/onvif/device_service".to_owned(),
    }
}

fn endpoint() -> EndpointReference {
    EndpointReference::parse(ENDPOINT).unwrap()
}

#[test]
fn the_default_rebind_has_nothing_to_set_up() {
    assert_eq!(Bare.rebind(&[iface(10), iface(11)]).unwrap(), [true, true]);
    assert_eq!(Bare.recv(&mut [0; 1], Duration::ZERO).unwrap(), None);
    Bare.send(0, peer(1), b"").unwrap();
}

#[test]
fn an_idle_config_serves_nothing_until_given_interfaces() {
    let idle = Config::idle(endpoint(), scopes(), svc()).unwrap();
    assert_eq!(idle.interfaces().len(), 0);
    let served = idle.with_interfaces(vec![iface(10)]).unwrap();
    assert_eq!(served.interfaces(), [iface(10)]);
    let emptied = served.with_interfaces(vec![]).unwrap();
    assert_eq!(emptied.interfaces().len(), 0);
    assert_eq!(
        Config::idle(endpoint(), vec!["no scheme".into()], svc()).unwrap_err(),
        ConfigError::InvalidScope("no scheme".into())
    );
    let long_path = ServiceAddress {
        port: 1,
        path: "relative".into(),
    };
    assert_eq!(
        Config::idle(endpoint(), scopes(), long_path).unwrap_err(),
        ConfigError::InvalidServicePath("relative".into())
    );
    let too_many = (0..=8).map(|n| iface(10 + n)).collect();
    assert_eq!(
        Config::idle(endpoint(), scopes(), svc())
            .unwrap()
            .with_interfaces(too_many)
            .unwrap_err(),
        ConfigError::TooManyInterfaces
    );
    let bad = Interface {
        addr: Ipv4Addr::UNSPECIFIED.into(),
        prefix_len: 24,
        index: 1,
    };
    assert_eq!(
        Config::idle(endpoint(), scopes(), svc())
            .unwrap()
            .with_interfaces(vec![bad])
            .unwrap_err(),
        ConfigError::InvalidInterface(bad.addr)
    );
}

#[test]
fn masking_out_a_routable_address_strands_no_link_local_sibling() {
    let v6 = |addr: &str| Interface {
        addr: addr.parse().unwrap(),
        prefix_len: 64,
        index: 3,
    };
    let config = config_with(vec![v6("fe80::1"), v6("2001:db8::1")]);
    assert_eq!(config.interfaces().len(), 2);
    let masked = config.with_usable_interfaces(&[true, false]);
    assert_eq!(masked.interfaces().len(), 0);
}

#[test]
fn a_watch_debug_hides_the_closure_and_clamps_the_interval() {
    let watch = Watch::new(Duration::ZERO, || None);
    assert_eq!(watch.0.every, Duration::from_millis(1));
    assert_eq!(
        Watch::new(Duration::MAX, || None).0.every,
        Duration::from_secs(86_400)
    );
    assert!(format!("{watch:?}").starts_with("Watch { every: 1ms"));
}

const WATCH_EVERY: Duration = Duration::from_secs(5);

/// A responder whose watch reads `wanted`, started with `initial`.
fn watched(initial: Vec<Interface>) -> (Rig, Arc<std::sync::Mutex<Option<Vec<Interface>>>>) {
    test_log::install();
    let wanted = Arc::new(std::sync::Mutex::new(Some(initial.clone())));
    let source = Arc::clone(&wanted);
    let watch = Watch::new(WATCH_EVERY, move || source.lock().unwrap().clone());
    let mut rig = rig_with(config_with(initial).with_watch(watch));
    rig.responder.start();
    rig.clock.advance(MAX_DELAY);
    rig.responder.step(POLL).unwrap();
    rig.wire.0.borrow_mut().sent.clear();
    (rig, wanted)
}

fn wifi() -> Interface {
    Interface {
        addr: Ipv4Addr::new(198, 51, 100, 20).into(),
        prefix_len: 24,
        index: 2,
    }
}

/// Let the watch interval and the announcement delay pass, then take what was sent.
fn tick(rig: &mut Rig) -> Vec<(usize, SocketAddr, String)> {
    rig.clock.advance(WATCH_EVERY);
    rig.responder.poll_watch();
    rig.clock.advance(MAX_DELAY);
    rig.responder.step(POLL).unwrap();
    std::mem::take(&mut rig.wire.0.borrow_mut().sent)
        .into_iter()
        .map(|(i, d, p)| (i, d, String::from_utf8(p).unwrap()))
        .collect()
}

/// Probe from `from`; the advertised `XAddrs` of the reply, if any.
fn probe_xaddr(rig: &mut Rig, from: SocketAddr, id: u32) -> Option<String> {
    let doc = PROBE_ONVIF.replace(
        "11111111-2222-3333-4444-555555555555",
        &format!("11111111-2222-3333-4444-{id:012}"),
    );
    rig.wire
        .0
        .borrow_mut()
        .inbox
        .push_back((from, doc.into_bytes()));
    rig.responder.step(POLL).unwrap();
    rig.clock.advance(MAX_DELAY);
    rig.responder.step(POLL).unwrap();
    let sent = std::mem::take(&mut rig.wire.0.borrow_mut().sent);
    let reply = String::from_utf8(sent.first()?.2.clone()).unwrap();
    assert_eq!(sent.len(), 1);
    Some(reply)
}

#[test]
fn losing_the_preferred_interface_fails_over_and_restoring_it_recovers() {
    let wired = iface(10);
    let (mut rig, wanted) = watched(vec![wired]);
    let lan = peer(20);
    let wifi_peer = SocketAddr::from((Ipv4Addr::new(198, 51, 100, 30), 40000));
    assert!(
        probe_xaddr(&mut rig, lan, 1)
            .unwrap()
            .contains("192.0.2.10:8080")
    );

    // The dock loses power: only Wi-Fi remains. It is announced, the wired address is not.
    *wanted.lock().unwrap() = Some(vec![wifi()]);
    let sent = tick(&mut rig);
    assert_eq!(
        sent.len(),
        2,
        "one `Hello` per dialect on the new interface"
    );
    for (interface, dest, payload) in &sent {
        assert_eq!((*interface, *dest), (0, wifi().group()));
        assert!(payload.contains("198.51.100.20:8080") && payload.contains("Hello"));
        assert!(!payload.contains("192.0.2.10"));
    }
    assert_eq!(rig.responder.engine.config.interfaces(), [wifi()]);
    assert!(probe_xaddr(&mut rig, peer(21), 2).is_none(), "stale subnet");
    let reply = probe_xaddr(&mut rig, wifi_peer, 3).unwrap();
    assert!(reply.contains("198.51.100.20:8080") && !reply.contains("192.0.2.10"));

    // The dock returns: wired is announced again, Wi-Fi is not re-announced.
    *wanted.lock().unwrap() = Some(vec![wired, wifi()]);
    let sent = tick(&mut rig);
    assert_eq!(sent.len(), 2);
    assert!(
        sent.iter()
            .all(|(i, _, p)| *i == 0 && p.contains("192.0.2.10:8080"))
    );
    assert!(
        probe_xaddr(&mut rig, lan, 4)
            .unwrap()
            .contains("192.0.2.10:8080")
    );

    // An unchanged set is neither rebound nor re-announced.
    let rebinds = rig.wire.0.borrow().rebinds.len();
    assert_eq!(tick(&mut rig), Vec::new());
    assert_eq!(rig.wire.0.borrow().rebinds.len(), rebinds);
}

#[test]
fn every_interface_vanishing_silences_discovery_until_one_returns() {
    let (mut rig, wanted) = watched(vec![iface(10)]);
    *wanted.lock().unwrap() = Some(vec![]);
    assert_eq!(tick(&mut rig), Vec::new());
    assert_eq!(rig.responder.engine.config.interfaces().len(), 0);
    assert!(probe_xaddr(&mut rig, peer(20), 1).is_none());
    *wanted.lock().unwrap() = Some(vec![iface(10)]);
    assert_eq!(tick(&mut rig).len(), 2);
    assert!(probe_xaddr(&mut rig, peer(20), 2).is_some());
}

#[test]
fn a_failed_enumeration_keeps_the_current_set() {
    let (mut rig, wanted) = watched(vec![iface(10)]);
    *wanted.lock().unwrap() = None;
    assert_eq!(tick(&mut rig), Vec::new());
    assert_eq!(rig.responder.engine.config.interfaces(), [iface(10)]);
    assert_eq!(rig.wire.0.borrow().rebinds.len(), 0);
}

#[test]
fn a_crowded_host_never_keeps_an_interface_that_vanished() {
    // Eight served: the wired one and seven bridges.
    let current: Vec<_> = (0..8).map(|n| iface(10 + n)).collect();
    let (mut rig, wanted) = watched(current.clone());
    // The wired one disappears; Wi-Fi and one more bridge appear: nine present.
    let mut present: Vec<_> = current[1..].to_vec();
    present.extend([wifi(), iface(30)]);
    assert_eq!(present.len(), 9);
    *wanted.lock().unwrap() = Some(present);
    tick(&mut rig);
    let served = rig.responder.engine.config.interfaces().to_vec();
    assert_eq!(served.len(), 8);
    assert!(
        !served.contains(&current[0]),
        "the vanished address is gone"
    );
    assert!(
        current[1..].iter().all(|i| served.contains(i)),
        "still present, still served"
    );
    assert!(
        served.contains(&wifi()),
        "the first newcomer takes the free slot"
    );
    // Selection is deterministic: the same enumeration again changes nothing.
    assert_eq!(tick(&mut rig), Vec::new());
    assert_eq!(rig.responder.engine.config.interfaces(), served);
}

#[test]
fn a_set_of_all_new_interfaces_past_the_limit_is_cut_not_refused() {
    let (mut rig, wanted) = watched(vec![iface(10)]);
    *wanted.lock().unwrap() = Some((0..9).map(|n| iface(40 + n)).collect());
    tick(&mut rig);
    let served = rig.responder.engine.config.interfaces();
    assert_eq!(served.len(), 8);
    assert!(!served.contains(&iface(10)));
}

#[test]
fn an_invalid_interface_advertises_nothing_instead_of_the_old_set() {
    let (mut rig, wanted) = watched(vec![iface(10)]);
    let bad = Interface {
        addr: Ipv4Addr::UNSPECIFIED.into(),
        prefix_len: 24,
        index: 1,
    };
    *wanted.lock().unwrap() = Some(vec![wifi(), bad]);
    tick(&mut rig);
    assert_eq!(rig.responder.engine.config.interfaces().len(), 0);
    assert!(probe_xaddr(&mut rig, peer(20), 1).is_none());
    *wanted.lock().unwrap() = Some(vec![wifi()]);
    assert_eq!(tick(&mut rig).len(), 2);
}

#[test]
fn a_failed_rebind_advertises_nothing_and_is_retried() {
    let (mut rig, wanted) = watched(vec![iface(10)]);
    *wanted.lock().unwrap() = Some(vec![wifi()]);
    rig.wire.0.borrow_mut().fail_rebind = true;
    assert_eq!(tick(&mut rig), Vec::new());
    assert_eq!(rig.responder.engine.config.interfaces().len(), 0);
    assert!(
        probe_xaddr(&mut rig, peer(20), 1).is_none(),
        "no stale address"
    );
    // Even a return to the previous set does not leave it silent.
    *wanted.lock().unwrap() = Some(vec![iface(10)]);
    rig.wire.0.borrow_mut().fail_rebind = false;
    assert_eq!(tick(&mut rig).len(), 2);
    assert_eq!(rig.responder.engine.config.interfaces(), [iface(10)]);
}

#[test]
fn an_interface_that_cannot_bind_claims_no_subnet_and_is_retried_until_it_binds() {
    let (mut rig, wanted) = watched(vec![iface(10)]);
    rig.wire.0.borrow_mut().unbindable = vec![wifi()];
    *wanted.lock().unwrap() = Some(vec![iface(10), wifi()]);
    assert_eq!(tick(&mut rig), Vec::new(), "wired is not new");
    assert_eq!(rig.responder.engine.config.interfaces(), [iface(10)]);
    assert_eq!(tick(&mut rig), Vec::new());
    assert_eq!(
        rig.wire.0.borrow().rebinds.len(),
        4, // two rebinds per attempt: the set, then the masked list
        "retried after a backoff"
    );
    rig.wire.0.borrow_mut().unbindable.clear();
    // The retry delay doubles (5 s, 10 s): wait it out.
    rig.clock.advance(WATCH_EVERY * 2);
    let sent = tick(&mut rig);
    assert_eq!(sent.len(), 2, "only the interface that now binds");
    assert!(sent.iter().all(|(_, _, p)| p.contains("198.51.100.20")));
    assert_eq!(tick(&mut rig), Vec::new());
    assert_eq!(rig.wire.0.borrow().rebinds.len(), 5, "settled");
}

#[test]
fn a_change_before_the_first_hello_went_out_still_announces_every_interface() {
    let wanted = Arc::new(std::sync::Mutex::new(Some(vec![iface(10), wifi()])));
    let source = Arc::clone(&wanted);
    let watch = Watch::new(WATCH_EVERY, move || source.lock().unwrap().clone());
    let mut rig = rig_with(config_with(vec![iface(10)]).with_watch(watch));
    rig.responder.start();
    rig.clock.advance(WATCH_EVERY);
    rig.responder.poll_watch();
    rig.clock.advance(MAX_DELAY);
    rig.responder.step(POLL).unwrap();
    let sent = std::mem::take(&mut rig.wire.0.borrow_mut().sent);
    assert_eq!(sent.len(), 4, "both dialects on both interfaces");
}

#[test]
fn run_polls_the_watch_between_steps() {
    let (mut rig, wanted) = watched(vec![iface(10)]);
    *wanted.lock().unwrap() = Some(vec![wifi()]);
    let stop = Arc::new(AtomicBool::new(false));
    rig.wire.0.borrow_mut().stop_on_receive = Some(Arc::clone(&stop));
    rig.clock.advance(WATCH_EVERY);
    rig.responder.run(&stop).unwrap();
    assert_eq!(rig.responder.engine.config.interfaces(), [wifi()]);
}

#[test]
fn polling_before_the_interval_or_for_an_equivalent_set_changes_nothing() {
    let (mut rig, wanted) = watched(vec![iface(10)]);
    *wanted.lock().unwrap() = Some(vec![wifi()]);
    rig.responder.poll_watch();
    assert_eq!(
        rig.responder.engine.config.interfaces(),
        [iface(10)],
        "not due"
    );
    // A lone link-local IPv6 address is dropped by normalization: same served set.
    let lone_link_local = Interface {
        addr: "fe80::1".parse().unwrap(),
        prefix_len: 64,
        index: 9,
    };
    *wanted.lock().unwrap() = Some(vec![iface(10), lone_link_local]);
    assert_eq!(tick(&mut rig), Vec::new());
    assert_eq!(rig.wire.0.borrow().rebinds.len(), 0);
    assert_eq!(tick(&mut rig), Vec::new());
}

#[test]
fn an_idle_responder_says_it_is_waiting() {
    test_log::install();
    let idle = Config::idle(endpoint(), scopes(), svc()).unwrap();
    let mut r = rig_with(idle);
    r.responder.start();
    r.clock.advance(MAX_DELAY);
    r.responder.step(POLL).unwrap();
    assert_eq!(r.wire.0.borrow().sent.len(), 0);
    assert!(
        test_log::lines()
            .iter()
            .any(|l| l.contains("No usable network interface for discovery yet"))
    );
}

#[test]
fn run_serves_the_interfaces_that_exist_now_not_the_startup_snapshot() {
    let (mut rig, wanted) = watched(vec![iface(10)]);
    *wanted.lock().unwrap() = Some(vec![wifi()]);
    let stop = Arc::new(AtomicBool::new(false));
    rig.wire.0.borrow_mut().stop_on_receive = Some(Arc::clone(&stop));
    rig.responder.run(&stop).unwrap();
    assert_eq!(rig.responder.engine.config.interfaces(), [wifi()]);
    let wire = rig.wire.0.borrow();
    assert_eq!(
        wire.sent.len(),
        2,
        "only the Byes, on the current interface"
    );
    assert!(
        wire.sent
            .iter()
            .all(|(i, d, _)| (*i, *d) == (0, wifi().group()))
    );
}

#[test]
fn link_local_entries_that_will_be_dropped_do_not_count_towards_the_limit() {
    let mut set: Vec<_> = (0..8_u8)
        .map(|n| Interface {
            addr: Ipv4Addr::new(192, 0, 2, 10 + n).into(),
            prefix_len: 24,
            index: 1,
        })
        .collect();
    set.push(Interface {
        addr: "fe80::1".parse().unwrap(),
        prefix_len: 64,
        index: 7,
    });
    let config = Config::idle(endpoint(), scopes(), svc()).unwrap();
    assert_eq!(config.with_interfaces(set).unwrap().interfaces().len(), 8);
}

/// Receive `from`'s probe and leave its reply queued.
fn queue_probe(rig: &mut Rig, from: SocketAddr, id: u32) {
    let doc = PROBE_ONVIF.replace(
        "11111111-2222-3333-4444-555555555555",
        &format!("11111111-2222-3333-4444-{id:012}"),
    );
    rig.wire
        .0
        .borrow_mut()
        .inbox
        .push_back((from, doc.into_bytes()));
    rig.responder.step(POLL).unwrap();
}

fn flush(rig: &mut Rig) -> Vec<String> {
    rig.clock.advance(MAX_DELAY);
    rig.responder.step(POLL).unwrap();
    std::mem::take(&mut rig.wire.0.borrow_mut().sent)
        .into_iter()
        .map(|(_, _, p)| String::from_utf8(p).unwrap())
        .collect()
}

#[test]
fn retrying_an_unbindable_interface_keeps_queued_replies_and_backs_off() {
    let (mut rig, wanted) = watched(vec![iface(10)]);
    rig.wire.0.borrow_mut().unbindable = vec![wifi()];
    *wanted.lock().unwrap() = Some(vec![iface(10), wifi()]);
    tick(&mut rig);
    queue_probe(&mut rig, peer(20), 1);
    // The retry (the delay is 5 s, then doubles) must not discard the delayed reply.
    rig.clock.advance(WATCH_EVERY);
    rig.responder.poll_watch();
    assert_eq!(rig.wire.0.borrow().rebinds.len(), 4);
    let replies = flush(&mut rig);
    assert_eq!(replies.len(), 1, "the delayed reply survived the retry");
    assert!(replies[0].contains("192.0.2.10:8080"));
    // A retransmit of the same probe is still de-duplicated, as without any retry.
    queue_probe(&mut rig, peer(20), 1);
    assert_eq!(flush(&mut rig).len(), 0);
    // Backoff: a long run of polls rebinds a handful of times, not once per poll.
    for _ in 0..100 {
        rig.clock.advance(WATCH_EVERY);
        rig.responder.poll_watch();
    }
    assert!(rig.wire.0.borrow().rebinds.len() < 24);
    assert_eq!(rig.responder.retry_every, MAX_RETRY);
}

#[test]
fn a_dropped_delayed_reply_does_not_suppress_the_clients_retransmit() {
    let (mut rig, wanted) = watched(vec![iface(10)]);
    queue_probe(&mut rig, peer(20), 1);
    // The interface the reply was queued on vanishes before the reply is due, then returns.
    *wanted.lock().unwrap() = Some(vec![wifi()]);
    rig.clock.advance(WATCH_EVERY);
    rig.responder.poll_watch();
    *wanted.lock().unwrap() = Some(vec![iface(10)]);
    rig.clock.advance(WATCH_EVERY);
    rig.responder.poll_watch();
    rig.wire.0.borrow_mut().sent.clear();
    flush(&mut rig);
    rig.wire.0.borrow_mut().sent.clear();
    // The retransmit is answered, not remembered as already answered.
    assert!(probe_xaddr(&mut rig, peer(20), 1).is_some());
}

#[test]
fn queued_replies_follow_their_interface_to_its_new_index() {
    let (mut rig, wanted) = watched(vec![iface(10), wifi()]);
    let wifi_peer = SocketAddr::from((Ipv4Addr::new(198, 51, 100, 30), 40000));
    queue_probe(&mut rig, wifi_peer, 1);
    // The wired interface vanishes: Wi-Fi moves from index 1 to 0 with its reply pending.
    *wanted.lock().unwrap() = Some(vec![wifi()]);
    rig.clock.advance(WATCH_EVERY);
    rig.responder.poll_watch();
    rig.wire.0.borrow_mut().sent.clear();
    let sent = {
        rig.clock.advance(MAX_DELAY);
        rig.responder.step(POLL).unwrap();
        std::mem::take(&mut rig.wire.0.borrow_mut().sent)
    };
    let reply = sent
        .iter()
        .find(|(_, dest, _)| *dest == wifi_peer)
        .expect("reply survived");
    assert_eq!(reply.0, 0);
}

fn v6(addr: &str, index: u32) -> Interface {
    Interface {
        addr: addr.parse().unwrap(),
        prefix_len: 64,
        index,
    }
}

#[test]
fn engine_and_socket_numbering_agree_after_a_mask_strands_a_link_local_sibling() {
    let (ll3, g3, g4) = (
        v6("fe80::1", 3),
        v6("2001:db8::1", 3),
        v6("2001:db8:1::1", 4),
    );
    let config = config_with(vec![ll3, g3, g4]);
    assert_eq!(config.interfaces(), [ll3, g3, g4]);
    // What `UdpTransport::bind` would hold when `g3` cannot bind: sockets for `ll3` and `g4`.
    let mut transport = MemTransport::default();
    transport.0.borrow_mut().unbindable = vec![g3];
    transport.0.borrow_mut().bound = vec![ll3, g4];
    let settled = config.serving(&mut transport, vec![true, false, true]);
    assert_eq!(
        settled.interfaces(),
        [g4],
        "`ll3` lost its routable sibling"
    );
    assert_eq!(
        transport.0.borrow().bound,
        [g4],
        "socket 0 is `g4`, as engine 0"
    );
    let clock = ManualClock::new();
    let mut responder = Responder::new(settled, Counter(1), transport.clone(), clock.clone());
    responder.start();
    clock.advance(MAX_DELAY);
    responder.step(POLL).unwrap();
    let wire = transport.0.borrow();
    assert_eq!(
        wire.left_from,
        [Some(g4), Some(g4)],
        "both `Hello` dialects"
    );
}

#[test]
fn serving_never_keeps_an_interface_without_a_socket() {
    test_log::install();
    let config = config_with(vec![iface(10), iface(11)]);
    let mut transport = MemTransport::default();
    transport.0.borrow_mut().fail_rebind = true;
    let settled = config.serving(&mut transport, vec![true, false]);
    assert_eq!(settled.interfaces().len(), 0);
}

/// `PROBE_ONVIF` with its `MessageID` replaced by one derived from `id`.
fn onvif_probe(id: u32) -> String {
    PROBE_ONVIF.replace(
        "11111111-2222-3333-4444-555555555555",
        &format!("11111111-2222-3333-4444-{id:012}"),
    )
}

/// Deliver `doc` from `from`, let its reply fall due, and count the sends attempted for it (the
/// successful ones plus the failed ones).
fn deliver(rig: &mut Rig, from: SocketAddr, doc: &str) -> u64 {
    let before = rig.responder.stats();
    rig.wire
        .0
        .borrow_mut()
        .inbox
        .push_back((from, doc.as_bytes().to_vec()));
    rig.responder.step(POLL).unwrap();
    rig.clock.advance(MAX_DELAY);
    rig.responder.step(POLL).unwrap();
    let after = rig.responder.stats();
    (after.sent - before.sent) + (after.send_errors - before.send_errors)
}

#[test]
fn a_failed_send_does_not_suppress_the_identical_retransmit() {
    let mut rig = rig();
    rig.wire.0.borrow_mut().fail_sends = true;
    assert_eq!(deliver(&mut rig, peer(20), PROBE_ONVIF), 1);
    assert_eq!(rig.responder.stats().send_errors, 1);
    rig.wire.0.borrow_mut().fail_sends = false;
    rig.clock.advance(Duration::from_secs(1));
    assert_eq!(deliver(&mut rig, peer(20), PROBE_ONVIF), 1, "answered");
    assert_eq!(rig.responder.stats().sent, 1);
    // Once a reply really went out, copies are suppressed again, however many arrive.
    for _ in 0..10 {
        assert_eq!(deliver(&mut rig, peer(20), PROBE_ONVIF), 0);
    }
    assert_eq!(rig.responder.stats().sent, 1);
}

#[test]
fn a_failed_send_does_not_suppress_a_retry_with_the_same_message_id() {
    let mut rig = rig();
    rig.wire.0.borrow_mut().fail_sends = true;
    assert_eq!(deliver(&mut rig, peer(20), PROBE_ONVIF), 1);
    rig.wire.0.borrow_mut().fail_sends = false;
    // Same `MessageID`, different bytes: only the semantic de-duplication could suppress it.
    let retry = format!("{PROBE_ONVIF}\n");
    assert_eq!(deliver(&mut rig, peer(20), &retry), 1, "answered");
    assert_eq!(rig.responder.stats().sent, 1);
    assert_eq!(deliver(&mut rig, peer(20), &retry), 0);
    assert_eq!(deliver(&mut rig, peer(20), PROBE_ONVIF), 0);
}

#[test]
fn retries_while_sends_keep_failing_stay_within_the_rate_limit() {
    // No refill within the test, so the per-source burst is the whole budget.
    let frozen = Limits {
        source_refill: Duration::from_secs(3600),
        ..Limits::default()
    };
    let mut rig = rig_with(config().with_limits(frozen));
    rig.wire.0.borrow_mut().fail_sends = true;
    let attempts: u64 = (0..50)
        .map(|_| deliver(&mut rig, peer(20), PROBE_ONVIF))
        .sum();
    assert_eq!(attempts, u64::from(Limits::default().source_burst));
    assert_eq!(rig.responder.stats().sent, 0);
}

#[test]
fn a_copy_dropped_for_a_queued_message_id_is_answered_when_that_reply_fails() {
    let mut rig = rig();
    rig.wire.0.borrow_mut().fail_sends = true;
    let copy = format!("{PROBE_ONVIF}\n");
    // The original queues a reply; a byte-different copy with the same `MessageID` is dropped.
    rig.wire
        .0
        .borrow_mut()
        .inbox
        .push_back((peer(20), PROBE_ONVIF.as_bytes().to_vec()));
    rig.wire
        .0
        .borrow_mut()
        .inbox
        .push_back((peer(21), copy.as_bytes().to_vec()));
    rig.responder.step(POLL).unwrap();
    rig.responder.step(POLL).unwrap();
    assert_eq!(rig.responder.stats().dropped, 1);
    rig.clock.advance(MAX_DELAY);
    rig.responder.step(POLL).unwrap();
    assert_eq!(rig.responder.stats().send_errors, 1, "the reply failed");
    rig.wire.0.borrow_mut().fail_sends = false;
    // Nothing was answered, so the copy's retransmit is answered, not raw-suppressed.
    assert_eq!(deliver(&mut rig, peer(21), &copy), 1);
}

#[test]
fn a_failed_send_forgets_only_its_own_request() {
    let mut rig = rig();
    let other = onvif_probe(2);
    assert_eq!(deliver(&mut rig, peer(21), &other), 1);
    rig.wire.0.borrow_mut().fail_sends = true;
    assert_eq!(deliver(&mut rig, peer(20), PROBE_ONVIF), 1);
    rig.wire.0.borrow_mut().fail_sends = false;
    assert_eq!(deliver(&mut rig, peer(21), &other), 0, "still suppressed");
    assert_eq!(deliver(&mut rig, peer(20), PROBE_ONVIF), 1);
}

#[test]
fn a_reply_refused_for_a_full_queue_does_not_suppress_the_retransmit() {
    let generous = Limits {
        global_burst: 1000,
        global_per_second: 1000,
        source_burst: 1000,
        ..Limits::default()
    };
    let mut rig = rig_with(config().with_limits(generous));
    let docs: Vec<(SocketAddr, String)> = (0..=MAX_PENDING)
        .map(|n| {
            let src = SocketAddr::from(([192, 0, 2, 100 + u8::try_from(n).unwrap()], 4000));
            (
                src,
                with_id(PROBE_ANY, &format!("aaaaaaaa-bbbb-cccc-dddd-{n:012}")),
            )
        })
        .collect();
    for (src, doc) in &docs {
        rig.wire
            .0
            .borrow_mut()
            .inbox
            .push_back((*src, doc.as_bytes().to_vec()));
        rig.responder.step(Duration::ZERO).unwrap();
    }
    assert_eq!(
        rig.responder.stats().dropped,
        1,
        "the last reply was refused"
    );
    flush(&mut rig);
    let (src, doc) = &docs[MAX_PENDING];
    assert_eq!(
        deliver(&mut rig, *src, doc),
        1,
        "its retransmit is answered"
    );
}

#[test]
fn dropping_one_queued_reply_keeps_what_other_interfaces_already_answered() {
    let (mut rig, wanted) = watched(vec![iface(10), wifi()]);
    let wired = peer(20);
    let wireless = SocketAddr::from((Ipv4Addr::new(198, 51, 100, 30), 40000));
    let answered = PROBE_ONVIF.to_owned();
    assert_eq!(deliver(&mut rig, wired, &answered), 1);
    // A reply queued on the Wi-Fi interface is discarded when that interface vanishes.
    let queued = onvif_probe(2);
    rig.wire
        .0
        .borrow_mut()
        .inbox
        .push_back((wireless, queued.clone().into_bytes()));
    rig.responder.step(POLL).unwrap();
    *wanted.lock().unwrap() = Some(vec![iface(10)]);
    rig.clock.advance(WATCH_EVERY);
    rig.responder.poll_watch();
    assert!(
        rig.responder.pending.is_empty(),
        "the Wi-Fi reply was discarded"
    );
    // The wired probe was already answered: a repeat stays suppressed.
    assert_eq!(deliver(&mut rig, wired, &answered), 0);
    // The dropped one is answered once its interface returns and the client retries.
    *wanted.lock().unwrap() = Some(vec![iface(10), wifi()]);
    rig.clock.advance(WATCH_EVERY);
    rig.responder.poll_watch();
    // Let the `Hello`s for the returning interface go out first; only the reply is counted.
    rig.clock.advance(MAX_DELAY);
    rig.responder.step(POLL).unwrap();
    assert_eq!(deliver(&mut rig, wireless, &queued), 1);
}

#[test]
fn a_stale_forget_does_not_remove_an_entry_recorded_again_after_eviction() {
    // Public `Limits` let a caller raise the budgets far past the dedup capacities.
    let high = Limits {
        global_burst: 100_000,
        global_per_second: 100_000,
        source_burst: 100_000,
        ..Limits::default()
    };
    let mut engine = Engine::new(config().with_limits(high), Counter(1));
    let now = Instant::now();
    let first = onvif_probe(1);
    let p1 = engine.handle(now, peer(20), first.as_bytes()).unwrap();
    // Enough distinct requests to evict both of P1's entries by capacity (raw 64, ids 256).
    for id in 2..2 + 300_u32 {
        engine
            .handle(now, peer(20), onvif_probe(id).as_bytes())
            .unwrap();
    }
    // The client retransmits; both entries are recorded again (same instant) for P2.
    let p2 = engine.handle(now, peer(20), first.as_bytes()).unwrap();
    // P1's send fails late: it must not take P2's entries with it.
    engine.forget(&p1);
    assert!(matches!(
        engine.handle(now, peer(20), first.as_bytes()),
        Err(Dropped::Duplicate)
    ));
    let retry = format!("{PROBE_ONVIF}\n").replace("555555555555", "000000000001");
    assert!(matches!(
        engine.handle(now, peer(20), retry.as_bytes()),
        Err(Dropped::Duplicate)
    ));
    // P2's own failure does forget them.
    engine.forget(&p2);
    assert!(engine.handle(now, peer(20), first.as_bytes()).is_ok());
}
