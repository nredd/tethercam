//! WS-Discovery responder: lets ONVIF clients find this camera on the LAN.
//!
//! The responder joins the discovery multicast group on the chosen interfaces, announces itself
//! with `Hello`, answers matching `Probe` and `Resolve` requests with a unicast reply after the
//! random delay the spec requires, and says `Bye` on shutdown.
//!
//! Layout:
//! - [`Engine`]: the pure protocol core. It takes a datagram and a timestamp and returns the
//!   reply to send (or why it was dropped). No sockets, no sleeping, fully unit-testable
//! - [`Responder`]: drives an [`Engine`] over a [`Transport`] with a [`Clock`], holding replies
//!   until their random delay elapses
//! - [`UdpTransport`]: the real UDP sockets. Tests use an in-memory [`Transport`] for protocol
//!   cases and loopback-only multicast for a socket round-trip
//!
//! Security posture:
//! - Discovery is unauthenticated by design, so it discloses only what ONVIF requires: the
//!   endpoint reference, the fixed ONVIF types, the caller's scopes and one `XAddrs` URL
//! - One reply per valid probe, unicast to the sender, at most [`MAX_REPLY`] bytes. Requests
//!   larger than [`MAX_PACKET`], from off-link or non-unicast sources, malformed, duplicated by
//!   `MessageID`, or over the per-source rate are dropped silently
//! - `XAddrs` carry the address of the interface whose subnet contains the sender. Plain sockets
//!   do not report the arrival interface, so the subnet is the proxy, and an off-link sender is
//!   never answered. Overlapping subnets pick the most specific one; two addresses on the same
//!   subnet cannot be told apart
//! - Amplification: a `ProbeMatches` is necessarily larger than a bare `Probe`, so a ratio of 1
//!   is not achievable. The contract is instead: at most one reply per accepted probe, each at
//!   most [`MAX_REPLY`] bytes, and total replies bounded by one global token bucket
//!   ([`Limits`], default burst 20, 10 per second) that every validated datagram spends from
//!   before it is parsed. Worst-case reflected traffic is therefore
//!   [`Limits::max_reflected_bytes_per_second`] (`global_per_second * MAX_REPLY`, 14000 bytes
//!   per second by default) however many sources, spoofed or not, send probes. A tighter
//!   per-source bucket (burst 4, one per second) sits in front of it, with least-recently-seen
//!   eviction so rotating sources cannot lock legitimate clients out of the table. Identical
//!   copies of a datagram (one per joined socket) are ignored without spending either budget,
//!   and `MessageID`s are de-duplicated
//! - Every socket binds the wildcard address on port 3702 and joins the group on its own
//!   interface, so multicast probes and unicast probes (add-by-IP flows) are both received.
//!   Both take the same validation, de-duplication and rate-limit path
//!
//! Assumptions:
//! - The endpoint reference is generated once by the caller (see
//!   [`EndpointReference::generate`]) and persisted, so the camera keeps its identity across
//!   restarts
//! - Both dialects are served: WS-Discovery 2005/04 with WS-Addressing 2004/08 (what ONVIF-era
//!   clients and NVRs speak) and WS-Discovery 1.1 (2009/01). A reply uses the request's dialect;
//!   `Hello` and `Bye` go out in both
//! - `Hello` is sent once per interface and dialect, not repeated, with the spec's random delay
//!
//! References:
//! - [WS-Discovery 1.1](http://docs.oasis-open.org/ws-dd/discovery/1.1/os/wsdd-discovery-1.1-spec-os.html)
//! - [ONVIF Core Specification](https://www.onvif.org/specs/core/ONVIF-Core-Specification.pdf),
//!   section 7 "Discovery"

mod limiter;
mod message;
mod net;
mod xml;

