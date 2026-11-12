//! A small, bounded HTTP/1.1 server for the ONVIF endpoints.
//!
//! Deliberately minimal, like the `rtsp` server: blocking `std::net`, one thread per
//! connection with a hard connection cap, per-read timeouts, bounded request line, header
//! block and body, no chunked request bodies, no `Expect`, keep-alive with a request cap.
//!
//! The request parser, [`read_request`], is total over arbitrary bytes.
//!
//! References:
//! - [HTTP/1.1, RFC 9112](https://www.rfc-editor.org/rfc/rfc9112)

use std::cell::Cell;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use thiserror::Error;

use super::service::{OnvifService, ServiceRequest, ServiceResponse};

/// Bounds applied to every connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpLimits {
    /// Maximum bytes of request line plus headers.
    pub max_header_bytes: usize,
    /// Maximum header fields.
    pub max_headers: usize,
    /// Maximum request body bytes.
    pub max_body_bytes: usize,
    /// Maximum simultaneous connections; extras get `503`.
    pub max_connections: usize,
    /// Absolute time a keep-alive connection may sit idle waiting for the next request.
    pub idle_timeout: Duration,
    /// Absolute time to receive the complete request line and headers, from the first byte.
    /// Progress does not extend it.
    pub head_timeout: Duration,
    /// Absolute time to receive the complete body once the head is in.
    pub body_timeout: Duration,
    /// Absolute time to write one complete response.
    pub write_timeout: Duration,
    /// Requests served on one connection before it is closed.
    pub max_requests_per_connection: usize,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            max_header_bytes: 8 * 1024,
            max_headers: 32,
            max_body_bytes: 64 * 1024,
            max_connections: 16,
            idle_timeout: Duration::from_secs(10),
            head_timeout: Duration::from_secs(10),
            body_timeout: Duration::from_secs(15),
            write_timeout: Duration::from_secs(10),
            max_requests_per_connection: 64,
        }
    }
}

/// Why a request could not be read.
#[derive(Debug, Error)]
pub enum HttpError {
    /// The peer closed the connection between requests.
    #[error("connection closed")]
    Closed,
    /// Socket failure or timeout.
    #[error("i/o: {0}")]
    Io(#[from] io::Error),
    /// Request line plus headers too large.
    #[error("header block too large")]
    HeaderTooLarge,
    /// Body larger than the limit.
    #[error("body too large")]
    BodyTooLarge,
    /// Not valid HTTP.
    #[error("malformed request: {0}")]
    Malformed(&'static str),
    /// `Transfer-Encoding` on a request.
    #[error("transfer encoding unsupported")]
    UnsupportedTransferEncoding,
    /// `POST` without `Content-Length`.
    #[error("content length required")]
    LengthRequired,
}

/// Why the server could not start.
#[derive(Debug, Error)]
pub enum ServerError {
    /// Binding or inspecting the listener failed.
    #[error("bind failed: {0}")]
    Bind(#[source] io::Error),
    /// The accept thread could not be spawned.
    #[error("thread spawn failed: {0}")]
    Spawn(#[source] io::Error),
}

impl HttpError {
    fn status(&self) -> u16 {
        match self {
            Self::BodyTooLarge => 413,
            Self::HeaderTooLarge => 431,
            Self::UnsupportedTransferEncoding => 501,
            Self::LengthRequired => 411,
            Self::Closed | Self::Io(_) | Self::Malformed(_) => 400,
        }
    }
}

/// A parsed request.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpRequest {
    /// Method, as sent.
    pub method: String,
    /// Request-target, as sent.
    pub target: String,
    /// Headers with lowercased names, in order.
    pub headers: Vec<(String, String)>,
    /// Body.
    pub body: Vec<u8>,
    /// Whether the connection may be reused after this request.
    pub keep_alive: bool,
}

impl std::fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpRequest")
            .field("contents", &"<redacted>")
            .finish()
    }
}

impl HttpRequest {
    /// Value of the first header with this (lowercase) name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Read one line (without its terminator) charging it against `budget`.
/// `Ok(None)` means clean EOF before any byte.
fn read_line<R: BufRead>(r: &mut R, budget: &mut usize) -> Result<Option<Vec<u8>>, HttpError> {
    let mut line = Vec::new();
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(HttpError::Malformed("truncated header"))
            };
        }
        let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        if take > *budget {
            return Err(HttpError::HeaderTooLarge);
        }
        *budget -= take;
        line.extend(chunk.iter().take(take));
        r.consume(take);
        if done {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
}

fn token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Read and validate one request. Total: any byte stream yields a request or an error.
///
/// # Errors
///
/// Returns [`HttpError`]; [`HttpError::Closed`] is the clean end of a keep-alive connection.
pub fn read_request<R: BufRead>(r: &mut R, limits: &HttpLimits) -> Result<HttpRequest, HttpError> {
    read_request_phased(r, limits, &mut |_| {})
}

/// Progress points inside [`read_request_phased`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The head is parsed and the body is about to be read.
    Body,
}

