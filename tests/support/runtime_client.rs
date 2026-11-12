//! Bounded real-socket consumer shared by runtime integration tests and the steady-state bench.
//!
//! References:
//! - [RTSP 1.0](https://www.rfc-editor.org/rfc/rfc2326)
//! - [RTP H.264](https://www.rfc-editor.org/rfc/rfc6184)

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use super::camera_api::rtp::Header;
use super::camera_api::rtsp::{Algorithm, ClientDigest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub(crate) const USER: &str = "synthetic-operator";
pub(crate) const PASSWORD: &str = "synthetic-password";
const WAIT: Duration = Duration::from_secs(15);
pub(crate) const MAX_BODY: usize = 64 * 1024 * 1024;
pub(crate) const MAX_HEAD: usize = 16 * 1024;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn timed_out(_: tokio::time::error::Elapsed) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "synthetic client deadline")
}

pub(crate) struct Reply {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}
impl Reply {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub(crate) fn nonce(value: &str) -> io::Result<&str> {
    value
        .split("nonce=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("challenge lacks nonce"))
}

pub(crate) fn authorization(method: &str, uri: &str, nonce: &str, nc: u32) -> String {
    ClientDigest {
        algorithm: Algorithm::Md5,
        username: USER,
        password: PASSWORD,
        realm: "tethercam",
        nonce,
        method,
        uri,
        nc,
        cnonce: "synthetic-client",
    }
    .header_value()
}

pub(crate) struct Client<S = TcpStream> {
    pub(crate) socket: S,
    buf: Vec<u8>,
    nonce: Option<String>,
    nc: u32,
}

impl Client {
    pub(crate) async fn open(address: SocketAddr) -> io::Result<Self> {
        let socket = tokio::time::timeout(WAIT, TcpStream::connect(address))
            .await
            .map_err(timed_out)??;
        Ok(Self::from_io(socket))
    }
}

impl<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin> Client<S> {
    /// Build an unauthenticated consumer over TCP or an injected finite byte stream.
    pub(crate) fn from_io(socket: S) -> Self {
        Self {
            socket,
            buf: Vec::new(),
            nonce: None,
            nc: 0,
        }
    }
    async fn fill(&mut self, deadline: tokio::time::Instant) -> io::Result<()> {
        if self.buf.len() > MAX_BODY + MAX_HEAD {
            return Err(invalid("reply exceeds bound"));
        }
        let mut bytes = [0; 4096];
        let count = tokio::time::timeout_at(deadline, self.socket.read(&mut bytes))
            .await
            .map_err(timed_out)??;
        if count == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        self.buf.extend(bytes.iter().take(count));
        Ok(())
    }

    pub(crate) async fn packet(
        &mut self, deadline: tokio::time::Instant,
    ) -> io::Result<(u8, Vec<u8>)> {
        while self.buf.len() < 4 {
            self.fill(deadline).await?;
        }
        let &[magic, channel, a, b] = self
            .buf
            .first_chunk::<4>()
            .ok_or_else(|| invalid("short interleaved head"))?;
        if magic != b'$' {
            return Err(invalid("expected interleaved RTP"));
        }
        let total = usize::from(u16::from_be_bytes([a, b])) + 4;
        while self.buf.len() < total {
            self.fill(deadline).await?;
        }
        let bytes: Vec<_> = self.buf.drain(..total).skip(4).collect();
        Ok((channel, bytes))
    }

    pub(crate) async fn reply(&mut self) -> io::Result<Reply> {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if self.buf.first() == Some(&b'$') {
                self.packet(deadline).await?;
                continue;
            }
            if let Some(end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                if end > MAX_HEAD {
                    return Err(invalid("reply headers exceed bound"));
                }
                let head = std::str::from_utf8(
                    self.buf
                        .get(..end)
                        .ok_or_else(|| invalid("short reply head"))?,
                )
                .map_err(|_| invalid("invalid reply head"))?;
                let mut lines = head.split("\r\n");
                let status = lines
                    .next()
                    .and_then(|s| s.split(' ').nth(1))
                    .ok_or_else(|| invalid("missing status"))?
                    .parse()
                    .map_err(|_| invalid("invalid status"))?;
                let headers: Vec<_> = lines
                    .map(|line| {
                        line.split_once(':')
                            .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
                            .ok_or_else(|| invalid("malformed reply header"))
                    })
                    .collect::<io::Result<_>>()?;
                let length = headers.iter().find(|(k, _)| k == "content-length").map_or(
                    Ok(0),
                    |(_, v)| {
                        v.parse::<usize>()
                            .map_err(|_| invalid("invalid reply length"))
                    },
                )?;
                if length > MAX_BODY {
                    return Err(invalid("reply body exceeds bound"));
                }
                let total = end + 4 + length;
                if self.buf.len() >= total {
                    let body = self.buf.drain(..total).skip(end + 4).collect();
                    return Ok(Reply {
                        status,
                        headers,
                        body,
                    });
                }
            } else if self.buf.len() > MAX_HEAD {
                return Err(invalid("reply headers exceed bound"));
            }
            self.fill(deadline).await?;
        }
    }

