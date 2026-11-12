//! One RTSP control connection: request loop, session state and media delivery.
//!
//! A connection owns at most one session. Closing the connection, for any reason, drops the
//! session, which releases its slot, its UDP sockets and its fan-out subscription.
//!
//! References:
//! - [RFC 2326 sections 10 (methods), 12.37 (`Session`), 12.39 (`Transport`)](https://www.rfc-editor.org/rfc/rfc2326)
//! - [RFC 3550 section 6.4.1 (sender reports)](https://www.rfc-editor.org/rfc/rfc3550)

use std::future::{Future, pending};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{OwnedSemaphorePermit, watch};
use tokio::time::sleep_until;

use super::digest::{AuthOutcome, hex};
use super::message::{Incoming, Method, Request, Response, encode_interleaved, parse_incoming};
use super::rtcp::{RtpHeader, SenderInfo, ntp_timestamp, write_sender_report};
use super::server::Shared;
use super::setup::{TransportSpec, format_transport, parse_transport};
use super::source::{Fanout, MediaSource, Packet, Subscription};

#[derive(Debug, thiserror::Error)]
#[error("RTSP connection I/O: {0}")]
struct ConnectionError(#[from] io::Error);

type ConnectionResult<T> = Result<T, ConnectionError>;

const PUBLIC_METHODS: &str = "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER";
/// Attempts at finding a free even/odd UDP port pair when the OS chooses.
const EPHEMERAL_PAIR_ATTEMPTS: usize = 32;

struct UdpPair {
    rtp: UdpSocket,
    rtcp: UdpSocket,
    server_ports: (u16, u16),
    media_dest: SocketAddr,
    control_dest: SocketAddr,
}

struct SendStats {
    ssrc: u32,
    last_timestamp: u32,
    last_at: Instant,
    packets: u32,
    octets: u32,
}

struct Playing<S: MediaSource> {
    subscription: Subscription<S>,
    /// `None` until the first packet is delivered.
    stats: Option<SendStats>,
    next_report: tokio::time::Instant,
}

struct Session<S: MediaSource> {
    id: String,
    mount: String,
    fanout: Arc<Fanout<S>>,
    spec: TransportSpec,
    udp: Option<UdpPair>,
    playing: Option<Playing<S>>,
    _permit: OwnedSemaphorePermit,
}

enum Event {
    Packet(Packet),
    ReportDue,
}

pub(crate) struct Connection<S: MediaSource> {
    /// Binds Digest nonces to this connection.
    id: u64,
    stream: TcpStream,
    peer: SocketAddr,
    local: SocketAddr,
    shared: Arc<Shared<S>>,
    stop: watch::Receiver<bool>,
    buf: Vec<u8>,
    out: Vec<u8>,
    session: Option<Session<S>>,
    last_activity: tokio::time::Instant,
    auth_failures: u32,
    /// Shared entropy seam for nonce challenges and session identifiers.
    fill_random: fn(&mut [u8]) -> Result<(), getrandom::Error>,
}

impl<S: MediaSource> Connection<S> {
    pub(crate) fn new(
        stream: TcpStream, peer: SocketAddr, local: SocketAddr, shared: Arc<Shared<S>>,
        stop: watch::Receiver<bool>,
    ) -> Self {
        Self {
            id: shared
                .next_connection
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            stream,
            peer,
            local,
            shared,
            stop,
            buf: Vec::new(),
            out: Vec::new(),
            session: None,
            last_activity: tokio::time::Instant::now(),
            auth_failures: 0,
            fill_random: getrandom::fill,
        }
    }

    /// Serve until the peer leaves, misbehaves, times out or the server stops.
    pub(crate) async fn run(mut self) {
        // An I/O error just means the peer is gone; either way the session is dropped.
        let _ = self.drive().await;
    }

    fn touch(&mut self) {
        self.last_activity = tokio::time::Instant::now();
    }

    fn deadline(&self) -> tokio::time::Instant {
        let config = &self.shared.config;
        let silence = if self.session.is_some() {
            config.session_timeout
        } else {
            config.idle_timeout
        };
        self.last_activity + silence
    }

    async fn drive(&mut self) -> ConnectionResult<()> {
        loop {
            loop {
                match parse_incoming(&self.buf, &self.shared.config.limits) {
                    Ok(None) => break,
                    Ok(Some((message, used))) => {
                        self.buf.drain(..used);
                        if !self.handle(message).await? {
                            return Ok(());
                        }
                    }
                    Err(error) => {
                        let reply =
                            Response::new(error.status()).with_header("Connection", "close");
                        return self.send(&reply, None).await;
                    }
                }
            }
            let deadline = self.deadline();
            tokio::select! {
                _ = self.stop.changed() => return Ok(()),
                () = sleep_until(deadline) => return Ok(()),
                read = read_more(&mut self.stream, &mut self.buf) => {
                    if read? == 0 {
                        return Ok(());
                    }
                }
                event = next_event(&mut self.session) => match event {
                    Event::Packet(packet) => self.send_rtp(&packet).await?,
                    Event::ReportDue => self.send_report().await?,
                },
            }
        }
    }

    /// Returns whether to keep the connection open.
    async fn handle(&mut self, message: Incoming) -> ConnectionResult<bool> {
        match message {
            Incoming::Interleaved { .. } => {
                // Client RTCP over the control connection: only proof of life is needed.
                if self.session.is_some() {
                    self.touch();
                }
                Ok(true)
            }
            Incoming::Request(request) => self.handle_request(&request).await,
        }
    }

    async fn handle_request(&mut self, request: &Request) -> ConnectionResult<bool> {
        let Some(cseq) = request.cseq() else {
            self.send(&Response::new(400), None).await?;
            return Ok(true);
        };
        let now = Instant::now();
        let outcome = self.shared.auth.authorize(
            self.id,
            request.method.as_str(),
            &request.uri,
            request.headers.get("authorization"),
            now,
        );
        if let AuthOutcome::Challenge {
            stale,
            failed_attempt,
        } = outcome
        {
            self.auth_failures += u32::from(failed_attempt);
            let reply =
                match self
                    .shared
                    .auth
                    .challenge_with(self.id, stale, now, &mut self.fill_random)
                {
                    Ok(values) => values.iter().fold(Response::new(401), |r, v| {
                        r.with_header("WWW-Authenticate", v)
                    }),
                    Err(_) => Response::new(500),
                };
            self.send(&reply, Some(cseq)).await?;
            return Ok(self.auth_failures < self.shared.config.max_auth_failures);
        }
        self.auth_failures = 0;
        self.touch();
        let reply = self.dispatch(request).await;
        self.send(&reply, Some(cseq)).await?;
        Ok(true)
    }

    async fn dispatch(&mut self, request: &Request) -> Response {
        match request.method {
            Method::Options => Response::new(200).with_header("Public", PUBLIC_METHODS),
            Method::Describe => self.describe(request),
            Method::Setup => self.setup(request).await,
            Method::Play => self.play(request),
            Method::Teardown => self.teardown(request),
            Method::GetParameter => self.get_parameter(request),
            Method::Other(_) => Response::new(501),
        }
    }

    fn mount(&self, uri: &str, setup: bool) -> Option<(&str, &Arc<Fanout<S>>)> {
        let path = uri_path(uri)?;
        std::iter::once((self.shared.config.mount_path.as_str(), &self.shared.fanout))
            .chain(
                self.shared
                    .mounts
                    .iter()
                    .map(|m| (m.path.as_str(), &m.fanout)),
            )
            .find(|(mount, fanout)| {
                if setup {
                    path == format!("{mount}/{}", fanout.source().track().control)
                } else {
                    path == *mount
                }
            })
    }

    fn describe(&self, request: &Request) -> Response {
        let Some((_, fanout)) = self.mount(&request.uri, false) else {
            return Response::new(404);
        };
        let track = fanout.source().track();
        let Ok(sdp) = track.to_sdp(self.local.ip(), "tethercam") else {
            return Response::new(500);
        };
        Response::new(200)
            .with_header(
                "Content-Base",
                &format!("{}/", request.uri.trim_end_matches('/')),
            )
            .with_body("application/sdp", sdp.into_bytes())
    }

    async fn setup(&mut self, request: &Request) -> Response {
        let Some((mount, fanout)) = self.mount(&request.uri, true) else {
            return Response::new(404);
        };
        let mount = mount.to_owned();
        let fanout = Arc::clone(fanout);
        let Some(header) = request.headers.get("transport") else {
            return Response::new(400);
        };
        let Ok(spec) = parse_transport(header) else {
            return Response::new(461);
        };
        match (session_id(request), &self.session) {
            (None, None) => {}
            (None, Some(_)) => return Response::new(455),
            (Some(id), Some(s)) if s.id == id => {
                if s.playing.is_some() || s.mount != mount {
                    return Response::new(455);
                }
            }
            (Some(_), _) => return Response::new(454),
        }

        let udp = match spec {
            TransportSpec::Udp {
                rtp_port,
                rtcp_port,
            } => {
                let client = self.peer.ip();
                match bind_udp_pair(
                    self.local.ip(),
                    self.shared.config.udp_ports.as_ref(),
                    SocketAddr::new(client, rtp_port),
                    SocketAddr::new(client, rtcp_port),
                )
                .await
                {
                    Ok(pair) => Some(pair),
                    Err(_) => return Response::new(500),
                }
            }
            TransportSpec::Tcp { .. } => None,
        };
        let server_ports = udp.as_ref().map(|u| u.server_ports);
        let id = if let Some(session) = self.session.as_mut() {
            session.spec = spec;
            session.udp = udp;
            session.id.clone()
        } else {
            let Ok(permit) = Arc::clone(&self.shared.sessions).try_acquire_owned() else {
                return Response::new(503);
            };
            let mut raw = [0_u8; 8];
            if (self.fill_random)(&mut raw).is_err() {
                return Response::new(500);
            }
            let id = hex(&raw);
            self.session = Some(Session {
                id: id.clone(),
                mount,
                fanout,
                spec,
                udp,
                playing: None,
                _permit: permit,
            });
            id
        };
        Response::new(200)
            .with_header("Session", &self.session_header(&id))
            .with_header("Transport", &format_transport(spec, server_ports))
    }

    fn session_header(&self, id: &str) -> String {
        format!(
            "{id};timeout={}",
            self.shared.config.session_timeout.as_secs()
        )
    }

    /// `Err` holds the `454` reply when the request names no session of this connection.
    fn require_session(&self, request: &Request) -> Result<(), Response> {
        match (session_id(request), &self.session) {
            (Some(id), Some(s)) if s.id == id => Ok(()),
            _ => Err(Response::new(454)),
        }
    }

    fn play(&mut self, request: &Request) -> Response {
        let Some(session) = self
            .session
            .as_mut()
            .filter(|s| session_id(request) == Some(s.id.as_str()))
        else {
            return Response::new(454);
        };
        let track = session.fanout.source().track();
        let mount = &session.mount;
        let path = uri_path(&request.uri);
        if path != Some(mount.as_str())
            && path != Some(format!("{mount}/{}", track.control).as_str())
        {
            return Response::new(404);
        }
        let interval = self.shared.config.rtcp_interval;
        let fanout = Arc::clone(&session.fanout);
        if session.playing.is_none() {
            session.playing = Some(Playing {
                subscription: fanout.subscribe(),
                stats: None,
                next_report: tokio::time::Instant::now() + interval,
            });
        }
        let id = session.id.clone();
        Response::new(200)
            .with_header("Session", &self.session_header(&id))
            .with_header("Range", "npt=0.000-")
    }

    fn teardown(&mut self, request: &Request) -> Response {
        if let Err(reply) = self.require_session(request) {
            return reply;
        }
        self.session = None;
        Response::new(200)
    }

    fn get_parameter(&self, request: &Request) -> Response {
        if !request.body.is_empty() {
            return Response::new(451);
        }
        if session_id(request).is_some()
            && let Err(reply) = self.require_session(request)
        {
            return reply;
        }
        Response::new(200)
    }

    async fn send(&mut self, response: &Response, cseq: Option<u32>) -> ConnectionResult<()> {
        self.out.clear();
        response.write_into(cseq, &mut self.out);
        write_all(
            &mut self.stream,
            &self.out,
            self.shared.config.write_timeout,
        )
        .await
    }

    async fn send_rtp(&mut self, packet: &Packet) -> ConnectionResult<()> {
        let bytes = packet.as_bytes();
        let timeout = self.shared.config.write_timeout;
        let interval = self.shared.config.rtcp_interval;
        let (Some(session), Ok(header)) = (self.session.as_mut(), RtpHeader::parse(bytes)) else {
            return Ok(());
        };
        let Some(playing) = session.playing.as_mut() else {
            return Ok(());
        };
        let delivered = match (&session.spec, &session.udp) {
            (TransportSpec::Tcp { rtp_channel, .. }, _) => {
                self.out.clear();
                if encode_interleaved(*rtp_channel, bytes, &mut self.out).is_ok() {
                    write_all(&mut self.stream, &self.out, timeout).await?;
                    true
                } else {
                    false
                }
            }
            (TransportSpec::Udp { .. }, Some(udp)) => {
                send_udp(&udp.rtp, bytes, udp.media_dest, timeout).await
            }
            (TransportSpec::Udp { .. }, None) => false,
        };
        if !delivered {
            return Ok(());
        }
        let payload = u32::try_from(header.payload_len).unwrap_or(u32::MAX);
        let first = playing.stats.is_none();
        let stats = playing.stats.get_or_insert(SendStats {
            ssrc: header.ssrc,
            last_timestamp: header.timestamp,
            last_at: Instant::now(),
            packets: 0,
            octets: 0,
        });
        stats.ssrc = header.ssrc;
        stats.last_timestamp = header.timestamp;
        stats.last_at = Instant::now();
        stats.packets = stats.packets.wrapping_add(1);
        stats.octets = stats.octets.wrapping_add(payload);
        if first {
            // Give receivers their wallclock mapping as early as possible.
            playing.next_report = tokio::time::Instant::now() + interval;
            return self.send_report().await;
        }
        Ok(())
    }

    async fn send_report(&mut self) -> ConnectionResult<()> {
        let timeout = self.shared.config.write_timeout;
        let interval = self.shared.config.rtcp_interval;
        let Some(session) = self.session.as_mut() else {
            return Ok(());
        };
        let clock_rate = u128::from(session.fanout.source().track().clock_rate);
        let Some(playing) = session.playing.as_mut() else {
            return Ok(());
        };
        playing.next_report = tokio::time::Instant::now() + interval;
        let Some(stats) = playing.stats.as_ref() else {
            return Ok(());
        };
        let ticks = stats.last_at.elapsed().as_micros() * clock_rate / 1_000_000;
        let advance = u32::try_from(ticks & u128::from(u32::MAX)).unwrap_or(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let info = SenderInfo {
            ssrc: stats.ssrc,
            ntp: ntp_timestamp(now),
            rtp_timestamp: stats.last_timestamp.wrapping_add(advance),
            packet_count: stats.packets,
            octet_count: stats.octets,
        };
        let mut report = Vec::with_capacity(48);
        write_sender_report(&info, &mut report);
        match (&session.spec, &session.udp) {
            (TransportSpec::Tcp { rtcp_channel, .. }, _) => {
                self.out.clear();
                // This fixed-size compound report always fits within the u16 framing limit.
                let _ = encode_interleaved(*rtcp_channel, &report, &mut self.out);
                write_all(&mut self.stream, &self.out, timeout).await?;
            }
            (TransportSpec::Udp { .. }, Some(udp)) => {
                let _ = send_udp(&udp.rtcp, &report, udp.control_dest, timeout).await;
            }
            (TransportSpec::Udp { .. }, None) => {}
        }
        Ok(())
    }
}

async fn read_more(stream: &mut TcpStream, buf: &mut Vec<u8>) -> ConnectionResult<usize> {
    buf.reserve(4096);
    Ok(stream.read_buf(buf).await?)
}

async fn write_all(stream: &mut TcpStream, data: &[u8], timeout: Duration) -> ConnectionResult<()> {
    write_with_timeout(stream.write_all(data), timeout).await
}

async fn write_with_timeout(
    write: impl Future<Output = io::Result<()>>, timeout: Duration,
) -> ConnectionResult<()> {
    match tokio::time::timeout(timeout, write).await {
        Ok(result) => Ok(result?),
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "client write stalled").into()),
    }
}