/// Like [`read_request`], calling `on_phase` at each transition so a caller can arm a new
/// deadline on the underlying transport.
///
/// # Errors
///
/// As [`read_request`].
pub fn read_request_phased<R: BufRead>(
    r: &mut R, limits: &HttpLimits, on_phase: &mut dyn FnMut(Phase),
) -> Result<HttpRequest, HttpError> {
    let mut budget = limits.max_header_bytes;
    let request_line = read_line(r, &mut budget)?.ok_or(HttpError::Closed)?;
    let request_line =
        String::from_utf8(request_line).map_err(|_| HttpError::Malformed("request line"))?;
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HttpError::Malformed("request line"));
    };
    if method.is_empty() || !method.bytes().all(token_char) {
        return Err(HttpError::Malformed("method"));
    }
    if !target.starts_with('/') || target.bytes().any(|b| b.is_ascii_control()) {
        return Err(HttpError::Malformed("target"));
    }
    let http11 = match version {
        "HTTP/1.1" => true,
        "HTTP/1.0" => false,
        _ => return Err(HttpError::Malformed("version")),
    };

    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let line = read_line(r, &mut budget)?.ok_or(HttpError::Malformed("truncated header"))?;
        if line.is_empty() {
            break;
        }
        if headers.len() >= limits.max_headers {
            return Err(HttpError::HeaderTooLarge);
        }
        let line = String::from_utf8(line).map_err(|_| HttpError::Malformed("header"))?;
        let (name, value) = line
            .split_once(':')
            .ok_or(HttpError::Malformed("header field"))?;
        if name.is_empty() || !name.bytes().all(token_char) {
            return Err(HttpError::Malformed("header name"));
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }

    let mut request = HttpRequest {
        method: method.to_owned(),
        target: target.to_owned(),
        headers,
        body: Vec::new(),
        keep_alive: http11,
    };
    if let Some(conn) = request.header("connection") {
        let conn = conn.to_ascii_lowercase();
        if conn.split(',').any(|t| t.trim() == "close") {
            request.keep_alive = false;
        } else if conn.split(',').any(|t| t.trim() == "keep-alive") {
            request.keep_alive = true;
        }
    }
    if request.header("transfer-encoding").is_some() {
        return Err(HttpError::UnsupportedTransferEncoding);
    }

    let mut lengths = request
        .headers
        .iter()
        .filter(|(n, _)| n == "content-length");
    let length = match (lengths.next(), lengths.next()) {
        (None, _) => None,
        (Some((_, v)), None) => Some(
            v.parse::<usize>()
                .ok()
                .filter(|_| v.bytes().all(|b| b.is_ascii_digit()))
                .ok_or(HttpError::Malformed("content-length"))?,
        ),
        (Some(_), Some(_)) => return Err(HttpError::Malformed("duplicate content-length")),
    };
    match length {
        Some(n) if n > limits.max_body_bytes => return Err(HttpError::BodyTooLarge),
        Some(n) => {
            on_phase(Phase::Body);
            let mut body = Vec::new();
            let got = r.take(n as u64).read_to_end(&mut body)?;
            if got != n {
                return Err(HttpError::Malformed("truncated body"));
            }
            request.body = body;
        }
        None if request.method == "POST" => return Err(HttpError::LengthRequired),
        None => {}
    }
    Ok(request)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Service Unavailable",
    }
}

/// Serialize a response.
///
/// # Errors
///
/// Returns [`HttpError::Io`] with the underlying write error.
pub fn write_response<W: Write>(
    w: &mut W, resp: &ServiceResponse, keep_alive: bool,
) -> Result<(), HttpError> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: {}\r\n",
        resp.status,
        reason(resp.status),
        resp.content_type,
        resp.body.len(),
        if keep_alive { "keep-alive" } else { "close" },
    );
    if let Some(challenge) = &resp.www_authenticate {
        head.push_str("WWW-Authenticate: ");
        head.push_str(challenge);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    w.write_all(head.as_bytes())?;
    w.write_all(&resp.body)?;
    w.flush()?;
    Ok(())
}

