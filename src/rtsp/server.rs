//! Listener, limits and shared state for the RTSP server.
//!
//! References:
//! - [RFC 2326, RTSP 1.0](https://www.rfc-editor.org/rfc/rfc2326)
//! - `docs/architecture.md`, "Security model"

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;

use super::connection::Connection;
use super::digest::{Authenticator, Credentials};
use super::message::Limits;
use super::sdp::{SdpError, TrackInfo};
use super::source::{Fanout, MediaSource};

/// Longest accepted timeout or interval (one year). Anything longer is a configuration mistake
/// and would risk `Instant` overflow.
const MAX_CONFIGURED_DURATION: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// Server tuning. Every resource a client can consume is bounded here.
///
/// [`RtspServer::bind`] rejects zero or oversized limits, and timeouts or intervals that are
/// zero or longer than one year.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Digest realm.
    pub realm: String,
    /// Path of the stream, e.g. `/stream`. `DESCRIBE` targets it, `SETUP` targets
    /// `<path>/<track control>`.
    pub mount_path: String,
    /// Request parser limits.
    pub limits: Limits,
    /// Maximum simultaneous TCP connections. Further connections are closed at accept.
    pub max_connections: usize,
    /// Maximum simultaneous sessions. Further `SETUP`s get `503`.
    pub max_sessions: usize,
    /// How long a connection may stay silent while it holds a session. Sessions are bound to
    /// their control connection, so expiry closes the connection and frees the session.
    pub session_timeout: Duration,
    /// How long a connection without a session may stay silent, or take to send one request.
    pub idle_timeout: Duration,
    /// Longest a single write to a client may stall before the client is disconnected.
    pub write_timeout: Duration,
    /// Lifetime of Digest nonces.
    pub nonce_lifetime: Duration,
    /// Packets buffered per playing client before new ones are dropped for it.
    pub client_queue: usize,
    /// Interval between RTCP sender reports.
    pub rtcp_interval: Duration,
    /// Even-aligned range to allocate server UDP port pairs from. `None` lets the OS choose.
    pub udp_ports: Option<RangeInclusive<u16>>,
    /// Consecutive failed Digest attempts after which a connection is closed.
    pub max_auth_failures: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            realm: "tethercam".into(),
            mount_path: "/stream".into(),
            limits: Limits::default(),
            max_connections: 32,
            max_sessions: 16,
            session_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(10),
            nonce_lifetime: Duration::from_secs(300),
            client_queue: 256,
            rtcp_interval: Duration::from_secs(5),
            udp_ports: None,
            max_auth_failures: 5,
        }
    }
}

/// Why the server could not start.
#[derive(Debug, Error)]
pub enum ServerError {
    /// Binding the listener failed.
    #[error("cannot bind RTSP listener: {0}")]
    Bind(#[from] io::Error),
    /// The source's track cannot be described in SDP.
    #[error(transparent)]
    Track(#[from] SdpError),
    /// The configuration is unusable.
    #[error("invalid configuration: {0}")]
    Config(&'static str),
}

/// State shared by every connection.
pub(crate) struct Shared<S: MediaSource> {
    pub(crate) fanout: Arc<Fanout<S>>,
    pub(crate) auth: Authenticator,
    pub(crate) config: ServerConfig,
    pub(crate) track: TrackInfo,
    pub(crate) sessions: Arc<Semaphore>,
    pub(crate) next_connection: std::sync::atomic::AtomicU64,
}

/// An RTSP 1.0 server over one [`MediaSource`].
pub struct RtspServer<S: MediaSource> {
    listener: TcpListener,
    shared: Arc<Shared<S>>,
}

impl<S: MediaSource> RtspServer<S> {
    /// Bind `addr` (port 0 picks a free port; see [`RtspServer::local_addr`]).
    ///
    /// # Errors
    ///
    /// [`ServerError`] when the address cannot be bound, the track is invalid or the config
    /// is unusable.
    pub async fn bind(
        addr: SocketAddr, source: Arc<S>, credentials: Credentials, config: ServerConfig,
    ) -> Result<Self, ServerError> {
        if !config.mount_path.starts_with('/')
            || config.mount_path.ends_with('/')
            || config
                .mount_path
                .chars()
                .any(|c| c.is_control() || c == ' ')
        {
            return Err(ServerError::Config("mount_path must look like /name"));
        }
        if config.max_connections == 0 || config.max_sessions == 0 {
            return Err(ServerError::Config(
                "connection and session limits must be non-zero",
            ));
        }
        // Deadlines are `Instant + duration`, which panics on overflow: bound every duration.
        let durations = [
            config.session_timeout,
            config.idle_timeout,
            config.write_timeout,
            config.nonce_lifetime,
            config.rtcp_interval,
        ];
        if durations
            .iter()
            .any(|d| d.is_zero() || *d > MAX_CONFIGURED_DURATION)
        {
            return Err(ServerError::Config(
                "timeouts and intervals must be non-zero and at most one year",
            ));
        }
        // Tokio panics above this bound; reject it instead.
        let ceiling = Semaphore::MAX_PERMITS;
        if config.max_connections > ceiling || config.max_sessions > ceiling {
            return Err(ServerError::Config(
                "connection and session limits exceed the supported maximum",
            ));
        }
        if config.client_queue == 0 || config.client_queue > ceiling {
            return Err(ServerError::Config(
                "client_queue must be in 1..=MAX_PERMITS",
            ));
        }
        let track = source.track();
        track.validate()?;
        let listener = TcpListener::bind(addr).await?;
        let shared = Shared {
            fanout: Fanout::new(source, config.client_queue),
            auth: Authenticator::new(credentials, config.nonce_lifetime),
            sessions: Arc::new(Semaphore::new(config.max_sessions)),
            next_connection: std::sync::atomic::AtomicU64::new(1),
            track,
            config,
        };
        Ok(Self {
            listener,
            shared: Arc::new(shared),
        })
    }

    /// The bound address.
    ///
    /// # Errors
    ///
    /// Any I/O error from the socket.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// The fan-out, e.g. to read [`Fanout::dropped_packets`].
    #[must_use]
    pub fn fanout(&self) -> &Arc<Fanout<S>> {
        &self.shared.fanout
    }

    /// Accept connections until `shutdown` resolves, then close every connection and return.
    pub async fn serve(self, shutdown: impl Future<Output = ()>) {
        let (stop_tx, stop_rx) = watch::channel(false);
        let connections = Arc::new(Semaphore::new(self.shared.config.max_connections));
        let mut tasks = JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                accepted = self.listener.accept() => {
                    let Ok((stream, peer)) = accepted else {
                        // Typically EMFILE or ECONNABORTED: back off instead of spinning.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    };
                    let Ok(permit) = Arc::clone(&connections).try_acquire_owned() else {
                        drop(stream);
                        continue;
                    };
                    let Ok(local) = stream.local_addr() else { continue };
                    let connection = Connection::new(
                        stream,
                        peer,
                        local,
                        Arc::clone(&self.shared),
                        stop_rx.clone(),
                    );
                    tasks.spawn(async move {
                        connection.run().await;
                        drop(permit);
                    });
                }
            }
        }
        let _ = stop_tx.send(true);
        while tasks.join_next().await.is_some() {}
    }
}