/// Send one datagram, giving up after `timeout`. UDP media is lossy by contract, so a stalled
/// or failed send just drops the datagram.
async fn send_udp(socket: &UdpSocket, data: &[u8], dest: SocketAddr, timeout: Duration) -> bool {
    matches!(
        tokio::time::timeout(timeout, socket.send_to(data, dest)).await,
        Ok(Ok(_))
    )
}

/// Resolves with the next thing to deliver to the playing client, or never when not playing.
async fn next_event<S: MediaSource>(session: &mut Option<Session<S>>) -> Event {
    let Some(Session {
        playing: Some(playing),
        ..
    }) = session
    else {
        return pending().await;
    };
    let reporting = playing.stats.is_some();
    let next_report = playing.next_report;
    tokio::select! {
        packet = playing.subscription.recv() => match packet {
            Some(packet) => Event::Packet(packet),
            None => pending().await,
        },
        () = sleep_until(next_report), if reporting => Event::ReportDue,
    }
}

fn session_id(request: &Request) -> Option<&str> {
    let value = request.headers.get("session")?;
    Some(value.split(';').next().unwrap_or(value).trim())
}

/// The path of an RTSP URI (or origin-form target) without query and trailing slash.
fn uri_path(uri: &str) -> Option<&str> {
    let rest = if uri.starts_with('/') {
        uri
    } else {
        let (scheme, after) = uri.split_once("://")?;
        if !scheme.eq_ignore_ascii_case("rtsp") {
            return None;
        }
        after.find('/').and_then(|i| after.get(i..)).unwrap_or("/")
    };
    // split always yields at least one element, including for an empty target.
    let path = rest.split(['?', '#']).next().unwrap_or_default();
    Some(if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        path
    })
}