use limiter::{Dedup, GlobalLimiter, SourceLimiter};
pub use message::{
    AppSequence, Dialect, MAX_MESSAGE_ID, ParseError, ProbeRequest, Request, ResolveRequest,
    Target, parse_request,
};
pub use net::{Datagram, GROUP_V4, GROUP_V6, PORT, Transport, UdpTransport};
pub use xml::{QName, SyntaxError};

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// Largest request accepted, in bytes.
pub const MAX_PACKET: usize = 4096;
/// Largest reply or announcement sent, in bytes. Fits a standard Ethernet MTU.
pub const MAX_REPLY: usize = 1400;
/// Upper bound of the random delay before answering or announcing (`APP_MAX_DELAY`).
pub const MAX_DELAY: Duration = Duration::from_millis(500);

/// Most interfaces served.
const MAX_INTERFACES: usize = 8;
/// Most scopes accepted from the caller.
const MAX_SCOPES: usize = 16;
/// Longest scope accepted from the caller.
const MAX_SCOPE_LEN: usize = 128;
/// Longest `XAddrs` path accepted from the caller.
const MAX_PATH_LEN: usize = 64;
/// Replies held back for their random delay. Beyond this, new replies are dropped.
const MAX_PENDING: usize = 64;
/// How long a `MessageID` is remembered.
const DEDUP_TTL: Duration = Duration::from_secs(60);
/// Most `MessageID`s remembered.
const DEDUP_CAPACITY: usize = 256;
/// How long an identical datagram from the same source is ignored without charging any budget
/// (the same multicast packet can arrive once per joined socket).
const RAW_TTL: Duration = Duration::from_secs(2);
/// Most recent datagrams remembered for that.
const RAW_CAPACITY: usize = 64;
/// Longest single wait in [`Responder::run`], bounding shutdown latency.
const POLL: Duration = Duration::from_millis(100);

/// Why a [`Config`] was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ConfigError {
    /// No usable interface was given.
    #[error("at least one interface with an advertisable address is required")]
    NoInterfaces,
    /// More interfaces than the responder serves.
    #[error("at most {MAX_INTERFACES} interfaces are supported")]
    TooManyInterfaces,
    /// An interface address is unusable or its prefix length is out of range.
    #[error("interface '{0}' is not a unicast address with a valid prefix length")]
    InvalidInterface(IpAddr),
    /// The endpoint reference is not a `urn:uuid:` URN.
    #[error("endpoint reference '{0}' is not a `urn:uuid:` URN")]
    InvalidEndpoint(String),
    /// A scope is empty, non-ASCII (percent-encode it), has whitespace or control characters,
    /// lacks a scheme, or is too long.
    #[error("scope '{0}' is not a valid scope URI")]
    InvalidScope(String),
    /// More scopes than the responder carries.
    #[error("at most {MAX_SCOPES} scopes are supported")]
    TooManyScopes,
    /// The service path does not start with `/` or has unsafe characters.
    #[error("service path '{0}' must start with `/` and contain only URL-safe ASCII")]
    InvalidServicePath(String),
    /// Scopes, addresses and identifiers do not fit [`MAX_REPLY`].
    #[error("description needs '{0}' bytes, more than the {MAX_REPLY}-byte limit")]
    DescriptionTooLarge(usize),
}

/// Why the responder stopped.
#[derive(Debug, Error)]
pub enum Error {
    /// A socket operation failed.
    #[error("discovery socket error")]
    Io(#[from] std::io::Error),
    /// A send named an interface index the transport does not have.
    #[error("no interface with index '{0}'")]
    NoSuchInterface(usize),
}

/// A source of randomness for delays and message ids. Not for secrets: nothing in discovery is
/// secret.
pub trait Entropy {
    /// Next 64 random bits.
    fn next_u64(&mut self) -> u64;
}

/// [`Entropy`] seeded per call from the standard library's randomly keyed hasher.
#[derive(Debug, Default)]
pub struct SystemEntropy {
    counter: u64,
}

impl Entropy for SystemEntropy {
    fn next_u64(&mut self) -> u64 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(self.counter);
        self.counter = self.counter.wrapping_add(1);
        hasher.finish()
    }
}

/// A monotonic time source, injectable so tests need not sleep.
pub trait Clock {
    /// The current instant.
    fn now(&self) -> Instant;
}

/// The real monotonic clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// The stable `urn:uuid:` identity of this camera on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointReference(String);

