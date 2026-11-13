//! The socket seam: a [`Transport`] trait and the real UDP implementation.
//!
//! Tests drive the responder through an in-memory `Transport`; [`UdpTransport`] is the only code
//! that touches the network.
//!
//! References:
//! - WS-Discovery 1.1, section 2.4 "Transport bindings" (UDP port 3702, IPv4 group
//!   `239.255.255.250`, IPv6 group `ff02::c`)

use super::{Error, Interface};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// The WS-Discovery UDP port.
pub const PORT: u16 = 3702;
/// The WS-Discovery IPv4 multicast group.
pub const GROUP_V4: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
/// The WS-Discovery IPv6 link-local multicast group.
pub const GROUP_V6: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xc);

/// How long to sleep between polls of the non-blocking sockets.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// A received datagram: its length in the caller's buffer and where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Datagram {
    /// Bytes written to the receive buffer.
    pub len: usize,
    /// Sender address.
    pub source: SocketAddr,
}

/// Datagram I/O for the responder.
pub trait Transport {
    /// Wait up to `timeout` for one datagram.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying socket fails.
    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> Result<Option<Datagram>, Error>;

    /// Send `payload` to `dest` out of interface number `interface`.
    ///
    /// # Errors
    ///
    /// Returns an error if the datagram could not be sent.
    fn send(&mut self, interface: usize, dest: SocketAddr, payload: &[u8]) -> Result<(), Error>;
}

/// One UDP socket per interface, bound to the wildcard address on the discovery port and
/// joined to the group on its interface. Interfaces that could not be set up are skipped; pass
/// [`UdpTransport::usable`] to `Config::with_usable_interfaces` so the responder numbers
/// interfaces like the sockets.
#[derive(Debug)]
pub struct UdpTransport {
    sockets: Vec<UdpSocket>,
    usable: Vec<bool>,
    /// Socket polled first on the next receive; advances after every datagram so a flood on
    /// one interface cannot starve the others.
    next: usize,
}

impl UdpTransport {
    /// Bind to the discovery port and join the multicast group on every interface.
    ///
    /// An interface that cannot be set up is skipped with a logged warning.
    ///
    /// # Errors
    ///
    /// Returns the last setup error if `interfaces` is not empty and none of them is usable.
    pub fn bind(interfaces: &[Interface]) -> Result<Self, Error> {
        Self::bind_inner(interfaces, PORT, true)
    }

    /// `join: false` skips only the multicast join and egress setup, so tests exercise the
    /// production bind without touching a multicast group.
    fn bind_inner(interfaces: &[Interface], port: u16, join: bool) -> Result<Self, Error> {
        let mut last_error = None;
        let mut sockets = Vec::new();
        let mut usable = Vec::new();
        for (n, i) in interfaces.iter().enumerate() {
            let opened = open(i, port, join);
            usable.push(opened.is_ok());
            match opened {
                Ok(socket) => sockets.push(socket),
                Err(error) => {
                    let (family, kind) = (if i.addr.is_ipv4() { 4 } else { 6 }, error.kind());
                    let (slot, total, os_index) = (n + 1, interfaces.len(), i.index);
                    log::warn!(
                        "Skipping IPv{family} discovery interface {slot}/{total} (OS index {os_index}): {kind:?}"
                    );
                    last_error = Some(error);
                }
            }
        }
        match last_error {
            Some(error) if sockets.is_empty() => Err(error.into()),
            _ => Ok(Self {
                sockets,
                usable,
                next: 0,
            }),
        }
    }

    /// Per input interface, whether it bound; feed to `Config::with_usable_interfaces`.
    #[must_use]
    pub fn usable(&self) -> &[bool] {
        &self.usable
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MulticastInterface {
    V4(Ipv4Addr),
    V6(u32),
}

impl From<&Interface> for MulticastInterface {
    fn from(interface: &Interface) -> Self {
        match interface.addr {
            IpAddr::V4(addr) => Self::V4(addr),
            IpAddr::V6(_) => Self::V6(interface.index),
        }
    }
}

impl MulticastInterface {
    fn binding(self, port: u16) -> (Domain, SocketAddr) {
        match self {
            Self::V4(_) => (
                Domain::IPV4,
                SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), port),
            ),
            Self::V6(_) => (
                Domain::IPV6,
                SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port),
            ),
        }
    }
}

