//! Listener, limits and shared state for the RTSP server.
//!
//! References:
//! - [RFC 2326, RTSP 1.0](https://www.rfc-editor.org/rfc/rfc2326)
//! - `docs/architecture.md`, "Security model"

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::ops::RangeInclusive;
use std::pin::Pin;
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
    /// Inspecting the listener address failed.
    #[error("cannot inspect RTSP listener: {0}")]
    LocalAddress(#[source] io::Error),
    /// The source's track cannot be described in SDP.
    #[error(transparent)]
    Track(#[from] SdpError),
    /// The configuration is unusable.
    #[error("invalid configuration: {0}")]
    Config(&'static str),
}

fn local_address(result: io::Result<SocketAddr>) -> Result<SocketAddr, ServerError> {
    result.map_err(ServerError::LocalAddress)
}

async fn accept_result(
    accepted: io::Result<(tokio::net::TcpStream, SocketAddr)>,
) -> Option<(tokio::net::TcpStream, SocketAddr)> {
    if let Ok(connection) = accepted {
        Some(connection)
    } else {
        // Typically EMFILE or ECONNABORTED: back off instead of spinning.
        tokio::time::sleep(Duration::from_millis(50)).await;
        None
    }
}

type Accepted = io::Result<(tokio::net::TcpStream, SocketAddr)>;
type Accept = for<'a> fn(&'a TcpListener) -> Pin<Box<dyn Future<Output = Accepted> + Send + 'a>>;

fn accept_socket(listener: &TcpListener) -> Pin<Box<dyn Future<Output = Accepted> + Send + '_>> {
    Box::pin(listener.accept())
}

async fn accept_ready(
    listener: &TcpListener, accept: Accept,
    inspect: fn(&tokio::net::TcpStream) -> io::Result<SocketAddr>,
) -> (tokio::net::TcpStream, SocketAddr, SocketAddr) {
    loop {
        let Some((stream, peer)) = accept_result(accept(listener).await).await else {
            continue;
        };
        let Ok(local) = inspect(&stream) else {
            continue;
        };
        return (stream, peer, local);
    }
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
    /// [`ServerError::LocalAddress`] if the socket cannot be inspected.
    pub fn local_addr(&self) -> Result<SocketAddr, ServerError> {
        local_address(self.listener.local_addr())
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
        let accept = accept_ready(
            &self.listener,
            accept_socket,
            tokio::net::TcpStream::local_addr,
        );
        tokio::pin!(accept);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                (stream, peer, local) = &mut accept => {
                    accept.set(accept_ready(&self.listener, accept_socket, tokio::net::TcpStream::local_addr));
                    let Ok(permit) = Arc::clone(&connections).try_acquire_owned() else {
                        drop(stream);
                        continue;
                    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    static ACCEPT_ATTEMPTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static ADDRESS_ATTEMPTS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    fn scripted_accept(
        listener: &TcpListener,
    ) -> Pin<Box<dyn Future<Output = Accepted> + Send + '_>> {
        Box::pin(async move {
            if ACCEPT_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Err(io::Error::other("synthetic accept failure"))
            } else {
                listener.accept().await
            }
        })
    }

    fn scripted_address(stream: &tokio::net::TcpStream) -> io::Result<SocketAddr> {
        if ADDRESS_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            Err(io::Error::other("synthetic address failure"))
        } else {
            stream.local_addr()
        }
    }

    #[tokio::test]
    async fn retries_accept_and_address_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let _first = tokio::net::TcpStream::connect(address).await.unwrap();
        let _second = tokio::net::TcpStream::connect(address).await.unwrap();
        let attempt = accept_ready(&listener, scripted_accept, scripted_address);
        tokio::pin!(attempt);
        std::future::poll_fn(|cx| {
            assert!(attempt.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(ACCEPT_ATTEMPTS.load(std::sync::atomic::Ordering::SeqCst), 1);
        let (_, _, local) = tokio::time::timeout(Duration::from_secs(2), &mut attempt)
            .await
            .unwrap();
        assert_eq!(local, address);
        assert_eq!(ACCEPT_ATTEMPTS.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(
            ADDRESS_ATTEMPTS.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }

    #[tokio::test]
    async fn accept_error_backs_off() {
        let failure = Err(io::Error::other("synthetic accept failure"));
        assert!(
            tokio::time::timeout(Duration::from_secs(2), accept_result(failure))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn local_address_preserves_io_error() {
        let error = local_address(Err(io::Error::other("synthetic failure"))).unwrap_err();
        assert_eq!(
            std::mem::discriminant(&error),
            std::mem::discriminant(&ServerError::LocalAddress(io::Error::other("other")))
        );
        assert!(error.to_string().contains("synthetic failure"));
        assert!(error.source().is_some());
        let address = "192.0.2.1:554".parse().unwrap();
        assert_eq!(local_address(Ok(address)).unwrap(), address);
    }
}