impl EndpointReference {
    /// Validate and normalize (to lowercase) an existing `urn:uuid:` reference.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidEndpoint`] if `s` is not `urn:uuid:` plus a 8-4-4-4-12 hex
    /// UUID.
    pub fn parse(s: &str) -> Result<Self, ConfigError> {
        let invalid = || ConfigError::InvalidEndpoint(s.to_owned());
        let uuid = s
            .get(..9)
            .filter(|p| p.eq_ignore_ascii_case("urn:uuid:"))
            .and_then(|_| s.get(9..))
            .ok_or_else(invalid)?;
        let groups: Vec<&str> = uuid.split('-').collect();
        let shape = [8, 4, 4, 4, 12];
        let ok = groups.len() == shape.len()
            && groups
                .iter()
                .zip(shape)
                .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()));
        if ok {
            Ok(Self(s.to_ascii_lowercase()))
        } else {
            Err(invalid())
        }
    }

    /// Use an already validated persisted UUID without reparsing its canonical text.
    #[must_use]
    pub fn from_uuid(uuid: uuid::Uuid) -> Self {
        Self(format!("urn:uuid:{uuid}"))
    }

    /// Generate a fresh random (version 4) reference. The caller persists it.
    pub fn generate(entropy: &mut impl Entropy) -> Self {
        Self(format!("urn:uuid:{}", random_uuid(entropy)))
    }

    /// The reference as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A random version 4 UUID in canonical text form.
fn random_uuid(entropy: &mut impl Entropy) -> String {
    let mut v = (u128::from(entropy.next_u64()) << 64) | u128::from(entropy.next_u64());
    v = (v & !(0xf_u128 << 76)) | (0x4_u128 << 76);
    v = (v & !(0x3_u128 << 62)) | (0x2_u128 << 62);
    let hex = format!("{v:032x}");
    let part = |r: std::ops::Range<usize>| hex.get(r).unwrap_or_default().to_owned();
    format!(
        "{}-{}-{}-{}-{}",
        part(0..8),
        part(8..12),
        part(12..16),
        part(16..20),
        part(20..32)
    )
}

/// A network interface the responder serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interface {
    /// The interface's unicast address, used in `XAddrs`.
    pub addr: IpAddr,
    /// Prefix length of the interface's subnet.
    pub prefix_len: u8,
    /// OS interface index, needed to join the IPv6 group. Ignored for IPv4.
    pub index: u32,
}

impl Interface {
    fn is_valid(&self) -> bool {
        let max = if self.addr.is_ipv4() { 32 } else { 128 };
        !self.addr.is_unspecified() && !self.addr.is_multicast() && self.prefix_len <= max
    }

    /// Whether `ip` is on this interface's subnet.
    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(a), IpAddr::V4(b)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix_len.min(32)))
                    .unwrap_or(0);
                u32::from(a) & mask == u32::from(b) & mask
            }
            (IpAddr::V6(a), IpAddr::V6(b)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix_len.min(128)))
                    .unwrap_or(0);
                u128::from(a) & mask == u128::from(b) & mask
            }
            _ => false,
        }
    }

    fn group(&self) -> SocketAddr {
        match self.addr {
            IpAddr::V4(_) => SocketAddr::new(GROUP_V4.into(), PORT),
            IpAddr::V6(_) => SocketAddr::new(GROUP_V6.into(), PORT),
        }
    }
}

/// Where the Device service listens, as advertised in `XAddrs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceAddress {
    /// TCP port of the Device service.
    pub port: u16,
    /// URL path of the Device service, e.g. `/onvif/device_service`.
    pub path: String,
}