fn error_response(status: u16) -> ServiceResponse {
    ServiceResponse {
        status,
        content_type: "text/plain; charset=utf-8",
        body: Vec::new(),
        www_authenticate: None,
    }
}

/// Decrements the connection counter on drop.
struct ConnectionGuard(Arc<AtomicUsize>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Longest deadline honoured; larger configured values are clamped so arming never overflows
/// `Instant` (e.g. `Duration::MAX`).
const MAX_DEADLINE: Duration = Duration::from_secs(365 * 24 * 3600);

/// A deadline `d` from now, clamped to [`MAX_DEADLINE`].
fn after(d: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(d.min(MAX_DEADLINE)).unwrap_or(now)
}

/// A socket wrapper enforcing one ABSOLUTE deadline across reads and writes.
///
/// Before every underlying operation the remaining time to the shared deadline is computed
/// from a monotonic clock and installed as the socket timeout, so a peer trickling one byte
/// per read still hits the deadline. The deadline is shared (`Rc<Cell>`) with the connection
/// loop, which re-arms it per phase.
struct DeadlineIo {
    stream: TcpStream,
    deadline: Rc<Cell<Instant>>,
}

impl DeadlineIo {
    fn remaining(&self) -> io::Result<Duration> {
        let left = self
            .deadline
            .get()
            .saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err(io::Error::new(io::ErrorKind::TimedOut, "deadline exceeded"))
        } else {
            Ok(left)
        }
    }
}

impl DeadlineIo {
    /// Arm a transport timeout from the absolute deadline, propagating setup failures.
    fn arm(
        &self, set_timeout: &mut dyn FnMut(Option<Duration>) -> io::Result<()>,
    ) -> io::Result<()> {
        set_timeout(Some(self.remaining()?))
    }
}

impl Read for DeadlineIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.arm(&mut |timeout| self.stream.set_read_timeout(timeout))?;
        self.stream.read(buf)
    }
}

impl Write for DeadlineIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.arm(&mut |timeout| self.stream.set_write_timeout(timeout))?;
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

fn serve_connection(
    stream: TcpStream, service: &OnvifService, limits: &HttpLimits, stop: &AtomicBool,
) {
    let local = stream.local_addr();
    let read_half = stream.try_clone();
    serve_connection_prepared(stream, service, limits, stop, &local, read_half);
}

fn serve_connection_prepared(
    stream: TcpStream, service: &OnvifService, limits: &HttpLimits, stop: &AtomicBool,
    local: &io::Result<SocketAddr>, read_half: io::Result<TcpStream>,
) {
    let Ok(local) = local else {
        return;
    };
    let Ok(read_half) = read_half else {
        return;
    };
    let deadline = Rc::new(Cell::new(Instant::now()));
    let mut reader = BufReader::new(DeadlineIo {
        stream: read_half,
        deadline: deadline.clone(),
    });
    let mut writer = DeadlineIo {
        stream,
        deadline: deadline.clone(),
    };

    for served in 1..=limits.max_requests_per_connection {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        // Idle: wait for the first byte of the next request, bounded absolutely.
        deadline.set(after(limits.idle_timeout));
        match reader.fill_buf() {
            Ok(buffered) if !buffered.is_empty() => {}
            _ => return,
        }
        // Head, then body, each with its own absolute deadline.
        deadline.set(after(limits.head_timeout));
        let body_timeout = limits.body_timeout;
        let req = match read_request_phased(&mut reader, limits, &mut |Phase::Body| {
            deadline.set(after(body_timeout));
        }) {
            Ok(req) => req,
            Err(HttpError::Closed | HttpError::Io(_)) => return,
            Err(e) => {
                // Framing is unreliable after a bad request: answer once and close.
                deadline.set(after(limits.write_timeout));
                let _ = write_response(&mut writer, &error_response(e.status()), false);
                return;
            }
        };
        let resp = service.handle(&ServiceRequest {
            method: &req.method,
            target: &req.target,
            authorization: req.header("authorization"),
            body: &req.body,
            local: *local,
        });
        let keep = req.keep_alive
            && served < limits.max_requests_per_connection
            && !stop.load(Ordering::SeqCst);
        deadline.set(after(limits.write_timeout));
        if write_response(&mut writer, &resp, keep).is_err() || !keep {
            return;
        }
    }
}

/// Owned work passed to the OS thread builder.
type ThreadTask = Box<dyn FnOnce() + Send>;

/// Spawn a named server task; keeping the builder here preserves OS spawn errors.
fn spawn_thread(name: &str, task: ThreadTask) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name(name.to_owned()).spawn(task)
}