    pub(crate) async fn request(
        &mut self, method: &str, uri: &str, extra: &str,
    ) -> io::Result<Reply> {
        self.nc = self
            .nc
            .checked_add(1)
            .ok_or_else(|| invalid("request counter exhausted"))?;
        let auth = self.nonce.as_ref().map_or_else(String::new, |nonce| {
            format!(
                "Authorization: {}\r\n",
                authorization(method, uri, nonce, self.nc)
            )
        });
        let bytes = format!(
            "{method} {uri} RTSP/1.0\r\nCSeq: {}\r\n{auth}{extra}\r\n",
            self.nc
        );
        tokio::time::timeout(WAIT, async {
            self.socket.write_all(bytes.as_bytes()).await?;
            self.reply().await
        })
        .await
        .map_err(timed_out)?
    }

    pub(crate) async fn authenticate(&mut self, uri: &str) -> io::Result<()> {
        let reply = self.request("OPTIONS", uri, "").await?;
        if reply.status != 401 {
            return Err(invalid("expected RTSP challenge"));
        }
        self.nonce = Some(
            nonce(
                reply
                    .header("www-authenticate")
                    .ok_or_else(|| invalid("missing challenge"))?,
            )?
            .into(),
        );
        if self.request("OPTIONS", uri, "").await?.status != 200 {
            return Err(invalid("authentication failed"));
        }
        Ok(())
    }

    pub(crate) async fn play(&mut self, uri: &str) -> io::Result<()> {
        let reply = self
            .request(
                "SETUP",
                &format!("{uri}/trackID=0"),
                "Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n",
            )
            .await?;
        if reply.status != 200 {
            return Err(invalid("SETUP failed"));
        }
        let session = reply
            .header("session")
            .and_then(|s| s.split(';').next())
            .ok_or_else(|| invalid("missing session"))?;
        if self
            .request("PLAY", uri, &format!("Session: {session}\r\n"))
            .await?
            .status
            != 200
        {
            return Err(invalid("PLAY failed"));
        }
        Ok(())
    }

    pub(crate) async fn frame(&mut self, expected_payload: u8) -> io::Result<Vec<Vec<u8>>> {
        let deadline = tokio::time::Instant::now() + WAIT;
        let mut packets = Vec::new();
        let mut bytes = 0;
        loop {
            let (channel, packet) = self.packet(deadline).await?;
            if channel != 0 {
                continue;
            }
            let header = Header::decode(&packet).map_err(|_| invalid("invalid RTP header"))?;
            if header.payload_type != expected_payload {
                return Err(invalid("unexpected RTP payload type"));
            }
            bytes += packet.len();
            if bytes > MAX_BODY {
                return Err(invalid("RTP frame exceeds bound"));
            }
            packets.push(packet);
            if header.marker {
                return Ok(packets);
            }
        }
    }
}

/// Reassemble the RFC 6184 single-NAL/FU-A forms the production packetizer emits.
pub(crate) fn annex_b(packets: &[Vec<u8>]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    for packet in packets {
        let payload = packet
            .get(12..)
            .ok_or_else(|| invalid("short RTP packet"))?;
        let indicator = payload
            .first()
            .copied()
            .ok_or_else(|| invalid("missing NAL"))?;
        if indicator & 0x1f == 28 {
            let fu = payload
                .get(1)
                .copied()
                .ok_or_else(|| invalid("missing FU header"))?;
            if fu & 0x80 != 0 {
                out.extend_from_slice(&[0, 0, 0, 1, (indicator & 0xe0) | (fu & 0x1f)]);
            }
            out.extend_from_slice(
                payload
                    .get(2..)
                    .ok_or_else(|| invalid("short FU payload"))?,
            );
        } else {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(payload);
        }
    }
    Ok(out)
}