/// Rate limits. Two layers: a per-source bucket, then one global bucket shared by all sources.
///
/// Every datagram that passes validation spends one token from each before it is parsed, so
/// replies are bounded by the global bucket however many (spoofed) sources there are:
/// worst-case reflected traffic is `global_per_second` x [`MAX_REPLY`] bytes per second, plus
/// the one-off `global_burst` x [`MAX_REPLY`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Datagrams accepted in a burst across all sources.
    pub global_burst: u32,
    /// Tokens returned to the global bucket per second.
    pub global_per_second: u32,
    /// Datagrams accepted in a burst from one source.
    pub source_burst: u32,
    /// One per-source token returns per this interval.
    pub source_refill: Duration,
    /// Sources tracked; the least recently seen is evicted when full.
    pub source_capacity: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            global_burst: 20,
            global_per_second: 10,
            source_burst: 4,
            source_refill: Duration::from_secs(1),
            source_capacity: 256,
        }
    }
}

impl Limits {
    /// Highest supported `global_per_second` (one token per microsecond).
    pub const MAX_PER_SECOND: u32 = 1_000_000;

    /// Upper bound on sustained reflected bytes per second, whatever the number of sources.
    #[must_use]
    pub fn max_reflected_bytes_per_second(&self) -> u64 {
        u64::from(self.global_per_second) * MAX_REPLY as u64
    }

    fn global_refill(&self) -> Duration {
        // Rounded up, so the interval never admits more than `global_per_second` tokens/s.
        Duration::from_nanos(1_000_000_000_u64.div_ceil(u64::from(self.global_per_second.max(1))))
    }
}

/// Everything the responder needs to know about this camera.
#[derive(Debug, Clone)]
pub struct Config {
    endpoint: EndpointReference,
    scopes: Vec<String>,
    interfaces: Vec<Interface>,
    service: ServiceAddress,
    metadata_version: u32,
    instance_id: u32,
    limits: Limits,
}

