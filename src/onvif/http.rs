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

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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
    /// Read and write timeout.
    pub io_timeout: Duration,
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
            io_timeout: Duration::from_secs(10),
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
#[derive(Debug, Clone, PartialEq, Eq)]
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

fn serve_connection(
    stream: TcpStream, service: &OnvifService, limits: &HttpLimits, stop: &AtomicBool,
) {
    let Ok(local) = stream.local_addr() else {
        return;
    };
    if stream.set_read_timeout(Some(limits.io_timeout)).is_err()
        || stream.set_write_timeout(Some(limits.io_timeout)).is_err()
    {
        return;
    }
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;

    for served in 1..=limits.max_requests_per_connection {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let req = match read_request(&mut reader, limits) {
            Ok(req) => req,
            Err(HttpError::Closed | HttpError::Io(_)) => return,
            Err(e) => {
                // Framing is unreliable after a bad request: answer once and close.
                let _ = write_response(&mut writer, &error_response(e.status()), false);
                return;
            }
        };
        let resp = service.handle(&ServiceRequest {
            method: &req.method,
            target: &req.target,
            authorization: req.header("authorization"),
            body: &req.body,
            local,
        });
        let keep = req.keep_alive
            && served < limits.max_requests_per_connection
            && !stop.load(Ordering::SeqCst);
        if write_response(&mut writer, &resp, keep).is_err() || !keep {
            return;
        }
    }
}

/// A running server. Dropping it stops accepting and lets in-flight connections finish.
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
        let local_addr = listener.local_addr().map_err(ServerError::Bind)?;
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicUsize::new(0));
        let accept_stop = stop.clone();
        let accept = thread::Builder::new()
            .name("onvif-accept".to_owned())
            .spawn(move || {
                for stream in listener.incoming() {
                    if accept_stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let Ok(stream) = stream else { continue };
                    if active.fetch_add(1, Ordering::SeqCst) >= limits.max_connections {
                        active.fetch_sub(1, Ordering::SeqCst);
                        let mut stream = stream;
                        let _ = stream.set_write_timeout(Some(limits.io_timeout));
                        let _ = write_response(&mut stream, &error_response(503), false);
                        continue;
                    }
                    let guard = ConnectionGuard(active.clone());
                    let (service, stop) = (service.clone(), accept_stop.clone());
                    let spawned =
                        thread::Builder::new()
                            .name("onvif-conn".to_owned())
                            .spawn(move || {
                                let _guard = guard;
                                serve_connection(stream, &service, &limits, &stop);
                            });
                    drop(spawned);
                }
            })
            .map_err(ServerError::Spawn)?;
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
    use super::*;
    use proptest::prelude::*;

    fn parse(bytes: &[u8]) -> Result<HttpRequest, HttpError> {
        read_request(&mut &bytes[..], &HttpLimits::default())
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
            assert_eq!(err.status(), *status, "{}", String::from_utf8_lossy(bytes));
        }
    }

    #[test]
    fn bounds_header_block() {
        let mut big = b"GET / HTTP/1.1\r\n".to_vec();
        big.extend(format!("X: {}\r\n\r\n", "a".repeat(9000)).bytes());
        assert!(matches!(parse(&big), Err(HttpError::HeaderTooLarge)));
        let mut many = b"GET / HTTP/1.1\r\n".to_vec();
        for i in 0..40 {
            many.extend(format!("X{i}: v\r\n").bytes());
        }
        many.extend(b"\r\n");
        assert!(matches!(parse(&many), Err(HttpError::HeaderTooLarge)));
        // A single endless line without a newline is bounded too.
        assert!(matches!(
            parse(&vec![b'a'; 100_000]),
            Err(HttpError::HeaderTooLarge)
        ));
    }

    #[test]
    fn clean_eof_is_closed() {
        assert!(matches!(parse(b""), Err(HttpError::Closed)));
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
