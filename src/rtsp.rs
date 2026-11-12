//! RTSP 1.0 server with Digest authentication.
//!
//! [`RtspServer`] serves one [`MediaSource`] to many clients. It implements `OPTIONS`,
//! `DESCRIBE`, `SETUP`, `PLAY`, `TEARDOWN` and `GET_PARAMETER`, carries RTP over the TCP
//! connection (interleaved) or over UDP unicast, and sends RTCP sender reports.
//!
//! Security and resource model:
//! - Digest authentication ([`digest`]) is required on every request, `OPTIONS` and loopback
//!   included. There is no Basic authentication and no unauthenticated surface
//! - Sessions use 64-bit CSPRNG identifiers, are bound to the control connection that created
//!   them, and die with it. UDP media only ever goes to the control connection's peer address
//! - Everything is bounded: request size, header count, body size, connections, sessions, per
//!   client queue depth, write stalls and silence ([`ServerConfig`])
//! - A slow client loses packets, it never blocks the source or other clients ([`Fanout`])
//! - The first `PLAY` calls [`MediaSource::on_first_subscriber`] and the last leaver calls
//!   [`MediaSource::on_last_unsubscribe`], so the camera pipeline runs only while watched
//!
//! Not implemented: `PAUSE`, `RECORD`, `ANNOUNCE`, multicast, `RTP-Info` in `PLAY` responses and
//! RTCP receiver reports (clients prove liveness with any authenticated request).
//!
//! References:
//! - [RFC 2326, RTSP 1.0](https://www.rfc-editor.org/rfc/rfc2326)
//! - [RFC 3550, RTP and RTCP](https://www.rfc-editor.org/rfc/rfc3550)
//! - [RFC 4566, SDP](https://www.rfc-editor.org/rfc/rfc4566)
//! - [RFC 7616, HTTP Digest](https://www.rfc-editor.org/rfc/rfc7616)

mod connection;
pub mod digest;
pub mod message;
pub mod rtcp;
pub mod sdp;
mod server;
pub mod setup;
pub mod source;

pub use digest::{Algorithm, Authenticator, ClientDigest, ConnectionId, Credentials, DigestError};
pub use message::{Headers, Limits, Method, ParseError, Request, Response};
pub use sdp::{MediaKind, SdpError, TrackInfo};
pub use server::{RtspServer, ServerConfig, ServerError};
pub use source::{Fanout, MediaSource, Packet, Subscription};