impl Config {
    /// Validate and assemble a configuration. The instance id defaults to the current Unix
    /// time, so it grows across restarts as `AppSequence` requires (two restarts inside one
    /// second would collide; callers that persist a counter should use
    /// [`Config::with_instance_id`]).
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] for unusable interfaces, scopes or service path, or a
    /// description too large to fit one datagram.
    pub fn new(
        endpoint: EndpointReference, scopes: Vec<String>, interfaces: Vec<Interface>,
        service: ServiceAddress,
    ) -> Result<Self, ConfigError> {
        if interfaces.is_empty() {
            return Err(ConfigError::NoInterfaces);
        }
        if interfaces.len() > MAX_INTERFACES {
            return Err(ConfigError::TooManyInterfaces);
        }
        if let Some(bad) = interfaces.iter().find(|i| !i.is_valid()) {
            return Err(ConfigError::InvalidInterface(bad.addr));
        }
        if scopes.len() > MAX_SCOPES {
            return Err(ConfigError::TooManyScopes);
        }
        if let Some(bad) = scopes.iter().find(|s| !valid_scope(s)) {
            return Err(ConfigError::InvalidScope(bad.clone()));
        }
        let path_ok = service.path.starts_with('/')
            && service.path.len() <= MAX_PATH_LEN
            && service
                .path
                .bytes()
                .all(|b| b.is_ascii_graphic() && !matches!(b, b'<' | b'>' | b'"' | b'\'' | b'&'));
        if !path_ok {
            return Err(ConfigError::InvalidServicePath(service.path));
        }
        // Link-local senders still need their subnet for matching, but an unscoped URL is
        // unusable. Keep such entries only when the same interface has an IPv6 URL address.
        let advertisable: Vec<_> = interfaces
            .iter()
            .filter_map(|i| match i.addr {
                IpAddr::V6(a) if !a.is_unicast_link_local() => Some(i.index),
                _ => None,
            })
            .collect();
        let interfaces: Vec<_> = interfaces
            .into_iter()
            .filter(|i| {
                let usable = !matches!(i.addr, IpAddr::V6(a) if a.is_unicast_link_local())
                    || (i.index != 0 && advertisable.contains(&i.index));
                if !usable {
                    log::warn!("Omitting IPv6 discovery on interface index '{}' because it has no non-link-local IPv6 address", i.index);
                }
                usable
            })
            .collect();
        if interfaces.is_empty() {
            return Err(ConfigError::NoInterfaces);
        }
        let instance_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| u32::try_from(d.as_secs()).ok())
            .unwrap_or(1);
        let config = Self {
            endpoint,
            scopes,
            interfaces,
            service,
            metadata_version: 1,
            instance_id,
            limits: Limits::default(),
        };
        config.check_size()?;
        Ok(config)
    }

    /// Set the `MetadataVersion`; bump it whenever scopes or addresses change.
    #[must_use]
    pub fn with_metadata_version(mut self, version: u32) -> Self {
        self.metadata_version = version;
        self
    }

    /// Set the `AppSequence` instance id (default: Unix time at construction).
    #[must_use]
    pub fn with_instance_id(mut self, id: u32) -> Self {
        self.instance_id = id;
        self
    }

    /// Replace the default [`Limits`]. Values are clamped to what the buckets represent
    /// exactly: every count is at least 1 and `global_per_second` is at most
    /// [`Limits::MAX_PER_SECOND`], so [`Limits::max_reflected_bytes_per_second`] on the stored
    /// limits is always a true bound.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = Limits {
            global_burst: limits.global_burst.max(1),
            global_per_second: limits.global_per_second.clamp(1, Limits::MAX_PER_SECOND),
            source_burst: limits.source_burst.max(1),
            source_refill: limits.source_refill.max(Duration::from_millis(1)),
            source_capacity: limits.source_capacity.max(1),
        };
        self
    }

    /// The limits in force, after clamping.
    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Keep only the interfaces whose entry in `usable` is `true` (missing entries count as
    /// unusable), so subnet matching and numbering follow the sockets that actually bound.
    #[must_use]
    pub fn with_usable_interfaces(mut self, usable: &[bool]) -> Self {
        let mut flags = usable.iter();
        self.interfaces
            .retain(|_| flags.next().copied().unwrap_or(false));
        self
    }

    /// The interfaces served, in the order [`Transport`] sees them.
    #[must_use]
    pub fn interfaces(&self) -> &[Interface] {
        &self.interfaces
    }

    /// Worst case: the longest `XAddrs` and the longest echoed `MessageID`, with the largest
    /// numbers in every counter.
    fn check_size(&self) -> Result<(), ConfigError> {
        let seq = AppSequence {
            instance_id: u32::MAX,
            message_number: u32::MAX,
        };
        let worst_id = "x".repeat(MAX_MESSAGE_ID);
        let uuid_id = format!("urn:uuid:{}", "0".repeat(36));
        let size = self
            .interfaces
            .iter()
            .map(|i| {
                let xaddr = self.xaddr(i);
                let target = Target {
                    endpoint: self.endpoint.as_str(),
                    scopes: &self.scopes,
                    xaddr: &xaddr,
                    metadata_version: u32::MAX,
                };
                // `MAX_MESSAGE_ID` bounds the escaped identifier, so this is the worst `RelatesTo`.
                Dialect::ALL
                    .into_iter()
                    .flat_map(|d| {
                        [
                            message::build_probe_matches(d, &uuid_id, &worst_id, seq, &target),
                            message::build_resolve_matches(d, &uuid_id, &worst_id, seq, &target),
                        ]
                    })
                    .map(|m| m.len())
                    .max()
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(0);
        if size > MAX_REPLY {
            Err(ConfigError::DescriptionTooLarge(size))
        } else {
            Ok(())
        }
    }

    fn xaddr(&self, interface: &Interface) -> String {
        let addr = match interface.addr {
            IpAddr::V6(a) if a.is_unicast_link_local() => self
                .interfaces
                .iter()
                .find_map(|i| match i.addr {
                    IpAddr::V6(candidate)
                        if i.index == interface.index && !candidate.is_unicast_link_local() =>
                    {
                        Some(i.addr)
                    }
                    _ => None,
                })
                .unwrap_or(interface.addr),
            addr => addr,
        };
        let host = match addr {
            IpAddr::V4(a) => a.to_string(),
            IpAddr::V6(a) => format!("[{a}]"),
        };
        let port = if self.service.port == 80 {
            String::new()
        } else {
            format!(":{}", self.service.port)
        };
        format!("http://{host}{port}{}", self.service.path)
    }
}

fn valid_scope(s: &str) -> bool {
    s.len() <= MAX_SCOPE_LEN
        && s.contains(':')
        && !s.starts_with(':')
        && s.is_ascii()
        && s.chars().all(|c| !c.is_whitespace() && !c.is_control())
}

/// Why a datagram or announcement produced no reply.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Dropped {
    /// The datagram is larger than [`MAX_PACKET`].
    #[error("datagram too large")]
    TooLarge,
    /// The source port is 0 or the address is not a unicast address.
    #[error("bad source address")]
    BadSource,
    /// The source is not on any served interface's subnet.
    #[error("source is off-link")]
    OffLink,
    /// The source or the shared global budget is exhausted.
    #[error("source rate limited")]
    RateLimited,
    /// The datagram is not a well-formed `Probe` or `Resolve`.
    #[error("malformed request: {0}")]
    Malformed(ParseError),
    /// The `MessageID`, or this exact datagram, was seen recently.
    #[error("duplicate message id")]
    Duplicate,
    /// The request does not match this target.
    #[error("request does not match")]
    NoMatch,
    /// The reply would exceed [`MAX_REPLY`].
    #[error("reply too large")]
    ReplyTooLarge,
}

/// Identity of a datagram for suppressing identical copies: sender plus a hash of the bytes.
fn raw_key(keys: &RandomState, source: SocketAddr, data: &[u8]) -> String {
    let mut hasher = keys.build_hasher();
    hasher.write(data);
    format!("{source}/{}/{:016x}", data.len(), hasher.finish())
}

/// A datagram to send once `due` has passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    /// Earliest send time (the spec's random delay).
    pub due: Instant,
    /// Destination: the requester, or the multicast group.
    pub dest: SocketAddr,
    /// Index into [`Config::interfaces`] to send from.
    pub interface: usize,
    /// The SOAP envelope.
    pub payload: Vec<u8>,
}