/// Accept connections with bounded admission; failed accepts are retried and failed
/// connection spawns drop their task, releasing both the socket and admission guard.
fn accept_connections(
    incoming: &mut dyn Iterator<Item = io::Result<TcpStream>>, service: &Arc<OnvifService>,
    limits: HttpLimits, stop: &Arc<AtomicBool>, active: &Arc<AtomicUsize>,
    spawn: &mut dyn FnMut(ThreadTask) -> io::Result<JoinHandle<()>>,
) {
    let mut tasks: Vec<JoinHandle<()>> = Vec::new();
    for stream in incoming {
        tasks.retain(|task| !task.is_finished());
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = stream else { continue };
        if active.fetch_add(1, Ordering::SeqCst) >= limits.max_connections {
            active.fetch_sub(1, Ordering::SeqCst);
            let mut io = DeadlineIo {
                stream,
                deadline: Rc::new(Cell::new(after(limits.write_timeout))),
            };
            let _ = write_response(&mut io, &error_response(503), false);
            continue;
        }
        let guard = ConnectionGuard(active.clone());
        let (service, stop) = (service.clone(), stop.clone());
        if let Ok(task) = spawn(Box::new(move || {
            let _guard = guard;
            serve_connection(stream, &service, &limits, &stop);
        })) {
            tasks.push(task);
        }
    }
    // No detached connection threads survive shutdown. Their existing absolute I/O deadlines
    // and actor snapshot budgets still apply; shutdown never cancels a PTP operation.
    for task in tasks {
        let _ = task.join();
    }
}

/// A running server. Dropping it stops accepting and joins in-flight connections.
#[derive(Debug)]
pub struct HttpServer {
    local_addr: SocketAddr,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl HttpServer {
    /// Bind `addr` and start serving. Pass port 0 to let the OS choose; read it back with
    /// [`HttpServer::local_addr`].
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when binding or starting the accept thread fails.
    pub fn start(
        addr: SocketAddr, service: Arc<OnvifService>, limits: HttpLimits,
    ) -> Result<Self, ServerError> {
        let listener = TcpListener::bind(addr).map_err(ServerError::Bind)?;
        let local_addr = listener.local_addr();
        let stop = Arc::new(AtomicBool::new(false));
        let accept_stop = stop.clone();
        Self::start_prepared(
            local_addr,
            stop,
            Box::new(move || {
                accept_connections(
                    &mut listener.incoming(),
                    &service,
                    limits,
                    &accept_stop,
                    &Arc::new(AtomicUsize::new(0)),
                    &mut |task| spawn_thread("onvif-conn", task),
                );
            }),
            &mut |task| spawn_thread("onvif-accept", task),
        )
    }

    /// Finish listener setup through the same error paths for real and injected OS results.
    fn start_prepared(
        local_addr: io::Result<SocketAddr>, stop: Arc<AtomicBool>, task: ThreadTask,
        spawn: &mut dyn FnMut(ThreadTask) -> io::Result<JoinHandle<()>>,
    ) -> Result<Self, ServerError> {
        let local_addr = local_addr.map_err(ServerError::Bind)?;
        let accept = spawn(task).map_err(ServerError::Spawn)?;
        Ok(Self {
            local_addr,
            stop,
            accept: Some(accept),
        })
    }

    /// The bound address.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stop accepting and join the accept thread. Idempotent.
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.accept.take() {
            // Wake the blocking `accept` with a throwaway connection.
            let ip = match self.local_addr.ip() {
                IpAddr::V4(a) if a.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(a) if a.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
                ip => ip,
            };
            let _ = TcpStream::connect_timeout(
                &SocketAddr::new(ip, self.local_addr.port()),
                Duration::from_secs(1),
            );
            let _ = handle.join();
        }
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn exhausted_accept_iterator_joins_every_spawned_connection() {
        let listener = super::TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = super::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        drop(peer);
        let active = super::Arc::new(super::AtomicUsize::new(0));
        super::accept_connections(
            &mut [Ok(stream)].into_iter(),
            &super::Arc::new(service()),
            super::HttpLimits::default(),
            &super::Arc::new(super::AtomicBool::new(false)),
            &active,
            &mut |task| super::spawn_thread("synthetic-http-connection", task),
        );
        assert_eq!(active.load(super::Ordering::SeqCst), 0);
    }
    use super::*;
    use proptest::prelude::*;

    struct TestReader<'a> {
        bytes: &'a [u8],
        fail_when_empty: bool,
    }

