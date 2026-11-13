//! Real WS-Discovery multicast on the loopback interface only.
//!
//! References:
//! - [WS-Discovery 1.1](https://docs.oasis-open.org/ws-dd/discovery/1.1/os/wsdd-discovery-1.1-spec-os.html)

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};
use tethercam::onvif::discovery::{
    Config, EndpointReference, Engine, Error, GROUP_V4, Interface, PORT, ServiceAddress,
    SystemEntropy, Transport, UdpTransport,
};

const PROBE: &str = r#"<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope" xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery"><e:Header><w:MessageID>uuid:11111111-2222-3333-4444-555555555555</w:MessageID><w:To>urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To><w:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action></e:Header><e:Body><d:Probe/></e:Body></e:Envelope>"#;

#[test]
fn multicast_probe_round_trip_on_loopback() {
    let interface = Interface {
        addr: Ipv4Addr::LOCALHOST.into(),
        prefix_len: 8,
        index: 0,
    };
    let mut receiver = match UdpTransport::bind(&[interface]) {
        Ok(receiver) => receiver,
        Err(Error::Io(error))
            if matches!(
                error.kind(),
                io::ErrorKind::Unsupported | io::ErrorKind::AddrNotAvailable
            ) =>
        {
            writeln!(io::stderr(), "Skipping loopback multicast: {error}").unwrap();
            return;
        }
        Err(error) => panic!("multicast setup failed: {error}"),
    };
    let sender = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
    sender
        .bind(&SockAddr::from(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))))
        .unwrap();
    sender.set_multicast_if_v4(&Ipv4Addr::LOCALHOST).unwrap();
    sender.set_multicast_loop_v4(true).unwrap();
    sender.set_multicast_ttl_v4(1).unwrap();
    let sender: UdpSocket = sender.into();
    sender
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    sender.send_to(PROBE.as_bytes(), (GROUP_V4, PORT)).unwrap();
    let mut buf = [0; 4096];
    let deadline = Instant::now() + Duration::from_secs(2);
    let datagram = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let datagram = receiver
            .recv(&mut buf, remaining)
            .unwrap()
            .expect("multicast probe was not received");
        if datagram.source == sender.local_addr().unwrap() {
            break datagram;
        }
        assert!(
            Instant::now() < deadline,
            "multicast probe was not received"
        );
    };
    assert_eq!(&buf[..datagram.len], PROBE.as_bytes());
    let config = Config::new(
        EndpointReference::parse("urn:uuid:01234567-89ab-4def-8123-456789abcdef").unwrap(),
        vec![],
        vec![interface],
        ServiceAddress {
            port: 8080,
            path: "/onvif/device_service".to_owned(),
        },
    )
    .unwrap();
    let mut engine = Engine::new(config, SystemEntropy::default());
    let reply = engine
        .handle(Instant::now(), datagram.source, &buf[..datagram.len])
        .unwrap();
    receiver
        .send(reply.interface, reply.dest, &reply.payload)
        .unwrap();
    let (len, _) = sender.recv_from(&mut buf).unwrap();
    let response = std::str::from_utf8(&buf[..len]).unwrap();
    assert!(response.contains("ProbeMatches"));
    assert!(response.contains("uuid:11111111-2222-3333-4444-555555555555"));
    assert!(response.contains("http://127.0.0.1:8080/onvif/device_service"));
}