/// The pure protocol core.
#[derive(Debug)]
pub struct Engine<E> {
    config: Config,
    entropy: E,
    dedup: Dedup,
    raw: Dedup,
    raw_keys: RandomState,
    limiter: SourceLimiter,
    global: GlobalLimiter,
    message_number: u32,
}

impl<E: Entropy> Engine<E> {
    /// Create an engine for `config`.
    #[must_use]
    pub fn new(config: Config, entropy: E) -> Self {
        let limits = config.limits;
        Self {
            entropy,
            dedup: Dedup::new(DEDUP_TTL, DEDUP_CAPACITY),
            raw: Dedup::new(RAW_TTL, RAW_CAPACITY),
            raw_keys: RandomState::new(),
            limiter: SourceLimiter::new(
                limits.source_burst,
                limits.source_refill,
                limits.source_capacity,
            ),
            global: GlobalLimiter::new(limits.global_burst, limits.global_refill()),
            config,
            message_number: 0,
        }
    }

    /// Answer one received datagram.
    ///
    /// Checks run cheapest first. Both rate limits (per source, then global) are charged before
    /// any parsing, so garbage costs its sender its budget and total work and replies are
    /// bounded by the global bucket regardless of the number of sources.
    ///
    /// # Errors
    ///
    /// Returns the [`Dropped`] reason when no reply is due.
    pub fn handle(
        &mut self, now: Instant, source: SocketAddr, data: &[u8],
    ) -> Result<Outgoing, Dropped> {
        if data.len() > MAX_PACKET {
            return Err(Dropped::TooLarge);
        }
        let ip = source.ip();
        if source.port() == 0 || ip.is_multicast() || ip.is_unspecified() {
            return Err(Dropped::BadSource);
        }
        let interface = self.arrival_interface(source).ok_or(Dropped::OffLink)?;
        // Copies are only remembered once admitted, so a datagram refused for budget can be
        // retried after the bucket refills.
        let raw_key = raw_key(&self.raw_keys, source, data);
        if self.raw.contains(&raw_key, now) {
            return Err(Dropped::Duplicate);
        }
        if !self.limiter.allow(ip, now) || !self.global.allow(now) {
            return Err(Dropped::RateLimited);
        }
        self.raw.insert(&raw_key, now);
        let request = parse_request(data).map_err(Dropped::Malformed)?;
        let (message_id, matched) = match &request {
            Request::Probe(p) => (
                p.message_id.as_str(),
                message::types_match(&p.types)
                    && message::scopes_match(
                        p.dialect,
                        p.match_by.as_deref(),
                        &p.scopes,
                        &self.config.scopes,
                    ),
            ),
            Request::Resolve(r) => (
                r.message_id.as_str(),
                r.endpoint
                    .eq_ignore_ascii_case(self.config.endpoint.as_str()),
            ),
        };
        if !self.dedup.insert(message_id, now) {
            return Err(Dropped::Duplicate);
        }
        if !matched {
            return Err(Dropped::NoMatch);
        }
        let reply_id = format!("urn:uuid:{}", random_uuid(&mut self.entropy));
        let seq = self.next_sequence();
        let xaddr = self.xaddr_at(interface);
        let target = self.target(&xaddr);
        let payload = match &request {
            Request::Probe(p) => {
                message::build_probe_matches(p.dialect, &reply_id, &p.message_id, seq, &target)
            }
            Request::Resolve(r) => {
                message::build_resolve_matches(r.dialect, &reply_id, &r.message_id, seq, &target)
            }
        };
        Ok(Outgoing {
            due: self.delayed(now),
            dest: source,
            interface,
            payload: payload.into_bytes(),
        })
    }