    impl Read for TestReader<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.bytes.is_empty() && self.fail_when_empty {
                return Err(io::Error::other("synthetic read failure"));
            }
            self.bytes.read(out)
        }
    }

    impl BufRead for TestReader<'_> {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            if self.bytes.is_empty() && self.fail_when_empty {
                return Err(io::Error::other("synthetic read failure"));
            }
            Ok(self.bytes)
        }
        fn consume(&mut self, amount: usize) {
            self.bytes.consume(amount);
        }
    }

    fn parse(bytes: &[u8]) -> Result<HttpRequest, HttpError> {
        read_request(
            &mut TestReader {
                bytes,
                fail_when_empty: false,
            },
            &HttpLimits::default(),
        )
    }

    #[test]
    fn parser_propagates_io_errors_during_headers_and_body() {
        for bytes in [
            b"GET / HTTP/1.1\r\n".as_slice(),
            b"POST / HTTP/1.1\r\nContent-Length: 3\r\n\r\nx".as_slice(),
        ] {
            let mut reader = TestReader {
                bytes,
                fail_when_empty: true,
            };
            assert_eq!(
                read_request(&mut reader, &HttpLimits::default())
                    .unwrap_err()
                    .status(),
                400
            );
        }
    }

    fn service() -> OnvifService {
        use crate::onvif::{auth::Credentials, model::example_model, time::ManualClock};
        OnvifService::with_digest_secret(
            example_model(),
            Credentials::new("operator", "synthetic-pass").unwrap(),
            Arc::new(ManualClock::new(0)),
            [9; 32],
        )
    }

    #[test]
    fn socket_setup_failures_and_stopped_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let _peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (socket, _) = listener.accept().unwrap();
        let error = || io::Error::other("synthetic failure");
        let limits = HttpLimits::default();
        let stopped = AtomicBool::new(true);
        serve_connection_prepared(
            socket.try_clone().unwrap(),
            &service(),
            &limits,
            &stopped,
            &Err(error()),
            Ok(socket.try_clone().unwrap()),
        );
        serve_connection_prepared(
            socket.try_clone().unwrap(),
            &service(),
            &limits,
            &stopped,
            &socket.local_addr(),
            Err(error()),
        );
        serve_connection(socket, &service(), &limits, &stopped);
    }

    #[test]
    fn deadline_timeout_setup_propagates_errors_without_extending_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_socket, _) = listener.accept().unwrap();
        let deadline = after(Duration::from_secs(1));
        let io = DeadlineIo {
            stream: peer,
            deadline: Rc::new(Cell::new(deadline)),
        };
        let mut calls = 0;
        let error = io
            .arm(&mut |timeout| {
                calls += 1;
                let remaining = timeout.unwrap();
                assert!(!remaining.is_zero());
                assert!(remaining <= Duration::from_secs(1));
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "synthetic timeout setup failure",
                ))
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(calls, 1);
        assert_eq!(io.deadline.get(), deadline);
    }

    #[test]
    fn listener_inspection_and_accept_thread_spawn_failures_propagate() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut spawn_calls = 0;
        let mut spawn = |task: ThreadTask| {
            spawn_calls += 1;
            drop(task);
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "synthetic thread limit",
            ))
        };
        let error = HttpServer::start_prepared(
            Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "synthetic listener inspection failure",
            )),
            stop.clone(),
            Box::new(thread::yield_now),
            &mut spawn,
        )
        .unwrap_err();
        assert!(
            matches!(error, ServerError::Bind(ref e) if e.kind() == io::ErrorKind::NotConnected)
        );
        let error = HttpServer::start_prepared(
            Ok("127.0.0.1:0".parse().unwrap()),
            stop,
            Box::new(thread::yield_now),
            &mut spawn,
        )
        .unwrap_err();
        assert!(
            matches!(error, ServerError::Spawn(ref e) if e.kind() == io::ErrorKind::WouldBlock)
        );
        assert_eq!(spawn_calls, 1);
    }

    #[test]
    fn accept_retries_errors_and_failed_connection_spawn_releases_admission() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (socket, _) = listener.accept().unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let calls = Cell::new(0);
        let mut spawn = |task: ThreadTask| {
            calls.set(calls.get() + 1);
            assert_eq!(active.load(Ordering::SeqCst), 1);
            drop(task);
            Err(io::Error::other("synthetic connection spawn failure"))
        };
        accept_connections(
            &mut [
                Err(io::Error::other("synthetic accept failure")),
                Ok(socket),
            ]
            .into_iter(),
            &Arc::new(service()),
            HttpLimits::default(),
            &Arc::new(AtomicBool::new(false)),
            &active,
            &mut spawn,
        );
        let mut rejected = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        rejected
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (socket, _) = listener.accept().unwrap();
        accept_connections(
            &mut [Ok(socket)].into_iter(),
            &Arc::new(service()),
            HttpLimits {
                max_connections: 0,
                ..HttpLimits::default()
            },
            &Arc::new(AtomicBool::new(false)),
            &active,
            &mut spawn,
        );
        let mut response = String::new();
        rejected.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 503 "));
        assert_eq!(calls.get(), 1);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn deadline_reader_and_writer_failures_are_bounded() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (socket, _) = listener.accept().unwrap();
        let deadline = Rc::new(Cell::new(after(Duration::from_millis(50))));
        let mut reader = BufReader::new(DeadlineIo {
            stream: socket.try_clone().unwrap(),
            deadline: deadline.clone(),
        });
        peer.write_all(b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nx")
            .unwrap();
        assert!(read_request_phased(&mut reader, &HttpLimits::default(), &mut |_| {}).is_err());
        deadline.set(Instant::now());
        let mut writer = DeadlineIo {
            stream: socket,
            deadline,
        };
        assert!(write_response(&mut writer, &error_response(500), false).is_err());
    }

    #[test]
    fn connection_pipeline_body_deadline_and_write_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        for (request, zero_write, responses) in [
            (
                b"GET /unknown HTTP/1.1\r\nContent-Length: 1\r\n\r\nxGET /unknown HTTP/1.1\r\n\r\n"
                    .as_slice(),
                false,
                2,
            ),
            (b"GET /unknown HTTP/1.1\r\n\r\n".as_slice(), true, 0),
            (b"GET /unknown HTTP/1.1\r\n".as_slice(), false, 0),
            (b"".as_slice(), false, 0),
        ] {
            let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let (socket, _) = listener.accept().unwrap();
            peer.write_all(request).unwrap();
            if request.is_empty() {
                peer.shutdown(std::net::Shutdown::Write).unwrap();
            }
            let limits = HttpLimits {
                max_requests_per_connection: 2,
                head_timeout: Duration::from_millis(50),
                write_timeout: if zero_write {
                    Duration::ZERO
                } else {
                    Duration::from_secs(1)
                },
                ..HttpLimits::default()
            };
            serve_connection(socket, &service(), &limits, &AtomicBool::new(false));
            let mut response = String::new();
            peer.read_to_string(&mut response).unwrap();
            assert_eq!(response.matches("HTTP/1.1 404 ").count(), responses);
        }
    }

    #[test]
    fn loopback_server_handles_success_and_framing_errors() {
        let limits = HttpLimits {
            max_header_bytes: 100,
            idle_timeout: Duration::from_millis(100),
            head_timeout: Duration::from_millis(100),
            body_timeout: Duration::from_millis(100),
            ..HttpLimits::default()
        };
        let mut server =
            HttpServer::start("127.0.0.1:0".parse().unwrap(), Arc::new(service()), limits).unwrap();
        for (request, status) in [
            (
                b"GET /unknown HTTP/1.1\r\nConnection: close\r\n\r\n".as_slice(),
                "404",
            ),
            (b"POST / HTTP/1.1\r\n\r\n".as_slice(), "411"),
            (b"GET / HTTP/1.1\r\nInvalid\r\n\r\n".as_slice(), "400"),
            (
                b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n".as_slice(),
                "501",
            ),
        ] {
            let mut peer = TcpStream::connect(server.local_addr()).unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            peer.write_all(request).unwrap();
            let mut response = String::new();
            peer.read_to_string(&mut response).unwrap();
            assert!(
                response.starts_with(&format!("HTTP/1.1 {status} ")),
                "{response}"
            );
        }
        server.shutdown();
    }

    #[test]
    fn shutdown_wakes_wildcard_listeners() {
        let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(
            HttpServer::start(
                occupied.local_addr().unwrap(),
                Arc::new(service()),
                HttpLimits::default()
            )
            .is_err()
        );
        let mut ipv4 = HttpServer::start(
            "0.0.0.0:0".parse().unwrap(),
            Arc::new(service()),
            HttpLimits::default(),
        )
        .unwrap();
        ipv4.shutdown();

        shutdown_ipv6(HttpServer::start(
            "[::]:0".parse().unwrap(),
            Arc::new(service()),
            HttpLimits::default(),
        ));
    }

    /// Only explicit address-family unavailability may skip the IPv6 environment check.
    fn ipv6_unavailable(error: &io::Error) -> bool {
        // Rust does not give EAFNOSUPPORT a distinct stable ErrorKind. Only interpret
        // a raw code on the platform that defines it, never another OS's errno table.
        error.kind() == io::ErrorKind::Unsupported
            || error
                .raw_os_error()
                .is_some_and(|code| Some(code) == FAMILY_UNAVAILABLE_CODE)
    }

    const FAMILY_UNAVAILABLE_CODE: Option<i32> = {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            Some(97)
        }
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        ))]
        {
            Some(47)
        }
        #[cfg(windows)]
        {
            Some(10047)
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly",
            windows
        )))]
        {
            None
        }
    };

    fn shutdown_ipv6(result: Result<HttpServer, ServerError>) {
        match result {
            Ok(mut ipv6) => ipv6.shutdown(),
            Err(error) => assert!(
                matches!(error, ServerError::Bind(ref e) if ipv6_unavailable(e)),
                "unexpected IPv6 startup failure: {error}"
            ),
        }
    }

    #[test]
    fn ipv6_skip_is_limited_to_unsupported_address_families() {
        shutdown_ipv6(Err(ServerError::Bind(io::Error::new(
            io::ErrorKind::Unsupported,
            "synthetic unsupported family",
        ))));
        assert!(
            FAMILY_UNAVAILABLE_CODE
                .is_none_or(|code| { ipv6_unavailable(&io::Error::from_raw_os_error(code)) })
        );
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::AddrInUse,
            io::ErrorKind::AddrNotAvailable,
            io::ErrorKind::Other,
        ] {
            assert!(!ipv6_unavailable(&io::Error::new(
                kind,
                "synthetic non-family error"
            )));
        }
    }

    #[test]
    fn status_codes_and_reasons_cover_all_responses() {
        assert_eq!(HttpError::HeaderTooLarge.status(), 431);
        for (status, text) in [
            (405, "Method Not Allowed"),
            (411, "Length Required"),
            (431, "Request Header Fields Too Large"),
            (500, "Internal Server Error"),
            (501, "Not Implemented"),
        ] {
            assert_eq!(reason(status), text);
        }
    }

    #[test]
    fn malformed_headers_and_connection_tokens() {
        for bytes in [
            b"GET / HTTP/1.1\r\n".as_slice(),
            b"GET / HTTP/1.1\r\nTruncated".as_slice(),
            b"GET / HTTP/1.1\r\nX: \xff\r\n\r\n".as_slice(),
        ] {
            assert!(parse(bytes).is_err());
        }
        assert!(parse(b"GET / HTTP/1.1\n\n").unwrap().keep_alive);
        assert!(parse(b"\xff / HTTP/1.1\r\n\r\n").is_err());
        assert!(
            parse(b"GET / HTTP/1.1\r\nConnection: upgrade\r\n\r\n")
                .unwrap()
                .keep_alive
        );
    }

    #[test]
    fn parsed_requests_redact_debug() {
        let request = parse(b"POST / HTTP/1.1\r\nAuthorization: synthetic-secret\r\nContent-Length: 16\r\n\r\nsynthetic-secret").unwrap();
        assert!(!format!("{request:?}").contains("synthetic-secret"));
        let service = ServiceRequest {
            method: &request.method,
            target: &request.target,
            authorization: request.header("authorization"),
            body: &request.body,
            local: "192.0.2.1:80".parse().unwrap(),
        };
        assert!(!format!("{service:?}").contains("synthetic-secret"));
    }

    #[test]
    fn parses_post_with_body() {
        let r = parse(b"POST /onvif/device_service HTTP/1.1\r\nHost: 192.0.2.1\r\nContent-Length: 3\r\n\r\nabc")
            .unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.target, "/onvif/device_service");
        assert_eq!(r.body, b"abc");
        assert_eq!(r.header("host"), Some("192.0.2.1"));
        assert!(r.keep_alive);
    }

    #[test]
    fn connection_semantics() {
        assert!(!parse(b"GET / HTTP/1.0\r\n\r\n").unwrap().keep_alive);
        assert!(
            parse(b"GET / HTTP/1.0\r\nConnection: keep-alive\r\n\r\n")
                .unwrap()
                .keep_alive
        );
        assert!(
            !parse(b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n")
                .unwrap()
                .keep_alive
        );
    }

    #[test]
    fn rejects_bad_requests() {
        let cases: &[(&[u8], u16)] = &[
            (b"GET / HTTP/2\r\n\r\n", 400),
            (b"GET /\r\n\r\n", 400),
            (b"GET http://x/ HTTP/1.1\r\n\r\n", 400),
            (b"G@T / HTTP/1.1\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nbad header\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\n x: y\r\n\r\n", 400),
            (b"POST / HTTP/1.1\r\n\r\n", 411),
            (
                b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
                501,
            ),
            (
                b"POST / HTTP/1.1\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx",
                400,
            ),
            (b"POST / HTTP/1.1\r\nContent-Length: -1\r\n\r\n", 400),
            (b"POST / HTTP/1.1\r\nContent-Length: 999999\r\n\r\n", 413),
            (b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nab", 400),
        ];
        for (bytes, status) in cases {
            let err = parse(bytes).unwrap_err();
            assert_eq!(err.status(), *status);
        }
    }

    #[test]
    fn bounds_header_block() {
        let mut big = b"GET / HTTP/1.1\r\n".to_vec();
        big.extend(format!("X: {}\r\n\r\n", "a".repeat(9000)).bytes());
        assert_eq!(parse(&big).unwrap_err().status(), 431);
        let mut many = b"GET / HTTP/1.1\r\n".to_vec();
        for i in 0..40 {
            many.extend(format!("X{i}: v\r\n").bytes());
        }
        many.extend(b"\r\n");
        assert_eq!(parse(&many).unwrap_err().status(), 431);
        // A single endless line without a newline is bounded too.
        assert_eq!(parse(&vec![b'a'; 100_000]).unwrap_err().status(), 431);
    }

    #[test]
    fn deadline_io_times_out_reads_and_writes_once_expired() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_server, _) = listener.accept().unwrap();
        let mut io = DeadlineIo {
            stream: client,
            deadline: Rc::new(Cell::new(Instant::now())),
        };
        let mut buf = [0_u8; 1];
        assert_eq!(
            io.read(&mut buf).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(io.write(b"x").unwrap_err().kind(), io::ErrorKind::TimedOut);
        io.deadline.set(Instant::now() + Duration::from_millis(50));
        let started = Instant::now();
        // Nothing is ever sent: the read must give up at the deadline, not block.
        assert!(io.read(&mut buf).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn clean_eof_is_closed() {
        assert_eq!(
            parse(b"").unwrap_err().to_string(),
            HttpError::Closed.to_string()
        );
    }

    struct ControlledWriter {
        bytes: Vec<u8>,
        fail: usize,
        calls: usize,
    }

    impl Write for ControlledWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls == self.fail {
                return Err(io::Error::other("synthetic write failure"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.calls += 1;
            if self.calls == self.fail {
                return Err(io::Error::other("synthetic flush failure"));
            }
            Ok(())
        }
    }

    #[test]
    fn response_io_errors_and_all_status_reasons() {
        for fail in 0..=3 {
            let mut writer = ControlledWriter {
                bytes: vec![],
                fail,
                calls: 0,
            };
            let response = ServiceResponse {
                status: 200,
                content_type: "text/plain",
                body: vec![1],
                www_authenticate: None,
            };
            assert_eq!(
                write_response(&mut writer, &response, false).is_ok(),
                fail == 0
            );
        }
        for status in [200, 400, 401, 404, 405, 411, 413, 431, 500, 501, 503] {
            for keep in [false, true] {
                let mut writer = ControlledWriter {
                    bytes: vec![],
                    fail: 0,
                    calls: 0,
                };
                let mut response = error_response(status);
                response.www_authenticate = Some("synthetic".into());
                write_response(&mut writer, &response, keep).unwrap();
                assert!(
                    String::from_utf8(writer.bytes)
                        .unwrap()
                        .contains(reason(status))
                );
            }
        }
    }

    #[test]
    fn writes_challenge() {
        let resp = ServiceResponse {
            status: 401,
            content_type: "text/plain",
            body: b"x".to_vec(),
            www_authenticate: Some("Digest realm=\"r\"".into()),
        };
        let mut out = Vec::new();
        write_response(&mut out, &resp, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 401 Unauthorized\r\n"));
        assert!(text.contains("WWW-Authenticate: Digest realm=\"r\"\r\n"));
        assert!(text.contains("Connection: close"));
        assert!(text.ends_with("\r\n\r\nx"));
    }

    proptest! {
        #[test]
        fn read_request_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..1024)) {
            let _ = parse(&bytes);
        }

        #[test]
        fn read_request_never_panics_on_header_shaped_input(
            s in "[A-Za-z/ :.0-9\\r\\n-]{0,300}"
        ) {
            let _ = parse(s.as_bytes());
        }
    }
}