async fn bind_udp_pair(
    ip: IpAddr, range: Option<&RangeInclusive<u16>>, media_dest: SocketAddr,
    control_dest: SocketAddr,
) -> ConnectionResult<UdpPair> {
    bind_udp_pair_with(
        ip,
        range,
        media_dest,
        control_dest,
        &mut bind_udp_socket,
        &mut udp_port,
    )
    .await
}

type UdpBind = Pin<Box<dyn Future<Output = io::Result<UdpSocket>> + Send>>;

fn bind_udp_socket(addr: SocketAddr) -> UdpBind {
    Box::pin(UdpSocket::bind(addr))
}

fn udp_port(socket: &UdpSocket) -> io::Result<u16> {
    socket.local_addr().map(|addr| addr.port())
}

/// Keep allocation retries testable without depending on the OS ephemeral-port sequence.
async fn bind_udp_pair_with(
    ip: IpAddr, range: Option<&RangeInclusive<u16>>, media_dest: SocketAddr,
    control_dest: SocketAddr, bind: &mut (dyn FnMut(SocketAddr) -> UdpBind + Send),
    port: &mut (dyn FnMut(&UdpSocket) -> io::Result<u16> + Send),
) -> ConnectionResult<UdpPair> {
    let finish = |rtp: UdpSocket,
                  rtcp: UdpSocket,
                  port: &mut dyn FnMut(&UdpSocket) -> io::Result<u16>|
     -> ConnectionResult<UdpPair> {
        Ok(UdpPair {
            server_ports: (port(&rtp)?, port(&rtcp)?),
            rtp,
            rtcp,
            media_dest,
            control_dest,
        })
    };
    if let Some(range) = range {
        let end = *range.end();
        let first = range.start().saturating_add(range.start() % 2);
        for rtp_port in (first..=end).step_by(2) {
            let Some(rtcp_port) = rtp_port.checked_add(1).filter(|p| *p <= end) else {
                break;
            };
            if let (Ok(rtp), Ok(rtcp)) = (
                bind(SocketAddr::new(ip, rtp_port)).await,
                bind(SocketAddr::new(ip, rtcp_port)).await,
            ) {
                return finish(rtp, rtcp, port);
            }
        }
    } else {
        for _ in 0..EPHEMERAL_PAIR_ATTEMPTS {
            let rtp = bind(SocketAddr::new(ip, 0)).await?;
            let chosen = port(&rtp)?;
            // An even u16 is at most 65534, so its successor is representable.
            if chosen % 2 == 0
                && let Ok(rtcp) = bind(SocketAddr::new(ip, chosen + 1)).await
            {
                return finish(rtp, rtcp, port);
            }
        }
    }
    Err(io::Error::new(io::ErrorKind::AddrNotAvailable, "no free UDP port pair").into())
}