    /// The interface a datagram from `source` most plausibly arrived on: the one with the most
    /// specific subnet containing it. A scoped IPv6 sender (`scope_id != 0`) only matches the
    /// interface with that index. Ties go to the first configured.
    fn arrival_interface(&self, source: SocketAddr) -> Option<usize> {
        let scope = match source {
            SocketAddr::V6(v6) if v6.scope_id() != 0 => Some(v6.scope_id()),
            _ => None,
        };
        self.config
            .interfaces
            .iter()
            .enumerate()
            .filter(|(_, i)| i.contains(source.ip()) && scope.is_none_or(|s| s == i.index))
            .min_by_key(|(n, i)| (std::cmp::Reverse(i.prefix_len), *n))
            .map(|(n, _)| n)
    }

    /// `Hello` announcements, each with its own random delay.
    pub fn hello(&mut self, now: Instant) -> Vec<Outgoing> {
        self.announce(now, true)
    }

    /// `Bye` announcements, due immediately.
    pub fn bye(&mut self, now: Instant) -> Vec<Outgoing> {
        self.announce(now, false)
    }

    /// One message per interface and dialect. ONVIF-era clients listen for the 2005/04
    /// dialect, WS-Discovery 1.1 clients for 2009/01, so both are announced.
    fn announce(&mut self, now: Instant, hello: bool) -> Vec<Outgoing> {
        let mut out = Vec::new();
        for (interface, i) in self.config.interfaces.clone().into_iter().enumerate() {
            for dialect in Dialect::ALL {
                let id = format!("urn:uuid:{}", random_uuid(&mut self.entropy));
                let seq = self.next_sequence();
                let xaddr = self.xaddr_at(interface);
                let target = self.target(&xaddr);
                let payload = if hello {
                    message::build_hello(dialect, &id, seq, &target)
                } else {
                    message::build_bye(dialect, &id, seq, &target)
                };
                let due = if hello { self.delayed(now) } else { now };
                out.push(Outgoing {
                    due,
                    dest: i.group(),
                    interface,
                    payload: payload.into_bytes(),
                });
            }
        }
        out
    }

    fn target<'a>(&'a self, xaddr: &'a str) -> Target<'a> {
        Target {
            endpoint: self.config.endpoint.as_str(),
            scopes: &self.config.scopes,
            xaddr,
            metadata_version: self.config.metadata_version,
        }
    }

    fn xaddr_at(&self, interface: usize) -> String {
        self.config
            .interfaces
            .get(interface)
            .map(|i| self.config.xaddr(i))
            .unwrap_or_default()
    }