fn open(interface: &Interface, port: u16, join: bool) -> io::Result<UdpSocket> {
    // Wildcard bind on every platform, so unicast probes (add-by-IP flows) arrive as well as
    // multicast ones. Several sockets share the port: each interface joins the group on its own.
    let multicast = MulticastInterface::from(interface);
    let (domain, bind_addr) = multicast.binding(port);
    finish_open(
        Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)),
        &socket_options(multicast, bind_addr, join),
    )
}

/// A concrete setup operation, kept separate from orchestration so OS failures can be
/// exercised without exhausting process descriptors or changing host network state.
#[derive(Debug, Clone, Copy)]
enum SocketOption {
    ReuseAddress,
    #[cfg(all(
        unix,
        not(any(
            target_os = "solaris",
            target_os = "illumos",
            target_os = "cygwin",
            target_os = "nuttx"
        ))
    ))]
    ReusePort,
    OnlyV6,
    Bind(SocketAddr),
    JoinV4(Ipv4Addr),
    JoinV6(u32),
    InterfaceV4(Ipv4Addr),
    TtlV4,
    LoopV4,
    HopsV6,
    LoopV6,
    InterfaceV6(u32),
    Nonblocking,
}

impl SocketOption {
    fn apply(self, socket: &Socket) -> io::Result<()> {
        match self {
            Self::ReuseAddress => socket.set_reuse_address(true),
            #[cfg(all(
                unix,
                not(any(
                    target_os = "solaris",
                    target_os = "illumos",
                    target_os = "cygwin",
                    target_os = "nuttx"
                ))
            ))]
            Self::ReusePort => socket.set_reuse_port(true),
            Self::OnlyV6 => socket.set_only_v6(true),
            Self::Bind(addr) => socket.bind(&SockAddr::from(addr)),
            Self::JoinV4(addr) => socket.join_multicast_v4(&GROUP_V4, &addr),
            Self::JoinV6(index) => socket.join_multicast_v6(&GROUP_V6, index),
            Self::InterfaceV4(addr) => socket.set_multicast_if_v4(&addr),
            Self::TtlV4 => socket.set_multicast_ttl_v4(1),
            Self::LoopV4 => socket.set_multicast_loop_v4(false),
            Self::HopsV6 => socket.set_multicast_hops_v6(1),
            Self::LoopV6 => socket.set_multicast_loop_v6(false),
            Self::InterfaceV6(index) => socket.set_multicast_if_v6(index),
            Self::Nonblocking => socket.set_nonblocking(true),
        }
    }
}

/// Preserve setup order: share, restrict family, bind, join, configure egress, nonblock.
fn socket_options(
    multicast: MulticastInterface, bind_addr: SocketAddr, join: bool,
) -> Vec<SocketOption> {
    let mut options = Vec::new();
    // Sharing only matters for the well-known port. An ephemeral port (0) must stay exclusive:
    // with `SO_REUSEPORT` set, Linux may hand the same ephemeral port to two sockets.
    if bind_addr.port() != 0 {
        options.push(SocketOption::ReuseAddress);
        // BSD-derived stacks need `SO_REUSEPORT` to share a UDP discovery port.
        #[cfg(all(
            unix,
            not(any(
                target_os = "solaris",
                target_os = "illumos",
                target_os = "cygwin",
                target_os = "nuttx"
            ))
        ))]
        options.push(SocketOption::ReusePort);
    }
    if matches!(multicast, MulticastInterface::V6(_)) {
        options.push(SocketOption::OnlyV6);
    }
    options.push(SocketOption::Bind(bind_addr));
    if join {
        match multicast {
            MulticastInterface::V4(addr) => options.extend([
                SocketOption::JoinV4(addr),
                SocketOption::InterfaceV4(addr),
                SocketOption::TtlV4,
                SocketOption::LoopV4,
            ]),
            MulticastInterface::V6(index) => options.extend([
                SocketOption::JoinV6(index),
                SocketOption::HopsV6,
                SocketOption::LoopV6,
                SocketOption::InterfaceV6(index),
            ]),
        }
    }
    options.push(SocketOption::Nonblocking);
    options
}

