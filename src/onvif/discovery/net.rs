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
/// joined to the group on its interface.
#[derive(Debug)]
pub struct UdpTransport {
    sockets: Vec<UdpSocket>,
    /// Socket polled first on the next receive; advances after every datagram so a flood on
    /// one interface cannot starve the others.
    next: usize,
}

impl UdpTransport {
    /// Bind to the discovery port and join the multicast group on every interface.
    ///
    /// # Errors
    ///
    /// Returns an error if a socket cannot be created, bound or joined.
    pub fn bind(interfaces: &[Interface]) -> Result<Self, Error> {
        Self::bind_inner(interfaces, PORT, true)
    }

    /// `join: false` skips only the multicast join and egress setup, so tests exercise the
    /// production bind without touching a multicast group.
    fn bind_inner(interfaces: &[Interface], port: u16, join: bool) -> Result<Self, Error> {
        let sockets = interfaces
            .iter()
            .map(|i| open(i, port, join))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self { sockets, next: 0 })
    }

    /// Local port of the socket for `interface`.
    #[cfg(test)]
    fn local_port(&self, interface: usize) -> Option<u16> {
        self.sockets
            .get(interface)
            .and_then(|s| s.local_addr().ok())
            .map(|a| a.port())
    }
}

fn open(interface: &Interface, port: u16, join: bool) -> io::Result<UdpSocket> {
    // Wildcard bind on every platform, so unicast probes (add-by-IP flows) arrive as well as
    // multicast ones. Several sockets share the port: each interface joins the group on its own.
    let (domain, bind_ip): (_, IpAddr) = match interface.addr {
        IpAddr::V4(_) => (Domain::IPV4, Ipv4Addr::UNSPECIFIED.into()),
        IpAddr::V6(_) => (Domain::IPV6, Ipv6Addr::UNSPECIFIED.into()),
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    // Other WS-Discovery stacks on the host share the port.
    socket.set_reuse_address(true)?;
    // BSD-derived stacks (macOS) need `SO_REUSEPORT` to share a UDP port with another responder.
    #[cfg(all(
        unix,
        not(any(
            target_os = "solaris",
            target_os = "illumos",
            target_os = "cygwin",
            target_os = "nuttx"
        ))
    ))]
    socket.set_reuse_port(true)?;
    if interface.addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.bind(&SockAddr::from(SocketAddr::new(bind_ip, port)))?;
    if join {
        match interface.addr {
            IpAddr::V4(addr) => {
                socket.join_multicast_v4(&GROUP_V4, &addr)?;
                socket.set_multicast_if_v4(&addr)?;
                socket.set_multicast_ttl_v4(1)?;
                socket.set_multicast_loop_v4(false)?;
            }
            IpAddr::V6(_) => {
                socket.join_multicast_v6(&GROUP_V6, interface.index)?;
                socket.set_multicast_if_v6(interface.index)?;
                socket.set_multicast_hops_v6(1)?;
                socket.set_multicast_loop_v6(false)?;
            }
        }
    }
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

impl Transport for UdpTransport {
    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> Result<Option<Datagram>, Error> {
        let start = Instant::now();
        loop {
            let count = self.sockets.len();
            for offset in 0..count {
                let index = (self.next + offset) % count;
                let Some(socket) = self.sockets.get(index) else {
                    continue;
                };
                match socket.recv_from(buf) {
                    Ok((len, source)) => {
                        self.next = (index + 1) % count;
                        return Ok(Some(Datagram { len, source }));
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock
                                | io::ErrorKind::Interrupted
                                | io::ErrorKind::ConnectionReset
                        ) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return Ok(None);
            }
            std::thread::sleep(POLL_INTERVAL.min(timeout.saturating_sub(elapsed)));
        }
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

    fn loopback() -> Interface {
        Interface {
            addr: Ipv4Addr::LOCALHOST.into(),
            prefix_len: 8,
            index: 0,
        }
    }

    #[test]
    fn unicast_round_trip_over_loopback() {
        let mut t = UdpTransport::bind_inner(&[loopback()], 0, false).unwrap();
        let port = t.local_port(0).unwrap();
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
    fn a_flooded_first_socket_does_not_starve_later_ones() {
        let mut t = UdpTransport::bind_inner(&[loopback(), loopback()], 0, false).unwrap();
        let (p0, p1) = (t.local_port(0).unwrap(), t.local_port(1).unwrap());
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
        let queued = |socket: &UdpSocket| {
            let mut probe = [0_u8; 16];
            (0..500).any(|_| {
                socket.peek_from(&mut probe).is_ok() || {
                    std::thread::sleep(Duration::from_millis(10));
                    false
                }
            })
        };
        assert!(queued(t.sockets.first().unwrap()) && queued(t.sockets.get(1).unwrap()));

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
        let port = transport.local_port(0).unwrap();
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