    fn next_sequence(&mut self) -> AppSequence {
        self.message_number = self.message_number.wrapping_add(1);
        AppSequence {
            instance_id: self.config.instance_id,
            message_number: self.message_number,
        }
    }

    /// `now` plus a uniform random delay in `0..=MAX_DELAY`.
    fn delayed(&mut self, now: Instant) -> Instant {
        let max_ms = u64::try_from(MAX_DELAY.as_millis()).unwrap_or(500);
        let delay = Duration::from_millis(self.entropy.next_u64() % (max_ms + 1));
        now.checked_add(delay).unwrap_or(now)
    }
}

/// Counters for observing a running [`Responder`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Datagrams received.
    pub received: u64,
    /// Replies and announcements sent.
    pub sent: u64,
    /// Datagrams that produced no reply.
    pub dropped: u64,
    /// Sends that failed at the socket.
    pub send_errors: u64,
}

/// Runs an [`Engine`] over a [`Transport`].
#[derive(Debug)]
pub struct Responder<T, E, C> {
    engine: Engine<E>,
    transport: T,
    clock: C,
    pending: Vec<Outgoing>,
    buf: Vec<u8>,
    stats: Stats,
}

impl<T: Transport, E: Entropy, C: Clock> Responder<T, E, C> {
    /// Assemble a responder. Nothing is sent until [`Responder::start`].
    #[must_use]
    pub fn new(config: Config, entropy: E, transport: T, clock: C) -> Self {
        Self {
            engine: Engine::new(config, entropy),
            transport,
            clock,
            pending: Vec::new(),
            buf: vec![0; MAX_PACKET + 1],
            stats: Stats::default(),
        }
    }

    /// Counters since creation.
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Queue the `Hello` announcements.
    pub fn start(&mut self) {
        let now = self.clock.now();
        let hellos = self.engine.hello(now);
        self.pending.extend(hellos);
    }

    /// Wait up to `max_wait` for one datagram (less if a queued reply falls due sooner), handle
    /// it, and send everything that is due.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if receiving fails. Send failures are counted in [`Stats`], not
    /// fatal: one unreachable peer must not stop discovery.
    pub fn step(&mut self, max_wait: Duration) -> Result<(), Error> {
        let before = self.clock.now();
        let wait = self
            .pending
            .iter()
            .map(|p| p.due.saturating_duration_since(before))
            .min()
            .map_or(max_wait, |d| d.min(max_wait));
        let received = self.transport.recv(&mut self.buf, wait)?;
        let now = self.clock.now();
        if let Some(datagram) = received {
            self.stats.received += 1;
            let data = self.buf.get(..datagram.len).unwrap_or_default();
            match self.engine.handle(now, datagram.source, data) {
                Ok(out) if self.pending.len() < MAX_PENDING => self.pending.push(out),
                Ok(_) | Err(_) => self.stats.dropped += 1,
            }
        }
        self.flush_due(now);
        Ok(())
    }

    /// Send `Bye` on every interface immediately and discard queued replies.
    pub fn shutdown(&mut self) {
        self.pending.clear();
        let now = self.clock.now();
        for out in self.engine.bye(now) {
            self.send(&out);
        }
    }

    /// Announce, serve until `stop` is set, then say `Bye`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if receiving fails. `Bye` is still attempted.
    pub fn run(&mut self, stop: &AtomicBool) -> Result<(), Error> {
        self.start();
        let mut result = Ok(());
        while !stop.load(Ordering::Relaxed) {
            if let Err(e) = self.step(POLL) {
                result = Err(e);
                break;
            }
        }
        self.shutdown();
        result
    }

    fn flush_due(&mut self, now: Instant) {
        let (mut due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|p| p.due <= now);
        self.pending = later;
        due.sort_by_key(|p| p.due);
        for out in &due {
            self.send(out);
        }
    }

    fn send(&mut self, out: &Outgoing) {
        match self.transport.send(out.interface, out.dest, &out.payload) {
            Ok(()) => self.stats.sent += 1,
            Err(_) => self.stats.send_errors += 1,
        }
    }
}

#[cfg(test)]
mod test_log;
#[cfg(test)]
mod tests;