/// Propagate creation and setup errors without publishing a partially configured socket.
fn finish_open(socket: io::Result<Socket>, options: &[SocketOption]) -> io::Result<UdpSocket> {
    let socket = socket?;
    for option in options {
        option.apply(&socket)?;
    }
    Ok(socket.into())
}

fn retry_receive(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted | io::ErrorKind::ConnectionReset
    )
}

/// Classify one OS receive result, including transient reset errors on Windows.
fn received(result: io::Result<(usize, SocketAddr)>) -> Result<Option<Datagram>, Error> {
    match result {
        Ok((len, source)) => Ok(Some(Datagram { len, source })),
        Err(error) if retry_receive(&error) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// The concrete receive syscall signature avoids generic coverage instantiations.
type Receive = fn(&UdpSocket, &mut [u8]) -> io::Result<(usize, SocketAddr)>;

impl UdpTransport {
    /// Poll real sockets through one receive syscall seam; tests can report fatal OS errors
    /// without invalidating descriptors (which would require unsafe code).
    fn recv_with(
        &mut self, buf: &mut [u8], timeout: Duration, receive: Receive,
    ) -> Result<Option<Datagram>, Error> {
        let start = Instant::now();
        loop {
            let count = self.sockets.len();
            for (index, socket) in self
                .sockets
                .iter()
                .enumerate()
                .cycle()
                .skip(self.next)
                .take(count)
            {
                if let Some(datagram) = received(receive(socket, buf))? {
                    self.next = (index + 1) % count;
                    return Ok(Some(datagram));
                }
            }
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return Ok(None);
            }
            std::thread::sleep(POLL_INTERVAL.min(timeout.saturating_sub(elapsed)));
        }
    }
}

impl Transport for UdpTransport {
    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> Result<Option<Datagram>, Error> {
        self.recv_with(buf, timeout, UdpSocket::recv_from)
    }

    fn send(&mut self, interface: usize, dest: SocketAddr, payload: &[u8]) -> Result<(), Error> {
        let socket = self
            .sockets
            .get(interface)
            .ok_or(Error::NoSuchInterface(interface))?;
        socket
            .send_to(payload, dest)
            .map(|_| ())
            .map_err(Error::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sock(transport: &UdpTransport, index: usize) -> &UdpSocket {
        &transport.sockets[index]
    }

    fn missing() -> Interface {
        Interface {
            addr: Ipv6Addr::LOCALHOST.into(),
            prefix_len: 128,
            index: u32::MAX,
        }
    }

    fn loopback() -> Interface {
        Interface {
            addr: Ipv4Addr::LOCALHOST.into(),
            prefix_len: 8,
            index: 0,
        }
    }

    #[test]
    fn socket_plan_and_receive_errors_are_family_specific() {
        let v4 = MulticastInterface::from(&loopback());
        let v6 = MulticastInterface::from(&Interface {
            addr: Ipv6Addr::LOCALHOST.into(),
            prefix_len: 128,
            index: 0,
        });
        assert_eq!(
            v4.binding(123).1,
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 123))
        );
        assert_eq!(
            v6.binding(123).1,
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, 123))
        );
        for kind in [
            io::ErrorKind::WouldBlock,
            io::ErrorKind::Interrupted,
            io::ErrorKind::ConnectionReset,
        ] {
            assert!(retry_receive(&io::Error::from(kind)));
        }
        assert!(!retry_receive(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
        assert!(UdpTransport::bind(&[]).unwrap().sockets.is_empty());
    }

    #[test]
    fn multicast_options_and_ipv4_join_use_loopback_without_sending_multicast() {
        let v4 = open(&loopback(), 0, true).unwrap();
        let socket = Socket::from(v4);
        assert_eq!(socket.multicast_if_v4().unwrap(), Ipv4Addr::LOCALHOST);
        assert_eq!(socket.multicast_ttl_v4().unwrap(), 1);
        assert!(!socket.multicast_loop_v4().unwrap());
        let v6 = Interface {
            addr: Ipv6Addr::LOCALHOST.into(),
            prefix_len: 128,
            index: 0,
        };
        let _v6 = open(&v6, 0, false).unwrap();
        let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        SocketOption::HopsV6.apply(&socket).unwrap();
        SocketOption::LoopV6.apply(&socket).unwrap();
        assert_eq!(socket.multicast_hops_v6().unwrap(), 1);
        assert!(!socket.multicast_loop_v6().unwrap());
        // Joining with a nonexistent interface fails on both macOS and Linux, without
        // requiring a multicast route or changing any permanent interface configuration.
        assert!(SocketOption::JoinV6(u32::MAX).apply(&socket).is_err());
        assert!(SocketOption::InterfaceV6(u32::MAX).apply(&socket).is_err());
        let plan = socket_options(
            MulticastInterface::V6(u32::MAX),
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)),
            true,
        );
        assert!(finish_open(Ok(socket), &plan).is_err());
    }

    #[test]
    fn socket_creation_and_setup_failures_do_not_publish_a_socket() {
        let invalid_domain = Socket::new(Domain::from(-1), Type::DGRAM, Some(Protocol::UDP));
        assert!(finish_open(invalid_domain, &[]).is_err());
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        // Binding a v4 socket to a v6 endpoint is a genuine, unprivileged OS error.
        assert!(
            finish_open(
                Ok(socket),
                &[SocketOption::Bind(SocketAddr::from((
                    Ipv6Addr::LOCALHOST,
                    0
                )))],
            )
            .is_err()
        );
        assert!(UdpTransport::bind_inner(&[missing()], 0, true).is_err());
        assert!(UdpTransport::bind_inner(&[missing(), missing()], 0, true).is_err());
    }

    #[test]
    fn one_unjoinable_interface_is_skipped_and_reported_in_the_mask() {
        let mut t = UdpTransport::bind_inner(&[missing(), loopback()], 0, true).unwrap();
        assert_eq!(t.usable(), [false, true]);
        assert_eq!(t.sockets.len(), 1);
        let dest = SocketAddr::from((Ipv4Addr::LOCALHOST, 9));
        assert_eq!(
            t.send(1, dest, b"x").unwrap_err().to_string(),
            "no interface with index '1'"
        );
        t.send(0, dest, b"x").unwrap();
        assert_eq!(t.recv(&mut [0; 16], Duration::ZERO).unwrap(), None);
    }

    #[test]
    fn the_skip_warning_names_family_position_index_and_kind_but_no_address() {
        crate::onvif::discovery::test_log::install();
        let v6 = Interface {
            addr: "2001:db8:5eed::1".parse().unwrap(),
            prefix_len: 64,
            index: u32::MAX - 6,
        };
        // The kind is whatever this OS reports for a join on a nonexistent interface.
        let kind = open(&v6, 0, true).unwrap_err().kind();
        UdpTransport::bind_inner(&[loopback(), v6], 0, true).unwrap();
        let lines = crate::onvif::discovery::test_log::lines();
        let line = lines
            .iter()
            .find(|l| l.contains("OS index 4294967289"))
            .unwrap();
        assert_eq!(
            line,
            &format!("Skipping IPv6 discovery interface 2/2 (OS index 4294967289): {kind:?}")
        );
        assert!(!line.contains("2001") && !line.contains("127."));
    }

    #[test]
    fn an_ipv4_interface_that_cannot_bind_is_skipped_too() {
        // A plain socket without address sharing makes every later bind to the port fail.
        let holder = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let port = holder.local_addr().unwrap().port();
        assert!(UdpTransport::bind_inner(&[loopback()], port, false).is_err());
        let t = UdpTransport::bind_inner(&[loopback(), missing()], port, false).unwrap();
        assert_eq!(t.usable(), [false, true]);
    }

    #[test]
    fn fixed_ports_are_shared_but_ephemeral_ports_are_exclusive() {
        // Reserve an exclusive OS-assigned port before enabling sharing. No bind to port
        // zero uses `SO_REUSEPORT`, even in this test, and there is no close/rebind race.
        let holder = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        holder
            .bind(&SockAddr::from(SocketAddr::from((
                Ipv4Addr::UNSPECIFIED,
                0,
            ))))
            .unwrap();
        let addr = holder.local_addr().unwrap().as_socket().unwrap();
        let port = addr.port();
        let mut options = socket_options(MulticastInterface::V4(Ipv4Addr::LOCALHOST), addr, false);
        options.retain(|option| !matches!(option, SocketOption::Bind(_)));
        let holder = Socket::from(finish_open(Ok(holder), &options).unwrap());
        assert!(holder.reuse_address().unwrap());
        let _shared = open(&loopback(), port, true).unwrap();
        let exclusive = open(&loopback(), 0, false).unwrap();
        assert!(UdpSocket::bind(exclusive.local_addr().unwrap()).is_err());
    }

    #[test]
    fn receive_classification_preserves_transient_and_fatal_errors() {
        for kind in [
            io::ErrorKind::WouldBlock,
            io::ErrorKind::Interrupted,
            io::ErrorKind::ConnectionReset,
        ] {
            assert_eq!(received(Err(io::Error::from(kind))).unwrap(), None);
        }
        let mut transport = UdpTransport::bind_inner(&[loopback()], 0, false).unwrap();
        let error = transport
            .recv_with(&mut [0; 16], Duration::ZERO, |_, _| {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            })
            .unwrap_err();
        assert!(
            matches!(error, Error::Io(ref error) if error.kind() == io::ErrorKind::PermissionDenied)
        );
        assert_eq!(transport.next, 0);
    }

    #[test]
    fn unicast_round_trip_over_loopback() {
        let mut t = UdpTransport::bind_inner(&[loopback()], 0, false).unwrap();
        let port = sock(&t, 0).local_addr().unwrap().port();
        let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        peer.send_to(b"ping", (Ipv4Addr::LOCALHOST, port)).unwrap();

        let mut buf = [0_u8; 16];
        let got = t.recv(&mut buf, Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(buf.get(..got.len), Some(&b"ping"[..]));
        assert_eq!(got.source, peer.local_addr().unwrap());

        t.send(0, got.source, b"pong").unwrap();
        let mut back = [0_u8; 16];
        let (n, _) = peer.recv_from(&mut back).unwrap();
        assert_eq!(back.get(..n), Some(&b"pong"[..]));
    }

    #[test]
    fn ipv6_loopback_is_v6_only_and_preserves_datagram_boundaries() {
        let interface = Interface {
            addr: Ipv6Addr::LOCALHOST.into(),
            prefix_len: 128,
            index: 0,
        };
        let mut transport = UdpTransport::bind_inner(&[interface], 0, false).unwrap();
        let address = SocketAddr::from((
            Ipv6Addr::LOCALHOST,
            sock(&transport, 0).local_addr().unwrap().port(),
        ));
        let socket = Socket::from(sock(&transport, 0).try_clone().unwrap());
        assert!(socket.only_v6().unwrap());
        let peer = UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        peer.send_to(b"", address).unwrap();
        peer.send_to(b"v6", address).unwrap();
        let mut buffer = [0; 16];
        let empty = transport
            .recv(&mut buffer, Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(empty.len, 0);
        assert_eq!(empty.source, peer.local_addr().unwrap());
        let datagram = transport
            .recv(&mut buffer, Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(&buffer[..datagram.len], b"v6");
        transport.send(0, datagram.source, b"reply").unwrap();
        let (len, source) = peer.recv_from(&mut buffer).unwrap();
        assert_eq!(&buffer[..len], b"reply");
        assert_eq!(source, address);
    }

    #[test]
    fn a_flooded_first_socket_does_not_starve_later_ones() {
        let mut t = UdpTransport::bind_inner(&[loopback(), loopback()], 0, false).unwrap();
        let (p0, p1) = (
            sock(&t, 0).local_addr().unwrap().port(),
            sock(&t, 1).local_addr().unwrap().port(),
        );
        assert_ne!(p0, p1);
        let flooder = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let quiet = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        for _ in 0..50 {
            flooder
                .send_to(b"flood", (Ipv4Addr::LOCALHOST, p0))
                .unwrap();
        }
        quiet.send_to(b"one", (Ipv4Addr::LOCALHOST, p1)).unwrap();
        let quiet_addr = quiet.local_addr().unwrap();
        // Make sure both queues are populated before receiving, whatever the host's timing.
        for socket in &t.sockets {
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            assert!(socket.peek_from(&mut [0; 16]).unwrap().0 > 0);
            socket.set_nonblocking(true).unwrap();
        }

        let mut buf = [0_u8; 16];
        let mut served_after = None;
        for n in 0..4 {
            let got = t.recv(&mut buf, Duration::from_secs(5)).unwrap().unwrap();
            if got.source == quiet_addr {
                served_after = Some(n);
                break;
            }
        }
        assert!(
            served_after.is_some_and(|n| n <= 1),
            "later socket served after {served_after:?} datagrams"
        );
    }

    #[test]
    fn empty_transport_and_invalid_destination_are_reported() {
        let mut empty = UdpTransport::bind(&[]).unwrap();
        assert_eq!(empty.recv(&mut [], Duration::ZERO).unwrap(), None);

        let mut transport = UdpTransport::bind_inner(&[loopback()], 0, false).unwrap();
        let ipv6_destination = SocketAddr::from((Ipv6Addr::LOCALHOST, 9));
        assert!(transport.send(0, ipv6_destination, b"x").is_err());
    }

    #[test]
    fn recv_times_out_when_idle() {
        let mut t = UdpTransport::bind_inner(&[loopback()], 0, false).unwrap();
        let mut buf = [0_u8; 16];
        assert_eq!(t.recv(&mut buf, Duration::from_millis(20)).unwrap(), None);
    }

    #[test]
    fn send_to_unknown_interface_is_an_error() {
        let mut t = UdpTransport::bind_inner(&[loopback()], 0, false).unwrap();
        let dest = SocketAddr::from((Ipv4Addr::LOCALHOST, 9));
        assert!(t.send(5, dest, b"x").is_err());
    }

    #[test]
    fn responder_answers_a_unicast_probe_over_real_sockets() {
        use crate::onvif::discovery::message::fixtures::PROBE_ONVIF;
        use crate::onvif::discovery::{
            Config, EndpointReference, Responder, ServiceAddress, SystemClock, SystemEntropy,
        };
        let config = Config::new(
            EndpointReference::parse("urn:uuid:01234567-89ab-4def-8123-456789abcdef").unwrap(),
            vec!["onvif://www.onvif.org/Profile/Streaming".to_owned()],
            vec![loopback()],
            ServiceAddress {
                port: 8080,
                path: "/onvif/device_service".to_owned(),
            },
        )
        .unwrap();
        let transport = UdpTransport::bind_inner(&[loopback()], 0, false).unwrap();
        let port = sock(&transport, 0).local_addr().unwrap().port();
        let mut responder =
            Responder::new(config, SystemEntropy::default(), transport, SystemClock);

        let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        peer.send_to(PROBE_ONVIF.as_bytes(), (Ipv4Addr::LOCALHOST, port))
            .unwrap();
        // The reply is held for up to the spec's random delay, then sent.
        for _ in 0..20 {
            responder.step(Duration::from_millis(50)).unwrap();
        }
        let mut buf = [0_u8; 2048];
        let (n, _) = peer.recv_from(&mut buf).unwrap();
        let reply = String::from_utf8_lossy(buf.get(..n).unwrap_or_default()).into_owned();
        assert!(reply.contains("ProbeMatches"));
        assert!(reply.contains("http://127.0.0.1:8080/onvif/device_service"));
        assert_eq!(responder.stats().sent, 1);
    }
}