#[cfg(test)]
mod failures;

#[cfg(test)]
mod tests {
    use super::*;

    async fn simulated_write(stall: bool, fail: bool) -> io::Result<()> {
        if stall {
            pending::<()>().await;
        }
        if fail {
            return Err(io::Error::other("synthetic failure"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn write_deadline_returns_typed_timeout() {
        let error = write_with_timeout(simulated_write(true, false), Duration::from_millis(1))
            .await
            .unwrap_err();
        assert_eq!(error.0.kind(), io::ErrorKind::TimedOut);
        let error = write_with_timeout(simulated_write(false, true), Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(error.0.kind(), io::ErrorKind::Other);
        assert!(
            write_with_timeout(simulated_write(false, false), Duration::from_secs(1))
                .await
                .is_ok()
        );
    }

    #[test]
    fn extracts_uri_paths() {
        assert_eq!(uri_path("rtsp://192.0.2.1:554/stream"), Some("/stream"));
        assert_eq!(uri_path("RTSP://192.0.2.1/stream/"), Some("/stream"));
        assert_eq!(
            uri_path("rtsp://192.0.2.1/stream/trackID=0?x=1"),
            Some("/stream/trackID=0")
        );
        assert_eq!(uri_path("rtsp://192.0.2.1"), Some("/"));
        assert_eq!(uri_path("/stream"), Some("/stream"));
        assert_eq!(uri_path("http://192.0.2.1/stream"), None);
        assert_eq!(uri_path("*"), None);
    }

    proptest::proptest! {
        #[test]
        fn uri_path_never_panics(uri in ".{0,200}") {
            let _ = uri_path(&uri);
        }
    }

    #[tokio::test]
    async fn udp_pairs_are_even_odd_and_honour_ranges() {
        let ip = IpAddr::from([127, 0, 0, 1]);
        let dest = SocketAddr::new(ip, 9);
        let pair = bind_udp_pair(ip, None, dest, dest).await.unwrap();
        assert_eq!(pair.server_ports.0 % 2, 0);
        assert_eq!(pair.server_ports.1, pair.server_ports.0 + 1);
        let (lo, _) = pair.server_ports;
        // A range holding only the occupied pair has no free pair.
        let taken = lo..=lo + 1;
        assert!(bind_udp_pair(ip, Some(&taken), dest, dest).await.is_err());
        let empty = RangeInclusive::new(1, 0);
        assert!(bind_udp_pair(ip, Some(&empty), dest, dest).await.is_err());
    }
}
